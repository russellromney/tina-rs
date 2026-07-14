//! Live Unix-domain bind/close proof through [`tina_runtime::LocalSystem`].

use std::convert::Infallible;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tina::{Shard, ShardId, stop_with};
use tina_runtime::{
    CallError, DefaultThreadedMailboxFactory, LocalSystem, ResultWaitError, StartupError,
    ThreadedRuntimeError, ThreadedTrySendError, UnixBindReply, unix_bind, unix_close_listener,
};

use crate::SpecimenReport;

static SOCKET_NONCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ProbeShard;

impl Shard for ProbeShard {
    fn id(&self) -> ShardId {
        ShardId::new(104)
    }
}

#[derive(Debug)]
enum Msg {
    Start,
    Bound(UnixBindReply),
    Closed(Result<(), CallError>),
}

struct Probe {
    path: std::path::PathBuf,
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
            Msg::Bound(Ok((listener, _path))) => unix_close_listener(listener).then(Msg::Closed),
            Msg::Bound(Err(error)) => stop_with(Err::<(), CallError>(error)),
            Msg::Closed(result) => stop_with(result),
        }
    }
}

/// Typed host/setup failure from the live Unix proof.
#[derive(Debug)]
pub enum LiveUnixError {
    Startup(StartupError),
    Register(ThreadedRuntimeError),
    Observe(ResultWaitError),
    Start(ThreadedTrySendError),
    Wait(ResultWaitError),
    Shutdown(ThreadedRuntimeError),
}

impl std::fmt::Display for LiveUnixError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Startup(error) => write!(formatter, "start live runtime: {error}"),
            Self::Register(error) => write!(formatter, "register live probe: {error}"),
            Self::Observe(error) => write!(formatter, "observe live probe: {error:?}"),
            Self::Start(error) => write!(formatter, "start live probe: {error}"),
            Self::Wait(error) => write!(formatter, "wait for live probe: {error:?}"),
            Self::Shutdown(error) => write!(formatter, "shut down live runtime: {error}"),
        }
    }
}

impl std::error::Error for LiveUnixError {}

/// Drives the live runtime and reports the platform's Unix-rail behavior.
pub fn smoke() -> Result<SpecimenReport, LiveUnixError> {
    let nonce = SOCKET_NONCE.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "specimen-live-unix-{}-{nonce}.sock",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let app = LocalSystem::single_shard(ProbeShard, DefaultThreadedMailboxFactory)
        .try_build()
        .map_err(LiveUnixError::Startup)?;

    let outcome = (|| {
        let address = app
            .register_root::<Probe, Infallible>(Probe { path: path.clone() }, 8)
            .map_err(LiveUnixError::Register)?;
        let waiter = app
            .observe_result::<Result<(), CallError>, _, _>(address)
            .map_err(LiveUnixError::Observe)?;
        app.try_send(address, Msg::Start)
            .map_err(LiveUnixError::Start)?;
        waiter
            .wait(Duration::from_secs(5))
            .map_err(LiveUnixError::Wait)
    })();

    let shutdown = app.shutdown().drain().join();
    let _ = std::fs::remove_file(&path);
    shutdown.map_err(LiveUnixError::Shutdown)?;
    let result = outcome?;

    let (ok, note) = if cfg!(unix) {
        (
            result == Ok(()),
            format!("live unix bind+close returned {result:?} (expected Ok on Unix)"),
        )
    } else {
        (
            result == Err(CallError::Unsupported),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_live_probes_use_typed_results_and_unique_paths() {
        for _ in 0..3 {
            let report = smoke().expect("live probe completes");
            assert!(report.ok, "{report:?}");
        }
    }
}
