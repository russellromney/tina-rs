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

use tina::time::{
    RecurringCatchUp, RecurringTick, RecurringTickDecision, RecurringTickToken,
};
use tina::{CallContext, RequestContext, prelude::*, reply_to_request};
use tina_runtime::{
    AdmitDecision, CallOutcome, DefaultThreadedMailboxFactory, DrainState, LocalPermitGate,
    LocalPermitName, Permit, ThreadedRuntime, ThreadedShutdownHandle, call, sleep,
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

#[derive(Debug)]
pub enum ShipperMsg {
    Submit {
        event: Event,
    },
    Stats,
    Stop,
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

#[derive(Debug)]
pub enum SinkMsg {
    Flush {
        batch: Vec<Event>,
    },
    Stats,
    Complete {
        req: RequestContext<SinkReply>,
        batch: Vec<Event>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SinkReply {
    Ack,
    Failed,
    Stats(SinkStats),
}

type ShipperAddr = Address<ShipperMsg, ShipperReply>;
type SinkAddr = Address<SinkMsg, SinkReply>;

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
    /// admission/completion counters used by Tina's Phase 101 helper.
    drain: DrainState,
    pending_stop: Option<RequestContext<ShipperReply>>,
    drained_events: usize,
    drained_batches: usize,
    stats: ShipperStats,
}

#[tina_runtime::isolate(message = ShipperMsg, reply = ShipperReply)]
impl Shipper {
    fn handle(
        &mut self,
        msg: ShipperMsg,
        ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        let now = ctx.now();
        match msg {
            ShipperMsg::Tick { token } => self.on_tick(token),
            ShipperMsg::FlushDone {
                kind,
                count,
                permit,
                outcome,
            } => self.on_flush_done(kind, count, permit, outcome, now),
            // Call-only shapes get dropped here. The host always calls them.
            ShipperMsg::Submit { .. } | ShipperMsg::Stats | ShipperMsg::Stop => noop(),
        }
    }

    fn handle_call(&mut self, msg: ShipperMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            ShipperMsg::Submit { event } => self.on_submit(event, call),
            ShipperMsg::Stats => call.reply(ShipperReply::Stats(self.snapshot())),
            ShipperMsg::Stop => self.on_stop(call),
            ShipperMsg::Tick { .. } | ShipperMsg::FlushDone { .. } => {
                call.reject(tina::CallRejectedReason::UnsupportedMessage)
            }
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

    fn on_submit(&mut self, event: Event, call: CallContext<'_, Self>) -> Effect<Self> {
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
            return Effect::Batch(vec![
                call.reply(ShipperReply::Accepted),
                self.start_flush(FlushKind::Size),
            ]);
        }

        // Time-based flush: arm the recurring tick the first time the buffer
        // goes non-empty and no flush is in flight. After a flush completes
        // FlushDone decides whether to re-arm.
        if self.flush_gate.is_idle()
            && self.flush_tick.next_due().is_none()
            && !self.buffer.is_empty()
        {
            return Effect::Batch(vec![call.reply(ShipperReply::Accepted), self.arm_tick(now)]);
        }
        call.reply(ShipperReply::Accepted)
    }

    fn on_stop(&mut self, call: CallContext<'_, Self>) -> Effect<Self> {
        if !self.drain.is_open() {
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
        self.pending_stop = Some(call.into_request_context());
        if self.flush_gate.is_idle() {
            self.start_flush(FlushKind::Drain)
        } else {
            noop()
        }
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
                return reply_to_request(req, reply);
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
        call::<SinkMsg, SinkReply>(self.sink, SinkMsg::Flush { batch }, self.flush_timeout).then(
            move |outcome| ShipperMsg::FlushDone {
                kind,
                count,
                permit,
                outcome,
            },
        )
    }

    fn arm_tick(&mut self, now: std::time::Instant) -> Effect<Self> {
        let decision = self.flush_tick.next(now);
        self.stats.ticks_armed += 1;
        match decision {
            RecurringTickDecision::Sleep { delay, token, .. } => {
                sleep(delay).then(move |_| ShipperMsg::Tick { token })
            }
            RecurringTickDecision::Skip(report) => {
                // Skip means whole periods were missed since this isolate
                // arm. With the helper's tightened semantics this only fires
                // when `report.missed_ticks > 0`; record the coalesced count
                // and let the caller's next action arm a fresh tick.
                self.stats.ticks_fired_stale =
                    self.stats.ticks_fired_stale.saturating_add(report.missed_ticks);
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

#[tina_runtime::isolate(message = SinkMsg, reply = SinkReply)]
impl Sink {
    fn handle(
        &mut self,
        msg: SinkMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            SinkMsg::Complete { req, batch } => self.complete(req, batch),
            SinkMsg::Flush { .. } | SinkMsg::Stats => noop(),
        }
    }

    fn handle_call(&mut self, msg: SinkMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            SinkMsg::Flush { batch } => {
                if self.flush_delay.is_zero() {
                    return self.reply_for_batch(call, batch);
                }
                call.defer(sleep(self.flush_delay))
                    .reply(move |req, _| SinkMsg::Complete { req, batch })
            }
            SinkMsg::Stats => call.reply(SinkReply::Stats(SinkStats {
                mailbox_capacity: self.mailbox_capacity,
                batches_received: self.received,
                events_received: self.events,
                failures_injected: self.failures,
            })),
            SinkMsg::Complete { .. } => call.reject(tina::CallRejectedReason::UnsupportedMessage),
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

    fn reply_for_batch(&mut self, call: CallContext<'_, Self>, batch: Vec<Event>) -> Effect<Self> {
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
            return reply_to_request(req, SinkReply::Failed);
        }
        self.events += batch.len() as u64;
        reply_to_request(req, SinkReply::Ack)
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
    let world = World::start(config)?;
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

    let stop_outcome = world.runtime.call_blocking(
        world.shipper,
        ShipperMsg::Stop,
        Duration::from_millis(config.stop_timeout_ms),
    )?;
    let (flushed_on_drain, drained_batches) = match stop_outcome {
        CallOutcome::Replied(ShipperReply::Stopped {
            flushed_on_drain,
            drained_batches,
        }) => (flushed_on_drain, drained_batches),
        other => anyhow::bail!("expected Stopped reply, got {other:?}"),
    };

    let stopping = world.runtime.call_blocking(
        world.shipper,
        ShipperMsg::Submit {
            event: Event {
                key: "after-stop".into(),
                value: 0,
            },
        },
        Duration::from_millis(config.call_timeout_ms),
    )?;
    anyhow::ensure!(
        matches!(stopping, CallOutcome::Replied(ShipperReply::Stopping)),
        "expected Stopping reply after Stop, got {stopping:?}"
    );

    let stats = world.shipper_stats()?;
    let sink = world.sink_stats()?;
    let stop_clean = world.shutdown()?;

    Ok(ShutdownReport {
        stop_clean,
        flushed_on_drain,
        drained_batches,
        stats,
        sink,
    })
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
            .register_with_capacity::<_, Infallible>(
                Sink::new(
                    config.sink_mailbox,
                    config.sink_fail_every,
                    Duration::from_millis(config.sink_flush_delay_ms),
                ),
                config.sink_mailbox,
            )
            .map_err(|e| anyhow::anyhow!("register sink: {e:?}"))?;
        let shipper = runtime
            .register_with_capacity::<_, Infallible>(
                Shipper::new(sink, config),
                config.shipper_mailbox,
            )
            .map_err(|e| anyhow::anyhow!("register shipper: {e:?}"))?;
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
                    ShipperMsg::Submit {
                        event: Event {
                            key: format!("k{i}"),
                            value: i as i64,
                        },
                    },
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
                    let outcome =
                        rt.call_blocking(shipper, ShipperMsg::Submit { event }, timeout)?;
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
            .call_blocking(self.shipper, ShipperMsg::Stats, self.call_timeout)?
        {
            CallOutcome::Replied(ShipperReply::Stats(stats)) => Ok(stats),
            other => anyhow::bail!("expected Stats reply, got {other:?}"),
        }
    }

    fn sink_stats(&self) -> anyhow::Result<SinkStats> {
        match self
            .runtime
            .call_blocking(self.sink, SinkMsg::Stats, self.call_timeout)?
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
                .call_blocking(self.shipper, ShipperMsg::Stop, self.stop_timeout)?;
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
