use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tina::{
    Address, AddressGeneration, Effect, IsolateId, RestartBudget, RestartPolicy,
    RestartableChildDefinition, Shard, ShardId, spawn, stop, stop_with,
};
use tina_runtime::{ResultWaitError, RuntimeEventKind};
use tina_sim::{MultiShardSimulator, Simulator, SimulatorConfig};
use tina_supervisor::SupervisorConfig;

#[derive(Debug, Clone, Copy)]
struct TestShard(u32);

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

#[derive(Debug)]
enum Msg {
    Finish(u32),
    FinishWithDrop(DropProbe),
    FinishInBatch(u32, DropProbe),
    Fail,
    Panic,
    Stop,
}

#[derive(Debug)]
struct DropProbe(Arc<AtomicUsize>);

impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct ResultIsolate;

#[tina_runtime::isolate(message = Msg, shard = TestShard)]
impl ResultIsolate {
    fn handle(&mut self, msg: Msg, _ctx: &mut Context<'_, TestShard, Self::Reply>) -> Effect<Self> {
        match msg {
            Msg::Finish(value) => stop_with(value),
            Msg::FinishWithDrop(value) => stop_with(value),
            Msg::FinishInBatch(value, tail) => {
                Effect::Batch(vec![stop_with(value), stop_with(tail)])
            }
            Msg::Fail => Effect::Fail,
            Msg::Panic => panic!("result isolate panic"),
            Msg::Stop => stop(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ParentMsg {
    Spawn,
}

struct Parent;

#[tina_runtime::isolate(
    message = ParentMsg,
    spawn = RestartableChildDefinition<ResultIsolate>,
    shard = TestShard
)]
impl Parent {
    fn handle(
        &mut self,
        msg: ParentMsg,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ParentMsg::Spawn => spawn(RestartableChildDefinition::new(|| ResultIsolate, 4)),
        }
    }
}

fn sim(shard: u32) -> Simulator<TestShard> {
    Simulator::new(TestShard(shard), SimulatorConfig::default())
}

#[test]
fn typed_result_matches_live_waiter_contract() {
    let mut sim = sim(1);
    let address = sim.register(ResultIsolate);
    let waiter = sim
        .observe_result::<u32, _, _>(address)
        .expect("claim typed result");

    sim.try_send(address, Msg::Finish(42)).expect("trigger");
    sim.run_until_quiescent();

    assert_eq!(waiter.wait(Duration::ZERO), Ok(42));
    assert_eq!(
        sim.observe_result::<u32, _, _>(address).unwrap_err(),
        ResultWaitError::AlreadyStopped
    );
}

#[test]
fn plain_stop_resolves_as_stopped_without_result() {
    let mut sim = sim(1);
    let address = sim.register(ResultIsolate);
    let waiter = sim
        .observe_result::<u32, _, _>(address)
        .expect("claim typed result");

    sim.try_send(address, Msg::Stop).expect("trigger");
    sim.run_until_quiescent();

    assert_eq!(
        waiter.wait(Duration::ZERO),
        Err(ResultWaitError::StoppedWithoutResult)
    );
}

#[test]
fn type_mismatch_drops_the_exact_payload_once() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut sim = sim(1);
    let address = sim.register(ResultIsolate);
    let waiter = sim
        .observe_result::<u32, _, _>(address)
        .expect("claim mismatched result type");

    sim.try_send(address, Msg::FinishWithDrop(DropProbe(Arc::clone(&drops))))
        .expect("trigger");
    sim.run_until_quiescent();

    assert_eq!(
        waiter.wait(Duration::ZERO),
        Err(ResultWaitError::TypeMismatch)
    );
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn batch_stop_with_delivers_the_first_payload_and_drops_the_tail_once() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut sim = sim(1);
    let address = sim.register(ResultIsolate);
    let waiter = sim
        .observe_result::<u32, _, _>(address)
        .expect("claim batch result");

    sim.try_send(
        address,
        Msg::FinishInBatch(43, DropProbe(Arc::clone(&drops))),
    )
    .expect("trigger batch stop");
    sim.run_until_quiescent();

    assert_eq!(waiter.wait(Duration::ZERO), Ok(43));
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn unobserved_result_is_dropped_exactly_once() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut sim = sim(1);
    let address = sim.register(ResultIsolate);

    sim.try_send(address, Msg::FinishWithDrop(DropProbe(Arc::clone(&drops))))
        .expect("trigger");
    sim.run_until_quiescent();

    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn duplicate_claim_is_rejected_and_dropped_claim_refills() {
    let mut sim = sim(1);
    let address = sim.register(ResultIsolate);
    let first = sim
        .observe_result::<u32, _, _>(address)
        .expect("first claim");
    assert_eq!(
        sim.observe_result::<u32, _, _>(address).unwrap_err(),
        ResultWaitError::AlreadyClaimed
    );

    drop(first);
    let replacement = sim
        .observe_result::<u32, _, _>(address)
        .expect("abandoned claim is reclaimed");
    sim.try_send(address, Msg::Finish(7)).expect("trigger");
    sim.run_until_quiescent();
    assert_eq!(replacement.wait(Duration::ZERO), Ok(7));
}

#[test]
fn timed_out_claim_is_reclaimed() {
    let mut sim = sim(1);
    let address = sim.register(ResultIsolate);
    let timed_out = sim
        .observe_result::<u32, _, _>(address)
        .expect("initial claim");
    assert_eq!(
        timed_out.wait(Duration::ZERO),
        Err(ResultWaitError::Timeout)
    );

    let replacement = sim
        .observe_result::<u32, _, _>(address)
        .expect("timed-out claim is reclaimed");
    sim.try_send(address, Msg::Finish(8)).expect("trigger");
    sim.run_until_quiescent();
    assert_eq!(replacement.wait(Duration::ZERO), Ok(8));
}

#[test]
fn fail_and_panic_resolve_as_stopped_without_result() {
    for message in [Msg::Fail, Msg::Panic] {
        let mut sim = sim(1);
        let address = sim.register(ResultIsolate);
        let waiter = sim
            .observe_result::<u32, _, _>(address)
            .expect("claim result before abnormal stop");

        sim.try_send(address, message)
            .expect("trigger abnormal stop");
        sim.run_until_quiescent();

        assert_eq!(
            waiter.wait(Duration::ZERO),
            Err(ResultWaitError::StoppedWithoutResult)
        );
    }
}

#[test]
fn supervised_restart_settles_old_claim_and_routes_new_result_to_replacement() {
    let mut sim = sim(1);
    let parent = sim.register(Parent);
    sim.supervise(
        parent,
        SupervisorConfig::new(RestartPolicy::OneForOne, RestartBudget::new(1)),
    );
    sim.try_send(parent, ParentMsg::Spawn).expect("spawn child");
    sim.run_until_quiescent();

    let child = sim
        .trace()
        .iter()
        .find_map(|event| match event.kind() {
            RuntimeEventKind::Spawned { child_isolate } => Some(child_isolate),
            _ => None,
        })
        .expect("spawned child");
    let old_address = result_address(&sim, child, AddressGeneration::new(0));
    let old_waiter = sim
        .observe_result::<u32, _, _>(old_address)
        .expect("claim old child result");

    sim.try_send(old_address, Msg::Panic)
        .expect("panic old child");
    sim.run_until_quiescent();
    assert_eq!(
        old_waiter.wait(Duration::ZERO),
        Err(ResultWaitError::StoppedWithoutResult)
    );

    let (replacement, replacement_generation) = sim
        .trace()
        .iter()
        .find_map(|event| match event.kind() {
            RuntimeEventKind::RestartChildCompleted {
                new_isolate,
                new_generation,
                ..
            } => Some((new_isolate, new_generation)),
            _ => None,
        })
        .expect("replacement child");
    let replacement_address = result_address(&sim, replacement, replacement_generation);
    let replacement_waiter = sim
        .observe_result::<u32, _, _>(replacement_address)
        .expect("claim replacement result");
    sim.try_send(replacement_address, Msg::Finish(19))
        .expect("finish replacement");
    sim.run_until_quiescent();
    assert_eq!(replacement_waiter.wait(Duration::ZERO), Ok(19));
}

fn result_address(
    sim: &Simulator<TestShard>,
    isolate: IsolateId,
    generation: AddressGeneration,
) -> Address<Msg> {
    Address::new_with_generation_in(
        sim.system_incarnation(),
        ShardId::new(1),
        isolate,
        generation,
    )
}

#[test]
fn configured_cap_is_enforced_and_abandoned_capacity_refills() {
    let mut sim = Simulator::new(
        TestShard(1),
        SimulatorConfig {
            result_observation_capacity: 1,
            ..SimulatorConfig::default()
        },
    );
    let first = sim.register(ResultIsolate);
    let second = sim.register(ResultIsolate);
    let first_waiter = sim.observe_result::<u32, _, _>(first).expect("first claim");
    assert_eq!(
        sim.observe_result::<u32, _, _>(second).unwrap_err(),
        ResultWaitError::ObservationFull
    );

    drop(first_waiter);
    let second_waiter = sim
        .observe_result::<u32, _, _>(second)
        .expect("abandoned capacity refills");
    sim.try_send(second, Msg::Finish(9)).expect("trigger");
    sim.run_until_quiescent();
    assert_eq!(second_waiter.wait(Duration::ZERO), Ok(9));
}

#[test]
fn provenance_and_shard_are_rejected_without_claiming_capacity() {
    let incarnation = tina::SystemIncarnation::new(9123);
    let config = SimulatorConfig {
        system_incarnation: Some(incarnation),
        result_observation_capacity: 1,
        ..SimulatorConfig::default()
    };
    let mut owner = Simulator::new(TestShard(1), config.clone());
    let local = owner.register(ResultIsolate);
    let mut sibling = Simulator::new(TestShard(2), config);
    let other_shard = sibling.register(ResultIsolate);
    assert_eq!(
        owner.observe_result::<u32, _, _>(other_shard).unwrap_err(),
        ResultWaitError::UnknownShard(ShardId::new(2))
    );

    let mut foreign = sim(1);
    let foreign_address = foreign.register(ResultIsolate);
    assert!(matches!(
        owner
            .observe_result::<u32, _, _>(foreign_address)
            .unwrap_err(),
        ResultWaitError::ForeignSystem { .. }
    ));

    let stale_generation: Address<Msg> = Address::new_with_generation_in(
        incarnation,
        ShardId::new(1),
        local.isolate(),
        AddressGeneration::new(local.generation().get() + 1),
    );
    assert_eq!(
        owner
            .observe_result::<u32, _, _>(stale_generation)
            .unwrap_err(),
        ResultWaitError::AlreadyStopped
    );

    let waiter = owner
        .observe_result::<u32, _, _>(local)
        .expect("authority rejections did not consume capacity");
    owner.try_send(local, Msg::Finish(11)).expect("trigger");
    owner.run_until_quiescent();
    assert_eq!(waiter.wait(Duration::ZERO), Ok(11));
}

#[test]
fn multi_shard_observer_routes_to_the_address_owner() {
    let mut sim =
        MultiShardSimulator::new([TestShard(2), TestShard(1)], SimulatorConfig::default());
    let address = sim.register_on(ShardId::new(2), ResultIsolate);
    let waiter = sim
        .observe_result::<u32, _, _>(address)
        .expect("route observer to shard two");

    sim.try_send(address, Msg::Finish(17)).expect("trigger");
    sim.run_until_quiescent();

    assert_eq!(waiter.wait(Duration::ZERO), Ok(17));
}

#[test]
fn dropping_the_simulator_resolves_waiter_as_runtime_stopped() {
    let waiter = {
        let mut sim = sim(1);
        let address = sim.register(ResultIsolate);
        sim.observe_result::<u32, _, _>(address)
            .expect("claim typed result")
    };

    assert_eq!(
        waiter.wait(Duration::ZERO),
        Err(ResultWaitError::RuntimeStopped)
    );
}
