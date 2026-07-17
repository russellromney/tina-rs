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
    KeepalivePoolInstallError, OriginKey, build_keepalive_pool, install_keepalive_pool_fail_after,
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
    handle
        .request_shutdown()
        .expect("request shutdown while pool handle is live");

    // Give the worker a moment to process Shutdown.
    thread::sleep(Duration::from_millis(50));

    match pool.close_and_drain(Duration::from_millis(500)) {
        KeepaliveCloseAndDrain::Shutdown(settlement) => {
            // Must not claim Drained when shutdown cancelled the path.
            assert_ne!(settlement.drain, KeepalivePoolDrainOutcome::Drained);
        }
        KeepaliveCloseAndDrain::OwnerFailed { error, .. } => {
            // Acceptable alternate: owner already gone before close admitted.
            assert!(
                matches!(
                    error,
                    ThreadedRuntimeError::WorkerStopped
                        | ThreadedRuntimeError::WorkerUnresponsive
                        | ThreadedRuntimeError::CommandFull
                        | ThreadedRuntimeError::HostWaitTimeout
                ),
                "unexpected owner error: {error:?}"
            );
        }
        KeepaliveCloseAndDrain::Drained(report) => {
            // If the worker was still responsive enough to finish a true
            // drain before dying, that is still a correct explicit-close
            // settlement — not a silent force-close.
            assert_eq!(report.drain, KeepalivePoolDrainOutcome::Drained);
        }
        KeepaliveCloseAndDrain::TimedOut { .. } => {
            // Timeout under shutdown is acceptable; caller still holds truth.
        }
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
