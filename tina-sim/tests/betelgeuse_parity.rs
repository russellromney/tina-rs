use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tina::{Mailbox, TrySendError, prelude::*};
use tina_runtime::{
    CallKind, MailboxFactory, Runtime, RuntimeCall, RuntimeEvent, RuntimeEventKind,
    SendRejectedReason, ThreadedRuntime, sleep_then,
};
use tina_sim::{Simulator, SimulatorConfig, dst::assert_projection_eq};

#[derive(Debug, Clone, Copy)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(91)
    }
}

struct TestMailbox<T> {
    capacity: usize,
    queue: Mutex<VecDeque<T>>,
    closed: Mutex<bool>,
}

impl<T> TestMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: Mutex::new(VecDeque::new()),
            closed: Mutex::new(false),
        }
    }
}

impl<T> Mailbox<T> for TestMailbox<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }

    fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        if *self.closed.lock().expect("closed mutex") {
            return Err(TrySendError::Closed(message));
        }
        let mut queue = self.queue.lock().expect("queue mutex");
        if queue.len() >= self.capacity {
            return Err(TrySendError::Full(message));
        }
        queue.push_back(message);
        Ok(())
    }

    fn recv(&self) -> Option<T> {
        self.queue.lock().expect("queue mutex").pop_front()
    }

    fn close(&self) {
        *self.closed.lock().expect("closed mutex") = true;
    }
}

#[derive(Debug, Clone, Copy)]
struct TestMailboxFactory;

impl MailboxFactory for TestMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(TestMailbox::new(capacity))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryMsg {
    TryWork,
    RetryNow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryObservation {
    Attempted(usize),
    Failed(usize),
    BackoffElapsed,
    Succeeded(usize),
}

#[derive(Debug)]
struct RetryWorker {
    attempts: usize,
    observations: Arc<Mutex<Vec<RetryObservation>>>,
}

impl Isolate for RetryWorker {
    tina::isolate_types! {
        message: RetryMsg,
        reply: (),
        send: Outbound<RetryMsg>,
        spawn: Infallible,
        call: RuntimeCall<RetryMsg>,
        shard: TestShard,
    }

    fn handle(&mut self, msg: Self::Message, ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            RetryMsg::TryWork => {
                self.attempts += 1;
                self.observations
                    .lock()
                    .expect("observations mutex")
                    .push(RetryObservation::Attempted(self.attempts));
                if self.attempts == 1 {
                    self.observations
                        .lock()
                        .expect("observations mutex")
                        .push(RetryObservation::Failed(self.attempts));
                    sleep_then(Duration::ZERO, RetryMsg::RetryNow)
                } else {
                    self.observations
                        .lock()
                        .expect("observations mutex")
                        .push(RetryObservation::Succeeded(self.attempts));
                    noop()
                }
            }
            RetryMsg::RetryNow => {
                self.observations
                    .lock()
                    .expect("observations mutex")
                    .push(RetryObservation::BackoffElapsed);
                ctx.send_self(RetryMsg::TryWork)
            }
        }
    }
}

fn expected_observations() -> Vec<RetryObservation> {
    vec![
        RetryObservation::Attempted(1),
        RetryObservation::Failed(1),
        RetryObservation::BackoffElapsed,
        RetryObservation::Attempted(2),
        RetryObservation::Succeeded(2),
    ]
}

fn count_sleep_completed(trace: &[RuntimeEvent]) -> usize {
    trace
        .iter()
        .filter(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallCompleted {
                    call_kind: CallKind::Sleep,
                    ..
                }
            )
        })
        .count()
}

fn run_oracle() -> (Vec<RetryObservation>, Vec<RuntimeEvent>) {
    let observations = Arc::new(Mutex::new(Vec::new()));
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let worker = runtime.register_with_capacity::<RetryWorker, RetryMsg>(
        RetryWorker {
            attempts: 0,
            observations: Arc::clone(&observations),
        },
        8,
    );
    runtime.try_send(worker, RetryMsg::TryWork).unwrap();

    let deadline = Instant::now() + Duration::from_secs(2);
    while observations.lock().expect("observations mutex").as_slice()
        != expected_observations().as_slice()
    {
        if Instant::now() > deadline {
            panic!(
                "oracle retry workload did not finish: {:#?}",
                runtime.trace()
            );
        }
        runtime.step();
        thread::yield_now();
    }

    (
        observations.lock().expect("observations mutex").clone(),
        runtime.trace().to_vec(),
    )
}

fn run_simulator() -> (Vec<RetryObservation>, Vec<RuntimeEvent>) {
    let observations = Arc::new(Mutex::new(Vec::new()));
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            seed: 25,
            ..Default::default()
        },
    );
    let worker = sim.register(RetryWorker {
        attempts: 0,
        observations: Arc::clone(&observations),
    });
    sim.try_send(worker, RetryMsg::TryWork).unwrap();
    sim.run_until_quiescent();
    (
        observations.lock().expect("observations mutex").clone(),
        sim.trace().to_vec(),
    )
}

fn run_betelgeuse() -> (Vec<RetryObservation>, Vec<RuntimeEvent>) {
    let observations = Arc::new(Mutex::new(Vec::new()));
    let runtime = ThreadedRuntime::new(TestShard, TestMailboxFactory);
    let worker = runtime
        .register_with_capacity::<RetryWorker, RetryMsg>(
            RetryWorker {
                attempts: 0,
                observations: Arc::clone(&observations),
            },
            8,
        )
        .expect("worker register accepts");
    runtime
        .try_send(worker, RetryMsg::TryWork)
        .expect("retry start accepts");

    let deadline = Instant::now() + Duration::from_secs(2);
    while observations.lock().expect("observations mutex").as_slice()
        != expected_observations().as_slice()
    {
        if Instant::now() > deadline {
            panic!(
                "Betelgeuse retry workload did not finish: {:#?}",
                runtime.trace()
            );
        }
        thread::yield_now();
    }

    (
        observations.lock().expect("observations mutex").clone(),
        runtime.shutdown().expect("Betelgeuse shutdown"),
    )
}

#[test]
fn retry_timer_workload_matches_oracle_simulator_and_betelgeuse_runner() {
    let (oracle_observations, oracle_trace) = run_oracle();
    let (sim_observations, sim_trace) = run_simulator();
    let (betelgeuse_observations, betelgeuse_trace) = run_betelgeuse();

    assert_eq!(oracle_observations, expected_observations());
    assert_projection_eq("oracle", &oracle_observations, "sim", &sim_observations);
    assert_projection_eq(
        "oracle",
        &oracle_observations,
        "betelgeuse",
        &betelgeuse_observations,
    );
    assert_projection_eq(
        "oracle sleep completions",
        &count_sleep_completed(&oracle_trace),
        "sim sleep completions",
        &count_sleep_completed(&sim_trace),
    );
    assert_projection_eq(
        "oracle sleep completions",
        &count_sleep_completed(&oracle_trace),
        "betelgeuse sleep completions",
        &count_sleep_completed(&betelgeuse_trace),
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParityTargetMsg {
    Data(u8),
    Stop,
}

#[derive(Debug)]
struct ParityTarget {
    observed: Arc<Mutex<Vec<u8>>>,
}

#[tina_runtime::isolate(message = ParityTargetMsg, shard = TestShard)]
impl ParityTarget {
    fn handle(&mut self, msg: ParityTargetMsg, _ctx: &mut Context<'_, TestShard>) -> Effect<Self> {
        match msg {
            ParityTargetMsg::Data(value) => {
                self.observed.lock().expect("observed mutex").push(value);
                noop()
            }
            ParityTargetMsg::Stop => stop(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParityDriverMsg {
    Send(u8),
    StopTarget,
}

#[derive(Debug)]
struct ParityDriver {
    target: Address<ParityTargetMsg>,
}

#[tina_runtime::isolate(
    message = ParityDriverMsg,
    send = Outbound<ParityTargetMsg>,
    shard = TestShard
)]
impl ParityDriver {
    fn handle(&mut self, msg: ParityDriverMsg, _ctx: &mut Context<'_, TestShard>) -> Effect<Self> {
        match msg {
            ParityDriverMsg::Send(value) => send(self.target, ParityTargetMsg::Data(value)),
            ParityDriverMsg::StopTarget => send(self.target, ParityTargetMsg::Stop),
        }
    }
}

fn parity_ops() -> [ParityDriverMsg; 5] {
    [
        ParityDriverMsg::Send(1),
        ParityDriverMsg::Send(2),
        ParityDriverMsg::StopTarget,
        ParityDriverMsg::Send(3),
        ParityDriverMsg::Send(4),
    ]
}

fn closed_rejections(trace: &[RuntimeEvent]) -> usize {
    trace
        .iter()
        .filter(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::SendRejected {
                    reason: SendRejectedReason::Closed,
                    ..
                }
            )
        })
        .count()
}

fn run_send_stop_oracle() -> (Vec<u8>, Vec<RuntimeEvent>) {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let target = runtime.register_with_capacity::<ParityTarget, Infallible>(
        ParityTarget {
            observed: Arc::clone(&observed),
        },
        4,
    );
    let driver =
        runtime.register_with_capacity::<ParityDriver, ParityTargetMsg>(ParityDriver { target }, 8);
    for op in parity_ops() {
        runtime.try_send(driver, op).unwrap();
    }
    for _ in 0..16 {
        runtime.step();
    }
    (
        observed.lock().expect("observed mutex").clone(),
        runtime.trace().to_vec(),
    )
}

fn run_send_stop_simulator() -> (Vec<u8>, Vec<RuntimeEvent>) {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let mut sim = Simulator::new(TestShard, SimulatorConfig::default());
    let target = sim.register_with_mailbox_capacity::<ParityTarget, ParityTargetMsg, Infallible>(
        ParityTarget {
            observed: Arc::clone(&observed),
        },
        4,
    );
    let driver = sim
        .register_with_mailbox_capacity::<ParityDriver, ParityDriverMsg, ParityTargetMsg>(
            ParityDriver { target },
            8,
        );
    for op in parity_ops() {
        sim.try_send(driver, op).unwrap();
    }
    sim.run_until_quiescent();
    (
        observed.lock().expect("observed mutex").clone(),
        sim.trace().to_vec(),
    )
}

fn run_send_stop_betelgeuse() -> (Vec<u8>, Vec<RuntimeEvent>) {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let runtime = ThreadedRuntime::new(TestShard, TestMailboxFactory);
    let target = runtime
        .register_with_capacity::<ParityTarget, Infallible>(
            ParityTarget {
                observed: Arc::clone(&observed),
            },
            4,
        )
        .expect("target register accepts");
    let driver = runtime
        .register_with_capacity::<ParityDriver, ParityTargetMsg>(ParityDriver { target }, 8)
        .expect("driver register accepts");
    for op in parity_ops() {
        runtime.try_send(driver, op).unwrap();
    }

    let deadline = Instant::now() + Duration::from_secs(2);
    while observed.lock().expect("observed mutex").as_slice() != [1, 2]
        || closed_rejections(runtime.trace().events()) < 2
    {
        if Instant::now() > deadline {
            panic!(
                "Betelgeuse send-stop workload did not settle: {:#?}",
                runtime.trace()
            );
        }
        thread::yield_now();
    }

    (
        observed.lock().expect("observed mutex").clone(),
        runtime.shutdown().expect("Betelgeuse shutdown"),
    )
}

#[test]
fn send_stop_workload_matches_oracle_simulator_and_betelgeuse_runner() {
    let (oracle_observed, oracle_trace) = run_send_stop_oracle();
    let (sim_observed, sim_trace) = run_send_stop_simulator();
    let (betelgeuse_observed, betelgeuse_trace) = run_send_stop_betelgeuse();

    assert_eq!(oracle_observed, vec![1, 2]);
    assert_projection_eq("oracle", &oracle_observed, "sim", &sim_observed);
    assert_projection_eq(
        "oracle",
        &oracle_observed,
        "betelgeuse",
        &betelgeuse_observed,
    );
    assert_projection_eq(
        "oracle closed rejections",
        &closed_rejections(&oracle_trace),
        "sim closed rejections",
        &closed_rejections(&sim_trace),
    );
    assert_projection_eq(
        "oracle closed rejections",
        &closed_rejections(&oracle_trace),
        "betelgeuse closed rejections",
        &closed_rejections(&betelgeuse_trace),
    );
}
