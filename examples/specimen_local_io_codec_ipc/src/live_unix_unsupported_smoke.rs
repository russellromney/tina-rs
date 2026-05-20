//! Live Unix-domain rail deferral smoke. This slice ships typed
//! `CallError::Unsupported` from the live driver for the new Unix rails
//! on every platform. Future work will swap that for a real OS-backed
//! implementation; until then this smoke drives the **live** runtime
//! (not the simulator) and asserts the typed answer, so a regression
//! that silently changes the deferral is caught.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tina::{Context, Effect, Isolate, Mailbox, Outbound, Shard, ShardId, TrySendError, noop};
use tina_runtime::{
    CallError, LocalSystem, MailboxFactory, RuntimeCall, UnixBindReply, UnixListenerId, unix_bind,
};

use crate::SpecimenReport;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ProbeShard;

impl Shard for ProbeShard {
    fn id(&self) -> ShardId {
        ShardId::new(104)
    }
}

struct ProbeMailbox<T> {
    capacity: usize,
    queue: Mutex<VecDeque<T>>,
    closed: Mutex<bool>,
}

impl<T> Mailbox<T> for ProbeMailbox<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }

    fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        if *self.closed.lock().expect("closed lock") {
            return Err(TrySendError::Closed(message));
        }
        let mut queue = self.queue.lock().expect("queue lock");
        if queue.len() >= self.capacity {
            return Err(TrySendError::Full(message));
        }
        queue.push_back(message);
        Ok(())
    }

    fn recv(&self) -> Option<T> {
        self.queue.lock().expect("queue lock").pop_front()
    }

    fn close(&self) {
        *self.closed.lock().expect("closed lock") = true;
    }
}

#[derive(Debug, Clone, Copy)]
struct ProbeMailboxFactory;

impl MailboxFactory for ProbeMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(ProbeMailbox {
            capacity,
            queue: Mutex::new(VecDeque::new()),
            closed: Mutex::new(false),
        })
    }
}

#[derive(Debug)]
enum Msg {
    Start,
    Bound(UnixBindReply),
}

struct Probe {
    /// `Some(result)` once `unix_bind` resolves, mapping the success
    /// payload away so the slot is `Result<(), CallError>`.
    observed: Arc<Mutex<Option<Result<UnixListenerId, CallError>>>>,
}

impl Isolate for Probe {
    type Message = Msg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = Infallible;
    type Call = RuntimeCall<Msg>;
    type Fact = Infallible;
    type Shard = ProbeShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            Msg::Start => unix_bind("/tmp/specimen_live_unsupported.sock").then(Msg::Bound),
            Msg::Bound(reply) => {
                *self.observed.lock().expect("observed lock") =
                    Some(reply.map(|(listener, _path)| listener));
                noop()
            }
        }
    }
}

/// Drives the live runtime, issues one `unix_bind`, and reports whether
/// it observed the typed `Unsupported` deferral.
pub fn smoke() -> SpecimenReport {
    let observed = Arc::new(Mutex::new(None));
    let app = LocalSystem::single_shard(ProbeShard, ProbeMailboxFactory).build();
    let address = app
        .register_root::<Probe, Infallible>(
            Probe {
                observed: Arc::clone(&observed),
            },
            8,
        )
        .expect("register probe");
    app.try_send(address, Msg::Start).expect("start probe");

    // Bounded wait for the live completion to land.
    let deadline = Instant::now() + Duration::from_secs(5);
    while observed.lock().expect("observed lock").is_none() {
        if Instant::now() > deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let result = *observed.lock().expect("observed lock");
    let _ = app.shutdown().drain().join();

    let saw_unsupported = matches!(result, Some(Err(CallError::Unsupported)));
    SpecimenReport {
        name: "live_unix_unsupported_smoke",
        bytes: 0,
        frames: 0,
        ok: saw_unsupported,
        note: format!("live unix_bind returned {result:?} (expected Unsupported)"),
    }
}
