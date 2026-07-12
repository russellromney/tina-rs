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
    LocalSystem, LocalSystemConfig, ServicePressureReport, ServicePressureSurface,
    SharedCapacityScope, SharedLease, SharedScopeFull, SleepReply, SplitServiceHandle,
    ThreadedRuntimeError, format_assertion_failure, sleep,
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
    pub timer_capacity: usize,
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
            timer_capacity: LocalSystemConfig::default().timer_capacity,
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
    pub timer_failed: usize,
    pub call_full: usize,
    pub call_closed: usize,
    pub call_timeout: usize,
    pub call_rejected: usize,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SoakReply {
    Ok,
    HttpFull { current: usize, max: usize },
    DbFull { current: usize, max: usize },
    TimerFailed(tina_runtime::CallError),
}

struct Soak {
    http_scope: SharedCapacityScope,
    db_scope: SharedCapacityScope,
    events: BoundedEventSink<SlowEvent>,
    fake_http: Duration,
    fake_db: Duration,
    slow_threshold: Duration,
    started_at: Instant,
}

tina::flow! {
    pub flow SoakFlow for Soak {
        reply SoakReply;

        step HttpReleased(http_lease: SharedLease, worker_id: usize, request_id: usize, started_ms: u64) -> raw request SleepReply {
            self.http_released(req, http_lease, worker_id, request_id, started_ms, outcome)
        }

        step DbReleased(db_lease: SharedLease, worker_id: usize, request_id: usize, started_ms: u64) -> raw request SleepReply {
            self.db_released(req, db_lease, worker_id, request_id, started_ms, outcome)
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
            fake_http: Duration::from_millis(config.fake_http_ms),
            fake_db: Duration::from_millis(config.fake_db_ms),
            slow_threshold: Duration::from_millis(config.slow_threshold_ms),
            started_at: Instant::now(),
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
        call.capture(|request| {
            if !request.is_open() {
                drop(http_lease);
                return noop();
            }
            let started_ms = self.now_ms();
            let fake = self.fake_http;
            sleep(fake).then_service_event_with_request(request, move |request, outcome| {
                SoakEvent::Flow(SoakFlow::HttpReleased(
                    request, http_lease, worker_id, request_id, started_ms, outcome,
                ))
            })
        })
    }

    fn http_released(
        &mut self,
        request: RequestContext<SoakReply>,
        http_lease: SharedLease,
        worker_id: usize,
        request_id: usize,
        started_ms: u64,
        outcome: SleepReply,
    ) -> Effect<Self> {
        if let Err(error) = outcome {
            drop(http_lease);
            return reply_to(request, SoakReply::TimerFailed(error));
        }
        if !request.is_open() {
            drop(http_lease);
            return noop();
        }
        let db_lease = match self.db_scope.try_admit(1) {
            Ok(lease) => lease,
            Err(SharedScopeFull { current, max, .. }) => {
                drop(http_lease);
                return reply_to(request, SoakReply::DbFull { current, max });
            }
        };
        drop(http_lease);
        let fake = self.fake_db;
        sleep(fake).then_service_event_with_request(request, move |request, outcome| {
            SoakEvent::Flow(SoakFlow::DbReleased(
                request, db_lease, worker_id, request_id, started_ms, outcome,
            ))
        })
    }

    fn db_released(
        &mut self,
        request: RequestContext<SoakReply>,
        db_lease: SharedLease,
        worker_id: usize,
        request_id: usize,
        started_ms: u64,
        outcome: SleepReply,
    ) -> Effect<Self> {
        drop(db_lease);
        if !request.is_open() {
            return noop();
        }
        if let Err(error) = outcome {
            return reply_to(request, SoakReply::TimerFailed(error));
        }
        let took = self.now_ms().saturating_sub(started_ms);
        if Duration::from_millis(took) >= self.slow_threshold {
            self.events.push(SlowEvent {
                worker_id,
                request_id,
                took_ms: took,
            });
        }
        reply_to(request, SoakReply::Ok)
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct OutcomeCounts {
    ok: usize,
    http_full: usize,
    db_full: usize,
    timer_failed: usize,
    call_full: usize,
    call_closed: usize,
    call_timeout: usize,
    call_rejected: usize,
}

fn classify_outcomes(
    outcomes: &[Result<CallOutcome<SoakReply>, ThreadedRuntimeError>],
) -> anyhow::Result<OutcomeCounts> {
    let mut counts = OutcomeCounts::default();
    for outcome in outcomes {
        match outcome {
            Ok(CallOutcome::Replied(SoakReply::Ok)) => counts.ok += 1,
            Ok(CallOutcome::Replied(SoakReply::HttpFull { .. })) => counts.http_full += 1,
            Ok(CallOutcome::Replied(SoakReply::DbFull { .. })) => counts.db_full += 1,
            Ok(CallOutcome::Replied(SoakReply::TimerFailed(_))) => counts.timer_failed += 1,
            Ok(CallOutcome::Full) => counts.call_full += 1,
            Ok(CallOutcome::Closed) => counts.call_closed += 1,
            Ok(CallOutcome::Timeout) => counts.call_timeout += 1,
            Ok(CallOutcome::Rejected(_)) => counts.call_rejected += 1,
            Err(error) => anyhow::bail!("host call failed: {error}"),
        }
    }
    Ok(counts)
}

type SoakSystem = LocalSystem<SingleShard, DefaultThreadedMailboxFactory>;

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
    let soak = Soak::new(&config);
    let http_scope = soak.http_scope.clone();
    let db_scope = soak.db_scope.clone();
    let events = soak.events.clone();
    let svc_result: Result<SplitServiceHandle<SoakEvent, SoakRequest, SoakReply>, _> = runtime
        .register_split_service::<Soak, SoakEvent, SoakRequest, Infallible>(
            soak,
            config.gateway_mailbox,
        );
    let svc = match svc_result {
        Ok(service) => service,
        Err(error) => {
            let shutdown_result = shutdown_runtime(shutdown, runtime);
            return match shutdown_result {
                Ok(()) => Err(anyhow::anyhow!("register soak gateway: {error:?}")),
                Err(shutdown_error) => Err(anyhow::anyhow!(
                    "register soak gateway: {error:?}; shutdown also failed: {shutdown_error}"
                )),
            };
        }
    };

    let timeout = Duration::from_millis(config.call_timeout_ms);
    let total = config.workers * config.requests_per_worker;
    let outcomes = Arc::new(Mutex::new(Vec::with_capacity(total)));
    let barrier = Arc::new(Barrier::new(config.workers + 1));
    let mut threads = Vec::with_capacity(config.workers);

    for worker_id in 0..config.workers {
        let rt = Arc::clone(&runtime);
        let gate = Arc::clone(&barrier);
        let out = Arc::clone(&outcomes);
        let requests = svc.requests;
        let per = config.requests_per_worker;
        threads.push(thread::spawn(move || {
            gate.wait();
            for request_id in 0..per {
                let r = rt.call_blocking_request(
                    requests,
                    SoakRequest::Request {
                        worker_id,
                        request_id,
                    },
                    timeout,
                );
                out.lock().expect("outcomes").push(r);
            }
        }));
    }
    barrier.wait();
    let mut worker_panicked = false;
    for worker in threads {
        worker_panicked |= worker.join().is_err();
    }

    let report_result = (|| -> anyhow::Result<RunReport> {
        if worker_panicked {
            anyhow::bail!("one or more soak worker threads panicked");
        }
        let outcomes = outcomes.lock().expect("outcomes");
        let counts = classify_outcomes(&outcomes)?;

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

        Ok(RunReport {
            total_requests: total,
            ok: counts.ok,
            http_full: counts.http_full,
            db_full: counts.db_full,
            timer_failed: counts.timer_failed,
            call_full: counts.call_full,
            call_closed: counts.call_closed,
            call_timeout: counts.call_timeout,
            call_rejected: counts.call_rejected,
            slow_events_accepted: event_snap.accepted,
            slow_events_dropped: event_snap.dropped,
            discovery_lines,
            service_summary_line,
            copyable_assertion_failures,
        })
    })();

    let shutdown_result = shutdown_runtime(shutdown, runtime);
    let http_after_shutdown = http_scope.snapshot();
    let db_after_shutdown = db_scope.snapshot();
    let settlement_result = if http_after_shutdown.current != 0 || db_after_shutdown.current != 0 {
        Err(anyhow::anyhow!(
            "shutdown leaked scope authority: http={} db={}",
            http_after_shutdown.current,
            db_after_shutdown.current
        ))
    } else {
        Ok(())
    };

    shutdown_result?;
    settlement_result?;
    let report = report_result?;

    // Echo lines to stdout so CI consumers see them.
    for line in &report.discovery_lines {
        println!("{line}");
    }
    println!("{}", report.service_summary_line);

    Ok(report)
}

fn shutdown_runtime(
    shutdown: tina_runtime::ThreadedShutdownHandle,
    runtime: Arc<SoakSystem>,
) -> anyhow::Result<()> {
    let terminal = shutdown.request_and_wait_report(Duration::from_secs(5))?;
    drop(runtime);
    terminal.ensure_clean()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_accounting_keeps_every_terminal_variant_distinct() {
        let outcomes = vec![
            Ok(CallOutcome::Replied(SoakReply::Ok)),
            Ok(CallOutcome::Replied(SoakReply::HttpFull {
                current: 1,
                max: 1,
            })),
            Ok(CallOutcome::Replied(SoakReply::DbFull {
                current: 1,
                max: 1,
            })),
            Ok(CallOutcome::Replied(SoakReply::TimerFailed(
                tina_runtime::CallError::TimerFull,
            ))),
            Ok(CallOutcome::Full),
            Ok(CallOutcome::Closed),
            Ok(CallOutcome::Timeout),
            Ok(CallOutcome::Rejected(
                tina::CallRejectedReason::UnsupportedMessage,
            )),
            Ok(CallOutcome::Rejected(
                tina::CallRejectedReason::ForeignSystem {
                    expected: tina::SystemIncarnation::new(1),
                    actual: tina::SystemIncarnation::new(2),
                },
            )),
        ];

        assert_eq!(
            classify_outcomes(&outcomes).unwrap(),
            OutcomeCounts {
                ok: 1,
                http_full: 1,
                db_full: 1,
                timer_failed: 1,
                call_full: 1,
                call_closed: 1,
                call_timeout: 1,
                call_rejected: 2,
            }
        );
    }

    #[test]
    fn outer_host_errors_remain_errors() {
        let errors = [
            ThreadedRuntimeError::ForeignSystem {
                expected: tina::SystemIncarnation::new(1),
                actual: tina::SystemIncarnation::new(2),
            },
            ThreadedRuntimeError::WorkerStopped,
            ThreadedRuntimeError::UnknownShard(ShardId::new(99)),
            ThreadedRuntimeError::DriverShutdownFailed,
            ThreadedRuntimeError::DriverParkFailed,
            ThreadedRuntimeError::CommandFull,
            ThreadedRuntimeError::HostWaitTimeout,
            ThreadedRuntimeError::WorkerUnresponsive,
        ];
        for error in errors {
            let message = classify_outcomes(&[Err(error)]).unwrap_err().to_string();
            assert!(message.starts_with("host call failed:"), "{message}");
            assert!(message.contains(&error.to_string()), "{message}");
        }
    }
}
