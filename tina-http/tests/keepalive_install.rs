//! Live proofs for LocalSystem keepalive-pool installation.
//!
//! Covers complete install, partial-install rollback, duplicate origin
//! conflict, consuming close success, timeout retention, owner failure, and
//! system shutdown settlement. Double-close is unrepresentable because close
//! consumes the handle.

mod common;

use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use tina::pool::{AcquireOutcome, CloseMode, PoolConfig, ReleaseDisposition, ReleaseOutcome};
use tina::prelude::*;
use tina_http::{
    HttpClientConfig, HttpRequest, HttpTarget, InstallKeepalivePool, KeepaliveCloseAndDrain,
    KeepaliveConnectionMsg, KeepaliveInstallStep, KeepaliveOutcome, KeepalivePoolCloseOutcome,
    KeepalivePoolConfigError, KeepalivePoolDrainOutcome, KeepalivePoolInstallConfig,
    KeepalivePoolInstallError, KeepaliveRollbackResult, MAX_KEEPALIVE_MAILBOX_CAPACITY,
    MAX_KEEPALIVE_POOL_CAPACITY, MAX_KEEPALIVE_POOL_WAITERS, OriginKey, build_keepalive_pool,
    install_keepalive_pool_fail_after, install_keepalive_pool_fail_after_with_rollback_failure,
    shutdown_keepalive_pool,
};
use tina_runtime::pool::{WorkerPoolMsg, WorkerPoolReply};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, LocalSystem, ThreadedRuntime, ThreadedRuntimeError,
};

use common::TestShard;

// =====================================================================
// Scripted keepalive peer
// =====================================================================

struct ScriptedServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl ScriptedServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind scripted server");
        listener
            .set_nonblocking(false)
            .expect("blocking listener mode");
        let addr = listener.local_addr().expect("local addr");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_bg = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            listener
                .set_nonblocking(true)
                .expect("nonblocking accept loop");
            while !stop_bg.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_millis(200)))
                            .ok();
                        let mut buf = [0u8; 4096];
                        loop {
                            if stop_bg.load(Ordering::Acquire) {
                                break;
                            }
                            match stream.read(&mut buf) {
                                Ok(0) => break,
                                Ok(n) => {
                                    // Minimal HTTP/1.1 keepalive response for each request head.
                                    if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                                        let body = b"ok";
                                        let response = format!(
                                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\nok",
                                            body.len()
                                        );
                                        let _ = stream.write_all(response.as_bytes());
                                    }
                                }
                                Err(err)
                                    if err.kind() == std::io::ErrorKind::WouldBlock
                                        || err.kind() == std::io::ErrorKind::TimedOut =>
                                {
                                    continue;
                                }
                                Err(_) => break,
                            }
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            addr,
            stop,
            handle: Some(handle),
        }
    }

    fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for ScriptedServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

// =====================================================================
// Helpers
// =====================================================================

fn system() -> LocalSystem<TestShard, DefaultThreadedMailboxFactory> {
    LocalSystem::single_shard(TestShard, DefaultThreadedMailboxFactory)
        .try_build()
        .expect("local system")
}

fn config(target: HttpTarget, capacity: usize) -> KeepalivePoolInstallConfig {
    KeepalivePoolInstallConfig::new(
        target,
        HttpClientConfig::pressure(),
        PoolConfig::new(capacity, 4),
        8,
        16,
    )
}

fn req() -> HttpRequest {
    HttpRequest {
        method: http::Method::GET,
        path: "/".into(),
        version: http::Version::HTTP_11,
        headers: http::HeaderMap::new(),
        body: tina_http::HttpRequestBody::Buffered(Vec::new()),
    }
}

// =====================================================================
// Install proofs
// =====================================================================

#[test]
fn complete_install_returns_usable_pool_and_drains_cleanly() {
    let server = ScriptedServer::start();
    let app = system();
    let target = HttpTarget::http(server.addr);
    let pool = app
        .install_keepalive_pool(config(target, 2))
        .expect("install keepalive pool");

    assert_eq!(pool.connections().len(), 2);
    assert_eq!(
        pool.origin(),
        &OriginKey::from_target(&HttpTarget::http(server.addr))
    );

    let lease = match app
        .call_blocking(pool.pool(), WorkerPoolMsg::Acquire, Duration::from_secs(2))
        .expect("acquire")
    {
        CallOutcome::Replied(WorkerPoolReply::Acquire(AcquireOutcome::Acquired(lease))) => lease,
        other => panic!("expected acquired lease, got {other:?}"),
    };
    let conn = *lease.handle();
    match app
        .call_blocking(
            conn,
            KeepaliveConnectionMsg::request(req(), Duration::from_secs(2)),
            Duration::from_secs(2),
        )
        .expect("request")
    {
        CallOutcome::Replied(KeepaliveOutcome::Request { result, .. }) => {
            assert!(result.is_ok(), "request must succeed: {result:?}");
        }
        other => panic!("expected request reply, got {other:?}"),
    }
    match app
        .call_blocking(
            pool.pool(),
            WorkerPoolMsg::Release {
                lease,
                disposition: ReleaseDisposition::Reuse,
            },
            Duration::from_secs(2),
        )
        .expect("release")
    {
        CallOutcome::Replied(WorkerPoolReply::Release(ReleaseOutcome::Released)) => {}
        other => panic!("expected released, got {other:?}"),
    }

    match pool.close_and_drain(Duration::from_secs(2)) {
        KeepaliveCloseAndDrain::Drained(report) => {
            assert_eq!(report.pool_close, KeepalivePoolCloseOutcome::Closed);
            assert_eq!(report.drain, KeepalivePoolDrainOutcome::Drained);
            assert_eq!(report.requested, 2);
            assert_eq!(report.stopped + report.already_closed, 2);
        }
        other => panic!("expected drained, got {other:?}"),
    }

    let _ = app.shutdown().join();
    server.stop();
}

#[test]
fn partial_install_rolls_back_every_registered_connection() {
    let server = ScriptedServer::start();
    let app = system();
    let target = HttpTarget::http(server.addr);

    // capacity 3; succeed two connections then fail before the third.
    let err = install_keepalive_pool_fail_after(&app, config(target.clone(), 3), 2)
        .expect_err("install must fail after two connections");

    match err {
        KeepalivePoolInstallError::Register {
            failed_at,
            rollback,
            ..
        } => {
            assert_eq!(failed_at, KeepaliveInstallStep::Connection { index: 2 });
            assert_eq!(rollback.connections_registered, 2);
            assert_eq!(
                rollback.connections_stopped + rollback.connections_already_closed,
                2,
                "rollback must settle every registered connection"
            );
            assert!(rollback.connection_stop_failures.is_empty());
            assert!(!rollback.pool_registered);
        }
        other => panic!("expected register rollback, got {other:?}"),
    }

    // Origin claim must be released so a clean install can succeed next.
    let pool = app
        .install_keepalive_pool(config(target, 1))
        .expect("install after rollback");
    match pool.close_and_drain(Duration::from_secs(2)) {
        KeepaliveCloseAndDrain::Drained(_) => {}
        other => panic!("expected drained, got {other:?}"),
    }
    let _ = app.shutdown().join();
    server.stop();
}

#[test]
fn partial_install_fails_on_pool_registration_and_rolls_back_connections() {
    let server = ScriptedServer::start();
    let app = system();
    let target = HttpTarget::http(server.addr);

    // capacity 2; succeed both connections, fail on pool registration.
    let err = install_keepalive_pool_fail_after(&app, config(target.clone(), 2), 2)
        .expect_err("install must fail on pool step");

    match err {
        KeepalivePoolInstallError::Register {
            failed_at,
            rollback,
            ..
        } => {
            assert_eq!(failed_at, KeepaliveInstallStep::Pool);
            assert_eq!(rollback.connections_registered, 2);
            assert_eq!(
                rollback.connections_stopped + rollback.connections_already_closed,
                2
            );
            assert!(!rollback.pool_registered);
        }
        other => panic!("expected pool-step rollback, got {other:?}"),
    }

    let _ = app.shutdown().join();
    server.stop();
}

#[test]
fn every_registration_boundary_rolls_back_completely() {
    for succeed_count in 0..=3 {
        let app = system();
        let target = HttpTarget::http(
            format!("127.0.0.1:{}", 10_000 + succeed_count)
                .parse()
                .unwrap(),
        );
        let error =
            install_keepalive_pool_fail_after(&app, config(target.clone(), 3), succeed_count)
                .expect_err("injected registration failure");
        match error {
            KeepalivePoolInstallError::Register {
                failed_at,
                rollback,
                recovery,
                ..
            } => {
                let expected_step = if succeed_count < 3 {
                    KeepaliveInstallStep::Connection {
                        index: succeed_count,
                    }
                } else {
                    KeepaliveInstallStep::Pool
                };
                assert_eq!(failed_at, expected_step);
                assert_eq!(rollback.connections_registered, succeed_count);
                assert_eq!(
                    rollback.connections_stopped + rollback.connections_already_closed,
                    succeed_count
                );
                assert!(rollback.connection_stop_failures.is_empty());
                assert!(recovery.is_none());
            }
            other => panic!("unexpected boundary failure: {other:?}"),
        }
        let pool = app
            .install_keepalive_pool(config(target, 1))
            .expect("complete rollback releases boundary claim");
        assert!(matches!(
            pool.close_and_drain(Duration::from_secs(2)),
            KeepaliveCloseAndDrain::Drained(_)
        ));
        let _ = app.shutdown().join();
    }
}

#[test]
fn duplicate_origin_install_returns_typed_conflict() {
    let server = ScriptedServer::start();
    let app = system();
    let target = HttpTarget::http(server.addr);

    let first = app
        .install_keepalive_pool(config(target.clone(), 1))
        .expect("first install");
    let second = app.install_keepalive_pool(config(target.clone(), 1));
    match second {
        Err(KeepalivePoolInstallError::Conflict { origin }) => {
            assert_eq!(origin, OriginKey::from_target(&target));
        }
        other => panic!("expected conflict, got {other:?}"),
    }

    match first.close_and_drain(Duration::from_secs(2)) {
        KeepaliveCloseAndDrain::Drained(_) => {}
        other => panic!("expected drained, got {other:?}"),
    }

    // After drain the origin claim is free again.
    let third = app
        .install_keepalive_pool(config(target, 1))
        .expect("install after drain releases claim");
    match third.close_and_drain(Duration::from_secs(2)) {
        KeepaliveCloseAndDrain::Drained(_) => {}
        other => panic!("expected drained, got {other:?}"),
    }

    let _ = app.shutdown().join();
    server.stop();
}

#[test]
fn invalid_config_is_refused_before_registration() {
    let app = system();
    let target = HttpTarget::http("127.0.0.1:9".parse().unwrap());
    let mut cfg = config(target, 1);
    cfg.pool_config = PoolConfig::new(0, 0);
    match app.install_keepalive_pool(cfg) {
        Err(KeepalivePoolInstallError::InvalidConfig(KeepalivePoolConfigError::ZeroCapacity)) => {}
        other => panic!("expected zero capacity, got {other:?}"),
    }
    let _ = app.shutdown().join();
}

#[test]
fn oversized_config_is_refused_before_install_claim_or_resources() {
    let app = system();
    let target = HttpTarget::http("127.0.0.1:9".parse().unwrap());
    let cases = [
        (
            PoolConfig::new(MAX_KEEPALIVE_POOL_CAPACITY + 1, 1),
            8,
            16,
            "pool_config.capacity",
        ),
        (
            PoolConfig::new(1, MAX_KEEPALIVE_POOL_WAITERS + 1),
            8,
            16,
            "pool_config.max_waiters",
        ),
        (
            PoolConfig::new(1, 1),
            MAX_KEEPALIVE_MAILBOX_CAPACITY + 1,
            16,
            "connection_mailbox_capacity",
        ),
        (
            PoolConfig::new(1, 1),
            8,
            MAX_KEEPALIVE_MAILBOX_CAPACITY + 1,
            "pool_mailbox_capacity",
        ),
    ];
    for (pool_config, connection_mailbox, pool_mailbox, field) in cases {
        let cfg = KeepalivePoolInstallConfig::new(
            target.clone(),
            HttpClientConfig::pressure(),
            pool_config,
            connection_mailbox,
            pool_mailbox,
        );
        match app.install_keepalive_pool(cfg) {
            Err(KeepalivePoolInstallError::InvalidConfig(KeepalivePoolConfigError::TooLarge {
                field: actual,
                ..
            })) if actual == field => {}
            other => panic!("expected TooLarge for {field}, got {other:?}"),
        }
    }

    // No rejected attempt acquired the same-origin claim or installed a resource.
    let pool = app
        .install_keepalive_pool(config(target, 1))
        .expect("valid install after every preflight rejection");
    assert!(matches!(
        pool.close_and_drain(Duration::from_secs(2)),
        KeepaliveCloseAndDrain::Drained(_)
    ));
    let _ = app.shutdown().join();
}

#[test]
fn dropping_installed_handle_keeps_origin_tombstoned() {
    let app = system();
    let target = HttpTarget::http("127.0.0.1:9".parse().unwrap());
    let pool = app
        .install_keepalive_pool(config(target.clone(), 1))
        .expect("install");
    drop(pool);

    assert!(matches!(
        app.install_keepalive_pool(config(target, 1)),
        Err(KeepalivePoolInstallError::Conflict { .. })
    ));
    let _ = app.shutdown().join();
}

#[test]
fn incomplete_rollback_retains_cleanup_and_conflict_authority() {
    let app = system();
    let target = HttpTarget::http("127.0.0.1:9".parse().unwrap());
    let error = install_keepalive_pool_fail_after_with_rollback_failure(
        &app,
        config(target.clone(), 2),
        2,
        1,
    )
    .expect_err("pool registration failure");
    let recovery = match error {
        KeepalivePoolInstallError::Register {
            rollback,
            recovery: Some(recovery),
            ..
        } => {
            assert_eq!(rollback.connections_stopped, 1);
            assert_eq!(rollback.connection_stop_failures.len(), 1);
            recovery
        }
        other => panic!("expected retained recovery, got {other:?}"),
    };

    assert!(matches!(
        app.install_keepalive_pool(config(target.clone(), 1)),
        Err(KeepalivePoolInstallError::Conflict { .. })
    ));
    match recovery.retry(Duration::from_secs(2)) {
        KeepaliveRollbackResult::Recovered(report) => {
            assert_eq!(
                report.connections_stopped + report.connections_already_closed,
                2
            );
            assert!(report.connection_stop_failures.is_empty());
        }
        other => panic!("expected complete recovery, got {other:?}"),
    }

    let pool = app
        .install_keepalive_pool(config(target, 1))
        .expect("claim released only after recovery");
    assert!(matches!(
        pool.close_and_drain(Duration::from_secs(2)),
        KeepaliveCloseAndDrain::Drained(_)
    ));
    let _ = app.shutdown().join();
}

#[test]
fn incomplete_rollback_owner_shutdown_is_typed_and_terminal() {
    let app = system();
    let target = HttpTarget::http("127.0.0.1:9".parse().unwrap());
    let error =
        install_keepalive_pool_fail_after_with_rollback_failure(&app, config(target, 2), 2, 1)
            .expect_err("pool registration failure");
    let recovery = match error {
        KeepalivePoolInstallError::Register {
            recovery: Some(recovery),
            ..
        } => recovery,
        other => panic!("expected retained recovery, got {other:?}"),
    };

    app.shutdown_handle()
        .request_and_wait_report(Duration::from_secs(2))
        .expect("owner shutdown");
    match recovery.retry(Duration::from_secs(2)) {
        KeepaliveRollbackResult::Shutdown(report) => {
            assert_eq!(report.connections_registered, 2);
            assert_eq!(
                report.connections_stopped + report.connections_already_closed,
                2
            );
            assert!(report.connection_stop_failures.is_empty());
        }
        other => panic!("expected typed rollback shutdown, got {other:?}"),
    }
    let _ = app.shutdown().join();
}

// =====================================================================
// Close / drain proofs
// =====================================================================

#[test]
fn close_and_drain_success_settles_every_connection() {
    let server = ScriptedServer::start();
    let app = system();
    let pool = app
        .install_keepalive_pool(config(HttpTarget::http(server.addr), 2))
        .expect("install");

    match pool.close_and_drain(Duration::from_secs(2)) {
        KeepaliveCloseAndDrain::Drained(report) => {
            assert_eq!(report.requested, 2);
            assert_eq!(report.stopped, 2);
            assert_eq!(report.already_closed, 0);
            assert_eq!(report.drain, KeepalivePoolDrainOutcome::Drained);
        }
        other => panic!("expected drained, got {other:?}"),
    }

    let _ = app.shutdown().join();
    server.stop();
}

#[test]
fn zero_total_deadline_returns_authority_without_starting_close() {
    let app = system();
    let pool = app
        .install_keepalive_pool(config(HttpTarget::http("127.0.0.1:9".parse().unwrap()), 2))
        .expect("install");

    let retained = match pool.close_and_drain(Duration::ZERO) {
        KeepaliveCloseAndDrain::TimedOut { pool, pending } => {
            assert_eq!(pending.leased, None);
            assert_eq!(pending.connections_live, 2);
            assert!(!pending.admission_closed);
            pool
        }
        other => panic!("zero total deadline must retain authority, got {other:?}"),
    };
    assert!(matches!(
        retained.close_and_drain(Duration::from_secs(2)),
        KeepaliveCloseAndDrain::Drained(_)
    ));
    let _ = app.shutdown().join();
}

#[test]
fn drain_timeout_retains_owned_handle_and_exact_pending_counts() {
    let server = ScriptedServer::start();
    let app = system();
    let pool = app
        .install_keepalive_pool(config(HttpTarget::http(server.addr), 1))
        .expect("install");

    let lease = match app
        .call_blocking(pool.pool(), WorkerPoolMsg::Acquire, Duration::from_secs(2))
        .expect("acquire")
    {
        CallOutcome::Replied(WorkerPoolReply::Acquire(AcquireOutcome::Acquired(lease))) => lease,
        other => panic!("expected acquired, got {other:?}"),
    };

    let retained = match pool.close_and_drain(Duration::from_millis(200)) {
        KeepaliveCloseAndDrain::TimedOut { pool, pending } => {
            assert_eq!(pending.leased, Some(1));
            assert_eq!(pending.connections_live, 1);
            assert!(pending.admission_closed);
            pool
        }
        other => panic!("expected timed out retention, got {other:?}"),
    };

    // Leased connection must still accept work — admitted work was not aborted.
    match app
        .call_blocking(
            *lease.handle(),
            KeepaliveConnectionMsg::request(req(), Duration::from_secs(2)),
            Duration::from_secs(2),
        )
        .expect("leased request after timeout")
    {
        CallOutcome::Replied(KeepaliveOutcome::Request { result, .. }) => {
            assert!(result.is_ok(), "leased connection must remain usable");
        }
        other => panic!("expected request reply, got {other:?}"),
    }

    match app
        .call_blocking(
            retained.pool(),
            WorkerPoolMsg::Release {
                lease,
                disposition: ReleaseDisposition::Reuse,
            },
            Duration::from_secs(2),
        )
        .expect("release")
    {
        CallOutcome::Replied(WorkerPoolReply::Release(ReleaseOutcome::Released)) => {}
        other => panic!("expected released, got {other:?}"),
    }

    match retained.close_and_drain(Duration::from_secs(2)) {
        KeepaliveCloseAndDrain::Drained(report) => {
            assert_eq!(report.drain, KeepalivePoolDrainOutcome::Drained);
            assert_eq!(report.stopped, 1);
        }
        other => panic!("expected drained after retry, got {other:?}"),
    }

    let _ = app.shutdown().join();
    server.stop();
}

#[test]
fn drain_timeout_reports_observed_leased_not_pool_capacity() {
    // Regression: capacity-seeded last_leased lied when capacity > outstanding
    // leases. Capacity 2 with one held lease must report pending.leased == 1,
    // never 2. A re-seed from connections.len()/capacity makes this fail when
    // the timeout path runs off the seed (before/without a later sample body).
    let server = ScriptedServer::start();
    let app = system();
    let pool = app
        .install_keepalive_pool(config(HttpTarget::http(server.addr), 2))
        .expect("install capacity 2");

    let lease = match app
        .call_blocking(pool.pool(), WorkerPoolMsg::Acquire, Duration::from_secs(2))
        .expect("acquire one of two")
    {
        CallOutcome::Replied(WorkerPoolReply::Acquire(AcquireOutcome::Acquired(lease))) => lease,
        other => panic!("expected acquired, got {other:?}"),
    };

    // Sanity: pressure sees exact leased=1 under capacity=2 before drain.
    match app
        .call_blocking(
            pool.pool(),
            WorkerPoolMsg::PressureReport,
            Duration::from_secs(2),
        )
        .expect("pressure")
    {
        CallOutcome::Replied(WorkerPoolReply::Pressure(report)) => {
            assert_eq!(report.capacity, 2);
            assert_eq!(report.leased, 1);
        }
        other => panic!("expected pressure, got {other:?}"),
    }

    let retained = match pool.close_and_drain(Duration::from_millis(200)) {
        KeepaliveCloseAndDrain::TimedOut { pool, pending } => {
            assert_eq!(
                pending.leased,
                Some(1),
                "pending.leased must be observed exact count, not capacity 2"
            );
            assert_ne!(pending.leased, Some(2), "capacity seed must not appear");
            assert_eq!(pending.connections_live, 2);
            assert!(pending.admission_closed);
            pool
        }
        other => panic!("expected timed out retention, got {other:?}"),
    };

    match app
        .call_blocking(
            retained.pool(),
            WorkerPoolMsg::Release {
                lease,
                disposition: ReleaseDisposition::Reuse,
            },
            Duration::from_secs(2),
        )
        .expect("release")
    {
        CallOutcome::Replied(WorkerPoolReply::Release(ReleaseOutcome::Released)) => {}
        other => panic!("expected released, got {other:?}"),
    }

    match retained.close_and_drain(Duration::from_secs(2)) {
        KeepaliveCloseAndDrain::Drained(report) => {
            assert_eq!(report.drain, KeepalivePoolDrainOutcome::Drained);
            assert_eq!(report.stopped, 2);
        }
        other => panic!("expected drained after release, got {other:?}"),
    }

    let _ = app.shutdown().join();
    server.stop();
}

#[test]
fn partial_connection_stop_timeout_reports_only_unsettled_slots() {
    let app = system();
    let pool = app
        .install_keepalive_pool(config(HttpTarget::http("127.0.0.1:9".parse().unwrap()), 2))
        .expect("install");

    let retained = match pool.close_and_drain_with_stop_timeout_at(Duration::from_secs(2), 1) {
        KeepaliveCloseAndDrain::TimedOut { pool, pending } => {
            assert_eq!(pending.leased, Some(0));
            assert_eq!(pending.connections_live, 1);
            assert!(pending.admission_closed);
            pool
        }
        other => panic!("expected injected stop timeout, got {other:?}"),
    };
    match retained.close_and_drain(Duration::from_secs(2)) {
        KeepaliveCloseAndDrain::Drained(report) => {
            assert_eq!(report.requested, 2);
            assert_eq!(report.stopped + report.already_closed, 2);
        }
        other => panic!("expected retry to finish remaining slot, got {other:?}"),
    }
    let _ = app.shutdown().join();
}

#[derive(Debug)]
enum SpinnerMsg {
    Tick,
}

struct Spinner {
    flag: Arc<AtomicBool>,
    entered: Arc<AtomicBool>,
}

#[tina_runtime::isolate(message = SpinnerMsg, reply = (), shard = TestShard)]
impl Spinner {
    fn handle(
        &mut self,
        msg: SpinnerMsg,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            SpinnerMsg::Tick => {
                self.entered.store(true, Ordering::Relaxed);
                let target = std::time::Instant::now() + Duration::from_millis(500);
                while std::time::Instant::now() < target {
                    if self.flag.load(Ordering::Relaxed) {
                        break;
                    }
                    std::hint::spin_loop();
                }
                noop()
            }
        }
    }

    fn handle_call(&mut self, _msg: SpinnerMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        call.reject(tina::CallRejectedReason::UnsupportedMessage)
    }
}

#[test]
fn owner_failure_is_distinct_from_drain_and_shutdown() {
    // Saturate the host-control queue while the shard worker is spinning so
    // close_and_drain surfaces OwnerFailed(CommandFull) — not TimedOut and
    // not Shutdown, and without claiming a full drain.
    let server = ScriptedServer::start();
    let app = LocalSystem::single_shard(TestShard, DefaultThreadedMailboxFactory)
        .ingress_capacity(1)
        .try_build()
        .expect("local system with tight ingress");

    let pool = app
        .install_keepalive_pool(config(HttpTarget::http(server.addr), 1))
        .expect("install");

    let release = Arc::new(AtomicBool::new(false));
    let entered = Arc::new(AtomicBool::new(false));
    let spinner = app
        .register_root::<Spinner, Infallible>(
            Spinner {
                flag: Arc::clone(&release),
                entered: Arc::clone(&entered),
            },
            4,
        )
        .expect("register spinner");
    app.try_send(spinner, SpinnerMsg::Tick)
        .expect("occupy worker");
    while !entered.load(Ordering::Acquire) {
        thread::yield_now();
    }
    // Fill the single-slot ingress so the next host-control command is refused.
    app.try_send(spinner, SpinnerMsg::Tick)
        .expect("fill ingress");

    let mut retained = match pool.close_and_drain(Duration::from_millis(200)) {
        KeepaliveCloseAndDrain::OwnerFailed {
            error,
            pool: retained,
            pending,
        } => {
            assert_eq!(error, ThreadedRuntimeError::CommandFull);
            assert_eq!(pending.connections_live, 1);
            retained
        }
        other => panic!("expected owner failed CommandFull, got {other:?}"),
    };

    // Handle retained — admitted work was not force-aborted. Free the worker
    // and finish an explicit drain.
    release.store(true, Ordering::Release);
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        match retained.close_and_drain(Duration::from_millis(200)) {
            KeepaliveCloseAndDrain::Drained(report) => {
                assert_eq!(report.drain, KeepalivePoolDrainOutcome::Drained);
                break;
            }
            KeepaliveCloseAndDrain::OwnerFailed {
                pool: again, error, ..
            } if matches!(error, ThreadedRuntimeError::CommandFull)
                && std::time::Instant::now() < deadline =>
            {
                retained = again;
                thread::yield_now();
            }
            KeepaliveCloseAndDrain::TimedOut { pool: again, .. }
                if std::time::Instant::now() < deadline =>
            {
                retained = again;
            }
            other => panic!("expected eventual drained after owner recovery, got {other:?}"),
        }
    }

    let _ = app.shutdown().join();
    server.stop();
}

#[test]
fn system_shutdown_settles_without_claiming_a_full_drain() {
    let server = ScriptedServer::start();
    let app = system();
    let pool = app
        .install_keepalive_pool(config(HttpTarget::http(server.addr), 1))
        .expect("install");

    // Request runtime shutdown while the install handle is still live.
    let handle = app.shutdown_handle();
    let terminal = handle
        .request_and_wait_report(Duration::from_secs(2))
        .expect("complete shutdown while pool handle is live");
    assert_eq!(
        terminal.topology().expect("terminal topology").shards()[0].state(),
        tina_runtime::LiveShardState::Stopped
    );

    match pool.close_and_drain(Duration::from_millis(500)) {
        KeepaliveCloseAndDrain::Shutdown(settlement) => {
            assert_ne!(settlement.drain, KeepalivePoolDrainOutcome::Drained);
        }
        other => panic!("proven stopped owner must classify as Shutdown, got {other:?}"),
    }

    let _ = app.shutdown().join();
    server.stop();
}

#[test]
fn consuming_close_makes_double_close_unrepresentable() {
    // Compile-time shape: close_and_drain takes self by value. After a
    // successful drain there is no handle left to call again. After a
    // timeout the only handle is the returned one — still one owner.
    let server = ScriptedServer::start();
    let app = system();
    let pool = app
        .install_keepalive_pool(config(HttpTarget::http(server.addr), 1))
        .expect("install");
    let report = match pool.close_and_drain(Duration::from_secs(2)) {
        KeepaliveCloseAndDrain::Drained(report) => report,
        other => panic!("expected drained, got {other:?}"),
    };
    assert_eq!(report.stopped, 1);
    // `pool` is moved; a second close cannot be written without the
    // retained timeout handle path.
    let _ = app.shutdown().join();
    server.stop();
}

#[test]
fn raw_threaded_build_and_shutdown_apis_still_work() {
    // Blast-radius: existing raw APIs retain behavior alongside the facade.
    let server = ScriptedServer::start();
    let runtime = ThreadedRuntime::try_new(TestShard, DefaultThreadedMailboxFactory)
        .expect("threaded runtime");
    let handles = build_keepalive_pool(
        &runtime,
        HttpTarget::http(server.addr),
        HttpClientConfig::pressure(),
        PoolConfig::new(1, 4),
        8,
        16,
    )
    .expect("build raw pool");
    let report =
        shutdown_keepalive_pool(&runtime, &handles, CloseMode::Drain, Duration::from_secs(2))
            .expect("raw shutdown");
    assert_eq!(report.drain, KeepalivePoolDrainOutcome::Drained);
    assert_eq!(report.stopped, 1);
    let _ = runtime.shutdown();
    server.stop();
}
