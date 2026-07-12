//! Runtime surface alignment.
//!
//! These tests pin the alignment between explicit-step `Runtime` and
//! `ThreadedRuntime`:
//!
//! - `Runtime::supervise` panics on unknown parent (setup-time assertion
//!   contract).
//! - `Runtime::try_supervise` returns `SuperviseError::UnknownParent` for
//!   the same case (non-panicking variant).
//! - `ThreadedRuntime::try_supervise` does the same on the worker side
//!   without crashing the worker thread.
//! - `ThreadedRuntime::send_and_observe` is the message-distinguishing
//!   strict path that mirrors `Runtime::try_send`'s semantics; the fast
//!   `ThreadedRuntime::try_send` is documented as fire-and-forget.

use std::convert::Infallible;
use std::time::Duration;

use tina::{RestartBudget, RestartPolicy, prelude::*};
use tina_runtime::{
    DefaultThreadedMailboxFactory, Runtime, SuperviseError, ThreadedRuntime, ThreadedRuntimeConfig,
};
use tina_supervisor::SupervisorConfig;

#[derive(Debug, Default)]
struct Inert;

#[tina::isolate(message = ())]
impl Inert {
    fn handle(
        &mut self,
        _msg: (),
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        noop()
    }
}

fn make_threaded() -> ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory> {
    ThreadedRuntime::with_config(
        SingleShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 8,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    )
}

#[test]
fn explicit_runtime_try_supervise_returns_unknown_parent_for_unregistered_address() {
    let factory = tina_runtime::DefaultMailboxFactory;
    let mut runtime = Runtime::new(SingleShard, factory);
    let stale: Address<()> = Address::new_in(
        runtime.system_incarnation(),
        SingleShard.id(),
        tina::IsolateId::new(999),
    );
    let outcome = runtime.try_supervise(
        stale,
        SupervisorConfig::new(RestartPolicy::OneForOne, RestartBudget::new(1)),
    );
    assert_eq!(outcome, Err(SuperviseError::UnknownParent));
}

#[test]
fn explicit_runtime_try_supervise_succeeds_for_registered_address() {
    let factory = tina_runtime::DefaultMailboxFactory;
    let mut runtime = Runtime::new(SingleShard, factory);
    let inert = runtime.register_with_capacity::<Inert, Infallible>(Inert, 4);
    let outcome = runtime.try_supervise(
        inert,
        SupervisorConfig::new(RestartPolicy::OneForOne, RestartBudget::new(1)),
    );
    assert_eq!(outcome, Ok(()));
}

#[test]
#[should_panic(expected = "supervise expected an address registered with this runtime")]
fn explicit_runtime_supervise_panics_on_unknown_parent() {
    let factory = tina_runtime::DefaultMailboxFactory;
    let mut runtime = Runtime::new(SingleShard, factory);
    let stale: Address<()> = Address::new_in(
        runtime.system_incarnation(),
        SingleShard.id(),
        tina::IsolateId::new(999),
    );
    runtime.supervise(
        stale,
        SupervisorConfig::new(RestartPolicy::OneForOne, RestartBudget::new(1)),
    );
}

#[test]
fn threaded_try_supervise_returns_unknown_parent_without_crashing_worker() {
    let runtime = make_threaded();
    let stale: Address<()> = Address::new_in(
        runtime.system_incarnation(),
        SingleShard.id(),
        tina::IsolateId::new(999),
    );
    let outcome = runtime
        .try_supervise(
            stale,
            SupervisorConfig::new(RestartPolicy::OneForOne, RestartBudget::new(1)),
        )
        .expect("worker thread alive");
    assert_eq!(outcome, Err(SuperviseError::UnknownParent));

    // Worker is still alive: register a new isolate and try a real
    // operation. If the worker had crashed on the unknown parent, this
    // would fail.
    let inert = runtime
        .register_with_capacity::<Inert, Infallible>(Inert, 4)
        .expect("register survives prior unknown-parent supervise");
    let real = runtime.try_supervise(
        inert,
        SupervisorConfig::new(RestartPolicy::OneForOne, RestartBudget::new(1)),
    );
    assert_eq!(real.expect("worker alive"), Ok(()));

    let _ = runtime.shutdown();
}
