//! Live/simulator vocabulary parity for atomic root bootstrap registration.

use std::cell::Cell;
use std::convert::Infallible;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;

use tina::prelude::*;
use tina_runtime::{IngressSendError, RegisterBootstrapError};
use tina_sim::{MultiShardSimulator, Simulator, SimulatorConfig};

#[derive(Debug, Clone, Copy)]
struct TestShard(u32);

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

#[derive(Debug)]
struct DropProbe(Rc<Cell<u32>>);

impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

#[derive(Debug)]
enum Msg {
    Bootstrap(DropProbe),
    Inspect,
}

struct Service {
    delivered: Rc<Cell<u32>>,
    service_drops: Rc<Cell<u32>>,
}

impl Drop for Service {
    fn drop(&mut self) {
        self.service_drops.set(self.service_drops.get() + 1);
    }
}

#[tina_runtime::isolate(message = Msg, shard = TestShard)]
impl Service {
    fn handle(
        &mut self,
        message: Msg,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        match message {
            Msg::Bootstrap(_authority) => {
                self.delivered.set(self.delivered.get() + 1);
                noop()
            }
            Msg::Inspect => noop(),
        }
    }
}

fn fresh_service() -> (Service, Rc<Cell<u32>>, Rc<Cell<u32>>) {
    let delivered = Rc::new(Cell::new(0));
    let service_drops = Rc::new(Cell::new(0));
    (
        Service {
            delivered: Rc::clone(&delivered),
            service_drops: Rc::clone(&service_drops),
        },
        delivered,
        service_drops,
    )
}

#[derive(Debug)]
enum SplitEvent {
    Bootstrap(DropProbe),
}

#[derive(Debug)]
enum SplitRequest {
    Inspect,
}

struct SplitService {
    booted: bool,
}

#[tina_runtime::isolate(
    event = SplitEvent,
    request = SplitRequest,
    shard = TestShard
)]
impl SplitService {
    fn handle_event(
        &mut self,
        event: SplitEvent,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            SplitEvent::Bootstrap(_authority) => self.booted = true,
        }
        noop()
    }

    fn handle_request(
        &mut self,
        request: SplitRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            SplitRequest::Inspect => call.reply(()),
        }
    }
}

#[test]
fn simulator_split_bootstrap_hides_envelope_and_preserves_authority() {
    let _ = SplitRequest::Inspect;
    let mut simulator = Simulator::new(TestShard(9), SimulatorConfig::default());
    let drops = Rc::new(Cell::new(0));
    let service = simulator
        .register_split_service_with_bootstrap::<SplitService, _, _, Infallible>(
            SplitService { booted: false },
            1,
            SplitEvent::Bootstrap(DropProbe(Rc::clone(&drops))),
        )
        .expect("typed split bootstrap");
    let extra_drops = Rc::new(Cell::new(0));
    assert!(matches!(
        simulator.try_send_event(
            service.events,
            SplitEvent::Bootstrap(DropProbe(Rc::clone(&extra_drops))),
        ),
        Err(IngressSendError::Full(_))
    ));
    simulator.run_until_quiescent();
    assert_eq!(drops.get(), 1);

    let refused_drops = Rc::new(Cell::new(0));
    let refused = simulator
        .register_split_service_with_bootstrap::<SplitService, _, _, Infallible>(
            SplitService { booted: false },
            0,
            SplitEvent::Bootstrap(DropProbe(Rc::clone(&refused_drops))),
        )
        .expect_err("zero-capacity split bootstrap");
    assert!(matches!(refused, RegisterBootstrapError::Full(_)));
    assert_eq!(refused_drops.get(), 0);
    drop(refused);
    assert_eq!(refused_drops.get(), 1);
}

#[test]
fn simulator_bootstrap_is_admitted_before_address_publication() {
    let mut simulator = Simulator::new(TestShard(1), SimulatorConfig::default());
    let (service, delivered, service_drops) = fresh_service();
    let message_drops = Rc::new(Cell::new(0));
    let address = simulator
        .register_with_capacity_and_bootstrap::<Service, Msg, Infallible>(
            service,
            1,
            Msg::Bootstrap(DropProbe(Rc::clone(&message_drops))),
        )
        .expect("bootstrap admitted");

    assert_eq!(address.system(), simulator.system_incarnation());
    assert!(matches!(
        simulator.try_send(address, Msg::Inspect),
        Err(IngressSendError::Full(Msg::Inspect))
    ));
    assert!(simulator.trace().is_empty());
    simulator.run_until_quiescent();
    assert_eq!(delivered.get(), 1);
    assert_eq!(message_drops.get(), 1);
    drop(simulator);
    assert_eq!(service_drops.get(), 1);
}

#[test]
fn simulator_bootstrap_registration_replays_identity_and_trace_exactly() {
    fn run() -> (IsolateId, tina_sim::ReplayArtifact) {
        let mut simulator = Simulator::new(TestShard(3), SimulatorConfig::default());
        let (refused_service, _, _) = fresh_service();
        let refused = simulator.register_with_capacity_and_bootstrap::<Service, Msg, Infallible>(
            refused_service,
            0,
            Msg::Bootstrap(DropProbe(Rc::new(Cell::new(0)))),
        );
        assert!(matches!(refused, Err(RegisterBootstrapError::Full(_))));

        let (service, _, _) = fresh_service();
        let address = simulator
            .register_with_capacity_and_bootstrap::<Service, Msg, Infallible>(
                service,
                1,
                Msg::Bootstrap(DropProbe(Rc::new(Cell::new(0)))),
            )
            .expect("bootstrap admitted after deterministic refused identity");
        simulator.run_until_quiescent();
        (address.isolate(), simulator.replay_artifact())
    }

    let first = run();
    let replay = run();
    assert_eq!(first.0, IsolateId::new(2));
    assert_eq!(replay.0, first.0);
    assert_eq!(replay.1, first.1);
}

#[test]
fn simulator_full_prefill_returns_authority_and_publishes_nothing() {
    let mut simulator = Simulator::new(TestShard(2), SimulatorConfig::default());
    let (service, delivered, service_drops) = fresh_service();
    let message_drops = Rc::new(Cell::new(0));
    let error = simulator
        .register_with_capacity_and_bootstrap::<Service, Msg, Infallible>(
            service,
            0,
            Msg::Bootstrap(DropProbe(Rc::clone(&message_drops))),
        )
        .expect_err("zero-capacity prefill");
    assert!(matches!(error, RegisterBootstrapError::Full(_)));
    assert_eq!(simulator.trace().len(), 0);
    assert_eq!(delivered.get(), 0);
    assert_eq!(service_drops.get(), 1);
    assert_eq!(message_drops.get(), 0);
    drop(error);
    assert_eq!(message_drops.get(), 1);

    let (retry_service, _, _) = fresh_service();
    let retry = simulator
        .register_with_capacity_and_bootstrap::<Service, Msg, Infallible>(
            retry_service,
            1,
            Msg::Bootstrap(DropProbe(Rc::new(Cell::new(0)))),
        )
        .expect("refill after refused prefill");
    assert_eq!(
        retry.isolate(),
        IsolateId::new(2),
        "simulator identity progression must match live rollback"
    );
}

#[test]
fn multi_shard_simulator_has_the_same_bootstrap_shape() {
    let mut simulator =
        MultiShardSimulator::new([TestShard(10), TestShard(20)], SimulatorConfig::default());
    let (service, delivered, _) = fresh_service();
    let message_drops = Rc::new(Cell::new(0));
    let address = simulator
        .register_with_capacity_and_bootstrap_on::<Service, Msg, Infallible>(
            ShardId::new(20),
            service,
            1,
            Msg::Bootstrap(DropProbe(Rc::clone(&message_drops))),
        )
        .expect("bootstrap admitted on owned shard");
    assert_eq!(address.system(), simulator.system_incarnation());
    assert_eq!(address.shard(), ShardId::new(20));
    simulator.run_until_quiescent();
    assert_eq!(delivered.get(), 1);
    assert_eq!(message_drops.get(), 1);
}

#[test]
fn multi_shard_simulator_split_bootstrap_uses_domain_event() {
    let mut simulator =
        MultiShardSimulator::new([TestShard(10), TestShard(20)], SimulatorConfig::default());
    let drops = Rc::new(Cell::new(0));
    let service = simulator
        .register_split_service_with_bootstrap_on::<SplitService, _, _, Infallible>(
            ShardId::new(20),
            SplitService { booted: false },
            1,
            SplitEvent::Bootstrap(DropProbe(Rc::clone(&drops))),
        )
        .expect("typed split bootstrap on owned shard");
    assert_eq!(service.events.address().shard(), ShardId::new(20));
    simulator.run_until_quiescent();
    assert_eq!(drops.get(), 1);
}

#[test]
fn multi_shard_unknown_shard_panics_and_drops_inputs_without_publication() {
    let mut simulator =
        MultiShardSimulator::new([TestShard(10), TestShard(20)], SimulatorConfig::default());
    let (service, delivered, service_drops) = fresh_service();
    let message_drops = Rc::new(Cell::new(0));

    let panic = catch_unwind(AssertUnwindSafe(|| {
        let _ = simulator.register_with_capacity_and_bootstrap_on::<Service, Msg, Infallible>(
            ShardId::new(99),
            service,
            1,
            Msg::Bootstrap(DropProbe(Rc::clone(&message_drops))),
        );
    }));

    assert!(panic.is_err());
    assert_eq!(service_drops.get(), 1);
    assert_eq!(message_drops.get(), 1);
    assert_eq!(delivered.get(), 0);
    assert!(simulator.trace().is_empty());
    simulator.run_until_quiescent();
    assert_eq!(delivered.get(), 0);
    assert!(simulator.trace().is_empty());
}
