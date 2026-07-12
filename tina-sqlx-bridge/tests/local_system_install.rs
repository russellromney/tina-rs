use std::error::Error as _;
use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use tina::prelude::*;
use tina_runtime::{CallOutcome, DefaultThreadedMailboxFactory, LocalSystem, ThreadedRuntimeError};
use tina_sqlx_bridge::{InstallError, PgConfig, PgError, PgMsg, PgRequest, PgWorker};

fn system() -> LocalSystem<SingleShard, DefaultThreadedMailboxFactory> {
    LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory)
        .try_build()
        .expect("start local system")
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
    let bridge = PgWorker::<SingleShard>::install_local_with_pool(
        &app,
        PgConfig::bridge_only().with_poll_interval(Duration::from_millis(1)),
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
