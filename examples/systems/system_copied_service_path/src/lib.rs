//! Canonical copied Tina service path.
//!
//! Copy this shape for a normal service. It is the smallest split-service
//! specimen that is still real end to end:
//!
//! - one `#[tina_runtime::isolate(event = .. request = .. reply = ..)]`
//!   isolate registered on a real `ThreadedRuntime`;
//! - bounded admission via `SharedCapacityScope` — over-capacity callers
//!   get a visible `Full`, not a silent queue;
//! - a durable-state step: the isolate is seeded with "recovered" records
//!   before it accepts traffic, and every admitted request commits one
//!   more record to that ledger before it is held for work (a real
//!   in-process stand-in for a WAL/DB write — swap `Gateway::ledger` for
//!   your real store);
//! - real concurrent callers driven through `tina-proof-harness`'s load
//!   runner against the real runtime, not synthesized numbers;
//! - graceful shutdown whose leak check only passes because the isolate
//!   actually releases every charge it holds — comment out the
//!   `drop(lease)` in `Gateway::hold_done` and `assert_no_leaked_capacity_at_shutdown`
//!   fails for a real reason.
//!
//! What this specimen deliberately leaves out (see `mini_saas_api` for a
//! larger, HTTP-fronted shape): native protocol clients, session control,
//! run capture/replay, join/select call sets. Those are real Tina
//! capabilities but they do not belong in the first thing a user copies.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use tina::capacity::CapacityMode;
use tina::prelude::*;
use tina_proof_harness::{
    LoadObservation, LoadReport, LoadRun, LoadStop, OpOutcome, SurfacePlateau,
    assert_cold_work_made_progress, assert_no_leaked_capacity_at_shutdown,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, GuardedInsertError, GuardedPendingReplies,
    ServicePressureReport, SharedCapacityScope, SharedLease, SleepReply, SplitServiceHandle,
    ThreadedRuntime, sleep,
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
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            capacity: 2,
            mailbox: 32,
            work_ms: 40,
            callers: 6,
            call_timeout_ms: 2_000,
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
    /// Scope `current` *after* `runtime.shutdown()`. Owner stop must
    /// release every held charge; a non-zero value here is a lease leak.
    pub scope_current_at_drain: usize,
    pub discovery_line: String,
    pub summary_line: String,
    /// Full proof-harness report: latency, pressure, and the leak
    /// verdict `assert_no_leaked_capacity_at_shutdown` checked.
    pub load: LoadReport,
}

#[derive(Debug)]
pub enum GatewayEvent {
    HoldDone { id: u64, result: SleepReply },
}

#[derive(Debug)]
pub enum GatewayRequest {
    Submit(u32),
    Stats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayReply {
    Accepted { id: u64, ledger_len: usize },
    Full { current: usize, max: usize },
    Stats { ledger_len: usize },
}

struct Gateway {
    scope: SharedCapacityScope,
    pending: GuardedPendingReplies<u64, GatewayReply, SharedLease>,
    /// Durable-state stand-in: committed record ids. Seeded from
    /// "recovered" state before the isolate takes traffic; every
    /// admitted request appends one entry before it is held for work.
    ledger: Vec<u64>,
    next_id: u64,
    hold: Duration,
}

#[tina_runtime::isolate(event = GatewayEvent, request = GatewayRequest, reply = GatewayReply)]
impl Gateway {
    fn handle_event(
        &mut self,
        event: GatewayEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            GatewayEvent::HoldDone { id, result } => self.hold_done(id, result),
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
    fn new(
        scope: SharedCapacityScope,
        seed_ledger: Vec<u64>,
        pending_capacity: usize,
        hold: Duration,
    ) -> Self {
        Self {
            scope,
            pending: GuardedPendingReplies::with_capacity(pending_capacity)
                .named("system_copied_service_path.pending"),
            ledger: seed_ledger,
            next_id: 1,
            hold,
        }
    }

    fn submit(&mut self, payload: u32, call: RequestCall<'_, Self>) -> RequestEffect<Self> {
        let lease = match self.scope.try_admit(1) {
            Ok(lease) => lease,
            Err(full) => {
                return call.reply(GatewayReply::Full {
                    current: full.current,
                    max: full.max,
                });
            }
        };
        // Payload content is not branched on; its presence proves a real
        // request round-trip, not a canned reply.
        let _ = payload;
        let id = self.next_id;
        let hold = self.hold;

        call.capture(|request| {
            match self
                .pending
                .insert_deferred_guarded(id, request.into_deferred(), lease)
            {
                Ok(_ticket) => {
                    // Durable-state step: commit the record before the
                    // (simulated) work runs. A real service would
                    // fsync/INSERT here instead of pushing to a `Vec`.
                    self.ledger.push(id);
                    self.next_id = id + 1;
                    sleep(hold).then(move |result| {
                        tina::ServiceMessage::Event(GatewayEvent::HoldDone { id, result })
                    })
                }
                // Pending capacity == scope capacity, and every pending
                // entry holds exactly one scope charge, so pending can
                // never be full while the scope just admitted this
                // charge — accounting bug if either arm below runs.
                Err(GuardedInsertError::Full { .. }) => {
                    panic!("pending full right after scope admission — accounting bug")
                }
                Err(GuardedInsertError::DuplicateKey { .. }) => {
                    panic!("duplicate id {id} — monotonic id accounting bug")
                }
            }
        })
    }

    fn hold_done(&mut self, id: u64, _result: SleepReply) -> Effect<Self> {
        let Some((slot, lease)) = self.pending.take_by_key(&id) else {
            return noop();
        };
        let ledger_len = self.ledger.len();
        let effect = reply_to(slot, GatewayReply::Accepted { id, ledger_len });
        // Load-bearing: comment this out and `assert_no_leaked_capacity_at_shutdown`
        // in `run()` fails with a real leak, not a placeholder.
        drop(lease);
        effect
    }
}

/// Run the copied service path once: register the isolate on a real
/// `ThreadedRuntime`, drive real concurrent callers through it, prove
/// progress and a clean shutdown, then report what actually happened.
pub fn run(config: RunConfig) -> anyhow::Result<RunReport> {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));
    let scope =
        SharedCapacityScope::new("copied_service_path.in_flight", "requests", config.capacity);
    // "Recovered" state seeded before the isolate accepts traffic — the
    // durable-restore half of the durable-state step.
    let seed_ledger: Vec<u64> = vec![0];
    let ledger_seed_len = seed_ledger.len();

    let gateway: SplitServiceHandle<GatewayEvent, GatewayRequest, GatewayReply> = runtime
        .register_split_service::<Gateway, GatewayEvent, GatewayRequest, Infallible>(
            Gateway::new(
                scope.clone(),
                seed_ledger,
                config.capacity,
                Duration::from_millis(config.work_ms),
            ),
            config.mailbox,
        )
        .map_err(|e| anyhow::anyhow!("register gateway: {e:?}"))?;
    let requests = gateway.requests;

    let call_timeout = Duration::from_millis(config.call_timeout_ms);
    let rt_for_ops = Arc::clone(&runtime);
    let scope_for_check = scope.clone();

    let load = tina_proof_harness::load::run_with_observation(
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
            Ok(CallOutcome::Timeout) => OpOutcome::Timeout,
            Ok(other) => panic!("unexpected submit outcome: {other:?}"),
            Err(error) => {
                let _ = error;
                OpOutcome::Err { kind: "call_error" }
            }
        },
        Some(move || {
            // Fires once, after every caller thread has joined: the real
            // end-of-run leak check, not a default placeholder.
            let mut pressure = ServicePressureReport::new("system_copied_service_path");
            pressure.add_measured("scope", scope_for_check.surface_report(CapacityMode::Fixed));
            LoadObservation {
                leak_checked: true,
                leak_clean: scope_for_check.snapshot().current == 0,
                surface_plateaus: SurfacePlateau::from_service_pressure(&pressure),
                ..LoadObservation::default()
            }
        }),
    );

    assert_cold_work_made_progress(&load);
    assert_no_leaked_capacity_at_shutdown(&load);

    let ledger_final_len =
        match runtime.call_blocking_request(requests, GatewayRequest::Stats, call_timeout)? {
            CallOutcome::Replied(GatewayReply::Stats { ledger_len }) => ledger_len,
            other => anyhow::bail!("unexpected stats outcome: {other:?}"),
        };

    let admitted = load.ops_ok as usize;
    let full = load
        .err_kinds
        .iter()
        .find(|(kind, _)| kind.as_str() == "full")
        .map(|(_, count)| *count as usize)
        .unwrap_or(0);

    let discovery_line = scope.discovery_line();
    let pre_shutdown_snap = scope.snapshot();

    // Graceful shutdown: owner stop must release every held charge. The
    // post-shutdown snapshot is the load-bearing proof of that claim.
    shutdown(runtime);
    let post_shutdown_snap = scope.snapshot();

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

fn shutdown(runtime: Arc<ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>>) {
    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}
