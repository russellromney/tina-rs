//! Soak-shaped specimen that prints CI-friendly discovery lines.
//!
//! The system pretends to be an HTTP+DB service: each request charges
//! the `soak.http.in_flight` shared scope, sleeps "fake_http", admits
//! against `soak.db.in_flight`, sleeps "fake_db", then replies. Slow
//! end-to-end requests push a `SlowEvent` into a bounded event sink
//! with a drop policy.
//!
//! The point is observability output, not the network stack. After N
//! requests the specimen emits:
//!
//! - One `capacity surface=…` line per scope (CI-greppable).
//! - One `events sink=…` line per event sink.
//! - One `service=…` summary line aggregating measured / unavailable
//!   surfaces.
//!
//! Every line is the same `key=value` shape the rest of Tina uses, so
//! the same grep + parser works.

use std::convert::Infallible;
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tina::capacity::CapacityMode;
use tina::prelude::*;
use tina_runtime::{
    BoundedEventSink, CallOutcome, CapacitySummary, DefaultThreadedMailboxFactory, DropPolicy,
    GuardedPendingReplies, ServicePressureReport, ServicePressureSurface, SharedCapacityScope,
    SharedLease, SharedScopeFull, SleepReply, SplitServiceHandle, ThreadedRuntime,
    format_assertion_failure, sleep,
};

#[derive(Debug, Clone, Copy)]
pub struct RunConfig {
    pub workers: usize,
    pub requests_per_worker: usize,
    pub http_in_flight_cap: usize,
    pub db_in_flight_cap: usize,
    pub fake_http_ms: u64,
    pub fake_db_ms: u64,
    pub slow_threshold_ms: u64,
    pub event_sink_cap: usize,
    pub gateway_mailbox: usize,
    pub pending_capacity: usize,
    pub call_timeout_ms: u64,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            workers: 8,
            requests_per_worker: 16,
            http_in_flight_cap: 4,
            db_in_flight_cap: 2,
            fake_http_ms: 5,
            fake_db_ms: 8,
            slow_threshold_ms: 12,
            event_sink_cap: 8,
            gateway_mailbox: 64,
            pending_capacity: 64,
            call_timeout_ms: 5_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunReport {
    pub total_requests: usize,
    pub ok: usize,
    pub http_full: usize,
    pub db_full: usize,
    pub pending_full: usize,
    pub slow_events_accepted: u64,
    pub slow_events_dropped: u64,
    pub discovery_lines: Vec<String>,
    pub service_summary_line: String,
    pub copyable_assertion_failures: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlowEvent {
    pub worker_id: usize,
    pub request_id: usize,
    pub took_ms: u64,
}

/// Caller-authority request: the only thing an outside caller can ask.
#[derive(Debug)]
pub enum SoakRequest {
    Request { worker_id: usize, request_id: usize },
}

/// Internal event: flow continuation, never caller authority.
pub enum SoakEvent {
    Flow(SoakFlow),
}

/// Split-service envelope for [`Soak`].
pub type SoakMsg = tina::ServiceMessage<SoakEvent, SoakRequest>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SoakReply {
    Ok,
    HttpFull {
        current: usize,
        max: usize,
    },
    DbFull {
        current: usize,
        max: usize,
    },
    /// `PendingReplies` rejected the slot; the request never reached
    /// the DB stage. Distinct from `HttpFull` so a debug operator
    /// reading replies can find the right surface.
    PendingFull {
        current: usize,
        max: usize,
    },
    /// `PendingReplies` rejected the slot for a duplicate qid. Should
    /// not happen with monotone qids; reserved so the variant cannot
    /// be silently swallowed.
    PendingDuplicate,
}

struct Soak {
    http_scope: SharedCapacityScope,
    db_scope: SharedCapacityScope,
    events: BoundedEventSink<SlowEvent>,
    pending: GuardedPendingReplies<u64, SoakReply, SharedLease>,
    fake_http: Duration,
    fake_db: Duration,
    slow_threshold: Duration,
    started_at: Instant,
    next_qid: u64,
}

// Both steps are timer wake-ups, not runtime calls: the caller's authority
// stays parked in `self.pending` keyed by `qid` (see `dispatch`/
// `http_released`), swapped from the http lease to the db lease between
// steps. Neither step carries a `RequestContext` in the message, so each
// uses `-> raw SleepReply` instead of the call-shaped `-> T` arrow: the
// field is `SleepReply` verbatim (no `CallOutcome` wrap), and the body has
// no `req` to thread. Continuations land as domain events via
// `then_service_event` so the split-service form never names the envelope.
tina::flow! {
    pub flow SoakFlow for Soak {
        reply SoakReply;

        step HttpReleased(qid: u64, worker_id: usize, request_id: usize, started_ms: u64) -> raw SleepReply {
            // The timer payload carries no information (sleep cannot fail
            // short of runtime shutdown); the pre-flow code ignored it too.
            let _ = outcome;
            self.http_released(qid, worker_id, request_id, started_ms)
        }

        step DbReleased(qid: u64, worker_id: usize, request_id: usize, started_ms: u64) -> raw SleepReply {
            let _ = outcome;
            self.db_released(qid, worker_id, request_id, started_ms)
        }
    }
}

#[tina_runtime::isolate(event = SoakEvent, request = SoakRequest, reply = SoakReply)]
impl Soak {
    fn handle_event(
        &mut self,
        event: SoakEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            SoakEvent::Flow(flow) => self.handle_soak_flow(flow),
        }
    }

    fn handle_request(
        &mut self,
        request: SoakRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            SoakRequest::Request {
                worker_id,
                request_id,
            } => self.dispatch(worker_id, request_id, call),
        }
    }
}

impl Soak {
    fn new(config: &RunConfig) -> Self {
        Self {
            http_scope: SharedCapacityScope::new(
                "soak.http.in_flight",
                "requests",
                config.http_in_flight_cap,
            ),
            db_scope: SharedCapacityScope::new(
                "soak.db.in_flight",
                "queries",
                config.db_in_flight_cap,
            ),
            events: BoundedEventSink::new(
                "soak.slow_requests",
                config.event_sink_cap,
                DropPolicy::DropOldest,
            ),
            pending: GuardedPendingReplies::with_capacity(config.pending_capacity)
                .named("system_soak_http_db.pending"),
            fake_http: Duration::from_millis(config.fake_http_ms),
            fake_db: Duration::from_millis(config.fake_db_ms),
            slow_threshold: Duration::from_millis(config.slow_threshold_ms),
            started_at: Instant::now(),
            next_qid: 1,
        }
    }

    fn now_ms(&self) -> u64 {
        self.started_at.elapsed().as_millis() as u64
    }

    fn dispatch(
        &mut self,
        worker_id: usize,
        request_id: usize,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        let http_lease = match self.http_scope.try_admit(1) {
            Ok(lease) => lease,
            Err(SharedScopeFull { current, max, .. }) => {
                return call.reply(SoakReply::HttpFull { current, max });
            }
        };
        let qid = self.next_qid;
        call.capture(|request| {
            match self
                .pending
                .insert_deferred_guarded(qid, request.into_deferred(), http_lease)
            {
                Ok(_ticket) => {
                    self.next_qid = qid + 1;
                    let started_ms = self.now_ms();
                    let fake = self.fake_http;
                    sleep(fake).then_service_event(move |result| {
                        SoakEvent::Flow(SoakFlow::HttpReleased(
                            qid, worker_id, request_id, started_ms, result,
                        ))
                    })
                }
                Err(tina_runtime::GuardedInsertError::Full { reply, .. }) => {
                    let cap = self.pending.capacity();
                    reply_to::<Self>(
                        reply,
                        SoakReply::PendingFull {
                            current: cap,
                            max: cap,
                        },
                    )
                }
                Err(tina_runtime::GuardedInsertError::DuplicateKey { reply, .. }) => {
                    reply_to::<Self>(reply, SoakReply::PendingDuplicate)
                }
            }
        })
    }

    fn http_released(
        &mut self,
        qid: u64,
        worker_id: usize,
        request_id: usize,
        started_ms: u64,
    ) -> Effect<Self> {
        // Take the slot+http_lease out so we can swap the guard.
        let Some((slot, http_lease)) = self.pending.take_by_key(&qid) else {
            return noop();
        };
        let db_lease = match self.db_scope.try_admit(1) {
            Ok(lease) => lease,
            Err(SharedScopeFull { current, max, .. }) => {
                drop(http_lease);
                return reply_to(slot, SoakReply::DbFull { current, max });
            }
        };
        drop(http_lease);
        // Re-admit the same caller under the same key, this time with
        // the DB lease as the guard. Admission cannot fail: we just
        // freed slot qid.
        self.pending
            .insert_deferred_guarded(qid, slot, db_lease)
            .map_err(|_| ())
            .expect("re-admission after take cannot fail");
        let fake = self.fake_db;
        sleep(fake).then_service_event(move |result| {
            SoakEvent::Flow(SoakFlow::DbReleased(
                qid, worker_id, request_id, started_ms, result,
            ))
        })
    }

    fn db_released(
        &mut self,
        qid: u64,
        worker_id: usize,
        request_id: usize,
        started_ms: u64,
    ) -> Effect<Self> {
        let took = self.now_ms().saturating_sub(started_ms);
        if Duration::from_millis(took) >= self.slow_threshold {
            self.events.push(SlowEvent {
                worker_id,
                request_id,
                took_ms: took,
            });
        }
        let Some((slot, db_lease)) = self.pending.take_by_key(&qid) else {
            return noop();
        };
        let effect = reply_to(slot, SoakReply::Ok);
        drop(db_lease);
        effect
    }
}

pub fn run(config: RunConfig) -> anyhow::Result<RunReport> {
    let runtime = Arc::new(ThreadedRuntime::try_new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    )?);
    let soak = Soak::new(&config);
    let http_scope = soak.http_scope.clone();
    let db_scope = soak.db_scope.clone();
    let events = soak.events.clone();
    let svc: SplitServiceHandle<SoakEvent, SoakRequest, SoakReply> = runtime
        .register_split_service::<Soak, SoakEvent, SoakRequest, Infallible>(
            soak,
            config.gateway_mailbox,
        )
        .map_err(|e| anyhow::anyhow!("register soak gateway: {e:?}"))?;

    let timeout = Duration::from_millis(config.call_timeout_ms);
    let total = config.workers * config.requests_per_worker;
    let outcomes = Arc::new(Mutex::new(Vec::with_capacity(total)));
    let barrier = Arc::new(Barrier::new(config.workers + 1));
    let mut threads = Vec::with_capacity(config.workers);

    for worker_id in 0..config.workers {
        let rt = Arc::clone(&runtime);
        let gate = Arc::clone(&barrier);
        let out = Arc::clone(&outcomes);
        let addr = svc.requests.address();
        let per = config.requests_per_worker;
        threads.push(thread::spawn(move || {
            gate.wait();
            for request_id in 0..per {
                let r = rt.call_blocking_typed(
                    addr,
                    SoakMsg::Request(SoakRequest::Request {
                        worker_id,
                        request_id,
                    }),
                    timeout,
                );
                out.lock().expect("outcomes").push(r);
            }
        }));
    }
    barrier.wait();
    for t in threads {
        t.join().expect("worker thread panicked");
    }

    let mut ok = 0usize;
    let mut http_full = 0usize;
    let mut db_full = 0usize;
    let mut pending_full = 0usize;
    for outcome in outcomes.lock().expect("outcomes").iter() {
        match outcome {
            Ok(CallOutcome::Replied(SoakReply::Ok)) => ok += 1,
            Ok(CallOutcome::Replied(SoakReply::HttpFull { .. })) => http_full += 1,
            Ok(CallOutcome::Replied(SoakReply::DbFull { .. })) => db_full += 1,
            Ok(CallOutcome::Replied(SoakReply::PendingFull { .. })) => pending_full += 1,
            Ok(CallOutcome::Replied(SoakReply::PendingDuplicate)) => {
                anyhow::bail!("pending duplicate qid — qid generator is broken");
            }
            other => anyhow::bail!("unexpected outcome: {other:?}"),
        }
    }

    // Aggregate everything into a ServicePressureReport .
    let mut summary = ServicePressureReport::new("soak_http_db");
    summary.add_surface(ServicePressureSurface::measured(
        "soak.http.in_flight",
        "scope",
        http_scope.surface_report(CapacityMode::Fixed),
    ));
    summary.add_surface(ServicePressureSurface::measured(
        "soak.db.in_flight",
        "scope",
        db_scope.surface_report(CapacityMode::Fixed),
    ));
    summary.add_surface(ServicePressureSurface::measured(
        "soak.slow_requests",
        "events",
        events.surface_report(CapacityMode::Fixed),
    ));
    summary.add_unavailable(
        "soak.outbound.pool",
        "pool_waiters",
        "no outbound pool installed in this soak",
    );

    let mut discovery_lines: Vec<String> = Vec::new();
    discovery_lines.push(http_scope.discovery_line());
    discovery_lines.push(db_scope.discovery_line());
    discovery_lines.push(events.discovery_line());
    for surface in &summary.surfaces {
        discovery_lines.push(surface.discovery_line());
    }

    let mut copyable_assertion_failures = Vec::new();
    let capacity_summary: CapacitySummary = summary
        .capacity_summary()
        .map_err(|e| anyhow::anyhow!("capacity summary: {e:?}"))?;
    if let Err(errors) = capacity_summary.assert_no_full() {
        for err in &errors {
            copyable_assertion_failures.push(format_assertion_failure(err));
        }
    }

    let event_snap = events.snapshot();
    let service_summary_line = summary.summary_line();

    shutdown(runtime);

    let report = RunReport {
        total_requests: total,
        ok,
        http_full,
        db_full,
        pending_full,
        slow_events_accepted: event_snap.accepted,
        slow_events_dropped: event_snap.dropped,
        discovery_lines,
        service_summary_line,
        copyable_assertion_failures,
    };

    // Echo lines to stdout so CI consumers see them.
    for line in &report.discovery_lines {
        println!("{line}");
    }
    println!("{}", report.service_summary_line);

    Ok(report)
}

fn shutdown(runtime: Arc<ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>>) {
    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}
