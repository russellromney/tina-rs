//! Request-aware raw flow steps preserve authority identically live and in sim.

use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{CallError, CallOutcome, DefaultMailboxFactory, Runtime, call_request, sleep};
use tina_sim::{Simulator, SimulatorConfig};

struct Lease {
    current: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
}

impl Lease {
    fn acquire(current: &Arc<AtomicUsize>, dropped: &Arc<AtomicUsize>) -> Self {
        current.fetch_add(1, Ordering::AcqRel);
        Self {
            current: Arc::clone(current),
            dropped: Arc::clone(dropped),
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.current.fetch_sub(1, Ordering::AcqRel);
        self.dropped.fetch_add(1, Ordering::AcqRel);
    }
}

#[derive(Debug, Clone, Copy)]
enum FlowRequest {
    Run,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlowReply {
    Done,
    TimerFailed(CallError),
}

enum FlowEvent {
    Continue(RequestFlow),
    Stop,
}

struct FlowService {
    delay: Duration,
    current: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
    started: Arc<AtomicBool>,
}

tina::flow! {
    flow RequestFlow for FlowService {
        reply FlowReply;

        step HttpReleased(http_lease: Lease) -> raw request tina_runtime::SleepReply {
            match outcome {
                Ok(()) if !req.is_open() => {
                    drop(http_lease);
                    noop()
                }
                Ok(()) => {
                    drop(http_lease);
                    let db_lease = Lease::acquire(&self.current, &self.dropped);
                    sleep(self.delay).then_service_event_with_request(
                        req,
                        move |req, outcome| {
                            FlowEvent::Continue(RequestFlow::DbReleased(req, db_lease, outcome))
                        },
                    )
                }
                Err(error) => {
                    drop(http_lease);
                    reply_to(req, FlowReply::TimerFailed(error))
                }
            }
        }

        step DbReleased(db_lease: Lease) -> raw request tina_runtime::SleepReply {
            drop(db_lease);
            match outcome {
                Ok(()) => reply_to(req, FlowReply::Done),
                Err(error) => reply_to(req, FlowReply::TimerFailed(error)),
            }
        }
    }
}

#[tina_runtime::isolate(event = FlowEvent, request = FlowRequest, reply = FlowReply)]
impl FlowService {
    fn handle_event(
        &mut self,
        event: FlowEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            FlowEvent::Continue(flow) => self.handle_request_flow(flow),
            FlowEvent::Stop => stop(),
        }
    }

    fn handle_request(
        &mut self,
        _request: FlowRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        call.capture(|req| {
            let lease = Lease::acquire(&self.current, &self.dropped);
            self.started.store(true, Ordering::Release);
            sleep(self.delay).then_service_event_with_request(req, move |req, outcome| {
                FlowEvent::Continue(RequestFlow::HttpReleased(req, lease, outcome))
            })
        })
    }
}

enum ClientMessage {
    Start(
        tina::ServiceRequestAddress<FlowEvent, FlowRequest, FlowReply>,
        Duration,
    ),
    Returned(CallOutcome<FlowReply>),
}

struct Client {
    outcome: Arc<Mutex<Option<CallOutcome<FlowReply>>>>,
}

#[tina_runtime::isolate(message = ClientMessage)]
impl Client {
    fn handle(
        &mut self,
        message: ClientMessage,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match message {
            ClientMessage::Start(service, timeout) => {
                call_request(service, FlowRequest::Run, timeout).then(ClientMessage::Returned)
            }
            ClientMessage::Returned(outcome) => {
                *self.outcome.lock().unwrap() = Some(outcome);
                noop()
            }
        }
    }
}

fn service(
    delay: Duration,
) -> (
    FlowService,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    Arc<AtomicBool>,
) {
    let current = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(AtomicBool::new(false));
    (
        FlowService {
            delay,
            current: Arc::clone(&current),
            dropped: Arc::clone(&dropped),
            started: Arc::clone(&started),
        },
        current,
        dropped,
        started,
    )
}

fn drive_sim(sim: &mut Simulator<SingleShard>) {
    for _ in 0..4 {
        while sim.step() > 0 {}
        sim.advance_time(Duration::from_millis(50));
    }
    while sim.step() > 0 {}
}

#[test]
fn live_and_sim_use_identical_request_aware_raw_flow() {
    let live_outcome = Arc::new(Mutex::new(None));
    let (live_service, live_current, live_dropped, _) = service(Duration::from_millis(1));
    let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);
    let address = runtime
        .register_split_service::<FlowService, FlowEvent, FlowRequest, Infallible>(live_service, 8)
        .requests;
    let client = runtime.register_with_capacity::<_, Infallible>(
        Client {
            outcome: Arc::clone(&live_outcome),
        },
        4,
    );
    assert!(
        runtime
            .try_send(
                client,
                ClientMessage::Start(address, Duration::from_secs(1)),
            )
            .is_ok()
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while live_outcome.lock().unwrap().is_none() && std::time::Instant::now() < deadline {
        runtime.step();
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        live_outcome.lock().unwrap().take(),
        Some(CallOutcome::Replied(FlowReply::Done))
    );
    assert_eq!(live_current.load(Ordering::Acquire), 0);
    assert_eq!(live_dropped.load(Ordering::Acquire), 2);

    let sim_outcome = Arc::new(Mutex::new(None));
    let (sim_service, sim_current, sim_dropped, _) = service(Duration::from_millis(1));
    let mut sim = Simulator::new(SingleShard, SimulatorConfig::default());
    let address = sim
        .register_split_service::<FlowService, FlowEvent, FlowRequest, Infallible>(sim_service, 8)
        .requests;
    let client = sim.register(Client {
        outcome: Arc::clone(&sim_outcome),
    });
    assert!(
        sim.try_send(
            client,
            ClientMessage::Start(address, Duration::from_secs(1)),
        )
        .is_ok()
    );
    drive_sim(&mut sim);
    assert_eq!(
        sim_outcome.lock().unwrap().take(),
        Some(CallOutcome::Replied(FlowReply::Done))
    );
    assert_eq!(sim_current.load(Ordering::Acquire), 0);
    assert_eq!(sim_dropped.load(Ordering::Acquire), 2);
}

#[test]
fn caller_timeout_does_not_leak_request_or_move_only_lease() {
    let outcome = Arc::new(Mutex::new(None));
    let (service, current, dropped, _) = service(Duration::from_millis(20));
    let mut sim = Simulator::new(SingleShard, SimulatorConfig::default());
    let address = sim
        .register_split_service::<FlowService, FlowEvent, FlowRequest, Infallible>(service, 8)
        .requests;
    let client = sim.register(Client {
        outcome: Arc::clone(&outcome),
    });
    assert!(
        sim.try_send(
            client,
            ClientMessage::Start(address, Duration::from_millis(1)),
        )
        .is_ok()
    );
    drive_sim(&mut sim);

    assert_eq!(outcome.lock().unwrap().take(), Some(CallOutcome::Timeout));
    assert_eq!(current.load(Ordering::Acquire), 0);
    assert_eq!(dropped.load(Ordering::Acquire), 1);
}

#[test]
fn owner_stop_cancels_timer_authority_and_closes_caller() {
    let outcome = Arc::new(Mutex::new(None));
    let (service, current, dropped, started) = service(Duration::from_secs(1));
    let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);
    let service = runtime
        .register_split_service::<FlowService, FlowEvent, FlowRequest, Infallible>(service, 8);
    let client = runtime.register_with_capacity::<_, Infallible>(
        Client {
            outcome: Arc::clone(&outcome),
        },
        4,
    );
    assert!(
        runtime
            .try_send(
                client,
                ClientMessage::Start(service.requests, Duration::from_secs(2)),
            )
            .is_ok()
    );
    while !started.load(Ordering::Acquire) {
        assert!(runtime.step() > 0, "flow must arm its first timer");
    }
    assert!(
        runtime
            .try_send_event(service.events, FlowEvent::Stop)
            .is_ok()
    );
    while runtime.step() > 0 {}

    assert_eq!(outcome.lock().unwrap().take(), Some(CallOutcome::Closed));
    assert_eq!(current.load(Ordering::Acquire), 0);
    assert_eq!(dropped.load(Ordering::Acquire), 1);
}
