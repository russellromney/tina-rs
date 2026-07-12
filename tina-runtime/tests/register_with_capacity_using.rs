//! Self-address at registration.
//!
//! `register_with_capacity_using(cap, |self_addr| ...)` lets the
//! constructor receive its own typed `Address`. Tests pin: same
//! generation as plain registration, address routes correctly,
//! panic semantics (no entry, id leaked, address dies loud), and
//! the threaded form actually handles messages (not just accepts
//! them).

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    DefaultMailboxFactory, DefaultThreadedMailboxFactory, PendingReplies,
    PendingRepliesTryCaptureError, Runtime, ThreadedRuntime, sleep,
};

#[derive(Debug)]
enum Msg {
    Tick,
    Stop,
}

#[derive(Debug)]
struct Echo {
    self_addr: Address<Msg>,
    seen: Arc<AtomicU32>,
}

#[tina_runtime::isolate(message = Msg)]
impl Echo {
    fn handle(
        &mut self,
        msg: Msg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            Msg::Tick => {
                self.seen.fetch_add(1, Ordering::Release);
                noop()
            }
            // self_addr is real and usable: stopping with our own
            // address as the typed report proves construct received
            // a routable Address.
            Msg::Stop => stop_with(self.self_addr),
        }
    }
}

#[test]
fn constructor_receives_typed_self_address() {
    let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);

    let seen = Arc::new(AtomicU32::new(0));
    let mut captured: Option<Address<Msg>> = None;
    let addr = runtime.register_with_capacity_using::<Echo, Infallible, _>(8, |self_addr| {
        captured = Some(self_addr);
        Echo {
            self_addr,
            seen: Arc::clone(&seen),
        }
    });

    let captured = captured.expect("constructor saw self_addr");
    assert_eq!(captured.shard(), addr.shard());
    assert_eq!(captured.isolate(), addr.isolate());
    assert_eq!(captured.generation(), addr.generation());
    assert_eq!(addr.generation().get(), 0);
}

#[test]
fn registered_address_routes_to_the_constructed_isolate() {
    let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);
    let seen = Arc::new(AtomicU32::new(0));

    // I is inferred from the closure return type; only Outbound
    // needs the turbofish.
    let addr = runtime.register_with_capacity_using::<_, Infallible, _>(4, |self_addr| Echo {
        self_addr,
        seen: Arc::clone(&seen),
    });

    runtime.try_send(addr, Msg::Tick).unwrap();
    runtime.try_send(addr, Msg::Tick).unwrap();
    while runtime.step() > 0 {}

    assert_eq!(seen.load(Ordering::Acquire), 2);
}

#[test]
fn generation_matches_plain_and_addresses_route_distinctly() {
    let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);
    let seen_plain = Arc::new(AtomicU32::new(0));
    let seen_using = Arc::new(AtomicU32::new(0));

    let plain = runtime.register_with_capacity::<Echo, Infallible>(
        Echo {
            self_addr: Address::new(SingleShard.id(), tina::IsolateId::new(0)),
            seen: Arc::clone(&seen_plain),
        },
        4,
    );
    let using = runtime.register_with_capacity_using::<Echo, Infallible, _>(4, |self_addr| Echo {
        self_addr,
        seen: Arc::clone(&seen_using),
    });

    assert_eq!(plain.generation(), using.generation());
    assert_ne!(plain.isolate(), using.isolate());

    runtime.try_send(plain, Msg::Tick).unwrap();
    runtime.try_send(using, Msg::Tick).unwrap();
    runtime.try_send(using, Msg::Tick).unwrap();
    while runtime.step() > 0 {}

    // Each address routes to its own isolate, not a shared one.
    assert_eq!(seen_plain.load(Ordering::Acquire), 1);
    assert_eq!(seen_using.load(Ordering::Acquire), 2);
}

#[test]
fn constructor_panic_propagates_and_does_not_register_entry() {
    let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);
    let seen = Arc::new(AtomicU32::new(0));

    let pre = runtime.register_with_capacity::<Echo, Infallible>(
        Echo {
            self_addr: Address::new(SingleShard.id(), tina::IsolateId::new(0)),
            seen: Arc::clone(&seen),
        },
        4,
    );

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.register_with_capacity_using::<Echo, Infallible, _>(4, |_self_addr| -> Echo {
            panic!("constructor panic");
        })
    }));

    assert!(result.is_err());

    // pre-existing isolate still routes after the failed registration.
    runtime.try_send(pre, Msg::Tick).unwrap();
    while runtime.step() > 0 {}
    assert_eq!(seen.load(Ordering::Acquire), 1);
}

#[test]
fn constructor_panic_does_not_reuse_id_for_next_register() {
    let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);
    let seen = Arc::new(AtomicU32::new(0));

    let _result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.register_with_capacity_using::<Echo, Infallible, _>(4, |_self_addr| -> Echo {
            panic!("first construct panics");
        })
    }));

    // Next registration must produce a working address, not collide
    // with the leaked id.
    let after = runtime.register_with_capacity_using::<Echo, Infallible, _>(4, |self_addr| Echo {
        self_addr,
        seen: Arc::clone(&seen),
    });
    runtime.try_send(after, Msg::Tick).unwrap();
    while runtime.step() > 0 {}
    assert_eq!(seen.load(Ordering::Acquire), 1);
}

#[test]
fn threaded_runtime_actually_handles_messages_through_the_helper() {
    // Replaces a sleep-and-pray smoke. Drives Tick + Stop, then
    // observes the typed result the constructor wired up. If the
    // helper had registered the wrong handler or the address did
    // not route, observe_result would not produce a value.
    let runtime = ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory);
    let seen = Arc::new(AtomicU32::new(0));

    let seen_for_ctor = Arc::clone(&seen);
    let addr = runtime
        .register_with_capacity_using::<Echo, Infallible, _>(4, move |self_addr| Echo {
            self_addr,
            seen: seen_for_ctor,
        })
        .expect("register");

    let result = runtime
        .observe_result::<Address<Msg>, _, _>(addr)
        .expect("observe_result");

    runtime.try_send(addr, Msg::Tick).unwrap();
    runtime.try_send(addr, Msg::Tick).unwrap();
    runtime.try_send(addr, Msg::Stop).unwrap();

    let reported = result
        .wait(Duration::from_secs(2))
        .expect("driver finishes");

    assert_eq!(reported.shard(), addr.shard());
    assert_eq!(reported.isolate(), addr.isolate());
    assert_eq!(reported.generation(), addr.generation());
    assert_eq!(seen.load(Ordering::Acquire), 2);
    let _ = runtime.shutdown();
}

#[test]
fn threaded_runtime_constructor_panic_surfaces_as_error() {
    let runtime = ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory);

    let result = runtime
        .register_with_capacity_using::<Echo, Infallible, _>(4, |_self_addr| -> Echo {
            panic!("ctor panic on worker")
        });

    assert!(result.is_err(), "expected worker-stopped error, got Ok");
    let _ = runtime.shutdown();
}

// Combined: register_with_capacity_using + drain_replies_into_stop
// in one isolate. Exercises both shipped helpers together.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Closed;

#[derive(Debug)]
enum FrontMsg {
    Submit(u32),
    Shutdown,
    Tick(#[allow(dead_code)] tina_runtime::SleepReply),
}

struct Front {
    pending: PendingReplies<u32, Closed>,
}

#[tina_runtime::isolate(message = FrontMsg, reply = Closed)]
impl Front {
    fn handle(
        &mut self,
        msg: FrontMsg,
        ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            FrontMsg::Submit(qid) => match self.pending.try_capture(ctx, qid) {
                // Sleep keeps the slot live until Shutdown fires.
                Ok(()) => sleep(Duration::from_millis(10)).then(FrontMsg::Tick),
                Err(PendingRepliesTryCaptureError::Full) => reply(Closed),
                Err(other) => panic!("try_capture: {other:?}"),
            },
            FrontMsg::Tick(_) => noop(),
            FrontMsg::Shutdown => self.pending.drain_replies_into_stop::<Self>(Closed),
        }
    }
}

#[test]
fn helpers_compose_self_address_plus_drain_replies_into_stop() {
    // Constructor closure builds the isolate value; the isolate later
    // calls drain_replies_into_stop on Shutdown. Stop only happens if
    // both helpers compose correctly.
    let runtime = ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory);

    let front = runtime
        .register_with_capacity_using::<Front, Infallible, _>(8, |_self_addr| Front {
            pending: PendingReplies::with_capacity(4),
        })
        .expect("register");

    let stopped = runtime
        .observe_isolate_complete(front)
        .expect("register completion observer");

    runtime.try_send(front, FrontMsg::Submit(1)).unwrap();
    runtime.try_send(front, FrontMsg::Submit(2)).unwrap();
    runtime.try_send(front, FrontMsg::Shutdown).unwrap();

    stopped
        .wait(Duration::from_secs(2))
        .expect("front stops after drain_replies_into_stop");
    let _ = runtime.shutdown();
}

// Marker: multi-shard parity is deferred. Replace the body with a
// real parity test when the multi-shard forms ship and drop the
// ignore. Empty body so an accidental un-ignore passes trivially.
#[test]
#[ignore = "multi-shard register_with_capacity_using_on is deferred; design note in .intent/phases/064-..."]
fn multi_shard_register_with_capacity_using_on_is_deferred() {}
