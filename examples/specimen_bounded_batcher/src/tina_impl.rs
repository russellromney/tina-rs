use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, ParkError, PendingReplies,
    request_effect_after_park, SleepReply, ThreadedRuntime, call_request, sleep,
};

use crate::{BATCH_SIZE, BATCH_TIMEOUT_MS, CALLERS, MAX_PENDING, Report};

const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Caller-authority request: the only thing an outside caller can ask.
#[derive(Debug)]
enum BatcherRequest {
    Submit(u64),
}

/// Internal event: the batch timer's continuation.
#[derive(Debug)]
enum BatcherEvent {
    Tick(u64, SleepReply),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatcherReply {
    Batched(u64),
    Full,
}

struct Batcher {
    pending: PendingReplies<u64, BatcherReply>,
    interval: Duration,
    items: Vec<u64>,
    qids: Vec<u64>,
    next_qid: u64,
    timer_gen: u64,
    pending_timer_gen: Option<u64>,
    size_flushes: Arc<AtomicUsize>,
    timer_flushes: Arc<AtomicUsize>,
}

#[tina_runtime::isolate(event = BatcherEvent, request = BatcherRequest, reply = BatcherReply)]
impl Batcher {
    fn handle_event(
        &mut self,
        event: BatcherEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            BatcherEvent::Tick(g, reply) => {
                if reply.is_err() || self.pending_timer_gen != Some(g) {
                    return noop();
                }
                self.pending_timer_gen = None;
                if self.items.is_empty() {
                    return noop();
                }
                self.timer_flushes.fetch_add(1, Ordering::Release);
                self.flush()
            }
        }
    }

    fn handle_request(
        &mut self,
        request: BatcherRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            BatcherRequest::Submit(item) => {
                let qid = self.next_qid;
                self.next_qid += 1;
                match self.pending.park_request(qid, call) {
                    Ok((_ticket, permit)) => {
                        self.items.push(item);
                        self.qids.push(qid);
                        if self.items.len() >= BATCH_SIZE {
                            self.size_flushes.fetch_add(1, Ordering::Release);
                            self.timer_gen += 1;
                            self.pending_timer_gen = None;
                            let flush_effect = self.flush();
                            return request_effect_after_park(permit, flush_effect);
                        }
                        if self.pending_timer_gen.is_none() {
                            self.timer_gen += 1;
                            let g = self.timer_gen;
                            self.pending_timer_gen = Some(g);
                            let tick_effect = sleep(self.interval)
                                .then_service_event(move |reply| BatcherEvent::Tick(g, reply));
                            return request_effect_after_park(permit, tick_effect);
                        }
                        request_effect_after_park(permit, noop())
                    }
                    Err(ParkError::Full { call, .. }) => call.reply(BatcherReply::Full),
                    Err(ParkError::DuplicateKey { call, .. }) => call.reply(BatcherReply::Full),
                }
            }
        }
    }
}

impl Batcher {
    fn flush(&mut self) -> Effect<Self> {
        let total: u64 = self.items.iter().sum();
        let mut effects = Vec::with_capacity(self.qids.len());
        for qid in self.qids.drain(..) {
            if let Some(slot) = self.pending.take(&qid) {
                effects.push(reply_to(slot, BatcherReply::Batched(total)));
            }
        }
        self.items.clear();
        Effect::Batch(effects)
    }
}

// --- Driver ------------------------------------------------------------

#[derive(Debug, Default, Clone, Copy)]
struct DriverOutcome {
    successes: usize,
    full_rejects: usize,
    failed: usize,
}

#[derive(Debug)]
enum DriverMsg {
    Begin,
    Returned(usize, CallOutcome<BatcherReply>),
}

struct Driver {
    batcher: tina::ServiceRequestAddress<BatcherEvent, BatcherRequest, BatcherReply>,
    items: Vec<u64>,
    remaining: usize,
    outcome: DriverOutcome,
}

#[tina_runtime::isolate(message = DriverMsg)]
impl Driver {
    fn handle(
        &mut self,
        msg: DriverMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            DriverMsg::Begin => {
                let batcher = self.batcher;
                let calls: Vec<_> = self
                    .items
                    .iter()
                    .copied()
                    .enumerate()
                    .map(|(i, item)| {
                        call_request(batcher, BatcherRequest::Submit(item), CALL_TIMEOUT)
                            .then(move |outcome| DriverMsg::Returned(i, outcome))
                    })
                    .collect();
                Effect::Batch(calls)
            }
            DriverMsg::Returned(_i, outcome) => {
                match outcome {
                    CallOutcome::Replied(BatcherReply::Batched(_)) => self.outcome.successes += 1,
                    CallOutcome::Replied(BatcherReply::Full) => self.outcome.full_rejects += 1,
                    _ => self.outcome.failed += 1,
                }
                self.remaining -= 1;
                if self.remaining == 0 {
                    stop_with(self.outcome)
                } else {
                    noop()
                }
            }
        }
    }
}

pub fn run() -> anyhow::Result<Report> {
    let runtime = ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory);

    let size_flushes = Arc::new(AtomicUsize::new(0));
    let timer_flushes = Arc::new(AtomicUsize::new(0));

    let batcher = runtime
        .register_split_service::<Batcher, BatcherEvent, BatcherRequest, Infallible>(
            Batcher {
                pending: PendingReplies::with_capacity(MAX_PENDING),
                interval: Duration::from_millis(BATCH_TIMEOUT_MS),
                items: Vec::with_capacity(BATCH_SIZE),
                qids: Vec::with_capacity(BATCH_SIZE),
                next_qid: 1,
                timer_gen: 0,
                pending_timer_gen: None,
                size_flushes: Arc::clone(&size_flushes),
                timer_flushes: Arc::clone(&timer_flushes),
            },
            64,
        )
        .map_err(|e| anyhow::anyhow!("register batcher: {e:?}"))?
        .requests;

    let items: Vec<u64> = (0..CALLERS as u64).map(|c| c + 1).collect();
    let driver = runtime
        .register_with_capacity::<_, Infallible>(
            Driver {
                batcher,
                items,
                remaining: CALLERS,
                outcome: DriverOutcome::default(),
            },
            64,
        )
        .map_err(|e| anyhow::anyhow!("register driver: {e:?}"))?;

    let result = runtime
        .observe_result::<DriverOutcome, _, _>(driver)
        .map_err(|e| anyhow::anyhow!("observe_result: {e:?}"))?;
    runtime
        .try_send(driver, DriverMsg::Begin)
        .map_err(|e| anyhow::anyhow!("kick driver: {e:?}"))?;
    let outcome = result
        .wait(Duration::from_secs(10))
        .map_err(|e| anyhow::anyhow!("driver finishes: {e:?}"))?;

    let _ = runtime.shutdown();

    Ok(Report {
        callers: CALLERS,
        successes: outcome.successes,
        full_rejects: outcome.full_rejects,
        failed: outcome.failed,
        batches_size_flushed: size_flushes.load(Ordering::Acquire),
        batches_timer_flushed: timer_flushes.load(Ordering::Acquire),
        exit_clean: true,
    })
}
