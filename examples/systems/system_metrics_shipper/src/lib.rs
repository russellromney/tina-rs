//! Metrics shipper system specimen.
//!
//! One isolate accepts metric events, batches them by size-or-time, and
//! single-flights each batch through a downstream sink (the HTTP/DB
//! stand-in). Every cap is reportable: ingress mailbox, in-memory buffer,
//! sink mailbox, batch size, batch window. Shutdown drains pending events
//! through one more flush before replying.
//!
//! The specimen is deliberately small. The rough bits live in the tick
//! token and the drain handshake, not in helper modules.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::{Barrier, Mutex};
use std::thread;
use std::time::Duration;

use tina::time::{RecurringCatchUp, RecurringTick, RecurringTickDecision, RecurringTickToken};
use tina::{RequestContext, prelude::*, reply_to};
use tina_runtime::lifecycle::{
    CloseAdmission, Health, Lifecycle, ResourceCloseReport, ResourceKind, ServiceShutdownReport,
    ServiceTopology, ShutdownChoreography, ShutdownStep, StepOutcome, TopologyComponent,
};
use tina_runtime::{
    AdmitDecision, CallOutcome, DefaultThreadedMailboxFactory, DrainStage, DrainState,
    LocalPermitGate, LocalPermitName, Permit, ThreadedRuntime, ThreadedShutdownHandle,
    call_request, sleep,
};

/// One submitted metric event. Payload is opaque; the specimen only cares
/// about counts and the order in which they are delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub key: String,
    pub value: i64,
}

#[derive(Debug, Clone)]
pub struct RunConfig {
    pub events: usize,
    pub callers: usize,
    pub buffer_capacity: usize,
    pub batch_size: usize,
    pub batch_window_ms: u64,
    pub shipper_mailbox: usize,
    pub sink_mailbox: usize,
    pub call_timeout_ms: u64,
    pub flush_timeout_ms: u64,
    pub stop_timeout_ms: u64,
    pub sink_fail_every: usize,
    pub sink_flush_delay_ms: u64,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            events: 64,
            callers: 8,
            buffer_capacity: 16,
            batch_size: 8,
            batch_window_ms: 40,
            shipper_mailbox: 4,
            sink_mailbox: 4,
            call_timeout_ms: 2_000,
            flush_timeout_ms: 1_000,
            stop_timeout_ms: 2_000,
            sink_fail_every: 0,
            sink_flush_delay_ms: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunReport {
    pub steady: SteadyReport,
    pub overload: OverloadReport,
    pub shutdown: ShutdownReport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteadyReport {
    pub submitted: usize,
    pub accepted: usize,
    pub dropped_full: usize,
    pub stopping_rejects: usize,
    pub shipper_mailbox_full: usize,
    pub stats: ShipperStats,
    pub sink: SinkStats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverloadReport {
    pub submitted: usize,
    pub accepted: usize,
    pub dropped_full: usize,
    pub shipper_mailbox_full: usize,
    pub stats: ShipperStats,
    pub sink: SinkStats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownReport {
    pub stop_clean: bool,
    pub flushed_on_drain: usize,
    pub drained_batches: usize,
    pub stats: ShipperStats,
    pub sink: SinkStats,
    /// Typed shipper-side lifecycle and pressure snapshot taken just
    /// before the host drove runtime shutdown. State is
    /// [`Lifecycle::Stopped`] because `ShipperReply::Stopped` arrived
    /// before this snapshot.
    pub health: Health,
    /// Typed startup topology: shipper isolate, sink isolate, buffer +
    /// flush gate + tick capacity surfaces. Built by the host so the
    /// non-HTTP service still has a "what is running" answer in the
    /// same vocabulary as `mini_saas_api`.
    pub topology: ServiceTopology,
    /// Typed host shutdown choreography: `DrainInFlight` (the shipper's
    /// own Stop handshake), `CloseResource sink.isolate`, and
    /// `StopOwner` (runtime shutdown). Proof that the lifecycle helper
    /// is not HTTP-shaped: nothing here touches `tina-http`.
    pub shutdown_choreography: ServiceShutdownReport,
    /// Lifecycle states the host observed the shipper passing through,
    /// in order. Canonical sequence is
    /// `[Starting, Ready, Draining, Stopped]`. Built explicitly so the
    /// non-HTTP service reports the same typed transition as
    /// `mini_saas_api`.
    pub lifecycle_transitions: Vec<Lifecycle>,
}

/// Snapshot of every shipper-side cap and pressure counter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ShipperStats {
    pub buffer_capacity: usize,
    pub buffer_high_water: usize,
    pub buffer_full_rejects: u64,
    pub stopping_rejects: u64,
    pub batches_flushed_by_size: u64,
    pub batches_flushed_by_time: u64,
    pub batches_flushed_on_drain: u64,
    pub flush_failures: u64,
    pub events_lost_on_flush: u64,
    pub ticks_armed: u64,
    pub ticks_fired_useful: u64,
    pub ticks_fired_stale: u64,
    pub ticks_fired_idle: u64,
}

/// Snapshot of every sink-side cap and pressure counter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SinkStats {
    pub mailbox_capacity: usize,
    pub batches_received: u64,
    pub events_received: u64,
    pub failures_injected: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlushKind {
    Size,
    Time,
    Drain,
}

/// Fire-and-forget facts the shipper accepts: timer ticks and flush
/// completions. Neither carries caller authority.
#[derive(Debug)]
pub enum ShipperEvent {
    Tick {
        token: RecurringTickToken,
    },
    FlushDone {
        kind: FlushKind,
        count: usize,
        permit: Permit,
        outcome: CallOutcome<SinkReply>,
    },
}

/// Caller-authority requests the host can ask the shipper.
#[derive(Debug)]
pub enum ShipperRequest {
    Submit { event: Event },
    Stats,
    Stop,
}

/// Split-service envelope for [`Shipper`].
pub type ShipperMsg = tina::ServiceMessage<ShipperEvent, ShipperRequest>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShipperReply {
    Accepted,
    Dropped,
    Stopping,
    Stats(ShipperStats),
    Stopped {
        flushed_on_drain: usize,
        drained_batches: usize,
    },
}

/// Fire-and-forget fact the sink accepts: its own deferred-flush
/// completion. Never sent by a caller.
#[derive(Debug)]
pub enum SinkEvent {
    Complete {
        req: RequestContext<SinkReply>,
        batch: Vec<Event>,
    },
}

/// The two caller-authority requests the sink accepts.
#[derive(Debug)]
pub enum SinkRequest {
    Flush { batch: Vec<Event> },
    Stats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SinkReply {
    Ack,
    Failed,
    Stats(SinkStats),
}

type ShipperAddr = Address<ShipperMsg, ShipperReply>;
type SinkAddr = tina::ServiceRequestAddress<SinkEvent, SinkRequest, SinkReply>;

struct Shipper {
    sink: SinkAddr,
    buffer_capacity: usize,
    batch_size: usize,
    flush_timeout: Duration,
    buffer: VecDeque<Event>,
    /// One in-flight flush at a time. The permit's name makes the
    /// "max-1 outstanding flush" rule structural rather than a bool.
    flush_gate: LocalPermitGate,
    /// Time-based flush schedule. Stale-tick detection lives in the token,
    /// so a size-triggered flush that pre-empts a pending tick is visibly
    /// rejected when the tick continuation lands.
    flush_tick: RecurringTick,
    /// Drain bookkeeping: who is waiting on the Stop reply, and the typed
    /// admission/completion counters used by Tina's capacity-aware registration helper.
    drain: DrainState,
    pending_stop: Option<RequestContext<ShipperReply>>,
    drained_events: usize,
    drained_batches: usize,
    stats: ShipperStats,
}

#[tina_runtime::isolate(event = ShipperEvent, request = ShipperRequest, reply = ShipperReply)]
impl Shipper {
    fn handle_event(
        &mut self,
        event: ShipperEvent,
        ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        let now = ctx.now();
        match event {
            ShipperEvent::Tick { token } => self.on_tick(token),
            ShipperEvent::FlushDone {
                kind,
                count,
                permit,
                outcome,
            } => self.on_flush_done(kind, count, permit, outcome, now),
        }
    }

    fn handle_request(
        &mut self,
        request: ShipperRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            ShipperRequest::Submit { event } => self.on_submit(event, call),
            ShipperRequest::Stats => call.reply(ShipperReply::Stats(self.snapshot())),
            ShipperRequest::Stop => self.on_stop(call),
        }
    }
}

impl Shipper {
    fn new(sink: SinkAddr, config: &RunConfig) -> Self {
        Self {
            sink,
            buffer_capacity: config.buffer_capacity,
            batch_size: config.batch_size,
            flush_timeout: Duration::from_millis(config.flush_timeout_ms),
            buffer: VecDeque::with_capacity(config.buffer_capacity),
            flush_gate: LocalPermitGate::with_capacity(1).named(LocalPermitName("flush")),
            flush_tick: RecurringTick::every(Duration::from_millis(config.batch_window_ms))
                .expect("batch_window_ms > 0")
                .catch_up(RecurringCatchUp::Skip),
            drain: DrainState::new(),
            pending_stop: None,
            drained_events: 0,
            drained_batches: 0,
            stats: ShipperStats {
                buffer_capacity: config.buffer_capacity,
                ..ShipperStats::default()
            },
        }
    }

    fn on_submit(&mut self, event: Event, call: RequestCall<'_, Self>) -> RequestEffect<Self> {
        let now = call.now();
        if !self.drain.is_open() {
            let _ = self.drain.admit();
            self.stats.stopping_rejects += 1;
            return call.reply(ShipperReply::Stopping);
        }
        // Capacity decision stays with the service: gate the buffer, not the
        // drain admission. Drain admission is for caller-obligation tracking
        // and is not the same as the in-memory buffer cap.
        if self.buffer.len() >= self.buffer_capacity {
            self.drain.record_full();
            self.stats.buffer_full_rejects += 1;
            return call.reply(ShipperReply::Dropped);
        }
        debug_assert!(matches!(self.drain.admit(), AdmitDecision::Accept));
        self.buffer.push_back(event);
        if self.buffer.len() > self.stats.buffer_high_water {
            self.stats.buffer_high_water = self.buffer.len();
        }

        // Size-based flush wins over the time window. Clearing the recurring
        // tick makes any in-flight Tick continuation visibly stale via
        // `flush_tick.validate()`.
        if self.buffer.len() >= self.batch_size && self.flush_gate.is_idle() {
            return call.reply_and(ShipperReply::Accepted, vec![self.start_flush(FlushKind::Size)]);
        }

        // Time-based flush: arm the recurring tick the first time the buffer
        // goes non-empty and no flush is in flight. After a flush completes
        // FlushDone decides whether to re-arm.
        if self.flush_gate.is_idle()
            && self.flush_tick.next_due().is_none()
            && !self.buffer.is_empty()
        {
            return call.reply_and(ShipperReply::Accepted, vec![self.arm_tick(now)]);
        }
        call.reply(ShipperReply::Accepted)
    }

    fn on_stop(&mut self, call: RequestCall<'_, Self>) -> RequestEffect<Self> {
        if !self.drain.is_open() {
            // Double-stop is a real policy reject, not a wrong-lane message.
            return call.reject(tina::CallRejectedReason::UnsupportedMessage);
        }
        self.drain.begin();
        // Any in-flight tick is now stale; drain handshake takes over.
        self.flush_tick.clear();

        if self.buffer.is_empty() && self.flush_gate.is_idle() {
            self.drain.finish();
            return call.reply(ShipperReply::Stopped {
                flushed_on_drain: self.drained_events,
                drained_batches: self.drained_batches,
            });
        }
        call.capture(|req| {
            self.pending_stop = Some(req);
            if self.flush_gate.is_idle() {
                self.start_flush(FlushKind::Drain)
            } else {
                noop()
            }
        })
    }

    fn on_tick(&mut self, token: RecurringTickToken) -> Effect<Self> {
        if self.flush_tick.validate(token).is_err() {
            self.stats.ticks_fired_stale += 1;
            return noop();
        }
        self.flush_tick.clear();
        if self.buffer.is_empty() || !self.flush_gate.is_idle() {
            self.stats.ticks_fired_idle += 1;
            return noop();
        }
        self.stats.ticks_fired_useful += 1;
        self.start_flush(FlushKind::Time)
    }

    fn on_flush_done(
        &mut self,
        kind: FlushKind,
        count: usize,
        permit: Permit,
        outcome: CallOutcome<SinkReply>,
        now: std::time::Instant,
    ) -> Effect<Self> {
        let _ = self
            .flush_gate
            .release(permit)
            .expect("flush permit released exactly once by FlushDone");
        let succeeded = matches!(outcome, CallOutcome::Replied(SinkReply::Ack));
        if succeeded {
            self.drain.record_complete();
            match kind {
                FlushKind::Size => self.stats.batches_flushed_by_size += 1,
                FlushKind::Time => self.stats.batches_flushed_by_time += 1,
                FlushKind::Drain => {
                    self.stats.batches_flushed_on_drain += 1;
                    self.drained_events += count;
                    self.drained_batches += 1;
                }
            }
        } else {
            self.drain.record_cancelled_or_retired();
            self.stats.flush_failures += 1;
            self.stats.events_lost_on_flush += count as u64;
        }

        let drain_done = !self.drain.is_open() && self.buffer.is_empty();
        if drain_done {
            if let Some(req) = self.pending_stop.take() {
                self.drain.finish();
                let reply = ShipperReply::Stopped {
                    flushed_on_drain: self.drained_events,
                    drained_batches: self.drained_batches,
                };
                return reply_to(req, reply);
            }
            return noop();
        }

        if !self.buffer.is_empty() {
            if !self.drain.is_open() {
                return self.start_flush(FlushKind::Drain);
            }
            if self.buffer.len() >= self.batch_size {
                return self.start_flush(FlushKind::Size);
            }
            // Smaller leftover after a flush: re-arm time window.
            if self.flush_tick.next_due().is_none() {
                return self.arm_tick(now);
            }
        }
        noop()
    }

    fn start_flush(&mut self, kind: FlushKind) -> Effect<Self> {
        let permit = self
            .flush_gate
            .try_admit()
            .expect("start_flush only called when flush_gate is idle");
        let take = self.batch_size.min(self.buffer.len());
        let batch: Vec<Event> = self.buffer.drain(..take).collect();
        let count = batch.len();
        // Any tick currently sleeping cannot satisfy a future flush because
        // `flush_tick.clear()` advances the helper's armed ordinal.
        self.flush_tick.clear();
        call_request::<SinkEvent, SinkRequest, SinkReply>(
            self.sink,
            SinkRequest::Flush { batch },
            self.flush_timeout,
        )
        .then(move |outcome| {
            ShipperMsg::Event(ShipperEvent::FlushDone {
                kind,
                count,
                permit,
                outcome,
            })
        })
    }

    fn arm_tick(&mut self, now: std::time::Instant) -> Effect<Self> {
        let decision = self.flush_tick.next(now);
        self.stats.ticks_armed += 1;
        match decision {
            RecurringTickDecision::Sleep { delay, token, .. } => {
                sleep(delay).then(move |_| ShipperMsg::Event(ShipperEvent::Tick { token }))
            }
            RecurringTickDecision::Skip(report) => {
                // Skip means whole periods were missed since this isolate
                // arm. With the helper's tightened semantics this only fires
                // when `report.missed_ticks > 0`; record the coalesced count
                // and let the caller's next action arm a fresh tick.
                self.stats.ticks_fired_stale = self
                    .stats
                    .ticks_fired_stale
                    .saturating_add(report.missed_ticks);
                noop()
            }
        }
    }

    fn snapshot(&self) -> ShipperStats {
        self.stats
    }
}

struct Sink {
    received: u64,
    events: u64,
    failures: u64,
    fail_every: usize,
    mailbox_capacity: usize,
    flush_delay: Duration,
}

#[tina_runtime::isolate(event = SinkEvent, request = SinkRequest, reply = SinkReply)]
impl Sink {
    fn handle_event(
        &mut self,
        event: SinkEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            SinkEvent::Complete { req, batch } => self.complete(req, batch),
        }
    }

    fn handle_request(
        &mut self,
        request: SinkRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            SinkRequest::Flush { batch } => {
                if self.flush_delay.is_zero() {
                    return self.reply_for_batch(call, batch);
                }
                call.defer(sleep(self.flush_delay)).reply(move |req, _| {
                    tina::ServiceMessage::Event(SinkEvent::Complete { req, batch })
                })
            }
            SinkRequest::Stats => call.reply(SinkReply::Stats(SinkStats {
                mailbox_capacity: self.mailbox_capacity,
                batches_received: self.received,
                events_received: self.events,
                failures_injected: self.failures,
            })),
        }
    }
}

impl Sink {
    fn new(mailbox_capacity: usize, fail_every: usize, flush_delay: Duration) -> Self {
        Self {
            received: 0,
            events: 0,
            failures: 0,
            fail_every,
            mailbox_capacity,
            flush_delay,
        }
    }

    fn reply_for_batch(
        &mut self,
        call: RequestCall<'_, Self>,
        batch: Vec<Event>,
    ) -> RequestEffect<Self> {
        self.received += 1;
        let should_fail = self.fail_every > 0 && self.received as usize % self.fail_every == 0;
        if should_fail {
            self.failures += 1;
            return call.reply(SinkReply::Failed);
        }
        self.events += batch.len() as u64;
        call.reply(SinkReply::Ack)
    }

    fn complete(&mut self, req: RequestContext<SinkReply>, batch: Vec<Event>) -> Effect<Self> {
        self.received += 1;
        let should_fail = self.fail_every > 0 && self.received as usize % self.fail_every == 0;
        if should_fail {
            self.failures += 1;
            return reply_to(req, SinkReply::Failed);
        }
        self.events += batch.len() as u64;
        reply_to(req, SinkReply::Ack)
    }
}

pub fn run(config: RunConfig) -> anyhow::Result<RunReport> {
    let steady_config = RunConfig {
        sink_fail_every: 0,
        ..config.clone()
    };
    let overload_config = RunConfig {
        // Force buffer overflow: parallel callers + slow sink + small
        // buffer. Each in-flight flush blocks for sink_flush_delay_ms so
        // the buffer fills and the next Submit gets a typed Dropped.
        events: config.events * 4,
        callers: 8,
        buffer_capacity: (config.buffer_capacity / 2).max(2),
        batch_size: (config.batch_size / 2).max(1),
        shipper_mailbox: 32,
        sink_mailbox: 4,
        sink_flush_delay_ms: 25,
        sink_fail_every: 0,
        ..config.clone()
    };
    let shutdown_config = RunConfig {
        sink_fail_every: 0,
        ..config.clone()
    };

    Ok(RunReport {
        steady: run_steady(&steady_config)?,
        overload: run_overload(&overload_config)?,
        shutdown: run_shutdown(&shutdown_config)?,
    })
}

pub fn run_steady(config: &RunConfig) -> anyhow::Result<SteadyReport> {
    let world = World::start(config)?;
    let outcomes = world.submit_burst(config.events, config.callers, false)?;
    let mut accepted = 0;
    let mut dropped_full = 0;
    let mut stopping_rejects = 0;
    let mut mailbox_full = 0;
    for outcome in &outcomes {
        match outcome {
            CallOutcome::Replied(ShipperReply::Accepted) => accepted += 1,
            CallOutcome::Replied(ShipperReply::Dropped) => dropped_full += 1,
            CallOutcome::Replied(ShipperReply::Stopping) => stopping_rejects += 1,
            CallOutcome::Full => mailbox_full += 1,
            other => anyhow::bail!("unexpected steady outcome: {other:?}"),
        }
    }
    world.wait_for_flushed_events(accepted, Duration::from_secs(2))?;
    let stats = world.shipper_stats()?;
    let sink = world.sink_stats()?;
    let shutdown_clean = world.stop_and_shutdown()?;
    anyhow::ensure!(shutdown_clean, "steady run shutdown not clean");

    Ok(SteadyReport {
        submitted: outcomes.len(),
        accepted,
        dropped_full,
        stopping_rejects,
        shipper_mailbox_full: mailbox_full,
        stats,
        sink,
    })
}

pub fn run_overload(config: &RunConfig) -> anyhow::Result<OverloadReport> {
    let world = World::start(config)?;
    let outcomes = world.submit_burst(config.events, config.callers, false)?;
    let mut accepted = 0;
    let mut dropped_full = 0;
    let mut mailbox_full = 0;
    for outcome in &outcomes {
        match outcome {
            CallOutcome::Replied(ShipperReply::Accepted) => accepted += 1,
            CallOutcome::Replied(ShipperReply::Dropped) => dropped_full += 1,
            CallOutcome::Replied(ShipperReply::Stopping) => {
                anyhow::bail!("overload submit hit Stopping while ingress should still be open");
            }
            CallOutcome::Full => mailbox_full += 1,
            other => anyhow::bail!("unexpected overload outcome: {other:?}"),
        }
    }
    world.wait_for_flushed_events(accepted, Duration::from_secs(3))?;
    let stats = world.shipper_stats()?;
    let sink = world.sink_stats()?;
    let _ = world.stop_and_shutdown()?;
    Ok(OverloadReport {
        submitted: outcomes.len(),
        accepted,
        dropped_full,
        shipper_mailbox_full: mailbox_full,
        stats,
        sink,
    })
}

pub fn run_shutdown(config: &RunConfig) -> anyhow::Result<ShutdownReport> {
    let mut lifecycle_transitions: Vec<Lifecycle> = vec![Lifecycle::Starting];
    let world = World::start(config)?;
    lifecycle_transitions.push(Lifecycle::Ready);
    let topology = build_topology(config);

    // Submit a partial batch (smaller than batch_size) so Stop must flush
    // on drain rather than ride a size-based flush.
    let partial = (config.batch_size - 1).max(1);
    let outcomes = world.submit_burst(partial, 1, true)?;
    let accepted = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, CallOutcome::Replied(ShipperReply::Accepted)))
        .count();
    anyhow::ensure!(
        accepted == partial,
        "shutdown precondition: expected all {partial} events accepted, got {accepted}"
    );

    // Typed shutdown choreography for a non-HTTP service. Same builder
    // and same step kinds as `mini_saas_api`, proving the helper is not
    // HTTP-shaped by accident.
    let mut choreo = ShutdownChoreography::new("system_metrics_shipper");

    let t_drain = std::time::Instant::now();
    let stop_outcome = world.runtime.call_blocking(
        world.shipper,
        ShipperMsg::Request(ShipperRequest::Stop),
        Duration::from_millis(config.stop_timeout_ms),
    )?;
    let (flushed_on_drain, drained_batches, drain_clean) = match stop_outcome {
        CallOutcome::Replied(ShipperReply::Stopped {
            flushed_on_drain,
            drained_batches,
        }) => {
            // Shipper has flipped its own DrainState through Stopped.
            lifecycle_transitions.push(Lifecycle::Draining);
            (flushed_on_drain, drained_batches, true)
        }
        other => {
            choreo.record(
                ShutdownStep::DrainInFlight,
                "shipper_stop_drain",
                t_drain.elapsed(),
                StepOutcome::Failed {
                    reason: format!("expected Stopped reply, got {other:?}"),
                },
            );
            anyhow::bail!("expected Stopped reply, got {other:?}")
        }
    };
    choreo.record(
        ShutdownStep::DrainInFlight,
        "shipper_stop_drain",
        t_drain.elapsed(),
        if drain_clean {
            StepOutcome::Clean
        } else {
            StepOutcome::Failed {
                reason: "drain did not return Stopped".to_owned(),
            }
        },
    );

    // Invariant after Stop: the shipper must refuse new admission with
    // Stopping. This is an assertion, not a shutdown phase — the act of
    // closing ingress already happened inside `ShipperRequest::Stop` which
    // flipped `DrainState::begin()`. Recording it as a choreography
    // step would (correctly) flag an ordering violation since
    // StopIngress sits before DrainInFlight; the invariant check still
    // proves the shipper is in the right state, just outside the
    // ordered choreography.
    let stopping = world.runtime.call_blocking(
        world.shipper,
        ShipperMsg::Request(ShipperRequest::Submit {
            event: Event {
                key: "after-stop".into(),
                value: 0,
            },
        }),
        Duration::from_millis(config.call_timeout_ms),
    )?;
    anyhow::ensure!(
        matches!(stopping, CallOutcome::Replied(ShipperReply::Stopping)),
        "expected Stopping reply after Stop, got {stopping:?}"
    );

    let stats = world.shipper_stats()?;
    let sink = world.sink_stats()?;

    // Health snapshot at the typed Stopped state, taken before the
    // runtime is torn down so live numbers reflect the shipper's
    // terminal counters.
    let pressure_snapshot = build_pressure_snapshot(&stats, &sink, config);
    let health =
        Health::new("system_metrics_shipper", Lifecycle::Stopped).with_pressure(pressure_snapshot);

    // The sink isolate is stopped implicitly when the runtime shuts
    // down. Record it as a close step so the report has the same shape
    // as a service that owns an explicit `close()` call.
    let t_sink = std::time::Instant::now();
    let sink_close = ResourceCloseReport::clean(
        "sink.isolate",
        ResourceKind::Child,
        CloseAdmission::Drain,
        t_sink.elapsed(),
    )
    .with_details(format!(
        "batches_received={} events_received={}",
        sink.batches_received, sink.events_received,
    ));
    choreo.record_close(&sink_close, "stop_sink_isolate");

    let t_owner = std::time::Instant::now();
    let stop_clean = world.shutdown()?;
    choreo.record(
        ShutdownStep::StopOwner,
        "runtime_shutdown",
        t_owner.elapsed(),
        if stop_clean {
            StepOutcome::Clean
        } else {
            StepOutcome::Failed {
                reason: "runtime shutdown reported error".to_owned(),
            }
        },
    );

    let shutdown_choreography = choreo.finish();
    lifecycle_transitions.push(Lifecycle::Stopped);

    Ok(ShutdownReport {
        stop_clean,
        flushed_on_drain,
        drained_batches,
        stats,
        sink,
        health,
        topology,
        shutdown_choreography,
        lifecycle_transitions,
    })
}

/// Build the typed startup topology snapshot for the shipper system.
/// Names every isolate (shipper, sink) and every bounded surface
/// (shipper mailbox, in-memory buffer, batch caps, flush gate, sink
/// mailbox) in one greppable report.
fn build_topology(config: &RunConfig) -> ServiceTopology {
    use tina::capacity::{CapacityMode, CapacitySurfaceReport};
    use tina_runtime::ServicePressureReport;

    let mut pressure = ServicePressureReport::new("system_metrics_shipper");
    pressure.add_measured(
        "mailbox",
        CapacitySurfaceReport::count(
            "shipper.mailbox",
            CapacityMode::Fixed,
            config.shipper_mailbox,
            0,
            0,
            0,
        ),
    );
    pressure.add_measured(
        "buffer",
        CapacitySurfaceReport::count(
            "shipper.buffer",
            CapacityMode::Fixed,
            config.buffer_capacity,
            0,
            0,
            0,
        ),
    );
    pressure.add_measured(
        "mailbox",
        CapacitySurfaceReport::count(
            "sink.mailbox",
            CapacityMode::Fixed,
            config.sink_mailbox,
            0,
            0,
            0,
        ),
    );
    pressure.add_unavailable(
        "shipper.flush_gate",
        "scope",
        "sampled live via LocalPermitGate inside the shipper",
    );

    let mut topology = ServiceTopology::new("system_metrics_shipper", Lifecycle::Ready);
    topology
        .push_component(
            TopologyComponent::new("shipper", "isolate", "")
                .with_notes("ingress + batcher + drain handshake"),
        )
        .push_component(
            TopologyComponent::new("sink", "isolate", "").with_notes("downstream HTTP/DB stand-in"),
        )
        .push_component(
            TopologyComponent::new(
                "flush_tick",
                "timer",
                format!("every_{}ms", config.batch_window_ms),
            )
            .with_notes("recurring tick with stale-token discipline"),
        );
    topology.with_pressure(pressure)
}

/// Build a small pressure snapshot for the typed `Health` report. Mirrors
/// the live counters carried in `ShipperStats` and `SinkStats` so a
/// dashboard can read state + bounded surfaces in one place.
fn build_pressure_snapshot(
    stats: &ShipperStats,
    sink: &SinkStats,
    config: &RunConfig,
) -> tina_runtime::ServicePressureReport {
    use tina::capacity::{CapacityMode, CapacitySurfaceReport};
    use tina_runtime::ServicePressureReport;

    let mut p = ServicePressureReport::new("system_metrics_shipper");
    p.add_measured(
        "buffer",
        CapacitySurfaceReport::count(
            "shipper.buffer",
            CapacityMode::Fixed,
            stats.buffer_capacity,
            0,
            stats.buffer_high_water,
            stats.buffer_full_rejects,
        ),
    );
    p.add_measured(
        "mailbox",
        CapacitySurfaceReport::count(
            "sink.mailbox",
            CapacityMode::Fixed,
            sink.mailbox_capacity,
            0,
            0,
            0,
        ),
    );
    p.add_measured(
        "mailbox",
        CapacitySurfaceReport::count(
            "shipper.mailbox",
            CapacityMode::Fixed,
            config.shipper_mailbox,
            0,
            0,
            0,
        ),
    );
    p
}

/// Map a `DrainState` stage to the typed [`Lifecycle`] vocabulary so a
/// shipper isolate can report state in the same words as
/// `mini_saas_api`.
pub fn lifecycle_for_drain_stage(stage: DrainStage) -> Lifecycle {
    match stage {
        DrainStage::Open => Lifecycle::Ready,
        DrainStage::Draining => Lifecycle::Draining,
        DrainStage::Stopped => Lifecycle::Stopped,
    }
}

struct World {
    runtime: Arc<ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>>,
    shutdown: ThreadedShutdownHandle,
    shipper: ShipperAddr,
    sink: SinkAddr,
    call_timeout: Duration,
    stop_timeout: Duration,
}

impl World {
    fn start(config: &RunConfig) -> anyhow::Result<Self> {
        let runtime = Arc::new(ThreadedRuntime::new(
            SingleShard,
            DefaultThreadedMailboxFactory,
        ));
        let sink = runtime
            .register_split_service::<Sink, SinkEvent, SinkRequest, Infallible>(
                Sink::new(
                    config.sink_mailbox,
                    config.sink_fail_every,
                    Duration::from_millis(config.sink_flush_delay_ms),
                ),
                config.sink_mailbox,
            )
            .map_err(|e| anyhow::anyhow!("register sink: {e:?}"))?
            .requests;
        let shipper = runtime
            .register_split_service::<Shipper, ShipperEvent, ShipperRequest, Infallible>(
                Shipper::new(sink, config),
                config.shipper_mailbox,
            )
            .map_err(|e| anyhow::anyhow!("register shipper: {e:?}"))?
            .requests
            .address()
            .address();
        // Cloneable shutdown handle: lets the host drive runtime
        // teardown without `Arc::try_unwrap(runtime)` once the burst
        // threads have joined.
        let shutdown = runtime.shutdown_handle();
        Ok(Self {
            runtime,
            shutdown,
            shipper,
            sink,
            call_timeout: Duration::from_millis(config.call_timeout_ms),
            stop_timeout: Duration::from_millis(config.stop_timeout_ms),
        })
    }

    fn submit_burst(
        &self,
        events: usize,
        callers: usize,
        sequential: bool,
    ) -> anyhow::Result<Vec<CallOutcome<ShipperReply>>> {
        if sequential || callers <= 1 {
            let mut outcomes = Vec::with_capacity(events);
            for i in 0..events {
                outcomes.push(self.runtime.call_blocking(
                    self.shipper,
                    ShipperMsg::Request(ShipperRequest::Submit {
                        event: Event {
                            key: format!("k{i}"),
                            value: i as i64,
                        },
                    }),
                    self.call_timeout,
                )?);
            }
            return Ok(outcomes);
        }
        let barrier = Arc::new(Barrier::new(callers + 1));
        let outcomes = Arc::new(Mutex::new(Vec::with_capacity(events)));
        let per_caller = events.div_ceil(callers);
        let mut threads = Vec::with_capacity(callers);
        for caller in 0..callers {
            let rt = Arc::clone(&self.runtime);
            let gate = Arc::clone(&barrier);
            let bucket = Arc::clone(&outcomes);
            let timeout = self.call_timeout;
            let shipper = self.shipper;
            let start = caller * per_caller;
            let end = ((caller + 1) * per_caller).min(events);
            threads.push(thread::spawn(move || -> anyhow::Result<()> {
                gate.wait();
                for i in start..end {
                    let event = Event {
                        key: format!("k{i}"),
                        value: i as i64,
                    };
                    let outcome = rt.call_blocking(
                        shipper,
                        ShipperMsg::Request(ShipperRequest::Submit { event }),
                        timeout,
                    )?;
                    bucket.lock().expect("outcomes lock").push(outcome);
                }
                Ok(())
            }));
        }
        barrier.wait();
        for handle in threads {
            handle
                .join()
                .map_err(|_| anyhow::anyhow!("submit thread panicked"))??;
        }
        let outcomes = Arc::try_unwrap(outcomes)
            .map_err(|_| anyhow::anyhow!("outcomes arc still shared"))?
            .into_inner()
            .map_err(|_| anyhow::anyhow!("outcomes poisoned"))?;
        Ok(outcomes)
    }

    fn shipper_stats(&self) -> anyhow::Result<ShipperStats> {
        match self
            .runtime
            .call_blocking(
                self.shipper,
                ShipperMsg::Request(ShipperRequest::Stats),
                self.call_timeout,
            )?
        {
            CallOutcome::Replied(ShipperReply::Stats(stats)) => Ok(stats),
            other => anyhow::bail!("expected Stats reply, got {other:?}"),
        }
    }

    fn sink_stats(&self) -> anyhow::Result<SinkStats> {
        match self
            .runtime
            .call_blocking_request(self.sink, SinkRequest::Stats, self.call_timeout)?
        {
            CallOutcome::Replied(SinkReply::Stats(stats)) => Ok(stats),
            other => anyhow::bail!("expected sink Stats reply, got {other:?}"),
        }
    }

    fn wait_for_flushed_events(
        &self,
        target_accepted: usize,
        timeout: Duration,
    ) -> anyhow::Result<()> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let stats = self.shipper_stats()?;
            let sink = self.sink_stats()?;
            let routed = sink.events_received + stats.events_lost_on_flush;
            if routed >= target_accepted as u64 {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                anyhow::bail!(
                    "timed out waiting for routed >= {target_accepted}: \
                     sink.events={} shipper.lost={} shipper.high_water={}",
                    sink.events_received,
                    stats.events_lost_on_flush,
                    stats.buffer_high_water
                );
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn stop_and_shutdown(self) -> anyhow::Result<bool> {
        let outcome =
            self.runtime
                .call_blocking(
                    self.shipper,
                    ShipperMsg::Request(ShipperRequest::Stop),
                    self.stop_timeout,
                )?;
        let clean = matches!(outcome, CallOutcome::Replied(ShipperReply::Stopped { .. }));
        self.shutdown()?;
        Ok(clean)
    }

    /// Tear down the runtime through the cloneable shutdown handle. No
    /// `Arc::try_unwrap(runtime)` ceremony: the handle requests shutdown,
    /// waits for the terminal report, and the runtime Arc can be dropped
    /// independently.
    ///
    /// Service-level drain (the shipper's own `Stop`/`DrainState`
    /// protocol) is the shipper's responsibility and is driven separately
    /// by [`Self::stop_and_shutdown`]; this helper controls the runtime
    /// only.
    fn shutdown(self) -> anyhow::Result<bool> {
        self.shutdown
            .request_shutdown()
            .map_err(|e| anyhow::anyhow!("runtime shutdown request: {e:?}"))?;
        let report = self
            .shutdown
            .wait_report(Duration::from_secs(5))
            .map_err(|e| anyhow::anyhow!("runtime shutdown wait: {e:?}"))?;
        // Drop the Arc; the runtime owner's `Drop` short-circuits via
        // the cached terminal report so this does not re-join.
        drop(self.runtime);
        Ok(report.error().is_none())
    }
}
