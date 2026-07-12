use std::cell::{Cell, RefCell};
use std::convert::Infallible;
use std::rc::Rc;

use tina::prelude::*;
use tina_runtime::IngressSendError;
use tina_sim::{MultiShardSimulator, Simulator, SimulatorConfig};

#[derive(Debug, Clone, Copy)]
struct SimShard(u32);

impl Shard for SimShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Msg {
    Record,
    Stop,
}

struct SelfAware {
    me: Address<Msg>,
    observed: Rc<RefCell<Vec<Address<Msg>>>>,
}

struct DropAuthority(Rc<Cell<u32>>);

impl Drop for DropAuthority {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

#[tina_runtime::isolate(message = Msg, shard = SimShard)]
impl SelfAware {
    fn handle(
        &mut self,
        message: Msg,
        _ctx: &mut Context<'_, SimShard, Self::Reply>,
    ) -> Effect<Self> {
        match message {
            Msg::Record => {
                self.observed.borrow_mut().push(self.me);
                noop()
            }
            Msg::Stop => stop(),
        }
    }
}

#[test]
fn single_shard_constructor_address_routes_and_keeps_mailbox_bound() {
    let mut sim = Simulator::new(SimShard(7), SimulatorConfig::default());
    let observed = Rc::new(RefCell::new(Vec::new()));
    let observed_in_ctor = Rc::clone(&observed);
    let address =
        sim.register_with_capacity_using::<SelfAware, Msg, Infallible, _>(1, move |me| SelfAware {
            me,
            observed: observed_in_ctor,
        });

    assert_eq!(address.system(), sim.system_incarnation());
    sim.try_send(address, Msg::Record).expect("first admission");
    assert!(matches!(
        sim.try_send(address, Msg::Stop),
        Err(IngressSendError::Full(Msg::Stop))
    ));
    sim.run_until_quiescent();
    assert_eq!(observed.borrow().as_slice(), &[address]);
}

#[test]
fn zero_capacity_constructor_runs_but_first_ingress_is_full() {
    let mut sim = Simulator::new(SimShard(7), SimulatorConfig::default());
    let constructed = Rc::new(Cell::new(0));
    let constructed_in_ctor = Rc::clone(&constructed);
    let address = sim.register_with_capacity_using::<SelfAware, Msg, Infallible, _>(0, move |me| {
        constructed_in_ctor.set(constructed_in_ctor.get() + 1);
        SelfAware {
            me,
            observed: Rc::new(RefCell::new(Vec::new())),
        }
    });
    assert_eq!(constructed.get(), 1);
    assert!(matches!(
        sim.try_send(address, Msg::Record),
        Err(IngressSendError::Full(Msg::Record))
    ));
}

#[test]
fn multi_shard_constructor_address_uses_requested_owner() {
    let mut sim = MultiShardSimulator::new([SimShard(3), SimShard(9)], SimulatorConfig::default());
    let observed = Rc::new(RefCell::new(Vec::new()));
    let observed_in_ctor = Rc::clone(&observed);
    let address = sim.register_with_capacity_using_on::<SelfAware, Msg, Infallible, _>(
        ShardId::new(9),
        2,
        move |me| SelfAware {
            me,
            observed: observed_in_ctor,
        },
    );

    assert_eq!(address.system(), sim.system_incarnation());
    assert_eq!(address.shard(), ShardId::new(9));
    sim.try_send(address, Msg::Record).expect("route to owner");
    sim.run_until_quiescent();
    assert_eq!(observed.borrow().as_slice(), &[address]);
}

#[test]
fn constructor_panic_publishes_no_entry_and_consumes_id() {
    let mut sim = Simulator::new(SimShard(7), SimulatorConfig::default());
    let leaked = Rc::new(RefCell::new(None));
    let leaked_in_ctor = Rc::clone(&leaked);
    let drops = Rc::new(Cell::new(0));
    let authority = DropAuthority(Rc::clone(&drops));
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        sim.register_with_capacity_using::<SelfAware, Msg, Infallible, _>(
            2,
            move |address| -> SelfAware {
                let _authority = authority;
                *leaked_in_ctor.borrow_mut() = Some(address);
                panic!("constructor failed")
            },
        )
    }));
    assert!(panicked.is_err());
    assert_eq!(drops.get(), 1);

    let leaked = leaked.borrow().expect("constructor saw address");
    let next =
        sim.register_with_capacity_using::<SelfAware, Msg, Infallible, _>(2, |me| SelfAware {
            me,
            observed: Rc::new(RefCell::new(Vec::new())),
        });
    assert_eq!(leaked.shard(), SimShard(7).id());
    assert_eq!(leaked.generation().get(), 0);
    assert_eq!(next.shard(), leaked.shard());
    assert_eq!(next.isolate().get(), leaked.isolate().get() + 1);
    assert_eq!(next.generation().get(), 0);
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = sim.try_send(leaked, Msg::Record);
        }))
        .is_err()
    );
}

#[test]
fn unknown_multi_shard_owner_does_not_run_constructor() {
    let mut sim = MultiShardSimulator::new([SimShard(3)], SimulatorConfig::default());
    let constructed = Rc::new(RefCell::new(0_u32));
    let constructed_in_ctor = Rc::clone(&constructed);
    let drops = Rc::new(Cell::new(0));
    let authority = DropAuthority(Rc::clone(&drops));
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        sim.register_with_capacity_using_on::<SelfAware, Msg, Infallible, _>(
            ShardId::new(99),
            2,
            move |me| {
                let _authority = authority;
                *constructed_in_ctor.borrow_mut() += 1;
                SelfAware {
                    me,
                    observed: Rc::new(RefCell::new(Vec::new())),
                }
            },
        )
    }));
    assert!(panicked.is_err());
    assert_eq!(*constructed.borrow(), 0);
    assert_eq!(drops.get(), 1);
}

fn replay(seed: u64) -> (Address<Msg>, Vec<String>) {
    let config = SimulatorConfig {
        system_incarnation: Some(tina::SystemIncarnation::new(0x51a7)),
        seed,
        ..SimulatorConfig::default()
    };
    let mut sim = Simulator::new(SimShard(7), config);
    let address =
        sim.register_with_capacity_using::<SelfAware, Msg, Infallible, _>(4, |me| SelfAware {
            me,
            observed: Rc::new(RefCell::new(Vec::new())),
        });
    sim.try_send(address, Msg::Record).expect("record");
    sim.try_send(address, Msg::Stop).expect("stop");
    sim.run_until_quiescent();
    let trace = sim
        .trace()
        .iter()
        .map(|event| format!("{event:?}"))
        .collect();
    (address, trace)
}

#[test]
fn constructor_registration_replays_deterministically() {
    assert_eq!(replay(0x5eed), replay(0x5eed));
}
