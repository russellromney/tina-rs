use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use tina::{
    Address, CallContext, CancelOutcome, Context, Effect, Isolate, RequestContext, SingleShard,
    noop, reply_to_request,
};
use tina_runtime::{
    CallGroup, CallGroupToken, CallOutcome, PendingReplies, RuntimeCall, SleepReply, call,
    call_cancelable, cancel_call, sleep,
};
use tina_sim::{Simulator, SimulatorConfig};

const CALL_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuoteRaceReport {
    pub replies: Vec<QuoteReply>,
    pub cancel_outcomes: usize,
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
enum ProviderMsg {
    Quote,
    Done(RequestContext<ProviderQuote>, SleepReply),
}

#[derive(Debug)]
struct Provider {
    quote: ProviderQuote,
    delay: Duration,
}

impl Isolate for Provider {
    type Message = ProviderMsg;
    type Reply = ProviderQuote;
    type Send = tina::Outbound<std::convert::Infallible>;
    type Spawn = std::convert::Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Call = RuntimeCall<ProviderMsg>;
    type Shard = SingleShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ProviderMsg::Quote => noop(),
            ProviderMsg::Done(req, Ok(())) => reply_to_request(req, self.quote),
            ProviderMsg::Done(_, Err(_)) => noop(),
        }
    }

    fn handle_call(&mut self, msg: Self::Message, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            ProviderMsg::Quote => call
                .defer(sleep(self.delay))
                .reply(|req, sleep_reply| ProviderMsg::Done(req, sleep_reply)),
            ProviderMsg::Done(_, _) => call.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }
}

#[derive(Debug)]
enum QuoteGatewayMsg {
    GetQuote,
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
struct PendingQuote {
    request: Option<RequestContext<QuoteReply>>,
    group: CallGroup<u32, ProviderQuote>,
}

#[derive(Debug)]
struct QuoteGateway {
    providers: [Address<ProviderMsg, ProviderQuote>; 2],
    pending: Option<PendingQuote>,
    cancel_outcomes: Rc<RefCell<usize>>,
}

impl QuoteGateway {
    fn start_race(&mut self, request: RequestContext<QuoteReply>) -> Effect<Self> {
        let mut group = CallGroup::with_capacity(self.providers.len());
        let mut effects = Vec::with_capacity(self.providers.len());

        for (idx, provider) in self.providers.iter().copied().enumerate() {
            let key = idx as u32;
            let token = group.reserve_token().expect("group sized to providers");
            let (effect, handle) = call_cancelable(provider, ProviderMsg::Quote, CALL_TIMEOUT)
                .then(move |outcome| QuoteGatewayMsg::ProviderReturned {
                    key,
                    token,
                    outcome,
                });
            group
                .insert_reserved(key, token, handle)
                .expect("fresh group accepts each provider");
            effects.push(effect);
        }

        self.pending = Some(PendingQuote {
            request: Some(request),
            group,
        });
        Effect::Batch(effects)
    }

    fn cancel_effects(
        requests: Vec<tina_runtime::CallGroupCancelRequest<u32, ProviderQuote>>,
    ) -> Vec<Effect<Self>> {
        requests
            .into_iter()
            .map(|request| {
                let (key, token, handle) = request.into_parts();
                cancel_call(handle).then(move |outcome| QuoteGatewayMsg::Cancelled {
                    key,
                    token,
                    outcome,
                })
            })
            .collect()
    }
}

impl Isolate for QuoteGateway {
    type Message = QuoteGatewayMsg;
    type Reply = QuoteReply;
    type Send = tina::Outbound<std::convert::Infallible>;
    type Spawn = std::convert::Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Call = RuntimeCall<QuoteGatewayMsg>;
    type Shard = SingleShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            QuoteGatewayMsg::GetQuote => noop(),
            QuoteGatewayMsg::ProviderReturned {
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
                    .group
                    .record_reply(key, token, outcome, |q| q.available)
                    .expect("continuation carries a live CallGroup token");

                let mut effects = Self::cancel_effects(step.cancel_losers);
                if let Some(reply) = winner {
                    if let Some(request) = pending.request.take() {
                        effects.insert(0, reply_to_request(request, reply));
                    }
                } else if step.report_ready {
                    if let Some(request) = pending.request.take() {
                        effects.insert(0, reply_to_request(request, QuoteReply::Unavailable));
                    }
                    self.pending = None;
                }

                if effects.is_empty() {
                    noop()
                } else {
                    Effect::Batch(effects)
                }
            }
            QuoteGatewayMsg::Cancelled {
                key,
                token,
                outcome,
            } => {
                *self.cancel_outcomes.borrow_mut() += 1;
                let Some(pending) = self.pending.as_mut() else {
                    return noop();
                };
                let ready = pending
                    .group
                    .record_cancel(key, token, outcome)
                    .expect("cancel continuation carries a live CallGroup token");
                if ready && pending.request.is_none() {
                    self.pending = None;
                }
                noop()
            }
        }
    }

    fn handle_call(&mut self, msg: Self::Message, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            QuoteGatewayMsg::GetQuote => {
                if self.pending.is_some() {
                    call.reply(QuoteReply::Busy)
                } else {
                    self.start_race(call.into_request_context())
                }
            }
            QuoteGatewayMsg::ProviderReturned { .. } | QuoteGatewayMsg::Cancelled { .. } => {
                call.reject(tina::CallRejectedReason::UnsupportedMessage)
            }
        }
    }
}

#[derive(Debug)]
enum QuoteClientMsg {
    Begin(Address<QuoteGatewayMsg, QuoteReply>),
    Returned(CallOutcome<QuoteReply>),
}

#[derive(Debug)]
struct QuoteClient {
    replies: Rc<RefCell<Vec<QuoteReply>>>,
}

impl Isolate for QuoteClient {
    type Message = QuoteClientMsg;
    type Reply = ();
    type Send = tina::Outbound<std::convert::Infallible>;
    type Spawn = std::convert::Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Call = RuntimeCall<QuoteClientMsg>;
    type Shard = SingleShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            QuoteClientMsg::Begin(gateway) => {
                call(gateway, QuoteGatewayMsg::GetQuote, CALL_TIMEOUT)
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
    let mut sim = Simulator::new(SingleShard, SimulatorConfig::default());
    let slow = sim.register(Provider {
        quote: ProviderQuote {
            provider: "slow",
            cents: 475,
            available: true,
        },
        delay: Duration::from_millis(30),
    });
    let fast = sim.register(Provider {
        quote: ProviderQuote {
            provider: "fast",
            cents: 525,
            available: true,
        },
        delay: Duration::from_millis(5),
    });

    let cancel_outcomes = Rc::new(RefCell::new(0));
    let gateway = sim.register(QuoteGateway {
        providers: [slow, fast],
        pending: None,
        cancel_outcomes: Rc::clone(&cancel_outcomes),
    });
    let replies = Rc::new(RefCell::new(Vec::new()));
    let client = sim.register(QuoteClient {
        replies: Rc::clone(&replies),
    });

    sim.try_send(client, QuoteClientMsg::Begin(gateway))
        .map_err(|e| anyhow::anyhow!("send quote client: {e:?}"))?;
    sim.run_until_quiescent();

    Ok(QuoteRaceReport {
        replies: replies.borrow().clone(),
        cancel_outcomes: *cancel_outcomes.borrow(),
        completed: true,
        rough_edges: vec![
            "two-provider race still needs token/handle plumbing",
            "gateway keeps pending state after replying until loser cancel settles",
        ],
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebouncedBatchReport {
    pub admitted: usize,
    pub full: usize,
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
}

#[derive(Debug)]
enum BatcherMsg {
    Submit(u64),
    Flush(SleepReply),
}

#[derive(Debug)]
struct Batcher {
    pending: PendingReplies<u64, BatchReply>,
    values: Vec<(u64, u64)>,
    next_qid: u64,
    next_batch: u64,
    timer_armed: bool,
    window: Duration,
}

impl Isolate for Batcher {
    type Message = BatcherMsg;
    type Reply = BatchReply;
    type Send = tina::Outbound<std::convert::Infallible>;
    type Spawn = std::convert::Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Call = RuntimeCall<BatcherMsg>;
    type Shard = SingleShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            BatcherMsg::Submit(_) => noop(),
            BatcherMsg::Flush(Ok(())) => {
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
            BatcherMsg::Flush(Err(_)) => self.pending.drain_replies_into_effect(BatchReply::Full),
        }
    }

    fn handle_call(&mut self, msg: Self::Message, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            BatcherMsg::Submit(value) => {
                if self.pending.len() >= self.pending.capacity() {
                    return call.reply(BatchReply::Full);
                }
                let qid = self.next_qid;
                self.next_qid += 1;
                self.pending
                    .try_insert(qid, call.into_request_context().into_deferred())
                    .expect("unique qid and pre-checked capacity");
                self.values.push((qid, value));
                if self.timer_armed {
                    noop()
                } else {
                    self.timer_armed = true;
                    sleep(self.window).then(BatcherMsg::Flush)
                }
            }
            BatcherMsg::Flush(_) => call.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }
}

#[derive(Debug)]
enum BatchClientMsg {
    Begin(Address<BatcherMsg, BatchReply>),
    Returned(CallOutcome<BatchReply>),
}

#[derive(Debug)]
struct BatchClient {
    values: Vec<u64>,
    replies: Rc<RefCell<Vec<BatchReply>>>,
}

impl Isolate for BatchClient {
    type Message = BatchClientMsg;
    type Reply = ();
    type Send = tina::Outbound<std::convert::Infallible>;
    type Spawn = std::convert::Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Call = RuntimeCall<BatchClientMsg>;
    type Shard = SingleShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            BatchClientMsg::Begin(batcher) => {
                let calls = self
                    .values
                    .iter()
                    .copied()
                    .map(|value| {
                        call(batcher, BatcherMsg::Submit(value), CALL_TIMEOUT)
                            .then(BatchClientMsg::Returned)
                    })
                    .collect();
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
    let batcher = sim.register(Batcher {
        pending: PendingReplies::with_capacity(3).named("ergonomics.batch.pending"),
        values: Vec::new(),
        next_qid: 1,
        next_batch: 1,
        timer_armed: false,
        window: Duration::from_millis(10),
    });
    let replies = Rc::new(RefCell::new(Vec::new()));
    let client = sim.register(BatchClient {
        values: vec![2, 3, 5, 7, 11],
        replies: Rc::clone(&replies),
    });

    sim.try_send(client, BatchClientMsg::Begin(batcher))
        .map_err(|e| anyhow::anyhow!("send batch client: {e:?}"))?;
    sim.run_until_quiescent();

    let replies = replies.borrow();
    let mut admitted = 0;
    let mut full = 0;
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
        }
    }

    Ok(DebouncedBatchReport {
        admitted,
        full,
        batch_sizes,
        sums,
        rough_edges: vec![
            "handle_call path needs manual PendingReplies insertion",
            "timer state is explicit and pleasant, but still another enum variant",
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
        assert!(report.completed);
    }

    #[test]
    fn debounced_batch_replies_to_admitted_callers_and_rejects_excess() {
        let report = run_debounced_batch_probe().unwrap();
        assert_eq!(report.admitted, 3);
        assert_eq!(report.full, 2);
        assert_eq!(report.batch_sizes, vec![3, 3, 3]);
        assert_eq!(report.sums, vec![10, 10, 10]);
    }
}
