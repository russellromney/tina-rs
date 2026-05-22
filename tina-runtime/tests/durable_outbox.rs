//! User-shaped proofs for the durable outbox over real journal rails.
//!
//! A webhook-style service records work before sending it, restarts, and
//! resumes the unsent work while never re-applying work it already completed.
//! These tests drive the outbox through the real runtime so the durable record
//! is an actual `journal_append`/`journal_replay` against a temp file.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use tina::{Mailbox, TrySendError, prelude::*};
use tina_runtime::{
    ApplyStatus, CallError, CommitConfidence, CompletionStart, DurableCompletion, DurableOutbox,
    DurableWork, JournalReplay, MailboxFactory, RecordError, RecordedWork, RecoveryError, Runtime,
    TailStatus, WorkId, journal_append, journal_replay,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct OutboxShard;

impl Shard for OutboxShard {
    fn id(&self) -> ShardId {
        ShardId::new(41)
    }
}

struct TestMailbox<T> {
    capacity: usize,
    queue: Mutex<VecDeque<T>>,
    closed: Mutex<bool>,
}

impl<T> TestMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: Mutex::new(VecDeque::new()),
            closed: Mutex::new(false),
        }
    }
}

impl<T> Mailbox<T> for TestMailbox<T> {
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
struct TestMailboxFactory;

impl MailboxFactory for TestMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(TestMailbox::new(capacity))
    }
}

/// Service messages. Continuation messages carry the staged token by move, the
/// blessed Tina pattern for threading runtime-owned work through a reply.
#[derive(Debug)]
enum WebhookMsg {
    /// Enqueue, record, send, then durably mark sent.
    Send(Vec<u8>),
    /// Enqueue, record, send, but stop before marking sent — simulates a crash
    /// after the side effect but before the completion is durable.
    SendThenCrash(Vec<u8>),
    Recorded(Result<(), CallError>, DurableWork<Vec<u8>>),
    Completed(Result<(), CallError>, DurableCompletion),
    Recover,
    RecoverLoaded(Result<JournalReplay, CallError>),
}

struct Observed {
    /// Payloads handed to the side effect ("sent"), in order.
    sent: Vec<String>,
    /// Free-form recovery / lifecycle notes.
    notes: Vec<String>,
}

struct WebhookService {
    journal_path: PathBuf,
    outbox: DurableOutbox<Vec<u8>>,
    /// Pending work recovered on restart, waiting to be resumed.
    resume: VecDeque<RecordedWork<Vec<u8>>>,
    observed: Arc<Mutex<Observed>>,
}

#[tina_runtime::isolate(message = WebhookMsg, shard = OutboxShard)]
impl WebhookService {
    fn handle(
        &mut self,
        msg: WebhookMsg,
        _ctx: &mut Context<'_, OutboxShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            WebhookMsg::Send(payload) => self.begin_send(payload, true),
            WebhookMsg::SendThenCrash(payload) => self.begin_send(payload, false),
            WebhookMsg::Recorded(result, staged) => self.on_recorded(result, staged),
            WebhookMsg::Completed(result, completion) => {
                match self.outbox.finish_complete(completion, result) {
                    Ok(committed) => self.note(format!("committed:{}", committed.work_id().0)),
                    Err(failed) => self.note(format!("complete-error:{:?}", failed.error)),
                }
                self.drive_resume()
            }
            WebhookMsg::Recover => {
                journal_replay(self.journal_path.clone()).then(WebhookMsg::RecoverLoaded)
            }
            WebhookMsg::RecoverLoaded(replay) => self.on_recovered(replay),
        }
    }
}

impl WebhookService {
    fn new(journal_path: PathBuf, capacity: usize, observed: Arc<Mutex<Observed>>) -> Self {
        Self {
            journal_path,
            outbox: DurableOutbox::new(capacity),
            resume: VecDeque::new(),
            observed,
        }
    }

    /// Stage and durably record one work item; carry both the token and whether
    /// to complete after sending into the continuation.
    fn begin_send(&mut self, payload: Vec<u8>, complete_after: bool) -> Effect<Self> {
        // Encode the "complete after send" intent into the payload so the
        // recorded continuation knows it without extra state.
        let framed = encode_intent(complete_after, &payload);
        match self.outbox.enqueue(framed) {
            Ok(staged) => {
                let index = staged.journal_index();
                let bytes = staged.journal_bytes().to_vec();
                journal_append(self.journal_path.clone(), index, bytes)
                    .then(move |result| WebhookMsg::Recorded(result, staged))
            }
            Err(full) => {
                self.note(format!("full:{}", as_text(&full.work)));
                noop()
            }
        }
    }

    fn on_recorded(
        &mut self,
        result: Result<(), CallError>,
        staged: DurableWork<Vec<u8>>,
    ) -> Effect<Self> {
        match self.outbox.record(staged, result) {
            Ok(recorded) => {
                let id = recorded.work_id();
                match self.outbox.apply(recorded) {
                    ApplyStatus::Apply(framed) => {
                        let (complete_after, payload) = decode_intent(&framed);
                        self.side_effect(&payload); // "send the webhook"
                        if complete_after {
                            self.complete(id)
                        } else {
                            self.note(format!("crash-before-complete:{}", id.0));
                            noop()
                        }
                    }
                    ApplyStatus::DuplicateWork(found) => {
                        self.note(format!("duplicate:{}", found.0));
                        noop()
                    }
                }
            }
            Err(RecordError::Append(failed)) => {
                self.note(format!("append-error:{:?}", failed.error));
                noop()
            }
            Err(RecordError::Stale(_stale)) => {
                self.note("stale-record".to_owned());
                noop()
            }
        }
    }

    fn complete(&mut self, id: WorkId) -> Effect<Self> {
        match self.outbox.begin_complete(id) {
            CompletionStart::Record(completion) => {
                let index = completion.journal_index();
                let bytes = completion.journal_bytes().to_vec();
                journal_append(self.journal_path.clone(), index, bytes)
                    .then(move |result| WebhookMsg::Completed(result, completion))
            }
            CompletionStart::AlreadyCompleted(found) => {
                self.note(format!("already-completed:{}", found.0));
                noop()
            }
            CompletionStart::NotPending(found) => {
                self.note(format!("not-pending:{}", found.0));
                noop()
            }
        }
    }

    fn on_recovered(&mut self, replay: Result<JournalReplay, CallError>) -> Effect<Self> {
        match DurableOutbox::<Vec<u8>>::recover(
            self.outbox.capacity(),
            replay,
            CommitConfidence::Clean,
        ) {
            Ok((outbox, report)) => {
                self.outbox = outbox;
                self.note(format!("tail:{}", tail_label(report.tail_status)));
                let mut completed: Vec<u64> = report.completed.iter().map(|id| id.0).collect();
                completed.sort_unstable();
                self.note(format!("recovered-completed:{completed:?}"));
                let mut pending: Vec<u64> =
                    report.pending.iter().map(|work| work.work_id().0).collect();
                pending.sort_unstable();
                self.note(format!("recovered-pending:{pending:?}"));
                self.resume = report.pending.into_iter().collect();
                self.drive_resume()
            }
            Err(RecoveryError::CorruptTail) => {
                self.note("recover-corrupt".to_owned());
                stop()
            }
            Err(other) => {
                self.note(format!("recover-error:{other:?}"));
                stop()
            }
        }
    }

    /// Resume one recovered pending item at a time, durably re-completing it.
    /// One in flight keeps journal append indices strictly increasing.
    fn drive_resume(&mut self) -> Effect<Self> {
        match self.resume.pop_front() {
            Some(recorded) => {
                let id = recorded.work_id();
                match self.outbox.apply(recorded) {
                    ApplyStatus::Apply(framed) => {
                        let (_complete_after, payload) = decode_intent(&framed);
                        self.note(format!("resend:{}", as_text(&payload)));
                        self.side_effect(&payload);
                        self.complete(id)
                    }
                    ApplyStatus::DuplicateWork(found) => {
                        self.note(format!("duplicate:{}", found.0));
                        noop()
                    }
                }
            }
            None => noop(),
        }
    }

    fn side_effect(&self, payload: &[u8]) {
        self.observed
            .lock()
            .expect("observed mutex")
            .sent
            .push(as_text(payload));
    }

    fn note(&self, note: String) {
        self.observed
            .lock()
            .expect("observed mutex")
            .notes
            .push(note);
    }
}

fn as_text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).expect("utf8 payload")
}

/// First byte flags "complete after send"; the rest is the user payload.
fn encode_intent(complete_after: bool, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + payload.len());
    out.push(u8::from(complete_after));
    out.extend_from_slice(payload);
    out
}

fn decode_intent(framed: &[u8]) -> (bool, Vec<u8>) {
    (framed[0] == 1, framed[1..].to_vec())
}

fn tail_label(status: TailStatus) -> &'static str {
    match status {
        TailStatus::Clean => "clean",
        TailStatus::TruncatedTailRepaired { .. } => "truncated-repaired",
        TailStatus::UncertainCommit => "uncertain",
    }
}

fn run_until_idle(runtime: &mut Runtime<OutboxShard, TestMailboxFactory>) {
    for _ in 0..1024 {
        let delivered = runtime.step();
        if delivered == 0 && !runtime.has_in_flight_calls() {
            return;
        }
    }
    panic!("runtime did not quiesce");
}

fn unique_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tina-outbox-{name}-{nanos}"));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn notes(observed: &Arc<Mutex<Observed>>) -> Vec<String> {
    observed.lock().expect("observed mutex").notes.clone()
}

fn sent(observed: &Arc<Mutex<Observed>>) -> Vec<String> {
    observed.lock().expect("observed mutex").sent.clone()
}

#[test]
fn restart_resumes_unsent_work_without_replaying_completed_work() {
    let dir = unique_dir("resume");
    let journal = dir.join("outbox.journal");
    let observed = Arc::new(Mutex::new(Observed {
        sent: Vec::new(),
        notes: Vec::new(),
    }));

    // Run 1: two items fully sent + marked sent; one sent but crashed before
    // marking sent (so it stays durably pending).
    let mut runtime = Runtime::new(OutboxShard, TestMailboxFactory);
    let address = runtime.register_with_capacity::<WebhookService, Infallible>(
        WebhookService::new(journal.clone(), 8, Arc::clone(&observed)),
        16,
    );
    runtime
        .try_send(address, WebhookMsg::Send(b"alpha".to_vec()))
        .unwrap();
    run_until_idle(&mut runtime);
    runtime
        .try_send(address, WebhookMsg::Send(b"beta".to_vec()))
        .unwrap();
    run_until_idle(&mut runtime);
    runtime
        .try_send(address, WebhookMsg::SendThenCrash(b"gamma".to_vec()))
        .unwrap();
    run_until_idle(&mut runtime);

    assert_eq!(sent(&observed), vec!["alpha", "beta", "gamma"]);
    let run1_notes = notes(&observed);
    assert!(run1_notes.iter().any(|note| note == "committed:1"));
    assert!(run1_notes.iter().any(|note| note == "committed:2"));
    assert!(
        run1_notes
            .iter()
            .any(|note| note == "crash-before-complete:3")
    );
    assert!(
        !run1_notes.iter().any(|note| note == "committed:3"),
        "gamma must not be marked sent before the crash: {run1_notes:?}"
    );

    // Run 2: fresh process recovers from the same journal.
    observed.lock().expect("observed mutex").sent.clear();
    observed.lock().expect("observed mutex").notes.clear();
    let mut restarted = Runtime::new(OutboxShard, TestMailboxFactory);
    let restarted_address = restarted.register_with_capacity::<WebhookService, Infallible>(
        WebhookService::new(journal, 8, Arc::clone(&observed)),
        16,
    );
    restarted
        .try_send(restarted_address, WebhookMsg::Recover)
        .unwrap();
    run_until_idle(&mut restarted);

    let run2_notes = notes(&observed);
    assert!(
        run2_notes.iter().any(|note| note == "tail:clean"),
        "{run2_notes:?}"
    );
    assert!(
        run2_notes
            .iter()
            .any(|note| note == "recovered-completed:[1, 2]"),
        "completed work should be recovered as completed, not pending: {run2_notes:?}"
    );
    assert!(
        run2_notes
            .iter()
            .any(|note| note == "recovered-pending:[3]"),
        "only the unsent work should be pending: {run2_notes:?}"
    );
    // Only gamma is resumed; alpha and beta are never re-sent.
    assert_eq!(sent(&observed), vec!["gamma"]);
    assert!(
        run2_notes.iter().any(|note| note == "committed:3"),
        "resumed work should be durably completed: {run2_notes:?}"
    );
}

#[test]
fn corrupt_journal_tail_stops_recovery_visibly() {
    let dir = unique_dir("corrupt");
    let journal = dir.join("outbox.journal");
    // Write one good outbox enqueue record, then corrupt its checksum byte.
    let mut bytes =
        tina_runtime::persistence::encode_journal_record(&tina_runtime::JournalRecord {
            index: 1,
            // tag(enqueue=0) + work id 1 (LE u64) + payload
            bytes: {
                let mut framed = vec![0_u8];
                framed.extend_from_slice(&1_u64.to_le_bytes());
                framed.extend_from_slice(b"x");
                framed
            },
        });
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    fs::write(&journal, bytes).expect("write corrupt journal");

    let observed = Arc::new(Mutex::new(Observed {
        sent: Vec::new(),
        notes: Vec::new(),
    }));
    let mut runtime = Runtime::new(OutboxShard, TestMailboxFactory);
    let address = runtime.register_with_capacity::<WebhookService, Infallible>(
        WebhookService::new(journal, 8, Arc::clone(&observed)),
        16,
    );
    runtime.try_send(address, WebhookMsg::Recover).unwrap();
    run_until_idle(&mut runtime);

    assert!(
        notes(&observed)
            .iter()
            .any(|note| note == "recover-corrupt"),
        "corrupt checksum must stop recovery visibly: {:?}",
        notes(&observed)
    );
    assert!(sent(&observed).is_empty());
}
