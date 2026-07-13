use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use tina::{
    CancelOutcome, Effect, RequestContext, ServiceRequestAddress, SingleShard, noop, reply_to,
    send_event,
};
use tina_runtime::{
    CallError, CallOutcome, CallReplyRejectedReason, CallSelectEvent, CallSelectSet,
    DeferredReplyRejectedReason, IngressSendError, RuntimeEventKind, SelectedCallOutcome,
    SharedWork, SharedWorkError, SleepReply, call_cancelable_request, call_request,
    request_effect_after_shared_wait, sleep,
};
use tina_sim::{Simulator, SimulatorConfig};

const CALL_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuoteRaceReport {
    pub replies: Vec<QuoteReply>,
    pub cancel_outcomes: Vec<CancelOutcome>,
    pub late_cancelled_rejections: usize,
    pub rough_edges: Vec<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuoteReply {
    Quote { provider: &'static str, cents: u32 },
    Unavailable,
    Busy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProviderQuote {
    provider: &'static str,
    cents: u32,
    available: bool,
}

#[derive(Debug)]
enum ProviderEvent {
    Done(RequestContext<ProviderQuote>, SleepReply),
}

#[derive(Debug)]
enum ProviderRequest {
    Quote,
}

#[derive(Debug)]
struct Provider {
    quote: ProviderQuote,
    delay: Duration,
}

#[tina_runtime::isolate(event = ProviderEvent, request = ProviderRequest, reply = ProviderQuote)]
impl Provider {
    fn handle_event(
        &mut self,
        event: ProviderEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            ProviderEvent::Done(req, Ok(())) => reply_to(req, self.quote),
            ProviderEvent::Done(_, Err(_)) => noop(),
        }
    }

    fn handle_request(
        &mut self,
        request: ProviderRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            ProviderRequest::Quote => call
                .defer(sleep(self.delay))
                .reply_service_event(ProviderEvent::Done),
        }
    }
}

#[derive(Debug)]
enum QuoteEvent {
    Race(CallSelectEvent<u32, ProviderQuote>),
}

#[derive(Debug)]
enum QuoteRequest {
    GetQuote,
}

#[derive(Debug)]
struct PendingQuote {
    request: Option<RequestContext<QuoteReply>>,
    set: CallSelectSet<u32, ProviderQuote>,
}

#[derive(Debug)]
struct QuoteGateway {
    providers: [ServiceRequestAddress<ProviderEvent, ProviderRequest, ProviderQuote>; 2],
    pending: Option<PendingQuote>,
    cancel_outcomes: Rc<RefCell<Vec<CancelOutcome>>>,
}

impl QuoteGateway {
    fn start_race(&mut self, request: RequestContext<QuoteReply>) -> Effect<Self> {
        let mut set = CallSelectSet::with_capacity(self.providers.len());
        let mut effects = Vec::with_capacity(self.providers.len());

        for (idx, provider) in self.providers.iter().copied().enumerate() {
            let key = idx as u32;
            let effect = set
                .start_service(
                    key,
                    call_cancelable_request(provider, ProviderRequest::Quote, CALL_TIMEOUT),
                    QuoteEvent::Race,
                )
                .expect("fresh set accepts each provider");
            effects.push(effect);
        }

        self.pending = Some(PendingQuote {
            request: Some(request),
            set,
        });
        Effect::Batch(effects)
    }
}

#[tina_runtime::isolate(event = QuoteEvent, request = QuoteRequest, reply = QuoteReply)]
impl QuoteGateway {
    fn handle_event(
        &mut self,
        event: QuoteEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            QuoteEvent::Race(event) => {
                let Some(pending) = self.pending.as_mut() else {
                    return noop();
                };
                let step = pending
                    .set
                    .advance_service(event, |quote| quote.available, QuoteEvent::Race)
                    .expect("race continuation carries a live CallSelectSet token");
                let winner = match &step.selected.outcome {
                    SelectedCallOutcome::Reply(CallOutcome::Replied(q))
                        if step.classified_success =>
                    {
                        Some(QuoteReply::Quote {
                            provider: q.provider,
                            cents: q.cents,
                        })
                    }
                    _ => None,
                };
                if let SelectedCallOutcome::Cancel(outcome) = step.selected.outcome {
                    self.cancel_outcomes.borrow_mut().push(outcome);
                }

                let mut reply = None;
                if let Some(winner_reply) = winner {
                    if let Some(request) = pending.request.take() {
                        reply = Some(reply_to(request, winner_reply));
                    }
                } else if step.complete && pending.request.is_some() {
                    if let Some(request) = pending.request.take() {
                        reply = Some(reply_to(request, QuoteReply::Unavailable));
                    }
                }
                if step.complete {
                    self.pending = None;
                }
                match reply {
                    Some(reply) => Effect::Batch(vec![reply, step.effect]),
                    None => step.effect,
                }
            }
        }
    }

    fn handle_request(
        &mut self,
        request: QuoteRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            QuoteRequest::GetQuote => {
                if self.pending.is_some() {
                    call.reply(QuoteReply::Busy)
                } else {
                    call.capture(|request| self.start_race(request))
                }
            }
        }
    }
}

#[derive(Debug)]
enum QuoteClientMsg {
    Begin(ServiceRequestAddress<QuoteEvent, QuoteRequest, QuoteReply>),
    Returned(CallOutcome<QuoteReply>),
}

#[derive(Debug)]
struct QuoteClient {
    replies: Rc<RefCell<Vec<QuoteReply>>>,
}

#[tina_runtime::isolate(message = QuoteClientMsg)]
impl QuoteClient {
    fn handle(
        &mut self,
        msg: QuoteClientMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            QuoteClientMsg::Begin(gateway) => {
                call_request(gateway, QuoteRequest::GetQuote, CALL_TIMEOUT)
                    .then(QuoteClientMsg::Returned)
            }
            QuoteClientMsg::Returned(CallOutcome::Replied(reply)) => {
                self.replies.borrow_mut().push(reply);
                noop()
            }
            QuoteClientMsg::Returned(_) => noop(),
        }
    }
}

pub fn run_quote_race_probe() -> anyhow::Result<QuoteRaceReport> {
    run_quote_race_with([
        ProviderQuote {
            provider: "slow",
            cents: 475,
            available: true,
        },
        ProviderQuote {
            provider: "fast",
            cents: 525,
            available: true,
        },
    ])
}

fn run_quote_race_with(quotes: [ProviderQuote; 2]) -> anyhow::Result<QuoteRaceReport> {
    let mut sim = Simulator::new(SingleShard, SimulatorConfig::default());
    let slow = sim
        .register_split_service::<Provider, ProviderEvent, ProviderRequest, std::convert::Infallible>(
            Provider {
                quote: quotes[0],
                delay: Duration::from_millis(30),
            },
            8,
        )
        .requests;
    let fast = sim
        .register_split_service::<Provider, ProviderEvent, ProviderRequest, std::convert::Infallible>(
            Provider {
                quote: quotes[1],
                delay: Duration::from_millis(5),
            },
            8,
        )
        .requests;

    let cancel_outcomes = Rc::new(RefCell::new(Vec::new()));
    let gateway = sim
        .register_split_service::<QuoteGateway, QuoteEvent, QuoteRequest, std::convert::Infallible>(
            QuoteGateway {
                providers: [slow, fast],
                pending: None,
                cancel_outcomes: Rc::clone(&cancel_outcomes),
            },
            8,
        )
        .requests;
    let replies = Rc::new(RefCell::new(Vec::new()));
    let client = sim.register(QuoteClient {
        replies: Rc::clone(&replies),
    });

    sim.try_send(client, QuoteClientMsg::Begin(gateway))
        .map_err(|error| match error {
            IngressSendError::Full(_) => anyhow::anyhow!("quote client mailbox full"),
            IngressSendError::Closed(_) => anyhow::anyhow!("quote client closed"),
            IngressSendError::ForeignSystem { .. } => {
                anyhow::anyhow!("quote client belongs to another simulator")
            }
        })?;
    sim.run_until_quiescent();

    let late_cancelled_rejections = sim
        .trace()
        .iter()
        .filter(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::DeferredReplyRejected {
                    reason: DeferredReplyRejectedReason::CallerCancelled,
                    ..
                } | RuntimeEventKind::CallReplyRejected {
                    reason: CallReplyRejectedReason::CallerCancelled,
                    ..
                }
            )
        })
        .count();

    Ok(QuoteRaceReport {
        replies: replies.borrow().clone(),
        cancel_outcomes: cancel_outcomes.borrow().clone(),
        late_cancelled_rejections,
        rough_edges: Vec::new(),
    })
}

pub fn run_quote_race_no_winner_probe() -> anyhow::Result<QuoteRaceReport> {
    run_quote_race_with([
        ProviderQuote {
            provider: "slow",
            cents: 475,
            available: false,
        },
        ProviderQuote {
            provider: "fast",
            cents: 525,
            available: false,
        },
    ])
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebouncedBatchReport {
    pub admitted: usize,
    pub full: usize,
    pub closed: usize,
    pub timer_failed: usize,
    pub call_full: usize,
    pub call_closed: usize,
    pub call_timeout: usize,
    pub call_rejected: usize,
    pub batch_ids: Vec<u64>,
    pub batch_sizes: Vec<usize>,
    pub sums: Vec<u64>,
    pub rough_edges: Vec<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchReply {
    Batched {
        batch_id: u64,
        size: usize,
        sum: u64,
    },
    Full,
    Closed,
    TimerFailed(CallError),
}

enum BatcherEvent {
    Drain,
    Flow(BatcherFlow),
}

#[derive(Debug)]
enum BatcherRequest {
    Submit(u64),
}

#[derive(Debug)]
struct Batcher {
    waiters: SharedWork<u64, BatchReply>,
    active: Option<ActiveBatch>,
    next_batch: u64,
    closed: bool,
    window: Duration,
}

#[derive(Debug)]
struct ActiveBatch {
    id: u64,
    values: Vec<u64>,
}

tina::flow! {
    flow BatcherFlow for Batcher {
        reply BatchReply;

        step Flush(batch_id: u64) -> raw SleepReply {
            self.flush(batch_id, outcome)
        }
    }
}

#[tina_runtime::isolate(event = BatcherEvent, request = BatcherRequest, reply = BatchReply)]
impl Batcher {
    fn handle_event(
        &mut self,
        event: BatcherEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            BatcherEvent::Drain => {
                self.closed = true;
                self.active = None;
                Effect::Batch(self.waiters.drain_all_with(|| BatchReply::Closed))
            }
            BatcherEvent::Flow(flow) => self.handle_batcher_flow(flow),
        }
    }

    fn handle_request(
        &mut self,
        request: BatcherRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            BatcherRequest::Submit(value) => {
                if self.closed {
                    return call.reply(BatchReply::Closed);
                }
                // SharedWork bounds live caller authority. The values are the
                // actual operations, so keep their producer bounded even when
                // an earlier caller times out and its waiter slot is reclaimed.
                if self
                    .active
                    .as_ref()
                    .is_some_and(|batch| batch.values.len() >= self.waiters.capacity())
                {
                    return call.reply(BatchReply::Full);
                }
                let batch_id = self
                    .active
                    .as_ref()
                    .map_or(self.next_batch, |batch| batch.id);
                match self.waiters.wait(batch_id, call) {
                    Ok((_ticket, permit)) => {
                        let should_arm = self.active.is_none();
                        let batch = self.active.get_or_insert_with(|| {
                            self.next_batch += 1;
                            ActiveBatch {
                                id: batch_id,
                                values: Vec::new(),
                            }
                        });
                        batch.values.push(value);
                        let effect = if should_arm {
                            sleep(self.window).then_service_event(move |outcome| {
                                BatcherEvent::Flow(BatcherFlow::Flush(batch_id, outcome))
                            })
                        } else {
                            noop()
                        };
                        request_effect_after_shared_wait(permit, effect)
                    }
                    Err(SharedWorkError::Full { call, .. })
                    | Err(SharedWorkError::KeyFull { call, .. }) => call.reply(BatchReply::Full),
                }
            }
        }
    }
}

impl Batcher {
    fn flush(&mut self, batch_id: u64, outcome: SleepReply) -> Effect<Self> {
        let Some(batch) = self.active.take() else {
            return noop();
        };
        if batch.id != batch_id {
            self.active = Some(batch);
            return noop();
        }

        let reply = match outcome {
            Ok(()) => BatchReply::Batched {
                batch_id,
                size: batch.values.len(),
                sum: batch.values.iter().sum(),
            },
            Err(error) => BatchReply::TimerFailed(error),
        };
        Effect::Batch(self.waiters.reply_all_clone(&batch_id, reply))
    }
}

enum BatchClientMsg {
    Begin(tina_runtime::SplitServiceHandle<BatcherEvent, BatcherRequest, BatchReply>),
    Returned(CallOutcome<BatchReply>),
}

#[derive(Debug)]
struct BatchClient {
    values: Vec<u64>,
    outcomes: Rc<RefCell<Vec<CallOutcome<BatchReply>>>>,
    drain_after_submit: bool,
}

#[tina_runtime::isolate(
    message = BatchClientMsg,
    send = tina::ServiceOutbound<BatcherEvent, BatcherRequest>
)]
impl BatchClient {
    fn handle(
        &mut self,
        msg: BatchClientMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            BatchClientMsg::Begin(batcher) => {
                let mut calls: Vec<_> = self
                    .values
                    .iter()
                    .copied()
                    .map(|value| {
                        call_request(
                            batcher.requests,
                            BatcherRequest::Submit(value),
                            CALL_TIMEOUT,
                        )
                        .then(BatchClientMsg::Returned)
                    })
                    .collect();
                if self.drain_after_submit {
                    calls.push(send_event(batcher.events, BatcherEvent::Drain));
                }
                Effect::Batch(calls)
            }
            BatchClientMsg::Returned(outcome) => {
                self.outcomes.borrow_mut().push(outcome);
                noop()
            }
        }
    }
}

pub fn run_debounced_batch_probe() -> anyhow::Result<DebouncedBatchReport> {
    let mut sim = Simulator::new(SingleShard, SimulatorConfig::default());
    let batcher = sim
        .register_split_service::<Batcher, BatcherEvent, BatcherRequest, std::convert::Infallible>(
            Batcher {
                waiters: SharedWork::with_capacity(3).named("ergonomics.batch.waiters"),
                active: None,
                next_batch: 1,
                closed: false,
                window: Duration::from_millis(10),
            },
            8,
        );
    let outcomes = Rc::new(RefCell::new(Vec::new()));
    let client = sim.register(BatchClient {
        values: vec![2, 3, 5, 7, 11],
        outcomes: Rc::clone(&outcomes),
        drain_after_submit: false,
    });

    sim.try_send(client, BatchClientMsg::Begin(batcher))
        .map_err(|error| match error {
            IngressSendError::Full(_) => anyhow::anyhow!("batch client mailbox full"),
            IngressSendError::Closed(_) => anyhow::anyhow!("batch client closed"),
            IngressSendError::ForeignSystem { .. } => {
                anyhow::anyhow!("batch client belongs to another simulator")
            }
        })?;
    sim.run_until_quiescent();

    Ok(classify_batch_outcomes(&outcomes.borrow()))
}

fn classify_batch_outcomes(outcomes: &[CallOutcome<BatchReply>]) -> DebouncedBatchReport {
    let mut admitted = 0;
    let mut full = 0;
    let mut closed = 0;
    let mut timer_failed = 0;
    let mut call_full = 0;
    let mut call_closed = 0;
    let mut call_timeout = 0;
    let mut call_rejected = 0;
    let mut batch_ids = Vec::new();
    let mut batch_sizes = Vec::new();
    let mut sums = Vec::new();
    for outcome in outcomes.iter() {
        match outcome {
            CallOutcome::Replied(BatchReply::Batched {
                batch_id,
                size,
                sum,
            }) => {
                admitted += 1;
                batch_ids.push(*batch_id);
                batch_sizes.push(*size);
                sums.push(*sum);
            }
            CallOutcome::Replied(BatchReply::Full) => full += 1,
            CallOutcome::Replied(BatchReply::Closed) => closed += 1,
            CallOutcome::Replied(BatchReply::TimerFailed(_)) => timer_failed += 1,
            CallOutcome::Full => call_full += 1,
            CallOutcome::Closed => call_closed += 1,
            CallOutcome::Timeout => call_timeout += 1,
            CallOutcome::Rejected(_) => call_rejected += 1,
        }
    }

    DebouncedBatchReport {
        admitted,
        full,
        closed,
        timer_failed,
        call_full,
        call_closed,
        call_timeout,
        call_rejected,
        batch_ids,
        batch_sizes,
        sums,
        rough_edges: Vec::new(),
    }
}

pub fn run_debounced_batch_drain_probe() -> anyhow::Result<DebouncedBatchReport> {
    let mut sim = Simulator::new(SingleShard, SimulatorConfig::default());
    let batcher = sim
        .register_split_service::<Batcher, BatcherEvent, BatcherRequest, std::convert::Infallible>(
            Batcher {
                waiters: SharedWork::with_capacity(4).named("ergonomics.batch.drain.waiters"),
                active: None,
                next_batch: 1,
                closed: false,
                window: Duration::from_millis(50),
            },
            8,
        );
    let outcomes = Rc::new(RefCell::new(Vec::new()));
    let client = sim.register(BatchClient {
        values: vec![2, 3, 5],
        outcomes: Rc::clone(&outcomes),
        drain_after_submit: true,
    });

    sim.try_send(client, BatchClientMsg::Begin(batcher))
        .map_err(|error| match error {
            IngressSendError::Full(_) => anyhow::anyhow!("drain batch client mailbox full"),
            IngressSendError::Closed(_) => anyhow::anyhow!("drain batch client closed"),
            IngressSendError::ForeignSystem { .. } => {
                anyhow::anyhow!("drain batch client belongs to another simulator")
            }
        })?;
    sim.run_until_quiescent();

    Ok(classify_batch_outcomes(&outcomes.borrow()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheFillReport {
    pub callers: usize,
    pub hits: usize,
    pub full: usize,
    pub upstream_calls: usize,
    pub values: Vec<u64>,
    pub rough_edges: Vec<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheReply {
    Hit(u64),
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FillReply(u64);

#[derive(Debug)]
enum UpstreamEvent {
    Done(RequestContext<FillReply>, SleepReply),
}

#[derive(Debug)]
enum UpstreamRequest {
    Fetch,
}

#[derive(Debug)]
struct Upstream {
    value: u64,
    delay: Duration,
    calls: Rc<RefCell<usize>>,
}

#[tina_runtime::isolate(event = UpstreamEvent, request = UpstreamRequest, reply = FillReply)]
impl Upstream {
    fn handle_event(
        &mut self,
        event: UpstreamEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            UpstreamEvent::Done(req, Ok(())) => reply_to(req, FillReply(self.value)),
            UpstreamEvent::Done(_, Err(_)) => noop(),
        }
    }

    fn handle_request(
        &mut self,
        request: UpstreamRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            UpstreamRequest::Fetch => {
                *self.calls.borrow_mut() += 1;
                call.defer(sleep(self.delay))
                    .reply_service_event(UpstreamEvent::Done)
            }
        }
    }
}

#[derive(Debug)]
enum CacheEvent {
    FillReturned(CallOutcome<FillReply>),
}

#[derive(Debug)]
enum CacheRequest {
    Get(&'static str),
}

#[derive(Debug)]
struct Cache {
    key: &'static str,
    cached: Option<u64>,
    filling: bool,
    waiters: SharedWork<&'static str, CacheReply>,
    upstream: ServiceRequestAddress<UpstreamEvent, UpstreamRequest, FillReply>,
}

#[tina_runtime::isolate(event = CacheEvent, request = CacheRequest, reply = CacheReply)]
impl Cache {
    fn handle_event(
        &mut self,
        event: CacheEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            CacheEvent::FillReturned(CallOutcome::Replied(FillReply(value))) => {
                self.filling = false;
                self.cached = Some(value);
                Effect::Batch(
                    self.waiters
                        .reply_all_with::<Self, _>(&self.key, || CacheReply::Hit(value)),
                )
            }
            CacheEvent::FillReturned(_) => {
                self.filling = false;
                Effect::Batch(
                    self.waiters
                        .close_all_clone::<Self>(&self.key, CacheReply::Full),
                )
            }
        }
    }

    fn handle_request(
        &mut self,
        request: CacheRequest,
        call_ctx: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            CacheRequest::Get(key) if key != self.key => call_ctx.reply(CacheReply::Full),
            CacheRequest::Get(_) => {
                if let Some(value) = self.cached {
                    return call_ctx.reply(CacheReply::Hit(value));
                }
                match self.waiters.wait(self.key, call_ctx) {
                    Ok((_ticket, permit)) => {
                        if self.filling {
                            request_effect_after_shared_wait(permit, noop())
                        } else {
                            self.filling = true;
                            let effect =
                                call_request(self.upstream, UpstreamRequest::Fetch, CALL_TIMEOUT)
                                    .then_service_event(CacheEvent::FillReturned);
                            request_effect_after_shared_wait(permit, effect)
                        }
                    }
                    Err(SharedWorkError::Full { call, .. })
                    | Err(SharedWorkError::KeyFull { call, .. }) => call.reply(CacheReply::Full),
                }
            }
        }
    }
}

#[derive(Debug)]
enum CacheClientMsg {
    Begin(ServiceRequestAddress<CacheEvent, CacheRequest, CacheReply>),
    Returned(CallOutcome<CacheReply>),
}

#[derive(Debug)]
struct CacheClient {
    callers: usize,
    replies: Rc<RefCell<Vec<CacheReply>>>,
}

#[tina_runtime::isolate(message = CacheClientMsg)]
impl CacheClient {
    fn handle(
        &mut self,
        msg: CacheClientMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            CacheClientMsg::Begin(cache) => {
                let calls = (0..self.callers)
                    .map(|_| {
                        call_request(cache, CacheRequest::Get("price:alpaca"), CALL_TIMEOUT)
                            .then(CacheClientMsg::Returned)
                    })
                    .collect();
                Effect::Batch(calls)
            }
            CacheClientMsg::Returned(CallOutcome::Replied(reply)) => {
                self.replies.borrow_mut().push(reply);
                noop()
            }
            CacheClientMsg::Returned(_) => noop(),
        }
    }
}

pub fn run_single_flight_cache_probe() -> anyhow::Result<CacheFillReport> {
    let mut sim = Simulator::new(SingleShard, SimulatorConfig::default());
    let upstream_calls = Rc::new(RefCell::new(0));
    let upstream = sim
        .register_split_service::<Upstream, UpstreamEvent, UpstreamRequest, std::convert::Infallible>(
            Upstream {
                value: 42,
                delay: Duration::from_millis(10),
                calls: Rc::clone(&upstream_calls),
            },
            8,
        )
        .requests;
    let cache = sim
        .register_split_service::<Cache, CacheEvent, CacheRequest, std::convert::Infallible>(
            Cache {
                key: "price:alpaca",
                cached: None,
                filling: false,
                waiters: SharedWork::with_capacity(3).named("ergonomics.cache.waiters"),
                upstream,
            },
            8,
        )
        .requests;
    let replies = Rc::new(RefCell::new(Vec::new()));
    let client = sim.register(CacheClient {
        callers: 5,
        replies: Rc::clone(&replies),
    });

    sim.try_send(client, CacheClientMsg::Begin(cache))
        .map_err(|error| match error {
            IngressSendError::Full(_) => anyhow::anyhow!("cache client mailbox full"),
            IngressSendError::Closed(_) => anyhow::anyhow!("cache client closed"),
            IngressSendError::ForeignSystem { .. } => {
                anyhow::anyhow!("cache client belongs to another simulator")
            }
        })?;
    sim.run_until_quiescent();

    let replies = replies.borrow();
    let mut hits = 0;
    let mut full = 0;
    let mut values = Vec::new();
    for reply in replies.iter().copied() {
        match reply {
            CacheReply::Hit(value) => {
                hits += 1;
                values.push(value);
            }
            CacheReply::Full => full += 1,
        }
    }

    Ok(CacheFillReport {
        callers: 5,
        hits,
        full,
        upstream_calls: *upstream_calls.borrow(),
        values,
        rough_edges: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;

    use tina_runtime::{CallKind, DefaultThreadedMailboxFactory, LocalSystem, LocalSystemConfig};

    #[derive(Debug)]
    enum TimerHolderMsg {
        Start,
        Done(SleepReply),
    }

    struct TimerHolder;

    #[tina_runtime::isolate(message = TimerHolderMsg)]
    impl TimerHolder {
        fn handle(
            &mut self,
            message: TimerHolderMsg,
            _ctx: &mut Context<'_, SingleShard>,
        ) -> Effect<Self> {
            match message {
                TimerHolderMsg::Start => sleep(Duration::from_secs(5)).then(TimerHolderMsg::Done),
                TimerHolderMsg::Done(_outcome) => noop(),
            }
        }
    }

    enum TimeoutBatchClientMsg {
        Begin(ServiceRequestAddress<BatcherEvent, BatcherRequest, BatchReply>),
        FirstReturned(CallOutcome<BatchReply>),
        SubmitSecond,
        SecondReturned(CallOutcome<BatchReply>),
        SubmitThird,
        ThirdReturned(CallOutcome<BatchReply>),
    }

    struct TimeoutBatchClient {
        batcher: Option<ServiceRequestAddress<BatcherEvent, BatcherRequest, BatchReply>>,
        outcomes: Rc<RefCell<Vec<CallOutcome<BatchReply>>>>,
    }

    #[tina_runtime::isolate(message = TimeoutBatchClientMsg)]
    impl TimeoutBatchClient {
        fn handle(
            &mut self,
            message: TimeoutBatchClientMsg,
            _ctx: &mut Context<'_, SingleShard, Self::Reply>,
        ) -> Effect<Self> {
            match message {
                TimeoutBatchClientMsg::Begin(batcher) => {
                    self.batcher = Some(batcher);
                    Effect::Batch(vec![
                        call_request(batcher, BatcherRequest::Submit(2), Duration::from_millis(1))
                            .then(TimeoutBatchClientMsg::FirstReturned),
                        sleep(Duration::from_millis(2))
                            .then(|_| TimeoutBatchClientMsg::SubmitSecond),
                        sleep(Duration::from_millis(12))
                            .then(|_| TimeoutBatchClientMsg::SubmitThird),
                    ])
                }
                TimeoutBatchClientMsg::SubmitSecond => call_request(
                    self.batcher.expect("begin stores batcher"),
                    BatcherRequest::Submit(3),
                    CALL_TIMEOUT,
                )
                .then(TimeoutBatchClientMsg::SecondReturned),
                TimeoutBatchClientMsg::SubmitThird => call_request(
                    self.batcher.expect("begin stores batcher"),
                    BatcherRequest::Submit(5),
                    CALL_TIMEOUT,
                )
                .then(TimeoutBatchClientMsg::ThirdReturned),
                TimeoutBatchClientMsg::FirstReturned(outcome)
                | TimeoutBatchClientMsg::SecondReturned(outcome)
                | TimeoutBatchClientMsg::ThirdReturned(outcome) => {
                    self.outcomes.borrow_mut().push(outcome);
                    noop()
                }
            }
        }
    }

    #[test]
    fn quote_race_accepts_fast_provider_and_cancels_loser() {
        let report = run_quote_race_probe().unwrap();
        assert_eq!(
            report.replies,
            vec![QuoteReply::Quote {
                provider: "fast",
                cents: 525,
            }]
        );
        assert_eq!(report.cancel_outcomes, vec![CancelOutcome::Cancelled]);
        assert_eq!(report.late_cancelled_rejections, 1);
        assert!(report.rough_edges.is_empty());
    }

    #[test]
    fn quote_race_no_provider_available_replies_unavailable() {
        let report = run_quote_race_no_winner_probe().unwrap();
        assert_eq!(report.replies, vec![QuoteReply::Unavailable]);
        assert!(report.cancel_outcomes.is_empty());
        assert_eq!(report.late_cancelled_rejections, 0);
        assert!(report.rough_edges.is_empty());
    }

    #[test]
    fn debounced_batch_replies_to_admitted_callers_and_rejects_excess() {
        let report = run_debounced_batch_probe().unwrap();
        assert_eq!(report.admitted, 3);
        assert_eq!(report.full, 2);
        assert_eq!(report.closed, 0);
        assert_eq!(report.timer_failed, 0);
        assert_eq!(report.call_full, 0);
        assert_eq!(report.call_closed, 0);
        assert_eq!(report.call_timeout, 0);
        assert_eq!(report.call_rejected, 0);
        assert_eq!(report.batch_ids, vec![1, 1, 1]);
        assert_eq!(report.batch_sizes, vec![3, 3, 3]);
        assert_eq!(report.sums, vec![10, 10, 10]);
        assert!(report.rough_edges.is_empty());
    }

    #[test]
    fn debounced_batch_drain_replies_closed_to_pending_callers() {
        let report = run_debounced_batch_drain_probe().unwrap();
        assert_eq!(report.admitted, 0);
        assert_eq!(report.full, 0);
        assert_eq!(report.closed, 3);
        assert_eq!(report.timer_failed, 0);
        assert_eq!(report.call_full, 0);
        assert_eq!(report.call_closed, 0);
        assert_eq!(report.call_timeout, 0);
        assert_eq!(report.call_rejected, 0);
        assert!(report.batch_ids.is_empty());
        assert!(report.rough_edges.is_empty());
    }

    #[test]
    fn stale_batch_timer_cannot_complete_the_active_batch() {
        let mut batcher = Batcher {
            waiters: SharedWork::with_capacity(2),
            active: Some(ActiveBatch {
                id: 2,
                values: vec![7, 11],
            }),
            next_batch: 3,
            closed: false,
            window: Duration::from_millis(10),
        };

        let _ = batcher.flush(1, Ok(()));
        let active = batcher.active.expect("newer batch must remain active");
        assert_eq!(active.id, 2);
        assert_eq!(active.values, vec![7, 11]);
    }

    #[test]
    fn batch_accounting_keeps_application_and_call_terminals_distinct() {
        let report = classify_batch_outcomes(&[
            CallOutcome::Replied(BatchReply::Batched {
                batch_id: 1,
                size: 2,
                sum: 13,
            }),
            CallOutcome::Replied(BatchReply::Full),
            CallOutcome::Replied(BatchReply::Closed),
            CallOutcome::Replied(BatchReply::TimerFailed(CallError::TimerFull)),
            CallOutcome::Full,
            CallOutcome::Closed,
            CallOutcome::Timeout,
            CallOutcome::Rejected(tina::CallRejectedReason::UnsupportedMessage),
        ]);

        assert_eq!(report.admitted, 1);
        assert_eq!(report.full, 1);
        assert_eq!(report.closed, 1);
        assert_eq!(report.timer_failed, 1);
        assert_eq!(report.call_full, 1);
        assert_eq!(report.call_closed, 1);
        assert_eq!(report.call_timeout, 1);
        assert_eq!(report.call_rejected, 1);
        assert_eq!(report.batch_ids, vec![1]);
        assert_eq!(report.batch_sizes, vec![2]);
        assert_eq!(report.sums, vec![13]);
    }

    #[test]
    fn live_timer_pressure_is_a_timer_failure_not_application_full() {
        let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory)
            .config(LocalSystemConfig {
                timer_capacity: 1,
                ..LocalSystemConfig::default()
            })
            .try_build()
            .expect("start one-timer system");
        let holder = app
            .register_root::<TimerHolder, Infallible>(TimerHolder, 4)
            .expect("register timer holder");
        app.try_send(holder, TimerHolderMsg::Start)
            .expect("start held timer");

        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            let trace = app.complete_trace().expect("read timer-holder trace");
            if trace.iter().any(|event| {
                matches!(
                    event.kind(),
                    RuntimeEventKind::CallDispatchAttempted {
                        call_kind: CallKind::Sleep,
                        ..
                    }
                )
            }) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timer holder did not arm"
            );
            std::thread::sleep(Duration::from_millis(1));
        }

        let batcher = app
            .register_split_service::<Batcher, BatcherEvent, BatcherRequest, Infallible>(
                Batcher {
                    waiters: SharedWork::with_capacity(1),
                    active: None,
                    next_batch: 1,
                    closed: false,
                    window: Duration::from_millis(10),
                },
                4,
            )
            .expect("register timer-pressure batcher");
        assert_eq!(
            app.call_blocking_request(
                batcher.requests,
                BatcherRequest::Submit(7),
                Duration::from_secs(1),
            )
            .expect("call timer-pressure batcher"),
            CallOutcome::Replied(BatchReply::TimerFailed(CallError::TimerFull)),
        );

        app.shutdown()
            .drain()
            .join_report()
            .ensure_clean()
            .expect("clean timer-pressure shutdown");
    }

    #[test]
    fn debounced_batch_refills_on_the_same_service() {
        let mut sim = Simulator::new(SingleShard, SimulatorConfig::default());
        let batcher = sim.register_split_service::<
            Batcher,
            BatcherEvent,
            BatcherRequest,
            std::convert::Infallible,
        >(
            Batcher {
                waiters: SharedWork::with_capacity(2),
                active: None,
                next_batch: 1,
                closed: false,
                window: Duration::from_millis(10),
            },
            8,
        );
        let outcomes = Rc::new(RefCell::new(Vec::new()));
        let client = sim.register(BatchClient {
            values: vec![2, 3],
            outcomes: Rc::clone(&outcomes),
            drain_after_submit: false,
        });

        for _ in 0..2 {
            match sim.try_send(client, BatchClientMsg::Begin(batcher)) {
                Ok(()) => {}
                Err(IngressSendError::Full(_)) => {
                    panic!("batch client mailbox unexpectedly full")
                }
                Err(IngressSendError::Closed(_)) => panic!("batch client unexpectedly closed"),
                Err(IngressSendError::ForeignSystem { .. }) => {
                    panic!("batch client unexpectedly belongs to another simulator")
                }
            }
            sim.run_until_quiescent();
        }

        let report = classify_batch_outcomes(&outcomes.borrow());
        assert_eq!(report.admitted, 4);
        assert_eq!(report.full, 0);
        assert_eq!(report.batch_ids, vec![1, 1, 2, 2]);
        assert_eq!(report.batch_sizes, vec![2, 2, 2, 2]);
        assert_eq!(report.sums, vec![5, 5, 5, 5]);
    }

    #[test]
    fn timed_out_caller_cannot_make_the_operation_batch_exceed_its_cap() {
        let mut sim = Simulator::new(SingleShard, SimulatorConfig::default());
        let batcher = sim.register_split_service::<
            Batcher,
            BatcherEvent,
            BatcherRequest,
            std::convert::Infallible,
        >(
            Batcher {
                waiters: SharedWork::with_capacity(1),
                active: None,
                next_batch: 1,
                closed: false,
                window: Duration::from_millis(10),
            },
            8,
        );
        let outcomes = Rc::new(RefCell::new(Vec::new()));
        let client = sim.register(TimeoutBatchClient {
            batcher: None,
            outcomes: Rc::clone(&outcomes),
        });

        match sim.try_send(client, TimeoutBatchClientMsg::Begin(batcher.requests)) {
            Ok(()) => {}
            Err(IngressSendError::Full(_)) => panic!("timeout client mailbox unexpectedly full"),
            Err(IngressSendError::Closed(_)) => panic!("timeout client unexpectedly closed"),
            Err(IngressSendError::ForeignSystem { .. }) => {
                panic!("timeout client unexpectedly belongs to another simulator")
            }
        }
        sim.run_until_quiescent();

        let report = classify_batch_outcomes(&outcomes.borrow());
        assert_eq!(report.call_timeout, 1);
        assert_eq!(report.full, 1);
        assert_eq!(report.admitted, 1);
        assert_eq!(report.batch_ids, vec![2]);
        assert_eq!(report.batch_sizes, vec![1]);
        assert_eq!(report.sums, vec![5]);
        assert_eq!(
            report.admitted
                + report.full
                + report.closed
                + report.timer_failed
                + report.call_full
                + report.call_closed
                + report.call_timeout
                + report.call_rejected,
            3,
            "every submitted request has exactly one terminal outcome",
        );
        assert_eq!(
            sim.trace()
                .iter()
                .filter(|event| matches!(
                    event.kind(),
                    RuntimeEventKind::DeferredReplyRejected {
                        reason: DeferredReplyRejectedReason::CallerTimedOut,
                        ..
                    }
                ))
                .count(),
            1,
            "the late first-batch reply observes the timed-out caller once",
        );
    }

    #[test]
    fn single_flight_cache_coalesces_waiters_and_reclaims_capacity() {
        let report = run_single_flight_cache_probe().unwrap();
        assert_eq!(report.callers, 5);
        assert_eq!(report.hits, 3);
        assert_eq!(report.full, 2);
        assert_eq!(report.upstream_calls, 1);
        assert_eq!(report.values, vec![42, 42, 42]);
        assert!(report.rough_edges.is_empty());
    }
}
