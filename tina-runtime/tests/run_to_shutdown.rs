use std::convert::Infallible;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tina::prelude::*;
use tina::{CallRejectedReason, Mailbox};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, LocalSystem, MailboxFactory, RunToShutdownError,
    ShutdownAndWaitError, ShutdownUncleanReason, ShutdownWaitError, TerminalShutdownError,
    ThreadedRuntimeError,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct TestShard(u32);

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

#[derive(Debug)]
enum ProbeMsg {
    Ping,
    Stop,
    Block,
}

struct Probe {
    dropped: Option<Arc<AtomicBool>>,
}

impl Drop for Probe {
    fn drop(&mut self) {
        if let Some(dropped) = &self.dropped {
            dropped.store(true, Ordering::Release);
        }
    }
}

#[tina_runtime::isolate(message = ProbeMsg, reply = u32, shard = TestShard)]
impl Probe {
    fn handle(
        &mut self,
        message: ProbeMsg,
        _ctx: &mut Context<'_, TestShard, u32>,
    ) -> Effect<Self> {
        match message {
            ProbeMsg::Stop => stop(),
            ProbeMsg::Block => {
                std::thread::sleep(Duration::from_millis(100));
                noop()
            }
            ProbeMsg::Ping => noop(),
        }
    }

    fn handle_call(&mut self, message: ProbeMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match message {
            ProbeMsg::Ping => call.reply(7),
            ProbeMsg::Stop | ProbeMsg::Block => call.reject(CallRejectedReason::UnsupportedMessage),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct CapacityPanicMailboxFactory;

impl MailboxFactory for CapacityPanicMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        if capacity == 13 {
            panic!("intentional registration mailbox panic");
        }
        DefaultThreadedMailboxFactory.create(capacity)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WorkError {
    Expected(&'static str),
    Runtime(ThreadedRuntimeError),
}

impl fmt::Display for WorkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Expected(message) => f.write_str(message),
            Self::Runtime(error) => write!(f, "runtime operation failed: {error}"),
        }
    }
}

impl std::error::Error for WorkError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Expected(_) => None,
            Self::Runtime(error) => Some(error),
        }
    }
}

fn app() -> LocalSystem<TestShard, DefaultThreadedMailboxFactory> {
    LocalSystem::single_shard(TestShard(1), DefaultThreadedMailboxFactory).build()
}

#[test]
fn clean_single_shard_run_returns_workload_value_and_settles_terminal_authority() {
    let result = app().run_to_shutdown(Duration::from_secs(2), |app| {
        let service = app
            .register_root::<_, Infallible>(Probe { dropped: None }, 8)
            .map_err(WorkError::Runtime)?;
        match app
            .call_blocking(service, ProbeMsg::Ping, Duration::from_secs(1))
            .map_err(WorkError::Runtime)?
        {
            CallOutcome::Replied(7) => Ok(42),
            _ => Err(WorkError::Expected("ping did not reply")),
        }
    });

    assert_eq!(result, Ok(42));
}

#[test]
fn workload_only_failure_preserves_the_original_error() {
    let result = app().run_to_shutdown(Duration::from_secs(2), |_app| -> Result<(), _> {
        Err(WorkError::Expected("primary"))
    });

    assert_eq!(
        result,
        Err(RunToShutdownError::Workload(WorkError::Expected("primary")))
    );
}

#[test]
fn shutdown_only_failure_preserves_unclean_terminal_report() {
    let app = LocalSystem::single_shard(TestShard(2), CapacityPanicMailboxFactory).build();
    let result = app.run_to_shutdown(Duration::from_secs(2), |app| {
        assert!(matches!(
            app.register_root::<_, Infallible>(Probe { dropped: None }, 13),
            Err(ThreadedRuntimeError::WorkerStopped)
        ));
        Ok::<_, WorkError>(9)
    });

    let Err(RunToShutdownError::Shutdown(TerminalShutdownError::Unclean(error))) = result else {
        panic!("expected typed unclean shutdown");
    };
    assert_eq!(
        error.report().unclean_reason(),
        Some(ShutdownUncleanReason::RuntimeError(
            ThreadedRuntimeError::WorkerStopped
        ))
    );
}

#[test]
fn dual_failure_keeps_workload_and_shutdown_values_separate() {
    let app = LocalSystem::single_shard(TestShard(3), CapacityPanicMailboxFactory).build();
    let result = app.run_to_shutdown(Duration::from_secs(2), |app| {
        let registration = app.register_root::<_, Infallible>(Probe { dropped: None }, 13);
        Err::<(), _>(WorkError::Runtime(
            registration.expect_err("registration must observe the failed worker"),
        ))
    });

    let Err(error @ RunToShutdownError::WorkloadAndShutdown { .. }) = result else {
        panic!("expected independent workload and shutdown failures");
    };
    assert_eq!(
        error.workload(),
        Some(&WorkError::Runtime(ThreadedRuntimeError::WorkerStopped))
    );
    assert!(matches!(
        error.shutdown(),
        Some(TerminalShutdownError::Unclean(_))
    ));
}

#[test]
fn bounded_terminal_timeout_remains_distinct_from_unclean_truth() {
    let result = app().run_to_shutdown(Duration::from_millis(1), |app| {
        let service = app
            .register_root::<_, Infallible>(Probe { dropped: None }, 8)
            .map_err(WorkError::Runtime)?;
        app.try_send(service, ProbeMsg::Block)
            .map_err(|_| WorkError::Expected("block send failed"))?;
        Ok::<_, WorkError>(())
    });

    assert_eq!(
        result,
        Err(RunToShutdownError::Shutdown(
            TerminalShutdownError::Observation(ShutdownAndWaitError::Wait(
                ShutdownWaitError::Timeout
            ))
        ))
    );
}

#[test]
fn multi_shard_registration_early_return_still_shuts_down_every_owner() {
    let result = LocalSystem::<TestShard, DefaultThreadedMailboxFactory>::multi_shard(
        DefaultThreadedMailboxFactory,
    )
    .shard(TestShard(10))
    .shard(TestShard(11))
    .build()
    .run_to_shutdown(Duration::from_secs(2), |app| {
        app.register_root_on::<_, Infallible>(ShardId::new(99), Probe { dropped: None }, 8)
            .map(|_| ())
            .map_err(WorkError::Runtime)
    });

    assert_eq!(
        result,
        Err(RunToShutdownError::Workload(WorkError::Runtime(
            ThreadedRuntimeError::UnknownShard(ShardId::new(99))
        )))
    );
}

#[test]
fn multi_shard_runner_preserves_failed_owner_terminal_truth() {
    let result = LocalSystem::<TestShard, CapacityPanicMailboxFactory>::multi_shard(
        CapacityPanicMailboxFactory,
    )
    .shard(TestShard(20))
    .shard(TestShard(21))
    .build()
    .run_to_shutdown(Duration::from_secs(2), |app| {
        assert!(matches!(
            app.register_root_on::<_, Infallible>(ShardId::new(21), Probe { dropped: None }, 13,),
            Err(ThreadedRuntimeError::WorkerStopped)
        ));
        Ok::<_, WorkError>(())
    });

    let Err(RunToShutdownError::Shutdown(TerminalShutdownError::Unclean(error))) = result else {
        panic!("expected typed multi-shard terminal failure");
    };
    assert_eq!(error.report().failed_shards(), &[ShardId::new(21)]);
    assert_eq!(
        error.report().unclean_reason(),
        Some(ShutdownUncleanReason::RuntimeError(
            ThreadedRuntimeError::WorkerStopped
        ))
    );
}

#[test]
fn closed_host_call_early_return_still_reaches_clean_shutdown() {
    let result = app().run_to_shutdown(Duration::from_secs(2), |app| -> Result<(), WorkError> {
        let service = app
            .register_root::<_, Infallible>(Probe { dropped: None }, 8)
            .map_err(WorkError::Runtime)?;
        let stopped = app
            .observe_isolate_complete(service)
            .map_err(WorkError::Runtime)?;
        app.try_send(service, ProbeMsg::Stop)
            .map_err(|_| WorkError::Expected("stop send failed"))?;
        stopped
            .wait(Duration::from_secs(1))
            .map_err(|_| WorkError::Expected("stop observation failed"))?;
        match app
            .call_blocking(service, ProbeMsg::Ping, Duration::from_secs(1))
            .map_err(WorkError::Runtime)?
        {
            CallOutcome::Closed => Err(WorkError::Expected("host call target closed")),
            _ => Err(WorkError::Expected("unexpected host call terminal outcome")),
        }
    });

    assert_eq!(
        result,
        Err(RunToShutdownError::Workload(WorkError::Expected(
            "host call target closed"
        )))
    );
}

#[test]
fn workload_panic_propagates_after_existing_bounded_drop_teardown() {
    let dropped = Arc::new(AtomicBool::new(false));
    let probe = Arc::clone(&dropped);
    let panic = std::panic::catch_unwind(|| {
        let _: Result<(), RunToShutdownError<WorkError>> =
            app().run_to_shutdown(Duration::from_secs(2), |app| {
                app.register_root::<_, Infallible>(
                    Probe {
                        dropped: Some(probe),
                    },
                    8,
                )
                .map_err(WorkError::Runtime)?;
                panic!("workload panic remains a panic")
            });
    });

    assert!(panic.is_err());
    assert!(dropped.load(Ordering::Acquire));
}
