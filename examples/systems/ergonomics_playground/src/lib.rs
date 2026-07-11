use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use tina::{
    CancelOutcome, Context, Effect, RequestContext, ServiceRequestAddress, SingleShard, noop,
    reply_to, send_event,
};
use tina_runtime::{
    CallGroupToken, CallOutcome, CallReplyRejectedReason, CallSelectSet,
    DeferredReplyRejectedReason, PendingReplies, RuntimeEventKind, SharedWork,
    SharedWorkError, SleepReply, call_cancelable_request, call_request, cancel_call,
    request_effect_after_park, request_effect_after_shared_wait, sleep,
};
use tina_sim::{Simulator, SimulatorConfig};

const CALL_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuoteRaceReport {
    pub replies: Vec<QuoteReply>,
    pub cancel_outcomes: usize,
    pub late_cancelled_rejections: usize,
    pub completed: bool,
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
    ProviderReturned {
        key: u32,
        token: CallGroupToken,
        outcome: CallOutcome<ProviderQuote>,
    },
    Cancelled {
        key: u32,
        token: CallGroupToken,
        outcome: CancelOutcome,
    },
}

#[derive(Debug)]
enum QuoteRequest {
    GetQuote,
}

#[derive(Debug)]
struct PendingQuote {
    request: Option<RequestContext<QuoteReply>>,
    // `CallSelectSet` + `record_classified_reply(.., |q| q.available)`
    // gives the same first-success-wins-and-cancels-losers race
    // `CallGroup` provided, using the `q.available` business classifier
    // instead of "first reply wins".
    set: CallSelectSet<u32, ProviderQuote>,
}

#[derive(Debug)]
struct QuoteGateway {
    providers: [ServiceRequestAddress<ProviderEvent, ProviderRequest, ProviderQuote>; 2],
    pending: Option<PendingQuote>,
    cancel_outcomes: Rc<RefCell<usize>>,
}

impl QuoteGateway {
    fn start_race(&mut self, request: RequestContext<QuoteReply>) -> Effect<Self> {
        let mut set = CallSelectSet::with_capacity(self.providers.len());
        let mut effects = Vec::with_capacity(self.providers.len());

        for (idx, provider) in self.providers.iter().copied().enumerate() {
            let key = idx as u32;
            let effect = set
                .start_cancelable_service_event(
                    key,
                    call_cancelable_request(provider, ProviderRequest::Quote, CALL_TIMEOUT),
                    |key, token, outcome| QuoteEvent::ProviderReturned {
                        key,
                        token,
                        outcome,
                    },
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

    fn cancel_effects(
        requests: Vec<tina_runtime::CallSetCancelRequest<u32, ProviderQuote>>,
    ) -> Vec<Effect<Self>> {
        requests
            .into_iter()
            .map(|request| {
                let (key, token, handle) = request.into_parts();
                cancel_call(handle).then_service_event(move |outcome| QuoteEvent::Cancelled {
                    key,
                    token,
                    outcome,
                })
            })
            .collect()
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
            QuoteEvent::ProviderReturned {
                key,
                token,
                outcome,
            } => {
                let Some(pending) = self.pending.as_mut() else {
                    return noop();
                };
                let winner = match &outcome {
                    CallOutcome::Replied(q) if q.available => Some(QuoteReply::Quote {
                        provider: q.provider,
                        cents: q.cents,
                    }),
                    _ => None,
                };
                let step = pending
                    .set
                    .record_classified_reply(key, token, outcome, |q| q.available)
                    .expect("continuation carries a live CallSelectSet token");

                let mut effects = Self::cancel_effects(step.cancel_losers);
                if let Some(reply) = winner {
                    if let Some(request) = pending.request.take() {
                        effects.insert(0, reply_to(request, reply));
                    }
                } else if pending.set.report_ready() {
                    if let Some(request) = pending.request.take() {
                        effects.insert(0, reply_to(request, QuoteReply::Unavailable));
                    }
                    self.pending = None;
                }

                if effects.is_empty() {
                    noop()
                } else {
                    Effect::Batch(effects)
                }
            }
            QuoteEvent::Cancelled {
                key,
                token,
                outcome,
            } => {
                *self.cancel_outcomes.borrow_mut() += 1;
                let Some(pending) = self.pending.as_mut() else {
                    return noop();
                };
                let _ = pending
                    .set
                    .record_cancel(key, token, outcome)
                    .expect("cancel continuation carries a live CallSelectSet token");
                if pending.set.report_ready() && pending.request.is_none() {
                    self.pending = None;
                }
                noop()
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

    let cancel_outcomes = Rc::new(RefCell::new(0));
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
        .map_err(|e| anyhow::anyhow!("send quote client: {e:?}"))?;
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
        cancel_outcomes: *cancel_outcomes.borrow(),
        late_cancelled_rejections,
        completed: true,
        rough_edges: vec![
            "two-provider race still needs token/handle plumbing",
            "gateway keeps pending state after replying until loser cancel settles",
        ],
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
}

#[derive(Debug)]
enum BatcherEvent {
    Drain,
    Flush(SleepReply),
}

#[derive(Debug)]
enum BatcherRequest {
    Submit(u64),
}

#[derive(Debug)]
struct Batcher {
    pending: PendingReplies<u64, BatchReply>,
    values: Vec<(u64, u64)>,
    next_qid: u64,
    next_batch: u64,
    timer_armed: bool,
    closed: bool,
    window: Duration,
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
                self.timer_armed = false;
                self.values.clear();
                self.pending.drain_replies_into_effect(BatchReply::Closed)
            }
            BatcherEvent::Flush(Ok(())) => {
                self.timer_armed = false;
                let size = self.values.len();
                let sum = self.values.iter().map(|(_, value)| *value).sum();
                let batch_id = self.next_batch;
                self.next_batch += 1;
                self.values.clear();
                self.pending
                    .drain_replies_with_into_effect(move |_| BatchReply::Batched {
                        batch_id,
                        size,
                        sum,
                    })
            }
            BatcherEvent::Flush(Err(_)) => self.pending.drain_replies_into_effect(BatchReply::Full),
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
                let qid = self.next_qid;
                let next_qid = qid + 1;
                match self.pending.park_request(qid, call) {
                    Ok((_ticket, permit)) => {
                        self.next_qid = next_qid;
                        self.values.push((qid, value));
                        let effect = if self.timer_armed {
                            noop()
                        } else {
                            self.timer_armed = true;
                            sleep(self.window).then_service_event(BatcherEvent::Flush)
                        };
                        request_effect_after_park(permit, effect)
                    }
                    Err(tina_runtime::ParkError::Full { call, .. }) => call.reply(BatchReply::Full),
                    Err(tina_runtime::ParkError::DuplicateKey { call, .. }) => {
                        // qid is monotonic so duplicate is unreachable.
                        call.reject(tina::CallRejectedReason::UnsupportedMessage)
                    }
                }
            }
        }
    }
}

#[derive(Debug)]
enum BatchClientMsg {
    Begin(tina_runtime::SplitServiceHandle<BatcherEvent, BatcherRequest, BatchReply>),
    Returned(CallOutcome<BatchReply>),
}

#[derive(Debug)]
struct BatchClient {
    values: Vec<u64>,
    replies: Rc<RefCell<Vec<BatchReply>>>,
    drain_after_submit: bool,
}

#[tina_runtime::isolate(
    message = BatchClientMsg,
    send = tina::Outbound<tina::ServiceMessage<BatcherEvent, BatcherRequest>>
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
            BatchClientMsg::Returned(CallOutcome::Replied(reply)) => {
                self.replies.borrow_mut().push(reply);
                noop()
            }
            BatchClientMsg::Returned(_) => noop(),
        }
    }
}

pub fn run_debounced_batch_probe() -> anyhow::Result<DebouncedBatchReport> {
    let mut sim = Simulator::new(SingleShard, SimulatorConfig::default());
    let batcher = sim
        .register_split_service::<Batcher, BatcherEvent, BatcherRequest, std::convert::Infallible>(
            Batcher {
                pending: PendingReplies::with_capacity(3).named("ergonomics.batch.pending"),
                values: Vec::new(),
                next_qid: 1,
                next_batch: 1,
                timer_armed: false,
                closed: false,
                window: Duration::from_millis(10),
            },
            8,
        );
    let replies = Rc::new(RefCell::new(Vec::new()));
    let client = sim.register(BatchClient {
        values: vec![2, 3, 5, 7, 11],
        replies: Rc::clone(&replies),
        drain_after_submit: false,
    });

    sim.try_send(client, BatchClientMsg::Begin(batcher))
        .map_err(|e| anyhow::anyhow!("send batch client: {e:?}"))?;
    sim.run_until_quiescent();

    let replies = replies.borrow();
    let mut admitted = 0;
    let mut full = 0;
    let mut closed = 0;
    let mut batch_sizes = Vec::new();
    let mut sums = Vec::new();
    for reply in replies.iter().copied() {
        match reply {
            BatchReply::Batched { size, sum, .. } => {
                admitted += 1;
                batch_sizes.push(size);
                sums.push(sum);
            }
            BatchReply::Full => full += 1,
            BatchReply::Closed => closed += 1,
        }
    }

    Ok(DebouncedBatchReport {
        admitted,
        full,
        closed,
        batch_sizes,
        sums,
        rough_edges: vec![
            "handle_call path needs manual PendingReplies insertion",
            "timer state is explicit and pleasant, but still another enum variant",
        ],
    })
}

pub fn run_debounced_batch_drain_probe() -> anyhow::Result<DebouncedBatchReport> {
    let mut sim = Simulator::new(SingleShard, SimulatorConfig::default());
    let batcher = sim
        .register_split_service::<Batcher, BatcherEvent, BatcherRequest, std::convert::Infallible>(
            Batcher {
                pending: PendingReplies::with_capacity(4).named("ergonomics.batch.drain.pending"),
                values: Vec::new(),
                next_qid: 1,
                next_batch: 1,
                timer_armed: false,
                closed: false,
                window: Duration::from_millis(50),
            },
            8,
        );
    let replies = Rc::new(RefCell::new(Vec::new()));
    let client = sim.register(BatchClient {
        values: vec![2, 3, 5],
        replies: Rc::clone(&replies),
        drain_after_submit: true,
    });

    sim.try_send(client, BatchClientMsg::Begin(batcher))
        .map_err(|e| anyhow::anyhow!("send drain batch client: {e:?}"))?;
    sim.run_until_quiescent();

    let replies = replies.borrow();
    let mut admitted = 0;
    let mut full = 0;
    let mut closed = 0;
    for reply in replies.iter().copied() {
        match reply {
            BatchReply::Batched { .. } => admitted += 1,
            BatchReply::Full => full += 1,
            BatchReply::Closed => closed += 1,
        }
    }

    Ok(DebouncedBatchReport {
        admitted,
        full,
        closed,
        batch_sizes: Vec::new(),
        sums: Vec::new(),
        rough_edges: vec![
            "drain is explicit and boring once PendingReplies owns the slots",
            "the already-armed timer still needs a later harmless Flush turn",
        ],
    })
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
                call.defer(sleep(self.delay)).reply_service_event(UpstreamEvent::Done)
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
                            let effect = call_request(
                                self.upstream,
                                UpstreamRequest::Fetch,
                                CALL_TIMEOUT,
                            )
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
                        call_request(
                            cache,
                            CacheRequest::Get("price:alpaca"),
                            CALL_TIMEOUT,
                        )
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
        .map_err(|e| anyhow::anyhow!("send cache client: {e:?}"))?;
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
        rough_edges: vec![
            "single-flight is a natural Tina shape once SharedWork owns the parked callers",
            "the service still names the fill-in-flight state and late fill policy",
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(report.cancel_outcomes, 1);
        assert_eq!(report.late_cancelled_rejections, 1);
        assert!(report.completed);
    }

    #[test]
    fn quote_race_no_provider_available_replies_unavailable() {
        let report = run_quote_race_no_winner_probe().unwrap();
        assert_eq!(report.replies, vec![QuoteReply::Unavailable]);
        assert_eq!(report.cancel_outcomes, 0);
        assert_eq!(report.late_cancelled_rejections, 0);
        assert!(report.completed);
    }

    #[test]
    fn debounced_batch_replies_to_admitted_callers_and_rejects_excess() {
        let report = run_debounced_batch_probe().unwrap();
        assert_eq!(report.admitted, 3);
        assert_eq!(report.full, 2);
        assert_eq!(report.closed, 0);
        assert_eq!(report.batch_sizes, vec![3, 3, 3]);
        assert_eq!(report.sums, vec![10, 10, 10]);
    }

    #[test]
    fn debounced_batch_drain_replies_closed_to_pending_callers() {
        let report = run_debounced_batch_drain_probe().unwrap();
        assert_eq!(report.admitted, 0);
        assert_eq!(report.full, 0);
        assert_eq!(report.closed, 3);
    }

    #[test]
    fn single_flight_cache_coalesces_waiters_and_reclaims_capacity() {
        let report = run_single_flight_cache_probe().unwrap();
        assert_eq!(report.callers, 5);
        assert_eq!(report.hits, 3);
        assert_eq!(report.full, 2);
        assert_eq!(report.upstream_calls, 1);
        assert_eq!(report.values, vec![42, 42, 42]);
    }
}
