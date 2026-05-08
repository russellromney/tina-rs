use std::cell::RefCell;
use std::convert::Infallible;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use tina::{Address, prelude::*};
use tina_runtime::{
    CallError, CallOutcome, RuntimeEventKind, SendOutcome, SendRejectedReason, call,
    journal_append, send_observed,
};
use tina_sim::dst::{DstRun, History, InvariantSuite, ShrinkConfig, assert_replays, delete_shrink};
use tina_sim::{MultiShardReplayArtifact, MultiShardSimulator, MultiShardSimulatorConfig};

const PORTABLE_SERVICE_SEED: u64 = 45_001;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct DstShard(u32);

impl Shard for DstShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Request {
    key: u64,
    body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WorkerMsg {
    Work(Request),
    Audited(SendOutcome, Request),
    Durable(Result<(), CallError>, Request, u64),
    Stop,
    Panic,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WorkerReply {
    Stored(u64),
    DurableFailure(CallError),
}

#[derive(Debug)]
struct Worker {
    journal_path: PathBuf,
    next_index: u64,
    audit: Address<AuditMsg>,
}

#[tina_runtime::isolate(
    message = WorkerMsg,
    reply = WorkerReply,
    call = tina_runtime::RuntimeCall<WorkerMsg>,
    shard = DstShard
)]
impl Worker {
    fn handle(
        &mut self,
        msg: WorkerMsg,
        _ctx: &mut Context<'_, DstShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            WorkerMsg::Work(request) => send_observed(self.audit, AuditMsg::Record(request.key))
                .reply(move |outcome| WorkerMsg::Audited(outcome, request)),
            WorkerMsg::Audited(SendOutcome::Accepted, request) => {
                let index = self.next_index;
                journal_append(
                    self.journal_path.clone(),
                    index,
                    format!("{}:{}", request.key, request.body).into_bytes(),
                )
                .reply(move |result| WorkerMsg::Durable(result, request, index))
            }
            WorkerMsg::Audited(SendOutcome::Full, _) => {
                reply(WorkerReply::DurableFailure(CallError::TargetFull))
            }
            WorkerMsg::Audited(SendOutcome::Closed, _) => {
                reply(WorkerReply::DurableFailure(CallError::TargetClosed))
            }
            WorkerMsg::Durable(Ok(()), request, index) => {
                self.next_index = index + 1;
                reply(WorkerReply::Stored(request.key))
            }
            WorkerMsg::Durable(Err(error), _, _) => reply(WorkerReply::DurableFailure(error)),
            WorkerMsg::Stop => stop(),
            WorkerMsg::Panic => panic!("baobab dst worker panic"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AuditMsg {
    Record(u64),
}

#[derive(Debug)]
struct AuditSink {
    observed: Rc<RefCell<Vec<u64>>>,
}

#[tina_runtime::isolate(message = AuditMsg, shard = DstShard)]
impl AuditSink {
    fn handle(
        &mut self,
        msg: AuditMsg,
        _ctx: &mut Context<'_, DstShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            AuditMsg::Record(key) => {
                self.observed.borrow_mut().push(key);
                noop()
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RouterMsg {
    Submit(Request),
    Returned(Request, CallOutcome<WorkerReply>),
    Stop,
}

#[derive(Debug)]
struct Router {
    even_worker: Address<WorkerMsg, WorkerReply>,
    odd_worker: Address<WorkerMsg, WorkerReply>,
    accepted: Rc<RefCell<Vec<u64>>>,
    rejected: Rc<RefCell<Vec<&'static str>>>,
}

#[tina_runtime::isolate(
    message = RouterMsg,
    call = tina_runtime::RuntimeCall<RouterMsg>,
    shard = DstShard
)]
impl Router {
    fn handle(
        &mut self,
        msg: RouterMsg,
        _ctx: &mut Context<'_, DstShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            RouterMsg::Submit(request) => {
                let target = if request.key % 2 == 0 {
                    self.even_worker
                } else {
                    self.odd_worker
                };
                call(
                    target,
                    WorkerMsg::Work(request.clone()),
                    Duration::from_millis(10),
                )
                .reply(move |outcome| RouterMsg::Returned(request, outcome))
            }
            RouterMsg::Returned(request, outcome) => {
                match outcome {
                    CallOutcome::Replied(WorkerReply::Stored(key)) => {
                        assert_eq!(key, request.key);
                        self.accepted.borrow_mut().push(key);
                    }
                    CallOutcome::Replied(WorkerReply::DurableFailure(_)) => {
                        self.rejected.borrow_mut().push("durable");
                    }
                    CallOutcome::Full => self.rejected.borrow_mut().push("full"),
                    CallOutcome::Closed => self.rejected.borrow_mut().push("closed"),
                    CallOutcome::Timeout => self.rejected.borrow_mut().push("timeout"),
                }
                noop()
            }
            RouterMsg::Stop => stop(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServiceOp {
    SubmitEven(u64),
    SubmitOdd(u64),
    SubmitEvenThenStopRouter(u64),
    StopEven,
    StopOdd,
    PanicEven,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ServiceProjection {
    accepted: Vec<u64>,
    closed: usize,
    full: usize,
    timeouts: usize,
    journal_appended: usize,
    audit_stored: Vec<u64>,
    reply_rejected: usize,
    panicked: usize,
}

fn portable_service_history() -> History<ServiceOp> {
    History::new(
        "portable service dst",
        PORTABLE_SERVICE_SEED,
        vec![
            ServiceOp::SubmitEven(2),
            ServiceOp::SubmitOdd(3),
            ServiceOp::StopEven,
            ServiceOp::SubmitEven(4),
            ServiceOp::SubmitOdd(5),
            ServiceOp::StopOdd,
            ServiceOp::SubmitOdd(7),
        ],
    )
}

fn run_service_history(
    history: &History<ServiceOp>,
) -> DstRun<ServiceProjection, MultiShardReplayArtifact> {
    run_service_history_with_audit_capacity(history, 8, 8)
}

fn run_service_history_with_audit_capacity(
    history: &History<ServiceOp>,
    even_audit_capacity: usize,
    odd_audit_capacity: usize,
) -> DstRun<ServiceProjection, MultiShardReplayArtifact> {
    let even_journal = deterministic_path(history.seed(), "even");
    let odd_journal = deterministic_path(history.seed(), "odd");
    let accepted = Rc::new(RefCell::new(Vec::new()));
    let rejected = Rc::new(RefCell::new(Vec::new()));
    let audit_stored = Rc::new(RefCell::new(Vec::new()));
    let mut sim = MultiShardSimulator::with_config(
        [DstShard(1), DstShard(2), DstShard(3)],
        tina_sim::SimulatorConfig {
            seed: history.seed(),
            ..tina_sim::SimulatorConfig::default()
        },
        MultiShardSimulatorConfig {
            shard_pair_capacity: 2,
        },
    );
    let even_audit = sim.register_with_capacity_on::<AuditSink, AuditMsg, Infallible>(
        ShardId::new(2),
        AuditSink {
            observed: Rc::clone(&audit_stored),
        },
        even_audit_capacity,
    );
    let odd_audit = sim.register_with_capacity_on::<AuditSink, AuditMsg, Infallible>(
        ShardId::new(3),
        AuditSink {
            observed: Rc::clone(&audit_stored),
        },
        odd_audit_capacity,
    );
    let even = sim.register_with_capacity_on::<Worker, WorkerMsg, Infallible>(
        ShardId::new(2),
        Worker {
            journal_path: even_journal,
            next_index: 1,
            audit: even_audit,
        },
        4,
    );
    let odd = sim.register_with_capacity_on::<Worker, WorkerMsg, Infallible>(
        ShardId::new(3),
        Worker {
            journal_path: odd_journal,
            next_index: 1,
            audit: odd_audit,
        },
        4,
    );
    let router = sim.register_with_capacity_on::<Router, RouterMsg, Infallible>(
        ShardId::new(1),
        Router {
            even_worker: even,
            odd_worker: odd,
            accepted: Rc::clone(&accepted),
            rejected: Rc::clone(&rejected),
        },
        8,
    );

    for op in history.operations() {
        match *op {
            ServiceOp::SubmitEven(key) | ServiceOp::SubmitOdd(key) => {
                let _ = sim.try_send(
                    router,
                    RouterMsg::Submit(Request {
                        key,
                        body: format!("body-{key}"),
                    }),
                );
            }
            ServiceOp::SubmitEvenThenStopRouter(key) => {
                sim.try_send(
                    router,
                    RouterMsg::Submit(Request {
                        key,
                        body: format!("body-{key}"),
                    }),
                )
                .expect("submit service op before requester stop");
                let _ = sim.step();
                let _ = sim.try_send(router, RouterMsg::Stop);
            }
            ServiceOp::StopEven => {
                let _ = sim.try_send(even, WorkerMsg::Stop);
            }
            ServiceOp::StopOdd => {
                let _ = sim.try_send(odd, WorkerMsg::Stop);
            }
            ServiceOp::PanicEven => {
                let _ = sim.try_send(even, WorkerMsg::Panic);
            }
        }
        sim.run_until_quiescent();
    }

    let trace = sim.trace().to_vec();
    InvariantSuite::standard().assert(&trace);
    let projection = ServiceProjection {
        accepted: accepted.borrow().clone(),
        closed: rejected
            .borrow()
            .iter()
            .filter(|reason| **reason == "closed")
            .count(),
        full: trace
            .iter()
            .filter(|event| {
                matches!(
                    event.kind(),
                    RuntimeEventKind::SendRejected {
                        reason: SendRejectedReason::Full,
                        ..
                    }
                )
            })
            .count(),
        timeouts: rejected
            .borrow()
            .iter()
            .filter(|reason| **reason == "timeout")
            .count(),
        journal_appended: trace
            .iter()
            .filter(|event| matches!(event.kind(), RuntimeEventKind::JournalAppended { .. }))
            .count(),
        audit_stored: audit_stored.borrow().clone(),
        reply_rejected: trace
            .iter()
            .filter(|event| matches!(event.kind(), RuntimeEventKind::CallReplyRejected { .. }))
            .count(),
        panicked: trace
            .iter()
            .filter(|event| matches!(event.kind(), RuntimeEventKind::HandlerPanicked))
            .count(),
    };
    let artifact = sim.replay_artifact();
    DstRun::new(projection, artifact)
}

fn deterministic_path(seed: u64, suffix: &str) -> PathBuf {
    PathBuf::from(format!("/tmp/tina-portable-service-dst-{seed}-{suffix}"))
}

#[test]
fn portable_service_dst_replays_whole_service_history() {
    let history = portable_service_history();
    let run = assert_replays(&history, run_service_history);

    assert_eq!(run.output().accepted, vec![2, 3, 5]);
    assert_eq!(run.output().closed, 2);
    assert_eq!(run.output().timeouts, 0);
    assert_eq!(run.output().journal_appended, 3);
    assert_eq!(run.output().audit_stored, vec![2, 3, 5]);
    assert_eq!(run.output().reply_rejected, 0);
    assert_eq!(run.output().panicked, 0);
}

#[test]
fn portable_service_dst_shrinks_to_closed_worker_history() {
    let history = portable_service_history();
    let shrunk = delete_shrink(
        &history,
        ShrinkConfig { max_attempts: 64 },
        "history still produces a closed worker outcome",
        |candidate| run_service_history(candidate).output().closed > 0,
    );

    assert!(shrunk.shrunk().len() < shrunk.original().len());
    assert!(run_service_history(shrunk.shrunk()).output().closed > 0);
}

#[test]
fn portable_service_dst_replays_observed_send_full_before_persistence() {
    let history = History::new(
        "portable service audit full dst",
        PORTABLE_SERVICE_SEED + 1,
        vec![ServiceOp::SubmitEven(2)],
    );
    let run = assert_replays(&history, |candidate| {
        run_service_history_with_audit_capacity(candidate, 0, 8)
    });

    assert!(run.output().accepted.is_empty());
    assert_eq!(run.output().audit_stored, Vec::<u64>::new());
    assert_eq!(run.output().journal_appended, 0);
    assert_eq!(run.output().full, 1);
    assert_eq!(run.output().reply_rejected, 0);
    assert_eq!(run.output().panicked, 0);
}

#[test]
fn baobab_dst_replays_observed_send_persistence_and_requester_stop() {
    let history = History::new(
        "baobab observed-send persistence requester stop dst",
        PORTABLE_SERVICE_SEED + 2,
        vec![ServiceOp::SubmitEvenThenStopRouter(8)],
    );
    let run = assert_replays(&history, run_service_history);

    assert!(run.output().accepted.is_empty());
    assert_eq!(run.output().audit_stored, vec![8]);
    assert_eq!(run.output().journal_appended, 1);
    // Stopping the router mid-call now cancels its pending isolate
    // calls (Rock 5: `CallCancelled { OwnerStopped }`). The worker
    // still finishes and its reply hits `CallReplyRejected
    // { NoPendingCall }` — that's the late-reply visibility we want.
    assert_eq!(run.output().reply_rejected, 1);
    assert_eq!(run.output().panicked, 0);
}

#[test]
fn baobab_dst_replays_pressure_shard_failure_and_topology_truth() {
    let history = History::new(
        "baobab pressure shard failure topology dst",
        PORTABLE_SERVICE_SEED + 3,
        vec![
            ServiceOp::PanicEven,
            ServiceOp::SubmitEven(10),
            ServiceOp::SubmitOdd(11),
        ],
    );
    let run = assert_replays(&history, run_service_history);

    assert_eq!(run.output().panicked, 1);
    assert_eq!(run.output().closed, 1);
    assert_eq!(run.output().accepted, vec![11]);
    assert_eq!(run.output().journal_appended, 1);
    assert_eq!(run.output().reply_rejected, 0);
}

#[test]
fn baobab_dst_shrinks_requester_stop_history() {
    let history = History::new(
        "baobab requester stop shrink dst",
        PORTABLE_SERVICE_SEED + 4,
        vec![
            ServiceOp::SubmitOdd(21),
            ServiceOp::SubmitEvenThenStopRouter(22),
            ServiceOp::SubmitOdd(23),
        ],
    );
    let shrunk = delete_shrink(
        &history,
        ShrinkConfig { max_attempts: 64 },
        "history still runs accepted worker side effects after requester stop without accepting a reply",
        |candidate| {
            let output = run_service_history(candidate).output().clone();
            output.accepted.is_empty() && output.journal_appended > 0
        },
    );

    assert!(shrunk.shrunk().len() < shrunk.original().len());
    let shrunk_output = run_service_history(shrunk.shrunk()).output().clone();
    assert!(shrunk_output.accepted.is_empty());
    assert!(shrunk_output.journal_appended > 0);
}
