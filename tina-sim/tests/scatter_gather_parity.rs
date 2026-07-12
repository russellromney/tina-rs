//! The same coordinator authoring form runs on Runtime and Simulator.

use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::sharded::{ScatterGatherConfig, ScatterGatherReport, ScatterGatherTargetOutcome};
use tina_runtime::{
    BoundedItems, CallOutcome, DefaultMailboxFactory, DefaultThreadedMailboxFactory,
    MultiShardRuntime, Runtime, ScatterGatherCompleted, ScatterGatherEvent,
    ScatterGatherOperations, ScatterGatherOperationsStart, ThreadedMultiShardRuntime,
    ThreadedRuntime, call_cancelable_request, call_request,
};
use tina_sim::{Simulator, SimulatorConfig};

#[derive(Debug, Clone, Copy)]
struct AppShard(u32);

impl Shard for AppShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

#[derive(Debug, Clone, Copy)]
enum WorkerRequest {
    Read,
}

struct Worker {
    value: Option<u32>,
    held: Vec<RequestContext<u32>>,
}

#[tina_runtime::isolate(request = WorkerRequest, reply = u32, shard = AppShard)]
impl Worker {
    fn handle_request(
        &mut self,
        _request: WorkerRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match self.value {
            Some(value) => call.reply(value),
            None => call.capture(|request| {
                self.held.push(request);
                noop()
            }),
        }
    }
}

type Report = ScatterGatherReport<u32, usize>;

#[derive(Debug)]
enum CoordReply {
    Report(Report),
    StartRejected(String),
}

#[derive(Debug)]
enum CoordRequest {
    ReadAll,
}

#[derive(Debug)]
enum CoordEvent {
    Scatter(ScatterGatherEvent<usize, u32>),
    Stop,
}

struct Coordinator {
    workers: Vec<tina_runtime::RequestServiceHandle<WorkerRequest, u32>>,
    operations: ScatterGatherOperations<usize, u32, CoordReply>,
    started: Arc<AtomicBool>,
    max_live: Arc<AtomicUsize>,
}

const MAX_IN_FLIGHT: usize = 8;

#[tina_runtime::isolate(
    event = CoordEvent,
    request = CoordRequest,
    reply = CoordReply,
    shard = AppShard
)]
impl Coordinator {
    fn handle_event(
        &mut self,
        event: CoordEvent,
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            CoordEvent::Scatter(event) => {
                let Some(advance) = self
                    .operations
                    .advance_service(event, CoordEvent::Scatter)
                    .expect("operation owner issued the continuation")
                else {
                    return noop();
                };
                match advance.completed {
                    Some(completed) => self.complete(completed, advance.effect),
                    None => advance.effect,
                }
            }
            CoordEvent::Stop => stop(),
        }
    }

    fn handle_request(
        &mut self,
        _request: CoordRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        let config = ScatterGatherConfig {
            max_targets: self.workers.len() + 1,
            collector_capacity: self.workers.len() + 1,
            per_target_timeout: Duration::from_millis(50),
            aggregate_timeout: Duration::from_millis(100),
        };
        let targets = BoundedItems::try_from_iter(
            config.max_targets,
            self.workers
                .iter()
                .copied()
                .enumerate()
                .map(|(key, worker)| (key, Some(worker)))
                .chain(std::iter::once((self.workers.len(), None))),
        )
        .expect("service-owned target cap covers workers plus missing probe");

        call.capture(|request| {
            match self.operations.start_service(
                request,
                config,
                targets,
                |worker, timeout| call_cancelable_request(worker, WorkerRequest::Read, timeout),
                CoordEvent::Scatter,
            ) {
                Ok(ScatterGatherOperationsStart::Ready(completed)) => {
                    reply_to(completed.request, CoordReply::Report(completed.report))
                }
                Ok(ScatterGatherOperationsStart::Running(effect)) => {
                    self.started.store(true, Ordering::Release);
                    self.max_live
                        .fetch_max(self.operations.len(), Ordering::AcqRel);
                    effect
                }
                Err(failure) => reply_to(
                    failure.request,
                    CoordReply::StartRejected(failure.error.to_string()),
                ),
            }
        })
    }
}

impl Coordinator {
    fn complete(
        &mut self,
        completed: ScatterGatherCompleted<usize, u32, CoordReply>,
        effect: Effect<Self>,
    ) -> Effect<Self> {
        batch([
            reply_to(completed.request, CoordReply::Report(completed.report)),
            effect,
        ])
    }
}

#[derive(Debug)]
enum ClientMessage {
    Start(tina::ServiceRequestAddress<CoordEvent, CoordRequest, CoordReply>),
    Returned(CallOutcome<CoordReply>),
}

struct Client {
    outcome: Arc<Mutex<Option<CallOutcome<CoordReply>>>>,
}

#[tina_runtime::isolate(message = ClientMessage, shard = AppShard)]
impl Client {
    fn handle(
        &mut self,
        message: ClientMessage,
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        match message {
            ClientMessage::Start(coordinator) => {
                call_request(coordinator, CoordRequest::ReadAll, Duration::from_secs(1))
                    .then(ClientMessage::Returned)
            }
            ClientMessage::Returned(outcome) => {
                *self.outcome.lock().unwrap() = Some(outcome);
                noop()
            }
        }
    }
}

fn assert_report(outcome: CallOutcome<CoordReply>) {
    let CallOutcome::Replied(CoordReply::Report(report)) = outcome else {
        match outcome {
            CallOutcome::Replied(CoordReply::StartRejected(error)) => {
                panic!("scatter start rejected: {error}")
            }
            other => panic!("unexpected coordinator outcome: {other:?}"),
        }
    };
    assert_eq!(
        report.outcomes,
        vec![
            (0, ScatterGatherTargetOutcome::Replied(10)),
            (1, ScatterGatherTargetOutcome::Replied(20)),
            (2, ScatterGatherTargetOutcome::MissingShard),
        ]
    );
}

#[test]
fn explicit_runtime_and_simulator_use_identical_scatter_authoring() {
    let runtime_outcome = Arc::new(Mutex::new(None));
    let mut runtime = Runtime::new(AppShard(0), DefaultMailboxFactory);
    let workers = vec![
        runtime.register_request_service::<_, _, Infallible>(
            Worker {
                value: Some(10),
                held: Vec::new(),
            },
            4,
        ),
        runtime.register_request_service::<_, _, Infallible>(
            Worker {
                value: Some(20),
                held: Vec::new(),
            },
            4,
        ),
    ];
    let coordinator = runtime
        .register_split_service::<Coordinator, CoordEvent, CoordRequest, Infallible>(
            Coordinator {
                workers,
                operations: ScatterGatherOperations::with_capacity(MAX_IN_FLIGHT),
                started: Arc::new(AtomicBool::new(false)),
                max_live: Arc::new(AtomicUsize::new(0)),
            },
            16,
        )
        .requests;
    let client = runtime.register_with_capacity::<_, Infallible>(
        Client {
            outcome: Arc::clone(&runtime_outcome),
        },
        4,
    );
    runtime
        .try_send(client, ClientMessage::Start(coordinator))
        .unwrap();
    while runtime.step() > 0 {}
    assert_report(
        runtime_outcome
            .lock()
            .unwrap()
            .take()
            .expect("runtime report"),
    );

    let sim_outcome = Arc::new(Mutex::new(None));
    let mut sim = Simulator::new(AppShard(0), SimulatorConfig::default());
    let workers = vec![
        sim.register_request_service::<_, _, Infallible>(
            Worker {
                value: Some(10),
                held: Vec::new(),
            },
            4,
        ),
        sim.register_request_service::<_, _, Infallible>(
            Worker {
                value: Some(20),
                held: Vec::new(),
            },
            4,
        ),
    ];
    let coordinator = sim
        .register_split_service::<Coordinator, CoordEvent, CoordRequest, Infallible>(
            Coordinator {
                workers,
                operations: ScatterGatherOperations::with_capacity(MAX_IN_FLIGHT),
                started: Arc::new(AtomicBool::new(false)),
                max_live: Arc::new(AtomicUsize::new(0)),
            },
            16,
        )
        .requests;
    let client = sim.register(Client {
        outcome: Arc::clone(&sim_outcome),
    });
    sim.try_send(client, ClientMessage::Start(coordinator))
        .unwrap();
    while sim.step() > 0 {}
    assert_report(sim_outcome.lock().unwrap().take().expect("sim report"));
}

#[test]
fn public_operation_tokens_route_a_bounded_concurrent_scatter_set() {
    let mut runtime = Runtime::new(AppShard(0), DefaultMailboxFactory);
    let workers = vec![
        runtime.register_request_service::<_, _, Infallible>(
            Worker {
                value: Some(10),
                held: Vec::new(),
            },
            16,
        ),
        runtime.register_request_service::<_, _, Infallible>(
            Worker {
                value: Some(20),
                held: Vec::new(),
            },
            16,
        ),
    ];
    let max_live = Arc::new(AtomicUsize::new(0));
    let coordinator = runtime
        .register_split_service::<Coordinator, CoordEvent, CoordRequest, Infallible>(
            Coordinator {
                workers,
                operations: ScatterGatherOperations::with_capacity(MAX_IN_FLIGHT),
                started: Arc::new(AtomicBool::new(false)),
                max_live: Arc::clone(&max_live),
            },
            64,
        )
        .requests;

    let outcomes: Vec<_> = (0..=MAX_IN_FLIGHT)
        .map(|_| Arc::new(Mutex::new(None)))
        .collect();
    for outcome in &outcomes {
        let client = runtime.register_with_capacity::<_, Infallible>(
            Client {
                outcome: Arc::clone(outcome),
            },
            4,
        );
        runtime
            .try_send(client, ClientMessage::Start(coordinator))
            .unwrap();
    }
    while runtime.step() > 0 {}

    assert_eq!(
        max_live.load(Ordering::Acquire),
        MAX_IN_FLIGHT,
        "all bounded operations must coexist without private qid correlation"
    );
    for outcome in &outcomes[..MAX_IN_FLIGHT] {
        assert_report(outcome.lock().unwrap().take().expect("concurrent report"));
    }
    assert!(matches!(
        outcomes[MAX_IN_FLIGHT].lock().unwrap().take(),
        Some(CallOutcome::Replied(CoordReply::StartRejected(_)))
    ));

    let refill = Arc::new(Mutex::new(None));
    let client = runtime.register_with_capacity::<_, Infallible>(
        Client {
            outcome: Arc::clone(&refill),
        },
        4,
    );
    runtime
        .try_send(client, ClientMessage::Start(coordinator))
        .unwrap();
    while runtime.step() > 0 {}
    assert_report(refill.lock().unwrap().take().expect("refill report"));
}

#[test]
fn owner_stop_closes_original_caller_with_child_authority_pending() {
    let outcome = Arc::new(Mutex::new(None));
    let started = Arc::new(AtomicBool::new(false));
    let mut runtime = Runtime::new(AppShard(0), DefaultMailboxFactory);
    let worker = runtime.register_request_service::<_, _, Infallible>(
        Worker {
            value: None,
            held: Vec::new(),
        },
        4,
    );
    let coordinator = runtime
        .register_split_service::<Coordinator, CoordEvent, CoordRequest, Infallible>(
            Coordinator {
                workers: vec![worker],
                operations: ScatterGatherOperations::with_capacity(MAX_IN_FLIGHT),
                started: Arc::clone(&started),
                max_live: Arc::new(AtomicUsize::new(0)),
            },
            8,
        );
    let client = runtime.register_with_capacity::<_, Infallible>(
        Client {
            outcome: Arc::clone(&outcome),
        },
        4,
    );
    runtime
        .try_send(client, ClientMessage::Start(coordinator.requests))
        .unwrap();
    while !started.load(Ordering::Acquire) {
        assert!(runtime.step() > 0, "coordinator must start scatter");
    }
    runtime
        .try_send_event(coordinator.events, CoordEvent::Stop)
        .unwrap();
    while runtime.step() > 0 {}
    assert!(matches!(
        outcome.lock().unwrap().take(),
        Some(CallOutcome::Closed)
    ));
}

#[test]
fn threaded_runtime_uses_the_same_scatter_coordinator() {
    let outcome = Arc::new(Mutex::new(None));
    let runtime = ThreadedRuntime::new(AppShard(0), DefaultThreadedMailboxFactory);
    let workers = vec![
        runtime
            .register_request_service::<_, _, Infallible>(
                Worker {
                    value: Some(10),
                    held: Vec::new(),
                },
                4,
            )
            .unwrap(),
        runtime
            .register_request_service::<_, _, Infallible>(
                Worker {
                    value: Some(20),
                    held: Vec::new(),
                },
                4,
            )
            .unwrap(),
    ];
    let coordinator = runtime
        .register_split_service::<Coordinator, CoordEvent, CoordRequest, Infallible>(
            Coordinator {
                workers,
                operations: ScatterGatherOperations::with_capacity(MAX_IN_FLIGHT),
                started: Arc::new(AtomicBool::new(false)),
                max_live: Arc::new(AtomicUsize::new(0)),
            },
            16,
        )
        .unwrap()
        .requests;
    let client = runtime
        .register_with_capacity::<_, Infallible>(
            Client {
                outcome: Arc::clone(&outcome),
            },
            4,
        )
        .unwrap();
    runtime
        .try_send(client, ClientMessage::Start(coordinator))
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while outcome.lock().unwrap().is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_report(outcome.lock().unwrap().take().expect("threaded report"));
    runtime.shutdown_report().ensure_clean().unwrap();
}

#[test]
fn explicit_multishard_uses_same_coordinator_for_cross_shard_calls() {
    let outcome = Arc::new(Mutex::new(None));
    let mut runtime = MultiShardRuntime::new([AppShard(11), AppShard(22)], DefaultMailboxFactory);
    let worker = runtime.register_request_service_on(
        ShardId::new(22),
        Worker {
            value: Some(10),
            held: Vec::new(),
        },
        8,
    );
    let coordinator = runtime
        .register_split_service_on(
            ShardId::new(11),
            Coordinator {
                workers: vec![worker],
                operations: ScatterGatherOperations::with_capacity(MAX_IN_FLIGHT),
                started: Arc::new(AtomicBool::new(false)),
                max_live: Arc::new(AtomicUsize::new(0)),
            },
            16,
        )
        .requests;
    let client = runtime.register_with_capacity_on::<_, Infallible>(
        ShardId::new(11),
        Client {
            outcome: Arc::clone(&outcome),
        },
        4,
    );
    runtime
        .try_send(client, ClientMessage::Start(coordinator))
        .unwrap();
    while runtime.step() > 0 {}
    let CallOutcome::Replied(CoordReply::Report(report)) =
        outcome.lock().unwrap().take().expect("multishard report")
    else {
        panic!("cross-shard scatter did not reply")
    };
    assert_eq!(
        report.outcomes,
        vec![
            (0, ScatterGatherTargetOutcome::Replied(10)),
            (1, ScatterGatherTargetOutcome::MissingShard),
        ]
    );
}

#[test]
fn threaded_multishard_uses_same_coordinator_for_cross_shard_calls() {
    let runtime =
        ThreadedMultiShardRuntime::new([AppShard(11), AppShard(22)], DefaultThreadedMailboxFactory);
    let worker = runtime
        .register_request_service_on(
            ShardId::new(22),
            Worker {
                value: Some(10),
                held: Vec::new(),
            },
            8,
        )
        .unwrap();
    let coordinator = runtime
        .register_split_service_on(
            ShardId::new(11),
            Coordinator {
                workers: vec![worker],
                operations: ScatterGatherOperations::with_capacity(MAX_IN_FLIGHT),
                started: Arc::new(AtomicBool::new(false)),
                max_live: Arc::new(AtomicUsize::new(0)),
            },
            16,
        )
        .unwrap()
        .requests;
    let outcome = runtime
        .call_blocking_request(coordinator, CoordRequest::ReadAll, Duration::from_secs(1))
        .unwrap();
    let CallOutcome::Replied(CoordReply::Report(report)) = outcome else {
        panic!("threaded cross-shard scatter did not reply")
    };
    assert_eq!(
        report.outcomes,
        vec![
            (0, ScatterGatherTargetOutcome::Replied(10)),
            (1, ScatterGatherTargetOutcome::MissingShard),
        ]
    );
    runtime.shutdown_report().ensure_clean().unwrap();
}
