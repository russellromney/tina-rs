#[allow(dead_code)]
mod common;

use std::error::Error as _;
use std::time::Duration;

use tina::prelude::*;
use tina_reqwest_bridge::{
    InstallError, ReqwestConfig, ReqwestConfigError, ReqwestMsg, ReqwestRequest, ReqwestWorker,
};
use tina_runtime::{CallOutcome, DefaultThreadedMailboxFactory, LocalSystem, ThreadedRuntimeError};

use common::{FakeServer, delayed_ok};

fn system() -> LocalSystem<SingleShard, DefaultThreadedMailboxFactory> {
    LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory)
        .try_build()
        .expect("start local system")
}

#[test]
fn install_local_returns_callable_address_closer_metrics_and_clean_shutdown() {
    let server = FakeServer::spawn(delayed_ok(b"local", Duration::ZERO));
    let app = system();
    let bridge = ReqwestWorker::<SingleShard>::install_local(&app, ReqwestConfig::default())
        .expect("install reqwest bridge");

    let outcome = app
        .call_blocking(
            bridge.address,
            ReqwestMsg::Send(ReqwestRequest::get(server.url("/local"))),
            Duration::from_secs(2),
        )
        .expect("host call reaches bridge");
    let response = match outcome {
        CallOutcome::Replied(Ok(response)) => response,
        other => panic!("unexpected reqwest outcome: {other:?}"),
    };
    assert_eq!(response.body, b"local");
    assert_eq!(bridge.metrics.snapshot().admitted, 1);
    assert!(!bridge.closer.is_closed());
    bridge.closer.close();
    assert!(bridge.closer.is_closed());

    app.shutdown()
        .drain()
        .join_report()
        .ensure_clean()
        .expect("clean local-system shutdown");
    server.stop();
}

#[test]
fn install_local_preserves_config_and_register_error_sources() {
    let app = system();
    let config_error = match ReqwestWorker::<SingleShard>::install_local(
        &app,
        ReqwestConfig::default().with_mailbox_capacity(0),
    ) {
        Err(error) => error,
        Ok(_) => panic!("invalid config must fail"),
    };
    assert!(matches!(
        &config_error,
        InstallError::Config(ReqwestConfigError::ZeroMailboxCapacity)
    ));
    assert!(
        config_error
            .source()
            .and_then(|source| source.downcast_ref::<ReqwestConfigError>())
            .is_some()
    );

    app.shutdown_handle()
        .request_and_wait_report(Duration::from_secs(2))
        .expect("stop local system")
        .ensure_clean()
        .expect("clean stopped report");
    let register_error =
        match ReqwestWorker::<SingleShard>::install_local(&app, ReqwestConfig::default()) {
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
