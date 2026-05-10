//! Tina side. The batcher is one isolate.
//!
//! No `select!`, no future to drop. The two events that drive a flush
//! — "next item arrived" and "interval timer fired" — are both
//! ordinary mailbox messages. State that would live across `select!`
//! arms in async/await (the buffer, the deadline, "is a timer in
//! flight?") is plain isolate state.
//!
//! Cancellation of a stale timer is the one extra ceremony: Tina has
//! no API to abort an in-flight `sleep`, so each `sleep(...).reply(...)`
//! carries a `gen: u64` and the handler ignores any `Tick` whose
//! generation does not match the latest scheduled one. That keeps
//! `timer_flushes` accurate even when a size-triggered flush runs
//! before the corresponding timer fires.

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
    /// Interval timer fired. The `u64` is the generation of the
    /// timer; if it does not match the batcher's latest generation,
    /// the tick is stale (a size flush already happened) and is
    /// ignored.
    Tick(u64, SleepReply),
    /// Producer has closed the burst. Flush any remaining items as a
    /// final timer-style flush, then `stop_with(report)`.
    BurstClosed,
}

struct Batcher {
    interval: Duration,
    /// Buffer of item indices, drained on every flush. Stored so the
    /// `Submit(u32)` payload is read deliberately rather than ignored.
    buffer: Vec<u32>,
    /// Generation of the latest scheduled timer. Incremented every
    /// time we schedule a new timer or invalidate an in-flight one.
    timer_gen: u64,
    /// `Some(gen)` when a timer with that generation is in flight.
    /// `None` when no timer is scheduled (buffer is empty or a size
    /// flush invalidated the previous one).
    pending_timer_gen: Option<u64>,
    report: Report,
}

#[tina_runtime::isolate(message = BatcherMsg)]
impl Batcher {
    fn handle(&mut self, msg: BatcherMsg, _ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
        match msg {
            BatcherMsg::Submit(item) => {
                self.report.items_seen += 1;
                self.buffer.push(item);
                if self.buffer.len() >= BATCH_SIZE {
                    self.buffer.clear();
                    self.report.size_flushes += 1;
                    // Invalidate any in-flight timer; its Tick will be
                    // ignored as stale.
                    self.timer_gen += 1;
                    self.pending_timer_gen = None;
                    return noop();
                }
                if self.pending_timer_gen.is_none() {
                    self.timer_gen += 1;
                    let g = self.timer_gen;
                    self.pending_timer_gen = Some(g);
                    sleep(self.interval).reply(move |reply| BatcherMsg::Tick(g, reply))
                } else {
                    noop()
                }
            }
            BatcherMsg::Tick(g, reply) => {
                if reply.is_err() {
                    // Sleep was cancelled (runtime shutdown). Treat as
                    // "no flush"; the burst-closed path handles
                    // anything still in the buffer.
                    self.pending_timer_gen = None;
                    return noop();
                }
                if self.pending_timer_gen != Some(g) {
                    // Stale tick; a size flush invalidated this
                    // generation. Ignore.
                    return noop();
                }
                self.pending_timer_gen = None;
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
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));

    let batcher = Batcher {
        interval: Duration::from_millis(BATCH_INTERVAL_MS),
        buffer: Vec::with_capacity(BATCH_SIZE),
        timer_gen: 0,
        pending_timer_gen: None,
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

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    Ok(report)
}
