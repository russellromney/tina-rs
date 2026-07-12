//! Canonical copied Tina service path.
//!
//! Copy this shape for a normal service. It is the smallest split-service
//! specimen that is still real end to end:
//!
//! - one `#[tina_runtime::isolate(event = .. request = .. reply = ..)]`
//!   isolate registered behind a fallible `LocalSystem` application facade;
//! - bounded admission via `SharedCapacityScope` — over-capacity callers
//!   get a visible `Full`, not a silent queue;
//! - a durable-state step: the isolate is seeded with "recovered" records
//!   before it accepts traffic, and every admitted request commits one
//!   more record to that ledger before it is held for work (a real
//!   in-process stand-in for a WAL/DB write — swap `Gateway::ledger` for
//!   your real store);
//! - real concurrent callers driven through `tina-proof-harness`'s load
//!   runner against the real runtime, not synthesized numbers;
//! - graceful shutdown whose leak check proves the request-aware flow owns
//!   every charge until completion or owner stop, and therefore releases every
//!   admitted charge exactly once.
//!
//! What this specimen deliberately leaves out (see `mini_saas_api` for a
//! larger, HTTP-fronted shape): native protocol clients, session control,
//! run capture/replay, join/select call sets. Those are real Tina
//! capabilities but they do not belong in the first thing a user copies.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

#[cfg(test)]
use std::{thread, time::Instant};

use tina::CallRejectedReason;
use tina::capacity::CapacityMode;
use tina::prelude::*;
use tina_proof_harness::{
    LoadObservation, LoadReport, LoadRun, LoadStop, OpOutcome, SurfacePlateau,
    assert_cold_work_made_progress, assert_no_leaked_capacity_at_shutdown,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, LocalSystem, LocalSystemConfig,
    ServicePressureReport, SharedCapacityScope, SharedLease, SleepReply, SplitServiceHandle,
    ThreadedRuntimeError, sleep,
};

/// Tunables for one run.
#[derive(Debug, Clone, Copy)]
pub struct RunConfig {
    /// In-flight admission cap (`SharedCapacityScope` weight budget).
    pub capacity: usize,
    pub mailbox: usize,
    /// How long each admitted request is "held" before it replies —
    /// stands in for real work (a downstream call, a computation).
    pub work_ms: u64,
    /// Concurrent callers. Set above `capacity` to exercise `Full`.
    pub callers: usize,
    pub call_timeout_ms: u64,
    /// Maximum concurrently armed runtime timers. Exposed so the specimen can
    /// prove the typed `WorkFailed(TimerFull)` path without hidden config.
    pub timer_capacity: usize,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            capacity: 2,
            mailbox: 32,
            work_ms: 40,
            callers: 6,
            call_timeout_ms: 2_000,
            timer_capacity: LocalSystemConfig::default().timer_capacity,
        }
    }
}

/// Aggregate report for one run. Every field here is read back from a
/// real runtime, not constructed ahead of time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunReport {
    pub admitted: usize,
    pub full: usize,
    /// Records recovered before the isolate accepted traffic — the
    /// durable-restore step.
    pub ledger_seed_len: usize,
    /// Ledger length read back from the isolate before shutdown: the
    /// seed plus one entry per admitted request.
    pub ledger_final_len: usize,
    pub scope_high_water: usize,
    pub scope_full_count: u64,
    pub scope_admitted: u64,
    pub scope_released: u64,
    /// Scope `current` after terminal runtime observation. Owner stop must
    /// release every held charge; a non-zero value here is a lease leak.
    pub scope_current_at_drain: usize,
    pub discovery_line: String,
    pub summary_line: String,
    /// Full proof-harness report: latency, pressure, and the leak
    /// verdict `assert_no_leaked_capacity_at_shutdown` checked.
    pub load: LoadReport,
}

enum GatewayEvent {
    Flow(GatewayFlow),
    #[cfg(test)]
    BlockWorker {
        entered: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    },
}

#[derive(Debug)]
enum GatewayRequest {
    Submit(u32),
    Stats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum GatewayReply {
    Accepted { id: u64, ledger_len: usize },
    Full { current: usize, max: usize },
    WorkFailed(tina_runtime::CallError),
    Stats { ledger_len: usize },
}

struct Gateway {
    scope: SharedCapacityScope,
    /// Durable-state stand-in: committed record ids. Seeded from
    /// "recovered" state before the isolate takes traffic; every
    /// admitted request appends one entry before it is held for work.
    ledger: Vec<u64>,
    next_id: u64,
    hold: Duration,
}

tina::flow! {
    flow GatewayFlow for Gateway {
        reply GatewayReply;

        step HoldDone(id: u64, lease: SharedLease) -> raw request SleepReply {
            self.hold_done(req, id, lease, outcome)
        }
    }
}

#[tina_runtime::isolate(event = GatewayEvent, request = GatewayRequest, reply = GatewayReply)]
impl Gateway {
    fn handle_event(
        &mut self,
        event: GatewayEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            GatewayEvent::Flow(flow) => self.handle_gateway_flow(flow),
            #[cfg(test)]
            GatewayEvent::BlockWorker { entered, release } => {
                entered.send(()).expect("signal blocked test worker");
                release
                    .recv_timeout(Duration::from_secs(5))
                    .expect("release blocked test worker");
                noop()
            }
        }
    }

    fn handle_request(
        &mut self,
        request: GatewayRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            GatewayRequest::Submit(payload) => self.submit(payload, call),
            GatewayRequest::Stats => call.reply(GatewayReply::Stats {
                ledger_len: self.ledger.len(),
            }),
        }
    }
}

impl Gateway {
    fn new(scope: SharedCapacityScope, seed_ledger: Vec<u64>, hold: Duration) -> Self {
        Self {
            scope,
            ledger: seed_ledger,
            next_id: 1,
            hold,
        }
    }

    fn submit(&mut self, payload: u32, call: RequestCall<'_, Self>) -> RequestEffect<Self> {
        // Payload content is not branched on; its presence proves a real
        // request round-trip, not a canned reply.
        let _ = payload;
        let id = self.next_id;
        let hold = self.hold;

        call.capture(|request| {
            if !request.is_open() {
                return noop();
            }
            let lease = match self.scope.try_admit(1) {
                Ok(lease) => lease,
                Err(full) => {
                    return reply_to(
                        request,
                        GatewayReply::Full {
                            current: full.current,
                            max: full.max,
                        },
                    );
                }
            };
            // Durable-state step: commit before simulated work starts.
            self.ledger.push(id);
            self.next_id = id + 1;
            sleep(hold).then_service_event_with_request(request, move |request, outcome| {
                GatewayEvent::Flow(GatewayFlow::HoldDone(request, id, lease, outcome))
            })
        })
    }

    fn hold_done(
        &mut self,
        request: RequestContext<GatewayReply>,
        id: u64,
        lease: SharedLease,
        result: SleepReply,
    ) -> Effect<Self> {
        drop(lease);
        if !request.is_open() {
            return noop();
        }
        if let Err(error) = result {
            return reply_to(request, GatewayReply::WorkFailed(error));
        }
        let ledger_len = self.ledger.len();
        reply_to(request, GatewayReply::Accepted { id, ledger_len })
    }
}

/// Run the copied service path once: register the isolate on a real
/// `LocalSystem`, drive real concurrent callers through it, prove
/// progress and a clean shutdown, then report what actually happened.
pub fn run(config: RunConfig) -> anyhow::Result<RunReport> {
    let local_config = LocalSystemConfig {
        timer_capacity: config.timer_capacity,
        ..LocalSystemConfig::default()
    };
    let runtime = Arc::new(
        LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory)
            .config(local_config)
            .try_build()?,
    );
    let shutdown = runtime.shutdown_handle();
    let scope =
        SharedCapacityScope::new("copied_service_path.in_flight", "requests", config.capacity);
    // "Recovered" state seeded before the isolate accepts traffic — the
    // durable-restore half of the durable-state step.
    let seed_ledger: Vec<u64> = vec![0];
    let ledger_seed_len = seed_ledger.len();

    let gateway_result: Result<SplitServiceHandle<GatewayEvent, GatewayRequest, GatewayReply>, _> =
        runtime.register_split_service::<Gateway, GatewayEvent, GatewayRequest, Infallible>(
            Gateway::new(
                scope.clone(),
                seed_ledger,
                Duration::from_millis(config.work_ms),
            ),
            config.mailbox,
        );
    let gateway = match gateway_result {
        Ok(gateway) => gateway,
        Err(error) => {
            let shutdown_result = shutdown_runtime(shutdown, runtime);
            return match shutdown_result {
                Ok(()) => Err(anyhow::anyhow!("register gateway: {error:?}")),
                Err(shutdown_error) => Err(anyhow::anyhow!(
                    "register gateway: {error:?}; shutdown also failed: {shutdown_error}"
                )),
            };
        }
    };
    let requests = gateway.requests;

    let call_timeout = Duration::from_millis(config.call_timeout_ms);
    let rt_for_ops = Arc::clone(&runtime);

    let mut load = tina_proof_harness::load::run_with_observation(
        LoadRun {
            workers: config.callers,
            stop: LoadStop::ops(config.callers as u64),
            label: "copied_service_path",
        },
        move |worker_id| match rt_for_ops.call_blocking_request(
            requests,
            GatewayRequest::Submit(worker_id as u32),
            call_timeout,
        ) {
            Ok(CallOutcome::Replied(GatewayReply::Accepted { .. })) => OpOutcome::Ok,
            Ok(CallOutcome::Replied(GatewayReply::Full { .. })) => OpOutcome::Err { kind: "full" },
            Ok(CallOutcome::Replied(GatewayReply::WorkFailed(error))) => OpOutcome::Err {
                kind: call_error_kind(error),
            },
            Ok(CallOutcome::Replied(GatewayReply::Stats { .. })) => OpOutcome::Err {
                kind: "unexpected_reply",
            },
            Ok(CallOutcome::Full) => OpOutcome::Err {
                kind: "mailbox_full",
            },
            Ok(CallOutcome::Closed) => OpOutcome::Err { kind: "closed" },
            Ok(CallOutcome::Timeout) => OpOutcome::Timeout,
            Ok(CallOutcome::Rejected(reason)) => OpOutcome::Err {
                kind: rejected_kind(reason),
            },
            Err(error) => OpOutcome::Err {
                kind: host_error_kind(error),
            },
        },
        None::<fn() -> LoadObservation>,
    );

    let stats_timeout = Duration::from_millis(
        config
            .work_ms
            .saturating_add(1_000)
            .max(config.call_timeout_ms),
    );
    let report_result = (|| -> anyhow::Result<_> {
        let ledger_final_len =
            match runtime.call_blocking_request(requests, GatewayRequest::Stats, stats_timeout) {
                Ok(CallOutcome::Replied(GatewayReply::Stats { ledger_len })) => ledger_len,
                Ok(CallOutcome::Replied(GatewayReply::Accepted { .. })) => {
                    anyhow::bail!("stats returned an Accepted reply")
                }
                Ok(CallOutcome::Replied(GatewayReply::Full { .. })) => {
                    anyhow::bail!("stats returned a Full reply")
                }
                Ok(CallOutcome::Replied(GatewayReply::WorkFailed(error))) => {
                    anyhow::bail!("stats returned a work failure: {error:?}")
                }
                Ok(CallOutcome::Full) => anyhow::bail!("stats mailbox was full"),
                Ok(CallOutcome::Closed) => anyhow::bail!("stats service was closed"),
                Ok(CallOutcome::Timeout) => anyhow::bail!("stats call timed out"),
                Ok(CallOutcome::Rejected(reason)) => anyhow::bail!("stats rejected: {reason:?}"),
                Err(error) => anyhow::bail!("stats host call failed: {error}"),
            };

        let discovery_line = scope.discovery_line();
        let pre_shutdown_snap = scope.snapshot();
        let admitted = pre_shutdown_snap.admitted as usize;
        let full = load
            .err_kinds
            .iter()
            .find(|(kind, _)| kind.as_str() == "full")
            .map(|(_, count)| *count as usize)
            .unwrap_or(0);
        Ok((
            admitted,
            full,
            ledger_final_len,
            discovery_line,
            pre_shutdown_snap,
        ))
    })();

    // Graceful shutdown: owner stop must release every held charge. The
    // post-shutdown snapshot is the load-bearing proof of that claim.
    let shutdown_result = shutdown_runtime(shutdown, runtime);
    let post_shutdown_snap = scope.snapshot();
    let settlement_result = if post_shutdown_snap.current == 0
        && post_shutdown_snap.admitted == post_shutdown_snap.released
    {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "shutdown leaked scope authority: current={} admitted={} released={}",
            post_shutdown_snap.current,
            post_shutdown_snap.admitted,
            post_shutdown_snap.released
        ))
    };

    shutdown_result?;
    settlement_result?;
    let (admitted, full, ledger_final_len, discovery_line, pre_shutdown_snap) = report_result?;

    let mut pressure = ServicePressureReport::new("system_copied_service_path");
    pressure.add_measured("scope", scope.surface_report(CapacityMode::Fixed));
    load.leak_checked = true;
    load.surface_plateaus = SurfacePlateau::from_service_pressure(&pressure);
    load.leak_clean = post_shutdown_snap.current == 0
        && post_shutdown_snap.admitted == post_shutdown_snap.released
        && load
            .surface_plateaus
            .iter()
            .all(|surface| surface.leak_clean);

    // Proof assertions run only after the runtime is terminal, so a failed
    // assertion cannot strand the worker or its linear authority.
    if load.ops_ok > 0 {
        assert_cold_work_made_progress(&load);
    } else if admitted == 0 {
        anyhow::bail!("load made no progress: no request reached admission");
    }
    assert_no_leaked_capacity_at_shutdown(&load);

    let summary_line = format!(
        "system=system_copied_service_path admitted={} full={} ledger_seed_len={} ledger_final_len={} scope_high_water={} scope_full_count={} scope_current_at_drain={}",
        admitted,
        full,
        ledger_seed_len,
        ledger_final_len,
        pre_shutdown_snap.high_water,
        post_shutdown_snap.full_count,
        post_shutdown_snap.current,
    );

    Ok(RunReport {
        admitted,
        full,
        ledger_seed_len,
        ledger_final_len,
        scope_high_water: pre_shutdown_snap.high_water,
        scope_full_count: post_shutdown_snap.full_count,
        scope_admitted: post_shutdown_snap.admitted,
        scope_released: post_shutdown_snap.released,
        scope_current_at_drain: post_shutdown_snap.current,
        discovery_line,
        summary_line,
        load,
    })
}

fn call_error_kind(error: tina_runtime::CallError) -> &'static str {
    use tina_runtime::CallError;

    match error {
        CallError::InvariantViolation => "work_invariant_violation",
        CallError::InvalidResource => "work_invalid_resource",
        CallError::NotFound => "work_not_found",
        CallError::Io => "work_io",
        CallError::Unsupported => "work_unsupported",
        CallError::ResourceBusy => "work_resource_busy",
        CallError::CorruptRecord => "work_corrupt_record",
        CallError::CommitUncertain => "work_commit_uncertain",
        CallError::StorageFull => "work_storage_full",
        CallError::StorageClosed => "work_storage_closed",
        CallError::TargetFull => "work_target_full",
        CallError::TargetClosed => "work_target_closed",
        CallError::Timeout => "work_timeout",
        CallError::Rejected(reason) => match reason {
            CallRejectedReason::ForeignSystem { .. } => "work_rejected_foreign_system",
            CallRejectedReason::ReplyAbandoned => "work_rejected_reply_abandoned",
            CallRejectedReason::HandlerPanicked => "work_rejected_handler_panicked",
            CallRejectedReason::UnsupportedMessage => "work_rejected_unsupported_message",
        },
        CallError::DnsFull => "work_dns_full",
        CallError::DnsClosed => "work_dns_closed",
        CallError::TlsFull => "work_tls_full",
        CallError::TlsClosed => "work_tls_closed",
        CallError::TlsCertificate => "work_tls_certificate",
        CallError::TlsName => "work_tls_name",
        CallError::TlsHandshake => "work_tls_handshake",
        CallError::TlsAlpnMismatch => "work_tls_alpn_mismatch",
        CallError::SignalFull => "work_signal_full",
        CallError::SignalClosed => "work_signal_closed",
        CallError::ProcessFull => "work_process_full",
        CallError::ProcessClosed => "work_process_closed",
        CallError::KillUncertain => "work_kill_uncertain",
        CallError::TimerFull => "work_timer_full",
    }
}

fn rejected_kind(reason: CallRejectedReason) -> &'static str {
    match reason {
        CallRejectedReason::ForeignSystem { .. } => "rejected_foreign_system",
        CallRejectedReason::ReplyAbandoned => "rejected_reply_abandoned",
        CallRejectedReason::HandlerPanicked => "rejected_handler_panicked",
        CallRejectedReason::UnsupportedMessage => "rejected_unsupported_message",
    }
}

fn host_error_kind(error: ThreadedRuntimeError) -> &'static str {
    match error {
        ThreadedRuntimeError::ForeignSystem { .. } => "host_foreign_system",
        ThreadedRuntimeError::ParentStopped => "host_parent_stopped",
        ThreadedRuntimeError::WorkerStopped => "host_worker_stopped",
        ThreadedRuntimeError::UnknownShard(_) => "host_unknown_shard",
        ThreadedRuntimeError::DriverShutdownFailed => "host_driver_shutdown_failed",
        ThreadedRuntimeError::DriverParkFailed => "host_driver_park_failed",
        ThreadedRuntimeError::CommandFull => "host_command_full",
        ThreadedRuntimeError::HostWaitTimeout => "host_wait_timeout",
        ThreadedRuntimeError::WorkerUnresponsive => "host_worker_unresponsive",
    }
}

fn shutdown_runtime(
    shutdown: tina_runtime::ThreadedShutdownHandle,
    runtime: Arc<LocalSystem<SingleShard, DefaultThreadedMailboxFactory>>,
) -> anyhow::Result<()> {
    let terminal = shutdown.request_and_wait_report(Duration::from_secs(5))?;
    drop(runtime);
    terminal.ensure_clean()?;
    Ok(())
}

#[cfg(test)]
mod adversarial_tests {
    use super::*;

    type TestSystem = Arc<LocalSystem<SingleShard, DefaultThreadedMailboxFactory>>;
    type TestGateway = SplitServiceHandle<GatewayEvent, GatewayRequest, GatewayReply>;

    fn test_system(
        scope: SharedCapacityScope,
        hold: Duration,
    ) -> (
        TestSystem,
        tina_runtime::ThreadedShutdownHandle,
        TestGateway,
    ) {
        let runtime = Arc::new(
            LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory)
                .try_build()
                .expect("test local system"),
        );
        let shutdown = runtime.shutdown_handle();
        let gateway = runtime
            .register_split_service::<Gateway, GatewayEvent, GatewayRequest, Infallible>(
                Gateway::new(scope, vec![0], hold),
                256,
            )
            .expect("register test gateway");
        (runtime, shutdown, gateway)
    }

    #[test]
    fn foreign_system_terminals_remain_distinct_in_reports() {
        let expected = tina::SystemIncarnation::new(1);
        let actual = tina::SystemIncarnation::new(2);

        assert_eq!(
            call_error_kind(tina_runtime::CallError::Rejected(
                CallRejectedReason::ForeignSystem { expected, actual },
            )),
            "work_rejected_foreign_system"
        );
        assert_eq!(
            rejected_kind(CallRejectedReason::ForeignSystem { expected, actual }),
            "rejected_foreign_system"
        );
        assert_eq!(
            host_error_kind(ThreadedRuntimeError::ForeignSystem { expected, actual }),
            "host_foreign_system"
        );
        assert_eq!(
            host_error_kind(ThreadedRuntimeError::ParentStopped),
            "host_parent_stopped"
        );
    }

    #[test]
    fn calls_closed_while_queued_never_cross_durable_admission_boundary() {
        let scope = SharedCapacityScope::new("queued", "requests", 256);
        let (runtime, shutdown, gateway) = test_system(scope.clone(), Duration::from_millis(1));
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();

        runtime
            .try_send_event(
                gateway.events,
                GatewayEvent::BlockWorker {
                    entered: entered_tx,
                    release: release_rx,
                },
            )
            .expect("queue worker blocker");
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("worker must enter blocker");

        for payload in 0..8 {
            let outcome = runtime.call_blocking_request(
                gateway.requests,
                GatewayRequest::Submit(payload),
                Duration::ZERO,
            );
            assert!(
                matches!(
                    outcome,
                    Ok(CallOutcome::Timeout) | Err(ThreadedRuntimeError::HostWaitTimeout)
                ),
                "zero-deadline queued call must time out: {outcome:?}"
            );
        }
        release_tx.send(()).expect("release worker");

        let ledger_len = match runtime
            .call_blocking_request(
                gateway.requests,
                GatewayRequest::Stats,
                Duration::from_secs(2),
            )
            .expect("stats host call")
        {
            CallOutcome::Replied(GatewayReply::Stats { ledger_len }) => ledger_len,
            other => panic!("unexpected stats outcome: {other:?}"),
        };
        let snapshot = scope.snapshot();
        assert_eq!(snapshot.admitted, 0, "closed calls reached admission");
        assert_eq!(snapshot.released, 0, "closed calls created leases");
        assert_eq!(ledger_len, 1, "closed calls committed durable records");

        shutdown_runtime(shutdown, runtime).expect("clean queued-call shutdown");
    }

    #[test]
    fn owner_shutdown_drops_in_flight_request_and_lease_exactly_once() {
        let scope = SharedCapacityScope::new("owner_stop", "requests", 1);
        let (runtime, shutdown, gateway) = test_system(scope.clone(), Duration::from_secs(30));
        let caller_runtime = Arc::clone(&runtime);
        let caller = thread::spawn(move || {
            caller_runtime.call_blocking_request(
                gateway.requests,
                GatewayRequest::Submit(7),
                Duration::from_secs(30),
            )
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        let driver_call_is_pending = || {
            runtime
                .topology()
                .shard(gateway.requests.address().shard())
                .is_some_and(|shard| shard.pending_driver_call_count() > 0)
        };
        while (scope.snapshot().current != 1 || !driver_call_is_pending())
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            scope.snapshot().current,
            1,
            "request never reached held work"
        );
        assert!(
            driver_call_is_pending(),
            "held sleep never reached driver admission"
        );

        shutdown_runtime(shutdown, runtime).expect("owner shutdown must be clean");
        let outcome = caller.join().expect("caller thread must not panic");
        assert!(
            matches!(
                outcome,
                Ok(CallOutcome::Closed)
                    | Ok(CallOutcome::Replied(GatewayReply::WorkFailed(
                        tina_runtime::CallError::TargetClosed
                    )))
                    | Err(ThreadedRuntimeError::WorkerStopped)
            ),
            "owner shutdown produced an unexpected terminal: {outcome:?}"
        );
        let snapshot = scope.snapshot();
        assert_eq!(snapshot.current, 0);
        assert_eq!(snapshot.admitted, 1);
        assert_eq!(snapshot.released, 1);
    }
}
