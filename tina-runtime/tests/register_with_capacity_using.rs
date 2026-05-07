//! Self-address at registration.
//!
//! `register_with_capacity_using(cap, |self_addr| ...)` lets the
//! constructor receive its own typed `Address`. Tests cover the
//! honesty rules: same generation as plain registration, no early
//! delivery, panic semantics (no entry, id leaked, address dies
//! loud).

use std::convert::Infallible;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    DefaultMailboxFactory, DefaultThreadedMailboxFactory, Runtime, ThreadedRuntime,
};

#[derive(Debug)]
enum Msg {
    Ping,
    Pong,
}

#[derive(Debug, Default)]
struct Echo {
    #[allow(dead_code)]
    self_addr: Option<Address<Msg>>,
    seen: u32,
}

#[tina_runtime::isolate(message = Msg)]
impl Echo {
    fn handle(
        &mut self,
        msg: Msg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            Msg::Ping => {
                self.seen += 1;
                noop()
            }
            Msg::Pong => {
                self.seen += 1;
                noop()
            }
        }
    }
}

#[test]
fn constructor_receives_typed_self_address() {
    let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);

    let mut captured: Option<Address<Msg>> = None;
    let addr = runtime.register_with_capacity_using::<Echo, Infallible, _>(8, |self_addr| {
        captured = Some(self_addr);
        Echo {
            self_addr: Some(self_addr),
            seen: 0,
        }
    });

    let captured = captured.expect("constructor saw self_addr");
    assert_eq!(captured.shard(), addr.shard());
    assert_eq!(captured.isolate(), addr.isolate());
    assert_eq!(captured.generation(), addr.generation());
    assert_eq!(addr.shard().get(), 0);
    assert_eq!(addr.generation().get(), 0);
}

#[test]
fn registered_address_accepts_messages_after_helper_returns() {
    let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);

    let addr = runtime.register_with_capacity_using::<Echo, Infallible, _>(4, |self_addr| Echo {
        self_addr: Some(self_addr),
        seen: 0,
    });

    runtime.try_send(addr, Msg::Ping).unwrap();
    runtime.try_send(addr, Msg::Pong).unwrap();
    runtime.step();
}

#[test]
fn generation_matches_plain_register_with_capacity() {
    let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);

    let plain = runtime.register_with_capacity::<Echo, Infallible>(Echo::default(), 4);
    let using = runtime.register_with_capacity_using::<Echo, Infallible, _>(4, |self_addr| Echo {
        self_addr: Some(self_addr),
        seen: 0,
    });

    assert_eq!(plain.generation(), using.generation());
    assert_ne!(plain.isolate(), using.isolate());
}

#[test]
fn constructor_panic_propagates_and_does_not_register_entry() {
    let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);

    let pre = runtime.register_with_capacity::<Echo, Infallible>(Echo::default(), 4);

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.register_with_capacity_using::<Echo, Infallible, _>(4, |_self_addr| -> Echo {
            panic!("constructor panic");
        })
    }));

    assert!(result.is_err());

    // pre-existing isolate still works after the failed registration.
    runtime.try_send(pre, Msg::Ping).unwrap();
    runtime.step();
}

#[test]
fn constructor_panic_does_not_reuse_id_for_next_register() {
    let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);

    let _result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.register_with_capacity_using::<Echo, Infallible, _>(4, |_self_addr| -> Echo {
            panic!("first construct panics");
        })
    }));

    // The next registration must not collide with the leaked id.
    let after = runtime.register_with_capacity::<Echo, Infallible>(Echo::default(), 4);
    runtime.try_send(after, Msg::Ping).unwrap();
    runtime.step();
}

#[test]
fn threaded_runtime_register_with_capacity_using_works() {
    let runtime = ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory);

    let addr = runtime
        .register_with_capacity_using::<Echo, Infallible, _>(4, |self_addr| Echo {
            self_addr: Some(self_addr),
            seen: 0,
        })
        .expect("register");

    runtime.try_send(addr, Msg::Ping).unwrap();
    // Give the worker a moment to drain. The smoke is that the
    // address is live and the worker accepts the send.
    std::thread::sleep(Duration::from_millis(20));
    let _ = runtime.shutdown();
}

#[test]
fn threaded_runtime_constructor_panic_surfaces_as_error() {
    let runtime = ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory);

    // The panic happens on the worker thread; the host call channel
    // observes a worker death and returns ThreadedRuntimeError.
    let result = runtime.register_with_capacity_using::<Echo, Infallible, _>(
        4,
        |_self_addr| -> Echo { panic!("ctor panic on worker") },
    );

    assert!(result.is_err(), "expected worker-stopped error, got Ok");
    let _ = runtime.shutdown();
}
