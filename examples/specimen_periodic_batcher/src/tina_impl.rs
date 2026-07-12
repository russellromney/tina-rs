//! Tina side. The batcher is one isolate.
//!
//! No `select!`, no future to drop. The two events that drive a flush
//! — "next item arrived" and "interval timer fired" — are both
//! ordinary mailbox messages. State that would live across `select!`
//! arms in async/await (the buffer, the deadline, "is a timer in
//! flight?") is plain isolate state.
//!
//! Cancellation of a stale timer is still explicit: Tina has no API to
//! abort an in-flight `sleep`, so each `sleep(...).then(...)` carries
//! the token chosen by `RecurringTick`. The handler validates the token
//! and ignores stale ticks.

use std::convert::Infallible;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{DefaultThreadedMailboxFactory, SleepReply, ThreadedRuntime, sleep};

use crate::{
    BATCH_INTERVAL_MS, BATCH_SIZE, PRODUCER_GAP_MS, Report, TOTAL_ITEMS, TRAILING_PAUSE_MS,
};

#[derive(Debug)]
enum BatcherMsg {
    /// One item from the producer.
    Submit(u32),
    /// Interval timer fired. If the token is stale, a size flush already
    /// invalidated it and the tick is ignored.
    Tick(RecurringTickToken, SleepReply),
    /// Producer has closed the burst. Flush any remaining items as a
    /// final timer-style flush, then `stop_with(report)`.
    BurstClosed,
}

struct Batcher {
    interval: RecurringTick,
    /// Buffer of item indices, drained on every flush. Stored so the
    /// `Submit(u32)` payload is read deliberately rather than ignored.
    buffer: Vec<u32>,
    /// `Some(token)` when a timer for that recurring tick is in flight.
    /// `None` when no timer is scheduled (buffer is empty or a size
    /// flush invalidated the previous one).
    pending_tick: Option<RecurringTickToken>,
    report: Report,
}

#[tina_runtime::isolate(message = BatcherMsg)]
impl Batcher {
    fn handle(&mut self, msg: BatcherMsg, ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
        match msg {
            BatcherMsg::Submit(item) => {
                self.report.items_seen += 1;
                self.buffer.push(item);
                if self.buffer.len() >= BATCH_SIZE {
                    self.buffer.clear();
                    self.report.size_flushes += 1;
                    // Invalidate any in-flight timer; its Tick will be ignored
                    // as stale. Clear the helper so the next item starts a
                    // fresh period from that later handler turn's runtime time.
                    self.pending_tick = None;
                    self.interval.clear();
                    return noop();
                }
                if self.pending_tick.is_none() {
                    match self.interval.next(ctx.now()) {
                        RecurringTickDecision::Sleep { delay, token, .. } => {
                            self.pending_tick = Some(token);
                            sleep(delay).then(move |reply| BatcherMsg::Tick(token, reply))
                        }
                        RecurringTickDecision::Skip(_) => noop(),
                    }
                } else {
                    noop()
                }
            }
            BatcherMsg::Tick(token, reply) => {
                if reply.is_err() {
                    // Sleep was cancelled (runtime shutdown). Treat as
                    // "no flush"; the burst-closed path handles
                    // anything still in the buffer.
                    if self.pending_tick == Some(token) {
                        self.pending_tick = None;
                    }
                    return noop();
                }
                if self.pending_tick != Some(token) || self.interval.validate(token).is_err() {
                    // Stale tick; a size flush invalidated this
                    // recurring tick. Ignore.
                    return noop();
                }
                self.pending_tick = None;
                if !self.buffer.is_empty() {
                    self.buffer.clear();
                    self.report.timer_flushes += 1;
                }
                noop()
            }
            BatcherMsg::BurstClosed => {
                if !self.buffer.is_empty() {
                    self.buffer.clear();
                    self.report.timer_flushes += 1;
                }
                self.report.exit_clean = true;
                stop_with(self.report)
            }
        }
    }
}

pub fn run() -> anyhow::Result<Report> {
    let runtime = Arc::new(ThreadedRuntime::try_new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    )?);
    let shutdown = runtime.shutdown_handle();

    let batcher = Batcher {
        interval: RecurringTick::every(Duration::from_millis(BATCH_INTERVAL_MS))
            .map_err(|e| anyhow::anyhow!("configure interval: {e:?}"))?,
        buffer: Vec::with_capacity(BATCH_SIZE),
        pending_tick: None,
        report: Report::default(),
    };
    let addr = runtime
        .register_with_capacity::<_, Infallible>(batcher, 64)
        .map_err(|e| anyhow::anyhow!("register batcher: {e:?}"))?;

    let waiter = runtime
        .observe_result::<Report, _, _>(addr)
        .map_err(|e| anyhow::anyhow!("observe_result: {e:?}"))?;

    // Producer: same script as the Tokio side. 10 items with
    // `PRODUCER_GAP_MS` between, trailing pause to let the timer
    // fire on the third batch, then the last 2 items, then a final
    // pause to let the timer fire again, then BurstClosed.
    for n in 0..(TOTAL_ITEMS - 2) {
        runtime
            .try_send(addr, BatcherMsg::Submit(n))
            .map_err(|e| anyhow::anyhow!("try_send {n}: {e:?}"))?;
        thread::sleep(Duration::from_millis(PRODUCER_GAP_MS));
    }
    thread::sleep(Duration::from_millis(TRAILING_PAUSE_MS));
    for n in (TOTAL_ITEMS - 2)..TOTAL_ITEMS {
        runtime
            .try_send(addr, BatcherMsg::Submit(n))
            .map_err(|e| anyhow::anyhow!("try_send {n}: {e:?}"))?;
        thread::sleep(Duration::from_millis(PRODUCER_GAP_MS));
    }
    thread::sleep(Duration::from_millis(BATCH_INTERVAL_MS * 3));

    runtime
        .try_send(addr, BatcherMsg::BurstClosed)
        .map_err(|e| anyhow::anyhow!("send BurstClosed: {e:?}"))?;

    let report = waiter
        .wait(Duration::from_secs(5))
        .map_err(|e| anyhow::anyhow!("batcher did not finish: {e:?}"))?;

    let terminal = shutdown.request_and_wait_report(Duration::from_secs(5))?;
    drop(runtime);
    terminal.ensure_clean()?;
    Ok(report)
}
