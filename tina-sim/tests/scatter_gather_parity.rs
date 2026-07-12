//! The same coordinator authoring form runs on Runtime and Simulator.

use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::sharded::{ScatterGatherConfig, ScatterGatherReport, ScatterGatherTargetOutcome};
use tina_runtime::{
    BoundedItems, CallGroupToken, CallOutcome, DefaultMailboxFactory, Runtime, ScatterGather,
    ScatterGatherCompleted, ScatterGatherStart, ScatterGatherToken, ThreadedRuntime,
    call_cancelable_request, call_request,
};
use tina_sim::{Simulator, SimulatorConfig};

#[derive(Debug, Clone, Copy)]
enum WorkerRequest {
    Read,
}

struct Worker {
    value: Option<u32>,
    held: Vec<RequestContext<u32>>,
}

#[tina_runtime::isolate(request = WorkerRequest, reply = u32)]
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
    Target(usize, CallGroupToken, CallOutcome<u32>),
    AggregateTimeout(ScatterGatherToken),
    Cancelled(usize, CallGroupToken, CancelOutcome),
    Stop,
}

struct Coordinator {
    workers: Vec<tina_runtime::RequestServiceHandle<WorkerRequest, u32>>,
    operation: Option<ScatterGather<usize, u32, CoordReply>>,
    started: Arc<AtomicBool>,
}

#[tina_runtime::isolate(event = CoordEvent, request = CoordRequest, reply = CoordReply)]
impl Coordinator {
    fn handle_event(
        &mut self,
        event: CoordEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        if matches!(event, CoordEvent::Stop) {
            return stop();
        }
        let Some(operation) = self.operation.as_mut() else {
            return noop();
        };
        match event {
            CoordEvent::Target(key, token, outcome) => {
                match operation
                    .record_reply(key, token, outcome)
                    .expect("continuation token came from ScatterGather::start")
                {
                    Some(completed) => self.complete(completed, noop()),
                    None => noop(),
                }
            }
            CoordEvent::AggregateTimeout(token) => {
                let Some(advance) = operation
                    .aggregate_timeout_service::<Self, _, _, _>(token, CoordEvent::Cancelled)
                    .expect("a current aggregate timer fires once")
                else {
                    return noop();
                };
                match advance.completed {
                    Some(completed) => self.complete(completed, advance.effect),
                    None => advance.effect,
                }
            }
            CoordEvent::Cancelled(key, token, outcome) => {
                match operation
                    .record_cancel(key, token, outcome)
                    .expect("cancel token came from aggregate expiry")
                {
                    Some(completed) => self.complete(completed, noop()),
                    None => noop(),
                }
            }
            CoordEvent::Stop => unreachable!("stop handled before operation lookup"),
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
            match ScatterGather::start_service::<Self, _, _, _, _, _, _, _>(
                request,
                config,
                targets,
                |worker, timeout| call_cancelable_request(worker, WorkerRequest::Read, timeout),
                CoordEvent::Target,
                CoordEvent::AggregateTimeout,
            ) {
                Ok(ScatterGatherStart::Ready(completed)) => {
                    reply_to(completed.request, CoordReply::Report(completed.report))
                }
                Ok(ScatterGatherStart::Running { operation, effect }) => {
                    self.started.store(true, Ordering::Release);
                    self.operation = Some(operation);
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
        self.operation = None;
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

#[tina_runtime::isolate(message = ClientMessage)]
impl Client {
    fn handle(
        &mut self,
        message: ClientMessage,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
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
    let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);
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
                operation: None,
                started: Arc::new(AtomicBool::new(false)),
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
    let mut sim = Simulator::new(SingleShard, SimulatorConfig::default());
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
                operation: None,
                started: Arc::new(AtomicBool::new(false)),
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
fn owner_stop_closes_original_caller_with_child_authority_pending() {
    let outcome = Arc::new(Mutex::new(None));
    let started = Arc::new(AtomicBool::new(false));
    let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);
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
                operation: None,
                started: Arc::clone(&started),
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
    let runtime = ThreadedRuntime::new(SingleShard, tina_runtime::DefaultThreadedMailboxFactory);
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
                operation: None,
                started: Arc::new(AtomicBool::new(false)),
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
