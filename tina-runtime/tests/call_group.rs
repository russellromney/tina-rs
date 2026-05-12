//! End-to-end proofs for `CallGroup` on the live runtime.
//!
//! Unit tests cover bounded storage and token ABA. These tests prove
//! the effect shape: first success wins, loser cancels are visible,
//! reports wait for cancel outcomes, late loser replies are trace
//! facts, and a `RequestContext` can be carried through the race and
//! replied exactly once.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallGroup, CallGroupToken, CallOutcome, CallReplyRejectedReason, DefaultThreadedMailboxFactory,
    DeferredReplyRejectedReason, RuntimeEventKind, SleepReply, ThreadedRuntime, call_with_handle,
    cancel_call, sleep,
};

const POLL_BUDGET: Duration = Duration::from_secs(5);

fn wait_for(total: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + total;
    while std::time::Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    cond()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WorkerReply {
    score: u8,
}

#[derive(Debug)]
enum WorkerMsg {
    Do,
    Done(SleepReply),
}

struct Worker {
    delay: Duration,
    score: u8,
}

#[tina_runtime::isolate(message = WorkerMsg, reply = WorkerReply)]
impl Worker {
    fn handle(
        &mut self,
        msg: WorkerMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            WorkerMsg::Do => sleep(self.delay).reply(WorkerMsg::Done),
            WorkerMsg::Done(Ok(())) => reply(WorkerReply { score: self.score }),
            WorkerMsg::Done(Err(_)) => stop(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SvcReply {
    Ready,
    NotReady,
}

#[derive(Debug)]
enum SvcMsg {
    Start,
    Returned {
        worker: u32,
        token: CallGroupToken,
        outcome: CallOutcome<WorkerReply>,
    },
    Cancelled {
        worker: u32,
        token: CallGroupToken,
        outcome: CancelOutcome,
    },
}

struct Svc {
    workers: Vec<Address<WorkerMsg, WorkerReply>>,
    group: Option<CallGroup<u32, WorkerReply>>,
    request: Option<RequestContext<SvcReply>>,
}

#[tina_runtime::isolate(message = SvcMsg, reply = SvcReply)]
impl Svc {
    fn handle(
        &mut self,
        msg: SvcMsg,
        ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            SvcMsg::Start => {
                self.request = Some(ctx.take_request_context().expect("request context"));
                let mut group = CallGroup::with_capacity(self.workers.len());
                let mut effects = Vec::with_capacity(self.workers.len());
                for (idx, worker) in self.workers.iter().enumerate() {
                    let key = idx as u32;
                    let token = group.reserve_token().expect("group sized to workers");
                    let (effect, handle) =
                        call_with_handle(*worker, WorkerMsg::Do, Duration::from_secs(2)).reply(
                            move |outcome| SvcMsg::Returned {
                                worker: key,
                                token,
                                outcome,
                            },
                        );
                    group
                        .insert_reserved(key, token, handle)
                        .expect("race group sized to workers");
                    effects.push(effect);
                }
                self.group = Some(group);
                Effect::Batch(effects)
            }
            SvcMsg::Returned {
                worker,
                token,
                outcome,
            } => {
                let group = self.group.as_mut().expect("group");
                let step = group
                    .record_reply(worker, token, outcome, |reply| reply.score >= 10)
                    .expect("known branch");
                let mut effects = Vec::with_capacity(step.cancel_losers.len());
                for request in step.cancel_losers {
                    let (worker, token, handle) = request.into_parts();
                    effects.push(cancel_call(handle).reply(move |outcome| SvcMsg::Cancelled {
                        worker,
                        token,
                        outcome,
                    }));
                }
                if step.report_ready {
                    return self.finish();
                }
                Effect::Batch(effects)
            }
            SvcMsg::Cancelled {
                worker,
                token,
                outcome,
            } => {
                let ready = self
                    .group
                    .as_mut()
                    .expect("group")
                    .record_cancel(worker, token, outcome)
                    .expect("bounded cancel report");
                if ready { self.finish() } else { noop() }
            }
        }
    }
}

impl Svc {
    fn finish(&mut self) -> Effect<Self> {
        let report = self.group.take().expect("group").into_report();
        let reply = match report.winner_outcome().map(|outcome| &outcome.outcome) {
            Some(CallOutcome::Replied(WorkerReply { score })) if *score >= 10 => SvcReply::Ready,
            _ => SvcReply::NotReady,
        };
        let request = self.request.take().expect("request");
        tina::reply_to_request(request, reply)
    }
}

#[derive(Debug)]
enum ClientMsg {
    Start(Address<SvcMsg, SvcReply>),
    Returned(CallOutcome<SvcReply>),
}

struct Client {
    out: Arc<Mutex<Vec<CallOutcome<SvcReply>>>>,
}

#[tina_runtime::isolate(message = ClientMsg)]
impl Client {
    fn handle(
        &mut self,
        msg: ClientMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ClientMsg::Start(svc) => tina_runtime::call(svc, SvcMsg::Start, Duration::from_secs(3))
                .reply(ClientMsg::Returned),
            ClientMsg::Returned(outcome) => {
                self.out.lock().expect("out lock").push(outcome);
                noop()
            }
        }
    }
}

#[test]
fn first_success_cancels_losers_and_request_context_replies_once() {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));

    let workers = vec![
        runtime
            .register_with_capacity::<_, Infallible>(
                Worker {
                    delay: Duration::from_millis(10),
                    score: 11,
                },
                8,
            )
            .expect("fast winner"),
        runtime
            .register_with_capacity::<_, Infallible>(
                Worker {
                    delay: Duration::from_millis(120),
                    score: 1,
                },
                8,
            )
            .expect("slow loser"),
        runtime
            .register_with_capacity::<_, Infallible>(
                Worker {
                    delay: Duration::from_millis(120),
                    score: 2,
                },
                8,
            )
            .expect("slow loser"),
    ];
    let svc = runtime
        .register_with_capacity::<_, Infallible>(
            Svc {
                workers,
                group: None,
                request: None,
            },
            32,
        )
        .expect("service");
    let out = Arc::new(Mutex::new(Vec::new()));
    let client = runtime
        .register_with_capacity::<_, Infallible>(Client { out: out.clone() }, 16)
        .expect("client");

    runtime
        .try_send(client, ClientMsg::Start(svc))
        .expect("start client");

    assert!(
        wait_for(POLL_BUDGET, || out.lock().expect("out lock").len() == 1),
        "client did not observe exactly one service reply"
    );
    assert_eq!(
        out.lock().expect("out lock").as_slice(),
        &[CallOutcome::Replied(SvcReply::Ready)]
    );

    assert!(
        wait_for(POLL_BUDGET, || runtime
            .trace()
            .events()
            .iter()
            .filter(|event| matches!(event.kind(), RuntimeEventKind::CallCancelled { .. }))
            .count()
            >= 2),
        "loser cancels were not traced"
    );
    assert!(
        wait_for(POLL_BUDGET, || runtime
            .trace()
            .events()
            .iter()
            .filter(|event| matches!(
                event.kind(),
                RuntimeEventKind::CallReplyRejected {
                    reason: CallReplyRejectedReason::CallerCancelled,
                    ..
                } | RuntimeEventKind::DeferredReplyRejected {
                    reason: DeferredReplyRejectedReason::CallerCancelled,
                    ..
                }
            ))
            .count()
            >= 2),
        "late loser replies were not traced as caller-cancelled rejections"
    );

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OwnerStopSummary {
    cancel_outcomes: usize,
    complete: bool,
}

#[derive(Debug)]
enum OwnerMsg {
    Begin,
    StopOwner,
    Returned {
        worker: u32,
        token: CallGroupToken,
        outcome: CallOutcome<WorkerReply>,
    },
    Cancelled {
        worker: u32,
        token: CallGroupToken,
        outcome: CancelOutcome,
    },
}

struct OwnerDriver {
    workers: Vec<Address<WorkerMsg, WorkerReply>>,
    group: CallGroup<u32, WorkerReply>,
}

#[tina_runtime::isolate(message = OwnerMsg)]
impl OwnerDriver {
    fn handle(
        &mut self,
        msg: OwnerMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            OwnerMsg::Begin => {
                let mut effects = Vec::with_capacity(self.workers.len());
                for (idx, worker) in self.workers.iter().enumerate() {
                    let key = idx as u32;
                    let token = self.group.reserve_token().expect("group sized to workers");
                    let (effect, handle) =
                        call_with_handle(*worker, WorkerMsg::Do, Duration::from_secs(2)).reply(
                            move |outcome| OwnerMsg::Returned {
                                worker: key,
                                token,
                                outcome,
                            },
                        );
                    self.group
                        .insert_reserved(key, token, handle)
                        .expect("owner group sized to workers");
                    effects.push(effect);
                }
                Effect::Batch(effects)
            }
            OwnerMsg::StopOwner => {
                let requests = self.group.drain_pending_for_cancel();
                let mut effects = Vec::with_capacity(requests.len());
                for request in requests {
                    let (worker, token, handle) = request.into_parts();
                    effects.push(
                        cancel_call(handle).reply(move |outcome| OwnerMsg::Cancelled {
                            worker,
                            token,
                            outcome,
                        }),
                    );
                }
                Effect::Batch(effects)
            }
            OwnerMsg::Returned {
                worker,
                token,
                outcome,
            } => {
                let _ = self.group.record_reply(worker, token, outcome, |_| false);
                noop()
            }
            OwnerMsg::Cancelled {
                worker,
                token,
                outcome,
            } => {
                let ready = self
                    .group
                    .record_cancel(worker, token, outcome)
                    .expect("bounded cancel report");
                if ready {
                    let group = std::mem::replace(&mut self.group, CallGroup::with_capacity(1));
                    let report = group.into_report();
                    stop_with(OwnerStopSummary {
                        cancel_outcomes: report.cancel_outcomes.len(),
                        complete: report.complete,
                    })
                } else {
                    noop()
                }
            }
        }
    }
}

#[test]
fn owner_stop_drains_group_and_waits_for_cancel_outcomes() {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));

    let workers = vec![
        runtime
            .register_with_capacity::<_, Infallible>(
                Worker {
                    delay: Duration::from_millis(150),
                    score: 1,
                },
                8,
            )
            .expect("worker"),
        runtime
            .register_with_capacity::<_, Infallible>(
                Worker {
                    delay: Duration::from_millis(150),
                    score: 2,
                },
                8,
            )
            .expect("worker"),
    ];
    let driver = runtime
        .register_with_capacity::<_, Infallible>(
            OwnerDriver {
                workers,
                group: CallGroup::with_capacity(2),
            },
            32,
        )
        .expect("driver");
    let result = runtime
        .observe_result::<OwnerStopSummary, _, _>(driver)
        .expect("observe result");

    runtime.try_send(driver, OwnerMsg::Begin).expect("begin");
    assert!(
        wait_for(POLL_BUDGET, || runtime
            .trace()
            .events()
            .iter()
            .filter(|event| matches!(event.kind(), RuntimeEventKind::CallDispatchAttempted { .. }))
            .count()
            >= 2),
        "owner-stop test calls did not dispatch"
    );
    runtime
        .try_send(driver, OwnerMsg::StopOwner)
        .expect("stop owner");

    let summary = result.wait(POLL_BUDGET).expect("owner summary");
    assert!(summary.complete);
    assert_eq!(summary.cancel_outcomes, 2);

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}
