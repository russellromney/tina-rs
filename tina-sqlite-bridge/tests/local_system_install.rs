use std::convert::Infallible;
use std::error::Error as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{CallOutcome, DefaultThreadedMailboxFactory, LocalSystem, ThreadedRuntimeError};
use tina_sqlite_bridge::{
    InstallError, SqliteConfig, SqliteConfigError, SqliteMsg, SqliteRequest, SqliteResponse,
    SqliteWorker,
};

fn system() -> LocalSystem<SingleShard, DefaultThreadedMailboxFactory> {
    LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory)
        .try_build()
        .expect("start local system")
}

#[derive(Debug)]
enum GateMsg {
    Hold,
}

struct Gate {
    entered: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
}

#[tina_runtime::isolate(message = GateMsg)]
impl Gate {
    fn handle(
        &mut self,
        _message: GateMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        self.entered.store(true, Ordering::Release);
        while !self.release.load(Ordering::Acquire) {
            std::hint::spin_loop();
        }
        noop()
    }
}

#[test]
fn install_local_returns_callable_address_closer_metrics_and_clean_shutdown() {
    let app = system();
    let bridge = SqliteWorker::<SingleShard>::install_local(
        &app,
        SqliteConfig::memory().with_poll_interval(Duration::from_millis(1)),
    )
    .expect("install sqlite bridge");

    let outcome = app
        .call_blocking(
            bridge.address.address(),
            SqliteMsg::Request(SqliteRequest::execute(
                "CREATE TABLE local_test (id INTEGER)",
            )),
            Duration::from_secs(2),
        )
        .expect("host call reaches bridge");
    assert!(matches!(
        outcome,
        CallOutcome::Replied(Ok(SqliteResponse::Executed { rows_changed: 0 }))
    ));
    let metrics = bridge.metrics.snapshot();
    assert_eq!(metrics.admitted, 1);
    assert_eq!(metrics.worker_executed, 1);
    assert!(!bridge.closer.is_closed());
    bridge.closer.close();
    assert!(bridge.closer.is_closed());

    app.shutdown()
        .drain()
        .join_report()
        .ensure_clean()
        .expect("clean local-system shutdown");
}

#[test]
fn install_local_preserves_config_setup_and_register_error_sources() {
    let app = system();
    let config_error = match SqliteWorker::<SingleShard>::install_local(
        &app,
        SqliteConfig::memory().with_mailbox_capacity(0),
    ) {
        Err(error) => error,
        Ok(_) => panic!("invalid config must fail"),
    };
    assert!(matches!(
        &config_error,
        InstallError::Config(SqliteConfigError::ZeroMailboxCapacity)
    ));
    assert!(
        config_error
            .source()
            .and_then(|source| source.downcast_ref::<SqliteConfigError>())
            .is_some()
    );

    let pragma_error = match SqliteWorker::<SingleShard>::install_local(
        &app,
        SqliteConfig::memory().with_pragma("definitely_not_a_pragma = ???"),
    ) {
        Err(error) => error,
        Ok(_) => panic!("invalid pragma must fail"),
    };
    assert!(matches!(&pragma_error, InstallError::Pragma { .. }));
    assert!(
        pragma_error
            .source()
            .and_then(|source| source.downcast_ref::<rusqlite::Error>())
            .is_some()
    );

    app.shutdown_handle()
        .request_and_wait_report(Duration::from_secs(2))
        .expect("stop local system")
        .ensure_clean()
        .expect("clean stopped report");
    let register_error =
        match SqliteWorker::<SingleShard>::install_local(&app, SqliteConfig::memory()) {
            Err(error) => error,
            Ok(_) => panic!("registration on stopped worker must fail"),
        };
    assert!(matches!(
        &register_error,
        InstallError::Register(ThreadedRuntimeError::WorkerStopped)
    ));
    assert!(
        register_error
            .source()
            .and_then(|source| source.downcast_ref::<ThreadedRuntimeError>())
            .is_some()
    );
}

#[test]
fn install_local_reports_command_full_joins_rejected_worker_and_refills() {
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory)
        .ingress_capacity(1)
        .try_build()
        .expect("start bounded local system");
    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let gate = app
        .register_root::<Gate, Infallible>(
            Gate {
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
            },
            2,
        )
        .expect("register gate");
    app.try_send(gate, GateMsg::Hold).expect("occupy worker");
    while !entered.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    app.try_send(gate, GateMsg::Hold)
        .expect("fill host-control queue");

    let error = match SqliteWorker::<SingleShard>::install_local(&app, SqliteConfig::memory()) {
        Err(error) => error,
        Ok(_) => panic!("saturated registration must fail"),
    };
    assert!(matches!(
        error,
        InstallError::Register(ThreadedRuntimeError::CommandFull)
    ));

    release.store(true, Ordering::Release);
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let bridge = loop {
        match SqliteWorker::<SingleShard>::install_local(&app, SqliteConfig::memory()) {
            Ok(bridge) => break bridge,
            Err(InstallError::Register(ThreadedRuntimeError::CommandFull))
                if std::time::Instant::now() < deadline =>
            {
                std::thread::yield_now();
            }
            Err(error) => panic!("registration after refill failed: {error}"),
        }
    };
    bridge.closer.close();
    app.shutdown()
        .drain()
        .join_report()
        .ensure_clean()
        .expect("clean shutdown after rejected and successful installs");
}
