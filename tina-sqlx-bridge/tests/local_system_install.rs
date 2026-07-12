use std::convert::Infallible;
use std::error::Error as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use tina::prelude::*;
use tina_runtime::{CallOutcome, DefaultThreadedMailboxFactory, LocalSystem, ThreadedRuntimeError};
use tina_sqlx_bridge::{InstallError, PgConfig, PgError, PgMsg, PgPoolConfig, PgRequest, PgWorker};

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

fn lazy_pool() -> (sqlx::PgPool, tokio::runtime::Runtime) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(1)
        .thread_name("tina-sqlx-local-install-test")
        .build()
        .expect("tokio runtime");
    let pool = {
        let _entered = runtime.handle().enter();
        PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(Duration::from_millis(100))
            .connect_lazy("postgres://test:test@127.0.0.1:1/test")
            .expect("lazy pool")
    };
    (pool, runtime)
}

#[test]
fn install_local_with_pool_returns_callable_address_closer_metrics_and_clean_shutdown() {
    let app = system();
    let (pool, tokio_runtime) = lazy_pool();
    let mut config = PgConfig::bridge_only()
        .with_poll_interval(Duration::from_millis(1))
        .with_pool(PgPoolConfig::new("ignored").with_max_connections(0))
        .with_cancel_on_timeout(0);
    config
        .cancel
        .as_mut()
        .expect("cancel config")
        .acquire_timeout = Duration::ZERO;
    let bridge = PgWorker::<SingleShard>::install_local_with_pool(
        &app,
        config,
        pool,
        tokio_runtime.handle().clone(),
    )
    .expect("install pg bridge");

    bridge.closer.close();
    let outcome = app
        .call_blocking(
            bridge.address.address(),
            PgMsg::Send(PgRequest::execute("SELECT 1")),
            Duration::from_secs(1),
        )
        .expect("host call reaches bridge");
    assert!(matches!(
        outcome,
        CallOutcome::Replied(Err(PgError::Closed))
    ));
    assert_eq!(bridge.metrics.snapshot().closed, 1);
    assert_eq!(bridge.metrics.snapshot().db_cancels_sent, 0);
    assert!(bridge.closer.is_closed());

    app.shutdown()
        .drain()
        .join_report()
        .ensure_clean()
        .expect("clean local-system shutdown");
    drop(tokio_runtime);
}

#[test]
fn install_local_preserves_config_pool_and_register_error_sources() {
    let app = system();
    let config_error = match PgWorker::<SingleShard>::install_local(
        &app,
        PgConfig::bridge_only().with_mailbox_capacity(0),
    ) {
        Err(error) => error,
        Ok(_) => panic!("invalid config must fail"),
    };
    assert!(matches!(&config_error, InstallError::Config(_)));
    assert!(config_error.source().is_some());

    let missing_pool = match PgWorker::<SingleShard>::install_local(&app, PgConfig::bridge_only()) {
        Err(error) => error,
        Ok(_) => panic!("missing pool config must fail"),
    };
    assert!(matches!(&missing_pool, InstallError::MissingPoolConfig));

    let pool_error = match PgWorker::<SingleShard>::install_local(
        &app,
        PgConfig::from_url("not a postgres connection URL"),
    ) {
        Err(error) => error,
        Ok(_) => panic!("malformed pool URL must fail"),
    };
    assert!(matches!(pool_error, InstallError::Pool(_)));
    assert!(
        pool_error
            .source()
            .and_then(|source| source.downcast_ref::<sqlx::Error>())
            .is_some()
    );

    app.shutdown_handle()
        .request_and_wait_report(Duration::from_secs(2))
        .expect("stop local system")
        .ensure_clean()
        .expect("clean stopped report");
    let (pool, tokio_runtime) = lazy_pool();
    let register_error = match PgWorker::<SingleShard>::install_local_with_pool(
        &app,
        PgConfig::bridge_only(),
        pool,
        tokio_runtime.handle().clone(),
    ) {
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
    drop(tokio_runtime);
}

#[test]
fn install_local_with_pool_reports_command_full_preserves_pool_and_refills() {
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

    let (pool, tokio_runtime) = lazy_pool();
    let error = match PgWorker::<SingleShard>::install_local_with_pool(
        &app,
        PgConfig::bridge_only(),
        pool.clone(),
        tokio_runtime.handle().clone(),
    ) {
        Err(error) => error,
        Ok(_) => panic!("saturated registration must fail"),
    };
    assert!(matches!(
        error,
        InstallError::Register(ThreadedRuntimeError::CommandFull)
    ));
    assert!(
        !pool.is_closed(),
        "failed supplied-pool registration must preserve caller ownership"
    );

    release.store(true, Ordering::Release);
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let bridge = loop {
        match PgWorker::<SingleShard>::install_local_with_pool(
            &app,
            PgConfig::bridge_only(),
            pool.clone(),
            tokio_runtime.handle().clone(),
        ) {
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
    assert!(!pool.is_closed());
    tokio_runtime.block_on(pool.close());
}
