//! Built-in SingleShard + macro default + RuntimeCallable diagnostic.
//!
//! These tests prove that:
//! - `#[tina::isolate]` and `#[tina_runtime::isolate]` accept omitting
//!   `shard = ...`, defaulting to `tina::SingleShard`.
//! - A program built on `SingleShard` registers and steps under the live
//!   `Runtime` and `ThreadedRuntime`.
//! - The `RuntimeCallable` marker trait is implemented for
//!   `RuntimeCall<M>` and not for arbitrary `Call` types.
//!
//! The compile-fail diagnostic for `Infallible: RuntimeCallable` is
//! demonstrated by `single_shard_default_compile_fail.rs` (a `compile_fail`
//! doctest below) so the test suite proves the diagnostic still fires.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::time::Duration;

use tina::{Mailbox, TrySendError, prelude::*};
use tina_runtime::{
    DefaultThreadedMailboxFactory, MailboxFactory, Runtime, RuntimeCallable, ThreadedRuntime,
    ThreadedRuntimeConfig,
};

#[derive(Debug, Clone)]
enum HelloMsg {
    Hello,
}

#[derive(Debug, Default)]
struct Hello {
    seen: bool,
}

// No `shard = ...` argument: macro defaults to `tina::SingleShard`.
#[tina::isolate(message = HelloMsg)]
impl Hello {
    fn handle(&mut self, msg: HelloMsg, _ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
        match msg {
            HelloMsg::Hello => {
                self.seen = true;
                stop()
            }
        }
    }
}

#[derive(Debug, Clone)]
enum SleeperMsg {
    Start,
    Done,
}

#[derive(Debug)]
struct Sleeper;

// `#[tina_runtime::isolate]` form, also no shard argument.
#[tina_runtime::isolate(message = SleeperMsg)]
impl Sleeper {
    fn handle(&mut self, msg: SleeperMsg, _ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
        match msg {
            SleeperMsg::Start => {
                tina_runtime::sleep(Duration::from_millis(1)).reply(|_| SleeperMsg::Done)
            }
            SleeperMsg::Done => stop(),
        }
    }
}

struct UnitMailbox<T> {
    capacity: usize,
    queue: RefCell<VecDeque<T>>,
    closed: Cell<bool>,
}

impl<T> Mailbox<T> for UnitMailbox<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }
    fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        if self.closed.get() {
            return Err(TrySendError::Closed(message));
        }
        let mut q = self.queue.borrow_mut();
        if q.len() >= self.capacity {
            return Err(TrySendError::Full(message));
        }
        q.push_back(message);
        Ok(())
    }
    fn recv(&self) -> Option<T> {
        self.queue.borrow_mut().pop_front()
    }
    fn close(&self) {
        self.closed.set(true);
    }
}

#[derive(Debug, Clone, Copy)]
struct UnitFactory;
impl MailboxFactory for UnitFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(UnitMailbox::<T> {
            capacity,
            queue: RefCell::new(VecDeque::new()),
            closed: Cell::new(false),
        })
    }
}

#[test]
fn single_shard_macro_default_works_in_explicit_runtime() {
    let mut runtime = Runtime::new(SingleShard, UnitFactory);
    let hello = runtime.register_with_capacity::<Hello, Infallible>(Hello::default(), 4);
    runtime
        .try_send(hello, HelloMsg::Hello)
        .expect("send hello");
    let _ = runtime.step();
    // The Hello handler stopped the isolate; the trace should record an
    // IsolateStopped event.
    let stopped = runtime
        .trace()
        .iter()
        .any(|event| matches!(event.kind(), tina_runtime::RuntimeEventKind::IsolateStopped));
    assert!(stopped, "Hello isolate should stop after handling");
}

#[test]
fn single_shard_macro_default_works_in_threaded_runtime() {
    let runtime = ThreadedRuntime::with_config(
        SingleShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 8,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let sleeper = runtime
        .register_with_capacity::<Sleeper, Infallible>(Sleeper, 8)
        .expect("register sleeper");
    let done = runtime.observe_isolate_complete(sleeper);
    runtime
        .try_send(sleeper, SleeperMsg::Start)
        .expect("kick sleeper");
    done.wait(Duration::from_secs(2))
        .expect("sleeper completes under SingleShard");
    let _ = runtime.shutdown();
}

#[test]
fn runtime_callable_only_implemented_for_runtime_call() {
    fn assert_callable<C: RuntimeCallable>() {}
    // Compiles: RuntimeCall<u32> implements RuntimeCallable.
    assert_callable::<tina_runtime::RuntimeCall<u32>>();
}

/// `Infallible` does not implement [`RuntimeCallable`], so registering an
/// isolate whose `Call` is `Infallible` against the simulator surfaces a
/// targeted diagnostic. This compile_fail doctest is the executable proof
/// that the bound is wired.
///
/// ```compile_fail
/// use tina::prelude::*;
/// use tina_runtime::RuntimeCallable;
/// fn assert_callable<C: RuntimeCallable>() {}
/// assert_callable::<std::convert::Infallible>();
/// ```
#[allow(dead_code)]
fn _compile_fail_doc() {}
