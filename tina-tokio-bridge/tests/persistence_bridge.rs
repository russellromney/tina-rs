use std::collections::VecDeque;
use std::convert::Infallible;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tina::{Mailbox, TrySendError, prelude::*};
use tina_runtime::{
    CallError, JournalReplay, MailboxFactory, SnapshotImage, ThreadedRuntimeConfig, journal_append,
    journal_replay, snapshot_load,
};
use tina_tokio_bridge::{BridgeHost, BridgeMessage, BridgeRequest, BridgeResponder};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct BridgeShard;

impl Shard for BridgeShard {
    fn id(&self) -> ShardId {
        ShardId::new(36)
    }
}

struct BridgeMailbox<T> {
    capacity: usize,
    queue: Mutex<VecDeque<T>>,
    closed: Mutex<bool>,
}

impl<T> BridgeMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: Mutex::new(VecDeque::new()),
            closed: Mutex::new(false),
        }
    }
}

impl<T> Mailbox<T> for BridgeMailbox<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }

    fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        if *self.closed.lock().expect("closed mutex") {
            return Err(TrySendError::Closed(message));
        }
        let mut queue = self.queue.lock().expect("queue mutex");
        if queue.len() == self.capacity {
            return Err(TrySendError::Full(message));
        }
        queue.push_back(message);
        Ok(())
    }

    fn recv(&self) -> Option<T> {
        self.queue.lock().expect("queue mutex").pop_front()
    }

    fn close(&self) {
        *self.closed.lock().expect("closed mutex") = true;
    }
}

#[derive(Debug, Clone, Copy)]
struct BridgeMailboxFactory;

impl MailboxFactory for BridgeMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(BridgeMailbox::new(capacity))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PersistRequest {
    Recover,
    Add(String),
    Get,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PersistReply {
    values: Vec<String>,
}

enum PersistBridgeMsg {
    Request(BridgeRequest<PersistRequest, PersistReply>),
    SnapshotLoaded(
        Result<Option<SnapshotImage>, CallError>,
        BridgeResponder<PersistReply>,
    ),
    JournalLoaded(
        Result<JournalReplay, CallError>,
        BridgeResponder<PersistReply>,
    ),
    MutationDurable(
        Result<(), CallError>,
        u64,
        Vec<u8>,
        BridgeResponder<PersistReply>,
    ),
}

impl From<BridgeRequest<PersistRequest, PersistReply>> for PersistBridgeMsg {
    fn from(request: BridgeRequest<PersistRequest, PersistReply>) -> Self {
        Self::Request(request)
    }
}

impl BridgeMessage for PersistBridgeMsg {
    fn bridge_cancelled(&self) -> bool {
        match self {
            Self::Request(request) => request.is_cancelled(),
            Self::SnapshotLoaded(_, responder)
            | Self::JournalLoaded(_, responder)
            | Self::MutationDurable(_, _, _, responder) => responder.is_closed(),
        }
    }
}

struct PersistService {
    snapshot_path: PathBuf,
    journal_path: PathBuf,
    values: Vec<String>,
    last_journal_index: u64,
    mutation_in_flight: bool,
    pending_mutations: VecDeque<(String, BridgeResponder<PersistReply>)>,
}

#[tina_runtime::isolate(message = PersistBridgeMsg, shard = BridgeShard)]
impl PersistService {
    fn handle(
        &mut self,
        msg: PersistBridgeMsg,
        _ctx: &mut Context<'_, BridgeShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            PersistBridgeMsg::Request(request) => {
                let (message, responder) = request.into_parts();
                match message {
                    PersistRequest::Recover => snapshot_load(self.snapshot_path.clone())
                        .then(move |result| PersistBridgeMsg::SnapshotLoaded(result, responder)),
                    PersistRequest::Add(value) => {
                        if self.mutation_in_flight {
                            self.pending_mutations.push_back((value, responder));
                            noop()
                        } else {
                            self.append_value(value, responder)
                        }
                    }
                    PersistRequest::Get => {
                        let _ = responder.respond(PersistReply {
                            values: self.values.clone(),
                        });
                        noop()
                    }
                }
            }
            PersistBridgeMsg::SnapshotLoaded(Ok(Some(snapshot)), responder) => {
                self.values = decode_values(&snapshot.bytes);
                self.last_journal_index = snapshot.last_journal_index;
                journal_replay(self.journal_path.clone())
                    .then(move |result| PersistBridgeMsg::JournalLoaded(result, responder))
            }
            PersistBridgeMsg::SnapshotLoaded(Ok(None), responder) => {
                journal_replay(self.journal_path.clone())
                    .then(move |result| PersistBridgeMsg::JournalLoaded(result, responder))
            }
            PersistBridgeMsg::SnapshotLoaded(Err(_), responder) => {
                let _ = responder.respond(PersistReply { values: Vec::new() });
                noop()
            }
            PersistBridgeMsg::JournalLoaded(Ok(replay), responder) => {
                for record in replay.records {
                    if record.index > self.last_journal_index {
                        self.values
                            .push(String::from_utf8(record.bytes).expect("utf8 record"));
                        self.last_journal_index = record.index;
                    }
                }
                let _ = responder.respond(PersistReply {
                    values: self.values.clone(),
                });
                noop()
            }
            PersistBridgeMsg::JournalLoaded(Err(_), responder) => {
                let _ = responder.respond(PersistReply { values: Vec::new() });
                noop()
            }
            PersistBridgeMsg::MutationDurable(Ok(()), index, record, responder) => {
                self.values
                    .push(String::from_utf8(record).expect("utf8 record"));
                self.last_journal_index = index;
                let _ = responder.respond(PersistReply {
                    values: self.values.clone(),
                });
                self.mutation_in_flight = false;
                self.append_next_pending()
            }
            PersistBridgeMsg::MutationDurable(Err(_), _, _, responder) => {
                let _ = responder.respond(PersistReply {
                    values: self.values.clone(),
                });
                self.mutation_in_flight = false;
                self.append_next_pending()
            }
        }
    }
}

impl PersistService {
    fn new(snapshot_path: PathBuf, journal_path: PathBuf) -> Self {
        Self {
            snapshot_path,
            journal_path,
            values: Vec::new(),
            last_journal_index: 0,
            mutation_in_flight: false,
            pending_mutations: VecDeque::new(),
        }
    }

    fn append_value(
        &mut self,
        value: String,
        responder: BridgeResponder<PersistReply>,
    ) -> Effect<Self> {
        let index = self.last_journal_index + 1;
        let record = value.into_bytes();
        self.mutation_in_flight = true;
        journal_append(self.journal_path.clone(), index, record.clone())
            .then(move |result| PersistBridgeMsg::MutationDurable(result, index, record, responder))
    }

    fn append_next_pending(&mut self) -> Effect<Self> {
        if let Some((value, responder)) = self.pending_mutations.pop_front() {
            self.append_value(value, responder)
        } else {
            noop()
        }
    }
}

fn decode_values(bytes: &[u8]) -> Vec<String> {
    if bytes.is_empty() {
        return Vec::new();
    }
    String::from_utf8(bytes.to_vec())
        .expect("utf8 snapshot")
        .split('\n')
        .map(str::to_owned)
        .collect()
}

fn unique_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tina-{name}-{nanos}"));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[tokio::test]
async fn tokio_bridge_request_persists_and_fresh_host_recovers() {
    let dir = unique_dir("bridge-persistence");
    let snapshot_path = dir.join("state.snapshot");
    let journal_path = dir.join("state.journal");

    let mut host = BridgeHost::new(
        BridgeShard,
        BridgeMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 16,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let bridge = host
        .register_bridge::<PersistService, PersistRequest, PersistReply, Infallible>(
            PersistService::new(snapshot_path.clone(), journal_path.clone()),
            16,
            Duration::from_secs(1),
        )
        .expect("register bridge persistence service");

    assert_eq!(
        bridge.call(PersistRequest::Add("hay".to_owned())).await,
        Ok(PersistReply {
            values: vec!["hay".to_owned()]
        })
    );
    assert_eq!(
        bridge.call(PersistRequest::Add("oats".to_owned())).await,
        Ok(PersistReply {
            values: vec!["hay".to_owned(), "oats".to_owned()]
        })
    );
    let llama = bridge.clone();
    let alpaca = bridge.clone();
    let (llama_reply, alpaca_reply) = tokio::join!(
        llama.call(PersistRequest::Add("llama".to_owned())),
        alpaca.call(PersistRequest::Add("alpaca".to_owned())),
    );
    assert!(llama_reply.is_ok());
    assert!(alpaca_reply.is_ok());
    let after_concurrent = bridge
        .call(PersistRequest::Get)
        .await
        .expect("get after concurrent writes");
    assert_eq!(after_concurrent.values.len(), 4);
    assert_eq!(
        &after_concurrent.values[..2],
        &["hay".to_owned(), "oats".to_owned()]
    );
    assert!(after_concurrent.values.contains(&"llama".to_owned()));
    assert!(after_concurrent.values.contains(&"alpaca".to_owned()));
    drop(llama);
    drop(alpaca);
    drop(bridge);
    let _trace = host.try_shutdown().expect("shutdown first host");

    let mut fresh_host = BridgeHost::new(
        BridgeShard,
        BridgeMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 16,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let fresh_bridge = fresh_host
        .register_bridge::<PersistService, PersistRequest, PersistReply, Infallible>(
            PersistService::new(snapshot_path, journal_path),
            16,
            Duration::from_secs(1),
        )
        .expect("register fresh bridge persistence service");

    assert_eq!(
        fresh_bridge.call(PersistRequest::Recover).await,
        Ok(after_concurrent.clone())
    );
    assert_eq!(
        fresh_bridge.call(PersistRequest::Get).await,
        Ok(after_concurrent)
    );
    drop(fresh_bridge);
    let trace = fresh_host.try_shutdown().expect("shutdown fresh host");
    assert!(trace.iter().any(|event| {
        matches!(
            event.kind(),
            tina_runtime::RuntimeEventKind::RecoveryFinished
        )
    }));
}
