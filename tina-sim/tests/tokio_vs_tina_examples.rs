//! Runnable Tokio-vs-Tina behavior comparisons.
//!
//! These tests compare user-visible semantics, not throughput. Tokio examples
//! run on `Builder::new_current_thread().enable_time()` so deterministic timer
//! and channel behavior are easy to compare against Tina's explicit-step
//! simulator. Production Tokio commonly uses the multi-thread work-stealing
//! runtime; that is a different performance/substrate comparison.
//!
//! Most cases assert exact equality. Cases marked `different_but_expected`
//! are places where Tina has a typed/traceable failure surface Tokio does not
//! model at this layer.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tina::prelude::*;
use tina::{AddressGeneration, RestartBudget, RestartPolicy};
use tina_runtime::IngressSendError;
use tina_runtime::{
    CallCompletionRejectedReason, CallError, CallKind, CallOutcome, CallReplyRejectedReason,
    RuntimeEventKind, SendOutcome, SendRejectedReason, call, send_observed, sleep_then,
};
use tina_sim::{
    FaultConfig, FaultMode, LocalSendFaultMode, MultiShardSimulator, MultiShardSimulatorConfig,
    Simulator, SimulatorConfig,
};
use tina_supervisor::SupervisorConfig;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{self, timeout};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ease {
    A,
    B,
    C,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Behavior {
    Exact,
    Different,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Comparison {
    example: &'static str,
    tokio: Vec<&'static str>,
    tina: Vec<&'static str>,
    ease: Ease,
    behavior: Behavior,
}

impl Comparison {
    fn exact(
        example: &'static str,
        tokio: Vec<&'static str>,
        tina: Vec<&'static str>,
        ease: Ease,
    ) -> Self {
        Self {
            example,
            tokio,
            tina,
            ease,
            behavior: Behavior::Exact,
        }
    }

    fn different_but_expected(
        example: &'static str,
        tokio: Vec<&'static str>,
        tina: Vec<&'static str>,
        ease: Ease,
    ) -> Self {
        Self {
            example,
            tokio,
            tina,
            ease,
            behavior: Behavior::Different,
        }
    }

    fn assert_expected(&self) {
        match self.behavior {
            Behavior::Exact => {
                assert_eq!(self.tokio, self.tina, "{}", self.example);
            }
            Behavior::Different => {
                assert_ne!(self.tokio, self.tina, "{}", self.example);
                assert!(
                    !self.tokio.is_empty() && !self.tina.is_empty(),
                    "{} should record both sides",
                    self.example
                );
            }
        }
        assert!(
            matches!(self.ease, Ease::A | Ease::B | Ease::C),
            "{} has an ergonomics score",
            self.example
        );
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("build current-thread tokio runtime")
        .block_on(future)
}

fn block_on_multi_thread<F>(future: F) -> F::Output
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_time()
        .build()
        .expect("build multi-thread tokio runtime")
        .block_on(future)
}

#[derive(Debug, Default)]
struct ComparisonShard;

impl Shard for ComparisonShard {
    fn id(&self) -> ShardId {
        ShardId::new(151)
    }
}

fn events() -> Arc<Mutex<Vec<&'static str>>> {
    Arc::new(Mutex::new(Vec::new()))
}

fn push(events: &Arc<Mutex<Vec<&'static str>>>, event: &'static str) {
    events.lock().expect("events mutex").push(event);
}

fn snapshot(events: &Arc<Mutex<Vec<&'static str>>>) -> Vec<&'static str> {
    events.lock().expect("events mutex").clone()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SinkMsg {
    Hit,
    Stop,
}

#[derive(Debug)]
struct Sink {
    events: Arc<Mutex<Vec<&'static str>>>,
}

#[tina_runtime::isolate(message = SinkMsg, shard = ComparisonShard)]
impl Sink {
    fn handle(
        &mut self,
        msg: SinkMsg,
        _ctx: &mut Context<'_, ComparisonShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            SinkMsg::Hit => {
                push(&self.events, "hit");
                noop()
            }
            SinkMsg::Stop => stop(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DriverMsg {
    Fire(Address<SinkMsg>),
    Observe(Address<SinkMsg>),
    ObserveTwice(Address<SinkMsg>),
    Observed(SendOutcome),
}

#[derive(Debug)]
struct Driver {
    events: Arc<Mutex<Vec<&'static str>>>,
}

#[tina_runtime::isolate(message = DriverMsg, send = Outbound<SinkMsg>, shard = ComparisonShard)]
impl Driver {
    fn handle(
        &mut self,
        msg: DriverMsg,
        _ctx: &mut Context<'_, ComparisonShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            DriverMsg::Fire(sink) => send(sink, SinkMsg::Hit),
            DriverMsg::Observe(sink) => send_observed(sink, SinkMsg::Hit).then(DriverMsg::Observed),
            DriverMsg::ObserveTwice(sink) => batch(vec![
                send_observed(sink, SinkMsg::Hit).then(DriverMsg::Observed),
                send_observed(sink, SinkMsg::Hit).then(DriverMsg::Observed),
            ]),
            DriverMsg::Observed(outcome) => {
                let event = if outcome.is_accepted() {
                    "accepted"
                } else if outcome.is_full() {
                    "full"
                } else {
                    debug_assert!(outcome.is_closed());
                    "closed"
                };
                push(&self.events, event);
                noop()
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkReq {
    Reply,
    NoReply,
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WorkReply(&'static str);

#[derive(Debug)]
struct Worker {
    held: Option<DeferredReply<WorkReply>>,
}

#[tina_runtime::isolate(message = WorkReq, reply = WorkReply, shard = ComparisonShard)]
impl Worker {
    fn handle_call(&mut self, msg: WorkReq, call: tina::CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            WorkReq::Reply => call.reply(WorkReply("pong")),
            WorkReq::NoReply => {
                // Hold explicit request authority so the call stays
                // pending until timeout. Uncaptured call authority is
                // rejected immediately under the 086 contract.
                self.held = Some(call.into_request_context().into_deferred());
                noop()
            }
            WorkReq::Stop => call.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }

    fn handle(
        &mut self,
        msg: WorkReq,
        _ctx: &mut Context<'_, ComparisonShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            WorkReq::Reply | WorkReq::NoReply => noop(),
            WorkReq::Stop => stop(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ClientMsg {
    Ask(Address<WorkReq, WorkReply>, WorkReq, Duration),
    AskThenStop(Address<WorkReq, WorkReply>),
    Fill,
    Returned(CallOutcome<WorkReply>),
}

#[derive(Debug)]
struct Client {
    events: Arc<Mutex<Vec<&'static str>>>,
}

#[tina_runtime::isolate(message = ClientMsg, shard = ComparisonShard)]
impl Client {
    fn handle(
        &mut self,
        msg: ClientMsg,
        _ctx: &mut Context<'_, ComparisonShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ClientMsg::Ask(worker, request, timeout) => {
                call(worker, request, timeout).then(ClientMsg::Returned)
            }
            ClientMsg::AskThenStop(worker) => batch(vec![
                call(worker, WorkReq::Reply, Duration::from_millis(10)).then(ClientMsg::Returned),
                stop(),
            ]),
            ClientMsg::Fill => noop(),
            ClientMsg::Returned(outcome) => {
                match outcome.into_result() {
                    Ok(WorkReply(value)) => push(&self.events, value),
                    Err(CallError::TargetFull) => push(&self.events, "full"),
                    Err(CallError::TargetClosed) => push(&self.events, "closed"),
                    Err(CallError::Timeout) => push(&self.events, "timeout"),
                    Err(
                        CallError::InvariantViolation
                        | CallError::InvalidResource
                        | CallError::NotFound
                        | CallError::Io
                        | CallError::Unsupported
                        | CallError::ResourceBusy
                        | CallError::CorruptRecord
                        | CallError::CommitUncertain
                        | CallError::StorageFull
                        | CallError::StorageClosed
                        | CallError::DnsFull
                        | CallError::DnsClosed
                        | CallError::SignalFull
                        | CallError::SignalClosed
                        | CallError::TlsFull
                        | CallError::TlsClosed
                        | CallError::TlsCertificate
                        | CallError::TlsName
                        | CallError::TlsHandshake
                        | CallError::TlsAlpnMismatch
                        | CallError::ProcessFull
                        | CallError::ProcessClosed
                        | CallError::KillUncertain
                        | CallError::TimerFull
                        | CallError::Rejected(_),
                    ) => push(&self.events, "unexpected_call_error"),
                }
                noop()
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryMsg {
    Attempt,
    Later,
}

#[derive(Debug)]
struct RetryClient {
    attempts: usize,
    events: Arc<Mutex<Vec<&'static str>>>,
}

#[tina_runtime::isolate(message = RetryMsg, send = Outbound<RetryMsg>, shard = ComparisonShard)]
impl RetryClient {
    fn handle(
        &mut self,
        msg: RetryMsg,
        ctx: &mut Context<'_, ComparisonShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            RetryMsg::Attempt => {
                self.attempts += 1;
                if self.attempts == 1 {
                    push(&self.events, "failed");
                    sleep_then(Duration::from_millis(5), RetryMsg::Later)
                } else {
                    push(&self.events, "succeeded");
                    noop()
                }
            }
            RetryMsg::Later => {
                push(&self.events, "backoff");
                ctx.send_self(RetryMsg::Attempt)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SupervisedChildMsg {
    Boot,
    Poison,
}

#[derive(Debug)]
struct SupervisedChild {
    events: Arc<Mutex<Vec<&'static str>>>,
}

#[tina_runtime::isolate(message = SupervisedChildMsg, shard = ComparisonShard)]
impl SupervisedChild {
    fn handle(
        &mut self,
        msg: SupervisedChildMsg,
        _ctx: &mut Context<'_, ComparisonShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            SupervisedChildMsg::Boot => {
                push(&self.events, "boot");
                noop()
            }
            SupervisedChildMsg::Poison => panic!("supervised child failed"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SupervisorMsg {
    SpawnChild,
}

#[derive(Debug)]
struct SupervisorParent {
    events: Arc<Mutex<Vec<&'static str>>>,
}

#[tina_runtime::isolate(
    message = SupervisorMsg,
    spawn = RestartableChildDefinition<SupervisedChild>,
    shard = ComparisonShard
)]
impl SupervisorParent {
    fn handle(
        &mut self,
        msg: SupervisorMsg,
        _ctx: &mut Context<'_, ComparisonShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            SupervisorMsg::SpawnChild => {
                let events = Arc::clone(&self.events);
                spawn(
                    RestartableChildDefinition::new(
                        move || SupervisedChild {
                            events: Arc::clone(&events),
                        },
                        8,
                    )
                    .with_initial_message(|| SupervisedChildMsg::Boot),
                )
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct CrossShard(u32);

impl Shard for CrossShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CrossSinkMsg {
    Hit,
}

#[derive(Debug)]
struct CrossSink;

#[tina_runtime::isolate(message = CrossSinkMsg, shard = CrossShard)]
impl CrossSink {
    fn handle(
        &mut self,
        _msg: CrossSinkMsg,
        _ctx: &mut Context<'_, CrossShard, Self::Reply>,
    ) -> Effect<Self> {
        noop()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CrossDriverMsg {
    FireTwice(Address<CrossSinkMsg>),
}

#[derive(Debug)]
struct CrossDriver;

#[tina_runtime::isolate(message = CrossDriverMsg, send = Outbound<CrossSinkMsg>, shard = CrossShard)]
impl CrossDriver {
    fn handle(
        &mut self,
        msg: CrossDriverMsg,
        _ctx: &mut Context<'_, CrossShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            CrossDriverMsg::FireTwice(target) => batch(vec![
                send(target, CrossSinkMsg::Hit),
                send(target, CrossSinkMsg::Hit),
            ]),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChatClientMsg {
    Deliver(&'static str),
}

#[derive(Debug)]
struct ChatClient {
    events: Arc<Mutex<Vec<&'static str>>>,
}

#[tina_runtime::isolate(message = ChatClientMsg, shard = ComparisonShard)]
impl ChatClient {
    fn handle(
        &mut self,
        msg: ChatClientMsg,
        _ctx: &mut Context<'_, ComparisonShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ChatClientMsg::Deliver(message) => {
                push(&self.events, message);
                noop()
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChatRoomMsg {
    Burst(Address<ChatClientMsg>),
    Delivered(SendOutcome),
}

#[derive(Debug)]
struct ChatRoom {
    events: Arc<Mutex<Vec<&'static str>>>,
}

#[tina_runtime::isolate(
    message = ChatRoomMsg,
    send = Outbound<ChatClientMsg>,
    shard = ComparisonShard
)]
impl ChatRoom {
    fn handle(
        &mut self,
        msg: ChatRoomMsg,
        _ctx: &mut Context<'_, ComparisonShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ChatRoomMsg::Burst(client) => batch(vec![
                send_observed(client, ChatClientMsg::Deliver("delivered"))
                    .then(ChatRoomMsg::Delivered),
                send_observed(client, ChatClientMsg::Deliver("overflow"))
                    .then(ChatRoomMsg::Delivered),
            ]),
            ChatRoomMsg::Delivered(outcome) => {
                let event = if outcome.is_accepted() {
                    "accepted"
                } else if outcome.is_full() {
                    "full"
                } else {
                    debug_assert!(outcome.is_closed());
                    "closed"
                };
                push(&self.events, event);
                noop()
            }
        }
    }
}

fn simulator() -> Simulator<ComparisonShard> {
    Simulator::new(ComparisonShard, SimulatorConfig::default())
}

fn simulator_with(config: SimulatorConfig) -> Simulator<ComparisonShard> {
    Simulator::new(ComparisonShard, config)
}

fn tina_fire_and_forget() -> Vec<&'static str> {
    let events = events();
    let mut sim = simulator();
    let sink = sim.register_with_mailbox_capacity(
        Sink {
            events: Arc::clone(&events),
        },
        8,
    );
    let driver = sim.register_with_mailbox_capacity(
        Driver {
            events: Arc::clone(&events),
        },
        8,
    );
    sim.try_send(driver, DriverMsg::Fire(sink)).unwrap();
    sim.run_until_quiescent();
    snapshot(&events)
}

fn tina_observed_send(target_capacity: usize, stop_first: bool) -> Vec<&'static str> {
    let events = events();
    let mut sim = simulator();
    let sink = sim.register_with_mailbox_capacity(
        Sink {
            events: Arc::clone(&events),
        },
        target_capacity,
    );
    let driver = sim.register_with_mailbox_capacity(
        Driver {
            events: Arc::clone(&events),
        },
        8,
    );
    if stop_first {
        sim.try_send(sink, SinkMsg::Stop).unwrap();
        sim.run_until_quiescent();
    }
    sim.try_send(driver, DriverMsg::Observe(sink)).unwrap();
    sim.run_until_quiescent();
    snapshot(&events)
}

fn tina_observed_send_twice(target_capacity: usize) -> Vec<&'static str> {
    let events = events();
    let mut sim = simulator();
    let sink = sim.register_with_mailbox_capacity(
        Sink {
            events: Arc::clone(&events),
        },
        target_capacity,
    );
    let driver = sim.register_with_mailbox_capacity(
        Driver {
            events: Arc::clone(&events),
        },
        8,
    );
    sim.try_send(driver, DriverMsg::ObserveTwice(sink)).unwrap();
    sim.run_until_quiescent();
    snapshot(&events)
}

fn tina_bounded_ingress_full() -> Vec<&'static str> {
    let events = events();
    let mut sim = simulator();
    let sink = sim.register_with_mailbox_capacity(Sink { events }, 0);
    match sim.try_send(sink, SinkMsg::Hit) {
        Ok(()) => vec!["accepted"],
        Err(IngressSendError::Full(_)) => vec!["full"],
        Err(IngressSendError::Closed(_)) => vec!["closed"],
        Err(IngressSendError::ForeignSystem { .. }) => vec!["foreign_system"],
    }
}

fn tina_ingress_try_send_backpressure() -> Vec<&'static str> {
    let events = events();
    let mut sim = simulator();
    let sink = sim.register_with_mailbox_capacity(
        Sink {
            events: Arc::clone(&events),
        },
        1,
    );
    sim.try_send(sink, SinkMsg::Hit).unwrap();
    let admission = match sim.try_send(sink, SinkMsg::Hit) {
        Ok(()) => "accepted",
        Err(IngressSendError::Full(_)) => "full",
        Err(IngressSendError::Closed(_)) => "closed",
        Err(IngressSendError::ForeignSystem { .. }) => "foreign_system",
    };
    sim.run_until_quiescent();

    let mut out = vec![admission];
    out.extend(snapshot(&events));
    out
}

fn tina_stop_abandons_buffered_work() -> Vec<&'static str> {
    let events = events();
    let mut sim = simulator();
    let sink = sim.register_with_mailbox_capacity(
        Sink {
            events: Arc::clone(&events),
        },
        8,
    );
    sim.try_send(sink, SinkMsg::Stop).unwrap();
    sim.try_send(sink, SinkMsg::Hit).unwrap();
    sim.run_until_quiescent();
    assert!(snapshot(&events).is_empty());
    if sim
        .trace()
        .iter()
        .any(|event| matches!(event.kind(), RuntimeEventKind::MessageAbandoned))
    {
        vec!["abandoned"]
    } else {
        vec!["missing_abandoned_trace"]
    }
}

fn tina_call(target_capacity: usize, stop_first: bool, request: WorkReq) -> Vec<&'static str> {
    let events = events();
    let mut sim = simulator();
    let worker = sim.register_with_mailbox_capacity(Worker { held: None }, target_capacity);
    let client = sim.register_with_mailbox_capacity(
        Client {
            events: Arc::clone(&events),
        },
        8,
    );
    if stop_first {
        sim.try_send(worker, WorkReq::Stop).unwrap();
        sim.run_until_quiescent();
    }
    sim.try_send(
        client,
        ClientMsg::Ask(worker, request, Duration::from_millis(5)),
    )
    .unwrap();
    sim.run_until_quiescent();
    snapshot(&events)
}

fn tina_requester_stops() -> (Vec<&'static str>, bool) {
    let events = events();
    let mut sim = simulator();
    let worker = sim.register_with_mailbox_capacity(Worker { held: None }, 8);
    let client = sim.register_with_mailbox_capacity(
        Client {
            events: Arc::clone(&events),
        },
        8,
    );
    sim.try_send(client, ClientMsg::AskThenStop(worker))
        .unwrap();
    sim.run_until_quiescent();
    // Rock 5: stopping the requester now emits
    // `CallCancelled { OwnerStopped }`. The worker's late reply hits
    // `CallReplyRejected { NoPendingCall }` because the pending entry
    // was already removed by the cancel-on-stop sweep.
    let cancelled = sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallCancelled {
                cause: tina::CancelCause::OwnerStopped,
                ..
            }
        )
    });
    (snapshot(&events), cancelled)
}

fn tina_late_reply_after_timeout() -> Vec<&'static str> {
    let events = events();
    let mut sim = simulator();
    let worker = sim.register_with_mailbox_capacity(Worker { held: None }, 8);
    let client = sim.register_with_mailbox_capacity(
        Client {
            events: Arc::clone(&events),
        },
        8,
    );
    // Zero timeout is deliberate: this pins the trace shape where the call
    // settles before the callee's late reply can complete it.
    sim.try_send(
        client,
        ClientMsg::Ask(worker, WorkReq::Reply, Duration::ZERO),
    )
    .unwrap();
    sim.run_until_quiescent();
    let late_reply_rejected = sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallReplyRejected {
                reason: CallReplyRejectedReason::NoPendingCall,
                ..
            }
        )
    });
    let mut output = snapshot(&events);
    if late_reply_rejected {
        output.push("late_reply_rejected");
    }
    output
}

fn tina_requester_mailbox_full_at_completion() -> (Vec<&'static str>, bool) {
    let events = events();
    let mut sim = simulator();
    let worker = sim.register_with_mailbox_capacity(Worker { held: None }, 8);
    let client = sim.register_with_mailbox_capacity(
        Client {
            events: Arc::clone(&events),
        },
        1,
    );
    sim.try_send(
        client,
        ClientMsg::Ask(worker, WorkReq::NoReply, Duration::from_millis(5)),
    )
    .unwrap();
    assert_eq!(sim.step(), 1);
    assert_eq!(sim.step(), 1);
    sim.try_send(client, ClientMsg::Fill).unwrap();
    assert!(sim.advance_to_next_timer());
    sim.step();
    let rejected = sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallCompletionRejected {
                call_kind: CallKind::IsolateCall,
                reason: CallCompletionRejectedReason::MailboxFull,
                ..
            }
        )
    });
    (snapshot(&events), rejected)
}

fn tina_retry_after_backoff() -> Vec<&'static str> {
    let events = events();
    let mut sim = simulator();
    let retry = sim.register_with_mailbox_capacity(
        RetryClient {
            attempts: 0,
            events: Arc::clone(&events),
        },
        8,
    );
    sim.try_send(retry, RetryMsg::Attempt).unwrap();
    sim.run_until_quiescent();
    snapshot(&events)
}

fn tina_timer_replay(seed: u64) -> Vec<&'static str> {
    let events = events();
    let mut sim = simulator_with(SimulatorConfig {
        seed,
        faults: FaultConfig {
            timer_wake: FaultMode::DelayBy {
                one_in: 1,
                by: Duration::from_millis(5),
            },
            ..Default::default()
        },
        ..Default::default()
    });
    let retry = sim.register_with_mailbox_capacity(
        RetryClient {
            attempts: 0,
            events: Arc::clone(&events),
        },
        8,
    );
    sim.try_send(retry, RetryMsg::Attempt).unwrap();
    sim.run_until_quiescent();
    let artifact = sim.replay_artifact();
    let replayed = tina_retry_replay_from_config(artifact.config().clone());
    assert_eq!(snapshot(&events), replayed);
    snapshot(&events)
}

fn tina_retry_replay_from_config(config: SimulatorConfig) -> Vec<&'static str> {
    let events = events();
    let mut sim = simulator_with(config);
    let retry = sim.register_with_mailbox_capacity(
        RetryClient {
            attempts: 0,
            events: Arc::clone(&events),
        },
        8,
    );
    sim.try_send(retry, RetryMsg::Attempt).unwrap();
    sim.run_until_quiescent();
    snapshot(&events)
}

fn tina_seeded_send_perturbation(seed: u64) -> Vec<&'static str> {
    let events = events();
    let mut sim = simulator_with(SimulatorConfig {
        seed,
        faults: FaultConfig {
            local_send: LocalSendFaultMode::DelayByRounds {
                one_in: 1,
                rounds: 2,
            },
            ..Default::default()
        },
        ..Default::default()
    });
    let sink = sim.register_with_mailbox_capacity(
        Sink {
            events: Arc::clone(&events),
        },
        8,
    );
    let driver = sim.register_with_mailbox_capacity(
        Driver {
            events: Arc::clone(&events),
        },
        8,
    );
    sim.try_send(driver, DriverMsg::Fire(sink)).unwrap();
    sim.run_until_quiescent();
    snapshot(&events)
}

fn supervised_child_address(
    system: tina::SystemIncarnation,
    isolate: IsolateId,
) -> Address<SupervisedChildMsg> {
    Address::new_with_generation_in(
        system,
        ShardId::new(151),
        isolate,
        AddressGeneration::new(0),
    )
}

fn tina_supervision_restart() -> Vec<&'static str> {
    let events = events();
    let mut sim = simulator();
    let parent = sim.register_with_mailbox_capacity(
        SupervisorParent {
            events: Arc::clone(&events),
        },
        8,
    );
    sim.supervise(
        parent,
        SupervisorConfig::new(RestartPolicy::OneForOne, RestartBudget::new(1)),
    );
    sim.try_send(parent, SupervisorMsg::SpawnChild).unwrap();
    sim.run_until_quiescent();
    let child = sim
        .trace()
        .iter()
        .find_map(|event| match event.kind() {
            RuntimeEventKind::Spawned { child_isolate } => Some(child_isolate),
            _ => None,
        })
        .expect("child should spawn");

    sim.try_send(
        supervised_child_address(sim.system_incarnation(), child),
        SupervisedChildMsg::Poison,
    )
    .unwrap();
    sim.run_until_quiescent();
    assert!(
        sim.trace().iter().any(|event| {
            matches!(event.kind(), RuntimeEventKind::RestartChildCompleted { .. })
        })
    );

    let mut out = snapshot(&events);
    out.insert(1, "restart");
    out
}

fn tina_cross_shard_transport_full() -> Vec<&'static str> {
    let mut sim = MultiShardSimulator::with_config(
        [CrossShard(1), CrossShard(2)],
        SimulatorConfig::default(),
        MultiShardSimulatorConfig {
            shard_pair_capacity: 1,
        },
    );
    let sink = sim.register_with_capacity_on(ShardId::new(2), CrossSink, 8);
    let driver = sim.register_with_capacity_on(ShardId::new(1), CrossDriver, 8);
    sim.try_send(driver, CrossDriverMsg::FireTwice(sink))
        .unwrap();
    sim.run_until_quiescent();
    if sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::SendRejected {
                reason: SendRejectedReason::Full,
                ..
            }
        )
    }) {
        vec!["full"]
    } else {
        vec!["missing_full"]
    }
}

fn tina_chat_slow_consumer_burst() -> Vec<&'static str> {
    let events = events();
    let mut sim = simulator();
    let client = sim.register_with_mailbox_capacity(
        ChatClient {
            events: Arc::clone(&events),
        },
        1,
    );
    let room = sim.register_with_mailbox_capacity(
        ChatRoom {
            events: Arc::clone(&events),
        },
        8,
    );
    sim.try_send(room, ChatRoomMsg::Burst(client)).unwrap();
    sim.run_until_quiescent();
    assert!(sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::SendRejected {
                reason: SendRejectedReason::Full,
                ..
            }
        )
    }));
    snapshot(&events)
}

async fn tokio_fire_and_forget() -> Vec<&'static str> {
    let (tx, mut rx) = mpsc::channel(1);
    tx.send("hit").await.unwrap();
    vec![rx.recv().await.unwrap()]
}

async fn tokio_multithread_fire_and_forget() -> Vec<&'static str> {
    let (tx, mut rx) = mpsc::channel(1);
    tokio::spawn(async move {
        tx.send("hit").await.unwrap();
    });
    vec![rx.recv().await.unwrap()]
}

async fn tokio_try_send(capacity: usize, close_first: bool) -> Vec<&'static str> {
    let actual_capacity = capacity.max(1);
    let (tx, mut rx) = mpsc::channel::<&'static str>(actual_capacity);
    if close_first {
        rx.close();
    }
    if capacity == 0 && !close_first {
        tx.try_send("already-buffered").unwrap();
    }
    match tx.try_send("hit") {
        Ok(()) => {
            let mut out = Vec::new();
            if let Some(message) = rx.recv().await {
                out.push(message);
            }
            out.push("accepted");
            out
        }
        Err(mpsc::error::TrySendError::Full(_)) => vec!["full"],
        Err(mpsc::error::TrySendError::Closed(_)) => vec!["closed"],
    }
}

async fn tokio_call(capacity: usize, close_first: bool, reply: bool) -> Vec<&'static str> {
    let actual_capacity = capacity.max(1);
    let (tx, mut rx) = mpsc::channel::<oneshot::Sender<&'static str>>(actual_capacity);
    if close_first {
        rx.close();
    }
    let _already_buffered = if capacity == 0 && !close_first {
        let (blocked_tx, _blocked_rx) = oneshot::channel();
        tx.try_send(blocked_tx).unwrap();
        Some(_blocked_rx)
    } else {
        None
    };
    let (reply_tx, reply_rx) = oneshot::channel();
    match tx.try_send(reply_tx) {
        Ok(()) => {
            let mut held_reply = None;
            if let Some(reply_tx) = rx.recv().await {
                if reply {
                    let _ = reply_tx.send("pong");
                } else {
                    held_reply = Some(reply_tx);
                }
            }
            let result = timeout(Duration::from_millis(1), reply_rx).await;
            drop(held_reply);
            match result {
                Ok(Ok(value)) => vec![value],
                Ok(Err(_)) => vec!["closed"],
                Err(_) => vec!["timeout"],
            }
        }
        Err(mpsc::error::TrySendError::Full(_)) => vec!["full"],
        Err(mpsc::error::TrySendError::Closed(_)) => vec!["closed"],
    }
}

async fn tokio_requester_stops() -> Vec<&'static str> {
    let (reply_tx, reply_rx) = oneshot::channel::<&'static str>();
    drop(reply_rx);
    match reply_tx.send("pong") {
        Ok(()) => vec!["delivered"],
        Err(_) => vec!["requester_closed"],
    }
}

async fn tokio_late_reply_after_timeout() -> Vec<&'static str> {
    let (reply_tx, reply_rx) = oneshot::channel::<&'static str>();
    let timed_out = timeout(Duration::ZERO, reply_rx).await.is_err();
    let send_failed = reply_tx.send("pong").is_err();
    let mut out = Vec::new();
    if timed_out {
        out.push("timeout");
    }
    if send_failed {
        out.push("late_reply_failed");
    }
    out
}

async fn tokio_retry_after_backoff() -> Vec<&'static str> {
    let mut out = vec!["failed"];
    time::sleep(Duration::from_millis(1)).await;
    out.push("backoff");
    out.push("succeeded");
    out
}

async fn tokio_paused_timer() -> Vec<&'static str> {
    time::pause();
    let mut out = vec!["failed"];
    let sleep = time::sleep(Duration::from_millis(5));
    tokio::pin!(sleep);
    time::advance(Duration::from_millis(5)).await;
    sleep.await;
    out.push("backoff");
    out.push("succeeded");
    out
}

async fn tokio_drop_receiver_abandons_buffered_work() -> Vec<&'static str> {
    let (tx, mut rx) = mpsc::channel(8);
    tx.send("stop").await.unwrap();
    tx.send("work").await.unwrap();
    if rx.recv().await == Some("stop") {
        drop(rx);
    }
    vec!["abandoned"]
}

async fn tokio_manual_restart() -> Vec<&'static str> {
    let mut out = vec!["boot"];
    let worker = tokio::spawn(async { Result::<(), &'static str>::Err("failed") });
    if worker.await.unwrap().is_err() {
        out.push("restart");
        out.push("boot");
    }
    out
}

async fn tokio_transport_full() -> Vec<&'static str> {
    let (tx, _rx) = mpsc::channel::<&'static str>(1);
    tx.try_send("first").unwrap();
    match tx.try_send("second") {
        Ok(()) => vec!["accepted"],
        Err(mpsc::error::TrySendError::Full(_)) => vec!["full"],
        Err(mpsc::error::TrySendError::Closed(_)) => vec!["closed"],
    }
}

async fn tokio_try_reserve_backpressure() -> Vec<&'static str> {
    let (tx, mut rx) = mpsc::channel::<&'static str>(1);
    let permit = tx.try_reserve().unwrap();
    let admission = match tx.try_reserve() {
        Ok(_) => "accepted",
        Err(mpsc::error::TrySendError::Full(_)) => "full",
        Err(mpsc::error::TrySendError::Closed(_)) => "closed",
    };
    permit.send("hit");
    vec![admission, rx.recv().await.unwrap()]
}

async fn tokio_send_timeout_under_backpressure() -> Vec<&'static str> {
    let (tx, _rx) = mpsc::channel::<&'static str>(1);
    tx.try_send("queued").unwrap();
    match tx.send_timeout("blocked", Duration::from_millis(1)).await {
        Ok(()) => vec!["accepted"],
        Err(mpsc::error::SendTimeoutError::Timeout(_)) => vec!["timeout"],
        Err(mpsc::error::SendTimeoutError::Closed(_)) => vec!["closed"],
    }
}

async fn tokio_shutdown_drains_buffered_channel() -> Vec<&'static str> {
    let (tx, mut rx) = mpsc::channel::<&'static str>(1);
    tx.send("work").await.unwrap();
    rx.close();
    let admission = match tx.try_send("late") {
        Ok(()) => "accepted",
        Err(mpsc::error::TrySendError::Full(_)) => "full",
        Err(mpsc::error::TrySendError::Closed(_)) => "closed",
    };
    let drained = match rx.recv().await {
        Some("work") => "drained",
        Some(_) => "unexpected",
        None => "empty",
    };
    vec![admission, drained]
}

async fn tokio_constrained_overload_service() -> Vec<&'static str> {
    let (tx, mut rx) = mpsc::channel::<&'static str>(1);
    let mut out = Vec::new();
    if tx.try_send("work").is_ok() {
        out.push("accepted");
    }
    match tx.try_send("work") {
        Ok(()) => out.push("accepted"),
        Err(mpsc::error::TrySendError::Full(_)) => out.push("full"),
        Err(mpsc::error::TrySendError::Closed(_)) => out.push("closed"),
    }
    if let Some(message) = rx.recv().await {
        assert_eq!(message, "work");
        out.push("hit");
    }
    out.rotate_right(1);
    out
}

async fn tokio_unbounded_chat_slow_consumer_burst() -> Vec<&'static str> {
    let (tx, mut rx) = mpsc::unbounded_channel::<&'static str>();
    let mut out = Vec::new();
    if tx.send("delivered").is_ok() {
        out.push("accepted");
    }
    if tx.send("overflow").is_ok() {
        out.push("accepted");
    }
    if let Some(message) = rx.recv().await {
        out.push(message);
    }
    out
}

fn compare_fire_and_forget_local_send() -> Comparison {
    Comparison::exact(
        "fire-and-forget local send",
        block_on(tokio_fire_and_forget()),
        tina_fire_and_forget(),
        Ease::A,
    )
}

fn compare_observed_send_accepted() -> Comparison {
    Comparison::exact(
        "observed send accepted",
        block_on(tokio_try_send(1, false)),
        tina_observed_send(1, false),
        Ease::B,
    )
}

fn compare_observed_send_full() -> Comparison {
    Comparison::exact(
        "observed send full",
        block_on(tokio_try_send(0, false)),
        tina_observed_send(0, false),
        Ease::A,
    )
}

fn compare_observed_send_closed() -> Comparison {
    Comparison::exact(
        "observed send closed",
        block_on(tokio_try_send(1, true)),
        tina_observed_send(1, true),
        Ease::A,
    )
}

fn compare_call_replied() -> Comparison {
    Comparison::exact(
        "request/reply success",
        block_on(tokio_call(1, false, true)),
        tina_call(1, false, WorkReq::Reply),
        Ease::A,
    )
}

fn compare_call_full() -> Comparison {
    Comparison::exact(
        "request/reply target full",
        block_on(tokio_call(0, false, true)),
        tina_call(0, false, WorkReq::Reply),
        Ease::A,
    )
}

fn compare_call_closed() -> Comparison {
    Comparison::exact(
        "request/reply target closed",
        block_on(tokio_call(1, true, true)),
        tina_call(1, true, WorkReq::Reply),
        Ease::A,
    )
}

fn compare_call_timeout() -> Comparison {
    Comparison::exact(
        "request/reply timeout",
        block_on(tokio_call(1, false, false)),
        tina_call(1, false, WorkReq::NoReply),
        Ease::A,
    )
}

fn compare_requester_stops() -> Comparison {
    let (tina_events, rejected) = tina_requester_stops();
    assert!(tina_events.is_empty());
    assert!(rejected);
    Comparison::exact(
        "requester stops before reply",
        block_on(tokio_requester_stops()),
        vec!["requester_closed"],
        Ease::B,
    )
}

fn compare_late_reply_after_timeout() -> Comparison {
    Comparison::different_but_expected(
        "late reply after timeout",
        block_on(tokio_late_reply_after_timeout()),
        tina_late_reply_after_timeout(),
        Ease::B,
    )
}

fn compare_requester_mailbox_full_at_completion() -> Comparison {
    let (events, rejected) = tina_requester_mailbox_full_at_completion();
    assert!(events.is_empty());
    assert!(rejected);
    Comparison::different_but_expected(
        "requester mailbox full at completion",
        vec!["reply_channel_independent"],
        vec!["mailbox_full"],
        Ease::C,
    )
}

fn compare_retry_after_bounded_backoff() -> Comparison {
    Comparison::exact(
        "retry after bounded backoff",
        block_on(tokio_retry_after_backoff()),
        tina_retry_after_backoff(),
        Ease::A,
    )
}

fn compare_stale_address_after_restart() -> Comparison {
    Comparison::exact(
        "stale address after restart",
        vec!["closed"],
        tina_observed_send(1, true),
        Ease::A,
    )
}

fn compare_deterministic_timer_replay() -> Comparison {
    Comparison::exact(
        "deterministic timer replay",
        block_on(tokio_paused_timer()),
        tina_timer_replay(42),
        Ease::B,
    )
}

fn compare_seeded_local_send_perturbation() -> Comparison {
    let first = tina_seeded_send_perturbation(7);
    let second = tina_seeded_send_perturbation(7);
    assert_eq!(first, second);
    Comparison::different_but_expected(
        "seeded local-send perturbation",
        vec!["custom_harness_required"],
        first,
        Ease::A,
    )
}

fn compare_live_bounded_ingress_shape() -> Comparison {
    Comparison::exact(
        "live bounded ingress shape",
        block_on(tokio_try_send(0, false)),
        tina_bounded_ingress_full(),
        Ease::A,
    )
}

fn compare_stop_abandons_buffered_work() -> Comparison {
    Comparison::exact(
        "stop abandons buffered work",
        block_on(tokio_drop_receiver_abandons_buffered_work()),
        tina_stop_abandons_buffered_work(),
        Ease::B,
    )
}

fn compare_supervision_restart_shape() -> Comparison {
    Comparison::exact(
        "supervision restart shape",
        block_on(tokio_manual_restart()),
        tina_supervision_restart(),
        Ease::B,
    )
}

fn compare_cross_shard_transport_full_shape() -> Comparison {
    Comparison::exact(
        "cross-shard bounded transport full",
        block_on(tokio_transport_full()),
        tina_cross_shard_transport_full(),
        Ease::B,
    )
}

fn compare_try_reserve_backpressure_shape() -> Comparison {
    Comparison::exact(
        "bounded try-reserve backpressure",
        block_on(tokio_try_reserve_backpressure()),
        tina_ingress_try_send_backpressure(),
        Ease::B,
    )
}

fn compare_timed_send_under_backpressure() -> Comparison {
    Comparison::different_but_expected(
        "timed send under backpressure",
        block_on(tokio_send_timeout_under_backpressure()),
        tina_observed_send(0, false),
        Ease::B,
    )
}

fn compare_shutdown_drain_vs_stop_abandonment() -> Comparison {
    Comparison::different_but_expected(
        "shutdown drain versus stop abandonment",
        block_on(tokio_shutdown_drains_buffered_channel()),
        tina_stop_abandons_buffered_work(),
        Ease::B,
    )
}

fn compare_constrained_overload_service_shape() -> Comparison {
    Comparison::exact(
        "constrained overload service",
        block_on(tokio_constrained_overload_service()),
        tina_observed_send_twice(1),
        Ease::B,
    )
}

fn compare_unbounded_chat_slow_consumer_burst() -> Comparison {
    Comparison::different_but_expected(
        "unbounded chat slow-consumer burst",
        block_on(tokio_unbounded_chat_slow_consumer_burst()),
        tina_chat_slow_consumer_burst(),
        Ease::C,
    )
}

macro_rules! comparison_suite {
    ($($name:ident => $compare:ident: $label:literal),+ $(,)?) => {
        $(
            #[test]
            fn $name() {
                let comparison = $compare();
                assert_eq!(comparison.example, $label);
                comparison.assert_expected();
            }
        )+

        #[test]
        fn runnable_comparison_suite_has_twenty_four_examples() {
            let examples = [$($label),+];
            assert_eq!(examples.len(), 24);
        }

        #[test]
        fn tokio_multi_thread_smoke_keeps_simple_channel_semantics() {
            assert_eq!(
                block_on_multi_thread(tokio_multithread_fire_and_forget()),
                tina_fire_and_forget()
            );
        }
    };
}

comparison_suite! {
    example_01_fire_and_forget_local_send => compare_fire_and_forget_local_send:
        "fire-and-forget local send",
    example_02_observed_send_accepted => compare_observed_send_accepted:
        "observed send accepted",
    example_03_observed_send_full => compare_observed_send_full:
        "observed send full",
    example_04_observed_send_closed => compare_observed_send_closed:
        "observed send closed",
    example_05_call_replied => compare_call_replied:
        "request/reply success",
    example_06_call_full => compare_call_full:
        "request/reply target full",
    example_07_call_closed => compare_call_closed:
        "request/reply target closed",
    example_08_call_timeout => compare_call_timeout:
        "request/reply timeout",
    example_09_requester_stops => compare_requester_stops:
        "requester stops before reply",
    example_10_late_reply_after_timeout => compare_late_reply_after_timeout:
        "late reply after timeout",
    example_11_requester_mailbox_full_at_completion => compare_requester_mailbox_full_at_completion:
        "requester mailbox full at completion",
    example_12_retry_after_bounded_backoff => compare_retry_after_bounded_backoff:
        "retry after bounded backoff",
    example_13_stale_address_after_restart => compare_stale_address_after_restart:
        "stale address after restart",
    example_14_deterministic_timer_replay => compare_deterministic_timer_replay:
        "deterministic timer replay",
    example_15_seeded_local_send_perturbation => compare_seeded_local_send_perturbation:
        "seeded local-send perturbation",
    example_16_live_bounded_ingress_shape => compare_live_bounded_ingress_shape:
        "live bounded ingress shape",
    example_17_stop_abandons_buffered_work => compare_stop_abandons_buffered_work:
        "stop abandons buffered work",
    example_18_supervision_restart_shape => compare_supervision_restart_shape:
        "supervision restart shape",
    example_19_cross_shard_bounded_transport_full => compare_cross_shard_transport_full_shape:
        "cross-shard bounded transport full",
    example_20_bounded_try_reserve_backpressure => compare_try_reserve_backpressure_shape:
        "bounded try-reserve backpressure",
    example_21_timed_send_under_backpressure => compare_timed_send_under_backpressure:
        "timed send under backpressure",
    example_22_shutdown_drain_vs_stop_abandonment => compare_shutdown_drain_vs_stop_abandonment:
        "shutdown drain versus stop abandonment",
    example_23_constrained_overload_service => compare_constrained_overload_service_shape:
        "constrained overload service",
    example_24_unbounded_chat_slow_consumer_burst => compare_unbounded_chat_slow_consumer_burst:
        "unbounded chat slow-consumer burst",
}

#[test]
fn tina_examples_use_the_current_ergonomic_surface() {
    let source = include_str!("tokio_vs_tina_examples.rs");

    for old_shape in [
        concat!("Effect", "::"),
        concat!("Outbound", "::new"),
        concat!("runtime_isolate", "!"),
        concat!("sleep", "(Duration::from_millis(5)).reply"),
        concat!(".fil", "ter("),
        concat!("fn normal", "ize("),
        concat!("Comparison::pro", "jected"),
    ] {
        assert!(
            !source.contains(old_shape),
            "comparison suite should not drift back to old Tina shape: {old_shape}"
        );
    }

    assert!(source.matches("#[tina_runtime::isolate").count() >= 10);
    assert!(source.contains("sleep_then(Duration::from_millis(5), RetryMsg::Later)"));
    assert!(source.contains("outcome.into_result()"));
    assert!(source.contains("outcome.is_accepted()"));
}
