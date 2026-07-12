//! Live Unix-domain rail smoke. Drives the **live** runtime (not the
//! simulator) through one `unix_bind` / `unix_close_listener` cycle and
//! reports what the live driver does:
//!
//! - On Unix platforms the live OS-backed lane binds a real socket and
//!   the smoke reports success.
//! - On non-Unix platforms there is no backend; `unix_bind` completes
//!   with typed `CallError::Unsupported`, and the smoke reports that the
//!   capability is honestly named (still `ok`, because the typed answer
//!   is the contract on that platform).

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tina::{Mailbox, Shard, ShardId, TrySendError, noop};
use tina_runtime::{
    CallError, LocalSystem, MailboxFactory, UnixBindReply, UnixListenerId, unix_bind,
    unix_close_listener,
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

    fn is_empty(&self) -> bool {
        self.queue.lock().expect("queue lock").is_empty()
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
    Closed(Result<(), CallError>),
}

/// Outcome the probe records: the bind result mapped to keep just the
/// success/error shape, and whether close succeeded (Unix only).
type Observed = Arc<Mutex<Option<Result<(), CallError>>>>;

struct Probe {
    path: std::path::PathBuf,
    listener: Option<UnixListenerId>,
    observed: Observed,
}

#[tina_runtime::isolate(message = Msg, shard = ProbeShard)]
impl Probe {
    fn handle(
        &mut self,
        msg: Msg,
        _ctx: &mut Context<'_, ProbeShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            Msg::Start => unix_bind(self.path.clone()).then(Msg::Bound),
            Msg::Bound(Ok((listener, _path))) => {
                self.listener = Some(listener);
                unix_close_listener(listener).then(Msg::Closed)
            }
            Msg::Bound(Err(error)) => {
                *self.observed.lock().expect("observed lock") = Some(Err(error));
                noop()
            }
            Msg::Closed(result) => {
                *self.observed.lock().expect("observed lock") = Some(result);
                noop()
            }
        }
    }
}

/// Drives the live runtime and reports the live Unix rail behavior.
pub fn smoke() -> anyhow::Result<SpecimenReport> {
    let path = std::env::temp_dir().join(format!("specimen-live-unix-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let observed: Observed = Arc::new(Mutex::new(None));
    let app = LocalSystem::single_shard(ProbeShard, ProbeMailboxFactory).try_build()?;
    let address = app
        .register_root::<Probe, Infallible>(
            Probe {
                path: path.clone(),
                listener: None,
                observed: Arc::clone(&observed),
            },
            8,
        )
        .map_err(|error| anyhow::anyhow!("register probe: {error:?}"))?;
    app.try_send(address, Msg::Start)
        .map_err(|error| anyhow::anyhow!("start probe: {error:?}"))?;

    let deadline = Instant::now() + Duration::from_secs(5);
    while observed.lock().expect("observed lock").is_none() {
        if Instant::now() > deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let result = *observed.lock().expect("observed lock");
    let shutdown = app.shutdown().drain().join();
    let _ = std::fs::remove_file(&path);
    shutdown.map_err(|error| anyhow::anyhow!("join live Unix runtime: {error:?}"))?;

    // On Unix the live lane must bind+close cleanly. On non-Unix the
    // contract is typed `Unsupported`. Either is a passing smoke for
    // the platform it runs on.
    let (ok, note) = if cfg!(unix) {
        (
            result == Some(Ok(())),
            format!("live unix bind+close returned {result:?} (expected Ok on Unix)"),
        )
    } else {
        (
            result == Some(Err(CallError::Unsupported)),
            format!("live unix_bind returned {result:?} (expected Unsupported off Unix)"),
        )
    };
    Ok(SpecimenReport {
        name: "live_unix_smoke",
        bytes: 0,
        frames: 0,
        ok,
        note,
    })
}
