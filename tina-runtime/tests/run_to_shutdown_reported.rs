use std::convert::Infallible;
use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tina::Mailbox;
use tina::prelude::*;
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, LocalSystem, MailboxFactory, ReportedWorkloadError,
    RunToShutdownError, ShutdownUncleanReason, TerminalShutdownError, ThreadedRuntimeError,
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
}

struct Probe;

#[tina_runtime::isolate(message = ProbeMsg, reply = u32, shard = TestShard)]
impl Probe {
    fn handle(
        &mut self,
        _message: ProbeMsg,
        _ctx: &mut Context<'_, TestShard, u32>,
    ) -> Effect<Self> {
        noop()
    }

    fn handle_call(&mut self, message: ProbeMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match message {
            ProbeMsg::Ping => call.reply(7),
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

#[derive(Debug, PartialEq, Eq)]
struct LeafError(&'static str);

impl fmt::Display for LeafError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl Error for LeafError {}

#[derive(Debug)]
struct GenericReport {
    context: &'static str,
    source: LeafError,
    drops: Option<Arc<AtomicUsize>>,
}

impl GenericReport {
    fn new(context: &'static str, source: &'static str) -> Self {
        Self {
            context,
            source: LeafError(source),
            drops: None,
        }
    }

    fn tracked(drops: Arc<AtomicUsize>) -> Self {
        Self {
            context: "tracked workload context",
            source: LeafError("tracked workload source"),
            drops: Some(drops),
        }
    }
}

impl fmt::Display for GenericReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.context)
    }
}

impl AsRef<dyn Error + Send + Sync + 'static> for GenericReport {
    fn as_ref(&self) -> &(dyn Error + Send + Sync + 'static) {
        &self.source
    }
}

impl Drop for GenericReport {
    fn drop(&mut self) {
        if let Some(drops) = &self.drops {
            drops.fetch_add(1, Ordering::AcqRel);
        }
    }
}

fn app() -> LocalSystem<TestShard, DefaultThreadedMailboxFactory> {
    LocalSystem::single_shard(TestShard(1), DefaultThreadedMailboxFactory).build()
}

fn single_anyhow_entrypoint() -> anyhow::Result<u32> {
    let value = app().run_to_shutdown_reported(Duration::from_secs(2), |app| {
        let service = app
            .register_root::<_, Infallible>(Probe, 8)
            .map_err(anyhow::Error::new)?;
        match app
            .call_blocking(service, ProbeMsg::Ping, Duration::from_secs(1))
            .map_err(anyhow::Error::new)?
        {
            CallOutcome::Replied(7) => Ok(42),
            outcome => Err(anyhow::anyhow!("unexpected call outcome: {outcome:?}")),
        }
    })?;
    Ok(value)
}

fn multi_anyhow_entrypoint() -> anyhow::Result<u32> {
    let value = LocalSystem::<TestShard, DefaultThreadedMailboxFactory>::multi_shard(
        DefaultThreadedMailboxFactory,
    )
    .shard(TestShard(10))
    .shard(TestShard(11))
    .build()
    .run_to_shutdown_reported(Duration::from_secs(2), |app| {
        let service = app
            .register_root_on::<_, Infallible>(ShardId::new(11), Probe, 8)
            .map_err(anyhow::Error::new)?;
        match app
            .call_blocking(service, ProbeMsg::Ping, Duration::from_secs(1))
            .map_err(anyhow::Error::new)?
        {
            CallOutcome::Replied(7) => Ok(84),
            outcome => Err(anyhow::anyhow!("unexpected call outcome: {outcome:?}")),
        }
    })?;
    Ok(value)
}

fn failing_anyhow_entrypoint() -> anyhow::Result<()> {
    app().run_to_shutdown_reported(Duration::from_secs(2), |_app| {
        Err::<(), _>(anyhow::Error::new(LeafError("storage offline")).context("load state"))
    })?;
    Ok(())
}

#[test]
fn anyhow_entrypoints_can_use_outer_question_mark_for_single_and_multi() {
    assert_eq!(single_anyhow_entrypoint().expect("single reported run"), 42);
    assert_eq!(multi_anyhow_entrypoint().expect("multi reported run"), 84);
}

#[test]
fn anyhow_outer_question_mark_preserves_the_failure_chain() {
    let error = failing_anyhow_entrypoint().expect_err("reported workload must fail");
    assert_eq!(error.to_string(), "workload failed: load state");
    assert_eq!(
        error.root_cause().downcast_ref::<LeafError>(),
        Some(&LeafError("storage offline"))
    );
}

#[test]
fn workload_only_failure_preserves_owned_anyhow_context_and_source() {
    let result = app().run_to_shutdown_reported(Duration::from_secs(2), |_app| {
        Err::<(), _>(anyhow::Error::new(LeafError("database unavailable")).context("load account"))
    });

    let Err(RunToShutdownError::Workload(error)) = result else {
        panic!("expected reported workload failure");
    };
    assert_eq!(error.get_ref().to_string(), "load account");
    assert_eq!(error.to_string(), "load account");
    assert_eq!(
        error.source().map(ToString::to_string).as_deref(),
        Some("load account")
    );

    let report = error.into_inner();
    let chain = report.chain().map(ToString::to_string).collect::<Vec<_>>();
    assert_eq!(chain, ["load account", "database unavailable"]);
}

#[test]
fn shutdown_only_failure_keeps_the_terminal_source_chain() {
    let app = LocalSystem::single_shard(TestShard(2), CapacityPanicMailboxFactory).build();
    let result = app.run_to_shutdown_reported(Duration::from_secs(2), |app| {
        assert!(matches!(
            app.register_root::<_, Infallible>(Probe, 13),
            Err(ThreadedRuntimeError::WorkerStopped)
        ));
        Ok::<_, anyhow::Error>(9)
    });

    let Err(error @ RunToShutdownError::Shutdown(TerminalShutdownError::Unclean(_))) = result
    else {
        panic!("expected typed unclean shutdown");
    };
    assert!(error.workload().is_none());
    let Some(TerminalShutdownError::Unclean(unclean)) = error.shutdown() else {
        panic!("expected retained terminal report");
    };
    assert_eq!(
        unclean.report().unclean_reason(),
        Some(ShutdownUncleanReason::RuntimeError(
            ThreadedRuntimeError::WorkerStopped
        ))
    );
    assert!(matches!(
        error.source().and_then(Error::source),
        Some(source) if source.downcast_ref::<tina_runtime::UncleanShutdownError>().is_some()
    ));
    assert!(matches!(
        error.source().and_then(Error::source).and_then(Error::source),
        Some(source) if source.downcast_ref::<ThreadedRuntimeError>()
            == Some(&ThreadedRuntimeError::WorkerStopped)
    ));
}

#[test]
fn dual_failure_retains_both_and_uses_the_workload_as_primary_source() {
    let app = LocalSystem::single_shard(TestShard(3), CapacityPanicMailboxFactory).build();
    let result = app.run_to_shutdown_reported(Duration::from_secs(2), |app| {
        let registration = app.register_root::<_, Infallible>(Probe, 13);
        let runtime = registration.expect_err("registration must observe failed worker");
        Err::<(), _>(anyhow::Error::new(runtime).context("install primary service"))
    });

    let Err(error @ RunToShutdownError::WorkloadAndShutdown { .. }) = result else {
        panic!("expected independent workload and shutdown failures");
    };
    let workload = error.workload().expect("workload is retained");
    assert_eq!(workload.get_ref().to_string(), "install primary service");
    assert!(matches!(
        error.shutdown(),
        Some(TerminalShutdownError::Unclean(_))
    ));
    let primary = error
        .source()
        .expect("dual failure exposes its workload as the primary source")
        .downcast_ref::<ReportedWorkloadError<anyhow::Error>>()
        .expect("primary source keeps the sized report adapter");
    assert!(std::ptr::eq(primary.get_ref(), workload.get_ref()));
    assert_eq!(
        error
            .source()
            .and_then(Error::source)
            .map(ToString::to_string)
            .as_deref(),
        Some("install primary service")
    );
    assert!(matches!(
        error.shutdown().and_then(Error::source).and_then(Error::source),
        Some(source) if source.downcast_ref::<ThreadedRuntimeError>()
            == Some(&ThreadedRuntimeError::WorkerStopped)
    ));
}

#[test]
fn generic_as_ref_report_is_preserved_without_anyhow_specific_behavior() {
    let result = app().run_to_shutdown_reported(Duration::from_secs(2), |_app| {
        Err::<(), _>(GenericReport::new(
            "generic report context",
            "generic report source",
        ))
    });

    let Err(RunToShutdownError::Workload(error)) = result else {
        panic!("expected generic reported workload failure");
    };
    assert_eq!(error.get_ref().context, "generic report context");
    assert_eq!(error.to_string(), "generic report context");
    assert!(matches!(
        error.source(),
        Some(source) if source.downcast_ref::<LeafError>()
            == Some(&LeafError("generic report source"))
    ));
    let report = error.into_inner();
    assert_eq!(report.source, LeafError("generic report source"));
}

#[test]
fn multi_shard_reported_workload_failure_preserves_owned_source() {
    let result = LocalSystem::<TestShard, DefaultThreadedMailboxFactory>::multi_shard(
        DefaultThreadedMailboxFactory,
    )
    .shard(TestShard(10))
    .shard(TestShard(11))
    .build()
    .run_to_shutdown_reported(Duration::from_secs(2), |_app| {
        Err::<(), _>(GenericReport::new(
            "multi report context",
            "multi report source",
        ))
    });

    let Err(RunToShutdownError::Workload(error)) = result else {
        panic!("expected multi-shard reported workload failure");
    };
    assert_eq!(error.get_ref().context, "multi report context");
    assert!(matches!(
        error.source(),
        Some(source) if source.downcast_ref::<LeafError>()
            == Some(&LeafError("multi report source"))
    ));
    assert_eq!(error.into_inner().source, LeafError("multi report source"));
}

#[test]
fn reported_workload_value_is_dropped_exactly_once() {
    let drops = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&drops);
    let result = app().run_to_shutdown_reported(Duration::from_secs(2), |_app| {
        Err::<(), _>(GenericReport::tracked(drops))
    });

    let Err(RunToShutdownError::Workload(error)) = result else {
        panic!("expected tracked workload failure");
    };
    assert_eq!(observed.load(Ordering::Acquire), 0);
    let report: GenericReport = ReportedWorkloadError::into_inner(error);
    assert_eq!(observed.load(Ordering::Acquire), 0);
    drop(report);
    assert_eq!(observed.load(Ordering::Acquire), 1);
}

#[test]
fn dual_failure_drops_the_owned_report_exactly_once() {
    let drops = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&drops);
    let app = LocalSystem::single_shard(TestShard(4), CapacityPanicMailboxFactory).build();
    let result = app.run_to_shutdown_reported(Duration::from_secs(2), |app| {
        assert!(matches!(
            app.register_root::<_, Infallible>(Probe, 13),
            Err(ThreadedRuntimeError::WorkerStopped)
        ));
        Err::<(), _>(GenericReport::tracked(drops))
    });

    let Err(error @ RunToShutdownError::WorkloadAndShutdown { .. }) = result else {
        panic!("expected tracked dual failure");
    };
    assert_eq!(observed.load(Ordering::Acquire), 0);
    assert!(error.workload().is_some());
    assert!(matches!(
        error.shutdown(),
        Some(TerminalShutdownError::Unclean(_))
    ));
    drop(error);
    assert_eq!(observed.load(Ordering::Acquire), 1);
}

#[test]
fn reported_runner_does_not_convert_workload_panics() {
    let panic = std::panic::catch_unwind(|| {
        let _: Result<(), RunToShutdownError<ReportedWorkloadError<GenericReport>>> = app()
            .run_to_shutdown_reported(
                Duration::from_secs(2),
                |_app| -> Result<(), GenericReport> { panic!("reported workload panic") },
            );
    });
    assert!(panic.is_err());
}
