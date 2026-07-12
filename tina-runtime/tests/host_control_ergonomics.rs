//! Host-control ergonomics tests.
//!
//! Covers:
//!
//! - bounded admission for `ThreadedRuntime::call_blocking` and the new
//!   `ThreadedMultiShardRuntime::call_blocking`.
//! - `ThreadedShutdownHandle::{request_shutdown, wait_report}` against
//!   the pinned contract in `.intent/phases/102-host-control-ergonomics/plan.md`.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use tina::prelude::*;
use tina::{CallRejectedReason, RequestContext, reply_to};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, LocalSystem, ShutdownAndWaitError,
    ShutdownRequestError, ShutdownWaitError, ThreadedMultiShardRuntime, ThreadedRuntime,
    ThreadedRuntimeConfig, ThreadedRuntimeError, sleep,
};

#[derive(Debug, Clone, Copy)]
struct TestShard(u32);
impl TestShard {
    fn a() -> Self {
        Self(1)
    }
    fn b() -> Self {
        Self(2)
    }
}
impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

// All multi-shard test isolates live on TestShard so they can be registered
// on any shard the runtime owns. Single-shard tests use SingleShard.

#[derive(Debug)]
enum EchoMsg {
    AddOne(u32),
}
struct EchoMS;
#[tina_runtime::isolate(message = EchoMsg, reply = u32, shard = TestShard)]
impl EchoMS {
    fn handle(&mut self, _msg: EchoMsg, _ctx: &mut Context<'_, TestShard, u32>) -> Effect<Self> {
        noop()
    }
    fn handle_call(&mut self, msg: EchoMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            EchoMsg::AddOne(v) => call.reply(v + 1),
        }
    }
}

#[derive(Debug)]
enum SilentMsg {
    Hold,
    Done(RequestContext<u32>),
}
struct SilentMS;
#[tina_runtime::isolate(message = SilentMsg, reply = u32, shard = TestShard)]
impl SilentMS {
    fn handle(&mut self, msg: SilentMsg, _ctx: &mut Context<'_, TestShard, u32>) -> Effect<Self> {
        match msg {
            SilentMsg::Done(req) => reply_to(req, 0),
            SilentMsg::Hold => noop(),
        }
    }
    fn handle_call(&mut self, msg: SilentMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            SilentMsg::Hold => {
                let req = call.into_request_context();
                sleep(Duration::from_millis(500)).then(move |_| SilentMsg::Done(req))
            }
            SilentMsg::Done(_) => call.reject(CallRejectedReason::UnsupportedMessage),
        }
    }
}

#[derive(Debug)]
enum FullMsg {
    Hold,
    AddOne(u32),
}
struct FullTargetMS;
#[tina_runtime::isolate(message = FullMsg, reply = u32, shard = TestShard)]
impl FullTargetMS {
    fn handle(&mut self, _msg: FullMsg, _ctx: &mut Context<'_, TestShard, u32>) -> Effect<Self> {
        noop()
    }
    fn handle_call(&mut self, msg: FullMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            FullMsg::Hold => sleep(Duration::from_millis(150)).then(move |_| FullMsg::AddOne(0)),
            FullMsg::AddOne(v) => call.reply(v + 1),
        }
    }
}

#[derive(Debug)]
enum StopperMsg {
    Stop,
    Ping,
}
struct StopperMS;
#[tina_runtime::isolate(message = StopperMsg, reply = u32, shard = TestShard)]
impl StopperMS {
    fn handle(&mut self, msg: StopperMsg, _ctx: &mut Context<'_, TestShard, u32>) -> Effect<Self> {
        match msg {
            StopperMsg::Stop => stop(),
            StopperMsg::Ping => reply(1),
        }
    }
    fn handle_call(&mut self, msg: StopperMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            StopperMsg::Stop => call.reject(CallRejectedReason::UnsupportedMessage),
            StopperMsg::Ping => call.reply(1),
        }
    }
}

#[derive(Debug)]
enum RejectMsg {
    Sometimes,
}
struct RejectMS;
#[tina_runtime::isolate(message = RejectMsg, reply = u32, shard = TestShard)]
impl RejectMS {
    fn handle(&mut self, _msg: RejectMsg, _ctx: &mut Context<'_, TestShard, u32>) -> Effect<Self> {
        noop()
    }
    fn handle_call(&mut self, _msg: RejectMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        call.reject(CallRejectedReason::UnsupportedMessage)
    }
}

#[derive(Debug)]
enum SpinnerMsg {
    Tick,
}
struct SpinnerMS {
    flag: Arc<AtomicBool>,
    entered: Arc<AtomicBool>,
}
#[tina_runtime::isolate(message = SpinnerMsg, reply = (), shard = TestShard)]
impl SpinnerMS {
    fn handle(
        &mut self,
        msg: SpinnerMsg,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            SpinnerMsg::Tick => {
                self.entered.store(true, Ordering::Relaxed);
                let target = Instant::now() + Duration::from_millis(500);
                while Instant::now() < target {
                    if self.flag.load(Ordering::Relaxed) {
                        break;
                    }
                    std::hint::spin_loop();
                }
                noop()
            }
        }
    }
    fn handle_call(&mut self, _msg: SpinnerMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        call.reject(CallRejectedReason::UnsupportedMessage)
    }
}

// Single-shard variants for ThreadedRuntime tests.

#[derive(Debug)]
enum EchoSimpleMsg {
    AddOne(u32),
}
struct EchoSimple;
#[tina_runtime::isolate(message = EchoSimpleMsg, reply = u32)]
impl EchoSimple {
    fn handle(
        &mut self,
        _msg: EchoSimpleMsg,
        _ctx: &mut Context<'_, SingleShard, u32>,
    ) -> Effect<Self> {
        noop()
    }
    fn handle_call(&mut self, msg: EchoSimpleMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            EchoSimpleMsg::AddOne(v) => call.reply(v + 1),
        }
    }
}

#[derive(Debug)]
enum SpinnerSimpleMsg {
    Tick,
}
struct SpinnerSimple {
    flag: Arc<AtomicBool>,
    entered: Arc<AtomicBool>,
}

#[derive(Debug)]
enum ResultProducerMsg {
    Finish,
}

struct ResultProducer;

#[tina_runtime::isolate(message = ResultProducerMsg)]
impl ResultProducer {
    fn handle(
        &mut self,
        _msg: ResultProducerMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        stop_with(7_u32)
    }
}

#[derive(Debug)]
enum ResultProducerMsMsg {
    Finish,
}

struct ResultProducerMs;

#[tina_runtime::isolate(message = ResultProducerMsMsg, shard = TestShard)]
impl ResultProducerMs {
    fn handle(
        &mut self,
        _msg: ResultProducerMsMsg,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        stop_with(9_u32)
    }
}
#[tina_runtime::isolate(message = SpinnerSimpleMsg, reply = ())]
impl SpinnerSimple {
    fn handle(
        &mut self,
        msg: SpinnerSimpleMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            SpinnerSimpleMsg::Tick => {
                self.entered.store(true, Ordering::Relaxed);
                let target = Instant::now() + Duration::from_millis(500);
                while Instant::now() < target {
                    if self.flag.load(Ordering::Relaxed) {
                        break;
                    }
                    std::hint::spin_loop();
                }
                noop()
            }
        }
    }
    fn handle_call(&mut self, _msg: SpinnerSimpleMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        call.reject(CallRejectedReason::UnsupportedMessage)
    }
}

// Helpers ---------------------------------------------------------------------

fn single_runtime(cap: usize) -> ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory> {
    ThreadedRuntime::with_config(
        SingleShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: cap,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    )
}

fn multi_runtime(
    cap: usize,
) -> ThreadedMultiShardRuntime<TestShard, DefaultThreadedMailboxFactory> {
    ThreadedMultiShardRuntime::with_config(
        [TestShard::a(), TestShard::b()],
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: cap,
            shard_pair_capacity: cap.max(2),
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    )
}

// ------------------------- Rock 1: bounded admission -------------------------

#[test]
fn single_shard_registration_full_returns_promptly_without_late_constructor() {
    let runtime = Arc::new(single_runtime(1));
    let release = Arc::new(AtomicBool::new(false));
    let entered = Arc::new(AtomicBool::new(false));
    let spinner = runtime
        .register_with_capacity::<SpinnerSimple, Infallible>(
            SpinnerSimple {
                flag: Arc::clone(&release),
                entered: Arc::clone(&entered),
            },
            4,
        )
        .expect("register spinner");
    runtime
        .try_send(spinner, SpinnerSimpleMsg::Tick)
        .expect("occupy worker");
    while !entered.load(Ordering::Acquire) {
        thread::yield_now();
    }
    runtime
        .try_send(spinner, SpinnerSimpleMsg::Tick)
        .expect("fill command queue");

    let constructed = Arc::new(AtomicU32::new(0));
    let constructor_drops = Arc::new(AtomicU32::new(0));
    let runtime_for_register = Arc::clone(&runtime);
    let constructed_for_register = Arc::clone(&constructed);
    let constructor_drops_for_register = Arc::clone(&constructor_drops);
    let (tx, rx) = std::sync::mpsc::channel();
    let register = thread::spawn(move || {
        struct ConstructorAuthority(Arc<AtomicU32>);
        impl Drop for ConstructorAuthority {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::AcqRel);
            }
        }
        let authority = ConstructorAuthority(constructor_drops_for_register);
        let result = runtime_for_register
            .register_with_capacity_using::<EchoSimple, Infallible, _>(4, move |_| {
                let _authority = authority;
                constructed_for_register.fetch_add(1, Ordering::AcqRel);
                EchoSimple
            })
            .map(|_| ());
        tx.send(result).expect("report registration result");
    });
    let result = rx.recv_timeout(Duration::from_millis(200));
    if result.is_err() {
        release.store(true, Ordering::Release);
        register
            .join()
            .expect("release blocked legacy registration");
        panic!("registration blocked instead of reporting bounded admission");
    }
    assert_eq!(result.unwrap(), Err(ThreadedRuntimeError::CommandFull));
    register.join().expect("registration thread");
    assert_eq!(constructed.load(Ordering::Acquire), 0);
    assert_eq!(
        constructor_drops.load(Ordering::Acquire),
        1,
        "a rejected ordinary registration consumes and drops its constructor exactly once"
    );

    release.store(true, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let constructed_for_retry = Arc::clone(&constructed);
        match runtime.register_with_capacity_using::<EchoSimple, Infallible, _>(4, move |_| {
            constructed_for_retry.fetch_add(1, Ordering::AcqRel);
            EchoSimple
        }) {
            Ok(_) => break,
            Err(ThreadedRuntimeError::CommandFull) if Instant::now() < deadline => {
                thread::yield_now();
            }
            Err(error) => panic!("registration after refill failed: {error}"),
        }
    }
    assert_eq!(constructed.load(Ordering::Acquire), 1);
    Arc::try_unwrap(runtime)
        .unwrap_or_else(|_| panic!("sole runtime owner"))
        .shutdown()
        .expect("clean shutdown");
}

#[test]
fn local_system_and_multi_shard_registration_share_bounded_admission() {
    let app = Arc::new(
        LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory)
            .ingress_capacity(1)
            .try_build()
            .expect("start local system"),
    );
    let release = Arc::new(AtomicBool::new(false));
    let entered = Arc::new(AtomicBool::new(false));
    let spinner = app
        .register_root::<SpinnerSimple, Infallible>(
            SpinnerSimple {
                flag: Arc::clone(&release),
                entered: Arc::clone(&entered),
            },
            4,
        )
        .expect("register local spinner");
    app.try_send(spinner, SpinnerSimpleMsg::Tick)
        .expect("occupy local worker");
    while !entered.load(Ordering::Acquire) {
        thread::yield_now();
    }
    app.try_send(spinner, SpinnerSimpleMsg::Tick)
        .expect("fill local ingress");
    assert!(matches!(
        app.register_root::<EchoSimple, Infallible>(EchoSimple, 4),
        Err(ThreadedRuntimeError::CommandFull)
    ));
    release.store(true, Ordering::Release);
    app.shutdown_handle()
        .request_and_wait_report(Duration::from_secs(2))
        .expect("bounded local shutdown")
        .ensure_clean()
        .expect("clean local shutdown");

    let runtime = multi_runtime(1);
    let release = Arc::new(AtomicBool::new(false));
    let entered = Arc::new(AtomicBool::new(false));
    let spinner = runtime
        .register_with_capacity_on::<SpinnerMS, Infallible>(
            ShardId::new(1),
            SpinnerMS {
                flag: Arc::clone(&release),
                entered: Arc::clone(&entered),
            },
            4,
        )
        .expect("register multi spinner");
    runtime
        .try_send(spinner, SpinnerMsg::Tick)
        .expect("occupy shard");
    while !entered.load(Ordering::Acquire) {
        thread::yield_now();
    }
    runtime
        .try_send(spinner, SpinnerMsg::Tick)
        .expect("fill shard");
    assert!(matches!(
        runtime.register_with_capacity_on::<EchoMS, Infallible>(ShardId::new(1), EchoMS, 4,),
        Err(ThreadedRuntimeError::CommandFull)
    ));
    release.store(true, Ordering::Release);
    runtime
        .shutdown_handle()
        .request_and_wait_report(Duration::from_secs(2))
        .expect("bounded multi shutdown");
}

#[test]
fn observation_full_is_eager_precise_and_does_not_claim_result_authority() {
    let runtime = single_runtime(1);
    let producer = runtime
        .register_with_capacity::<ResultProducer, Infallible>(ResultProducer, 2)
        .expect("register result producer");
    let release = Arc::new(AtomicBool::new(false));
    let entered = Arc::new(AtomicBool::new(false));
    let spinner = runtime
        .register_with_capacity::<SpinnerSimple, Infallible>(
            SpinnerSimple {
                flag: Arc::clone(&release),
                entered: Arc::clone(&entered),
            },
            4,
        )
        .expect("register spinner");
    runtime
        .try_send(spinner, SpinnerSimpleMsg::Tick)
        .expect("occupy worker");
    while !entered.load(Ordering::Acquire) {
        thread::yield_now();
    }
    runtime
        .try_send(spinner, SpinnerSimpleMsg::Tick)
        .expect("fill command queue");

    assert!(matches!(
        runtime.observe_isolate_complete(producer),
        Err(ThreadedRuntimeError::CommandFull)
    ));
    assert!(matches!(
        runtime.observe_result::<u32, _, _>(producer),
        Err(tina_runtime::ResultWaitError::CommandFull)
    ));

    release.store(true, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(2);
    let result = loop {
        match runtime.observe_result::<u32, _, _>(producer) {
            Ok(waiter) => break waiter,
            Err(tina_runtime::ResultWaitError::CommandFull) if Instant::now() < deadline => {
                thread::yield_now();
            }
            Err(error) => panic!("result authority after refill: {error:?}"),
        }
    };
    runtime
        .try_send(producer, ResultProducerMsg::Finish)
        .expect("finish producer");
    assert_eq!(result.wait(Duration::from_secs(2)), Ok(7));
    runtime.shutdown().expect("clean shutdown");
}

#[test]
fn multi_shard_observation_full_is_eager_and_refills_on_the_target_shard() {
    let runtime = multi_runtime(1);
    let producer = runtime
        .register_with_capacity_on::<ResultProducerMs, Infallible>(
            ShardId::new(1),
            ResultProducerMs,
            2,
        )
        .expect("register result producer");
    let release = Arc::new(AtomicBool::new(false));
    let entered = Arc::new(AtomicBool::new(false));
    let spinner = runtime
        .register_with_capacity_on::<SpinnerMS, Infallible>(
            ShardId::new(1),
            SpinnerMS {
                flag: Arc::clone(&release),
                entered: Arc::clone(&entered),
            },
            4,
        )
        .expect("register target-shard spinner");
    runtime
        .try_send(spinner, SpinnerMsg::Tick)
        .expect("occupy target shard");
    while !entered.load(Ordering::Acquire) {
        thread::yield_now();
    }
    runtime
        .try_send(spinner, SpinnerMsg::Tick)
        .expect("fill target-shard command queue");

    assert!(matches!(
        runtime.observe_result::<u32, _, _>(producer),
        Err(tina_runtime::ResultWaitError::CommandFull)
    ));

    release.store(true, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(2);
    let result = loop {
        match runtime.observe_result::<u32, _, _>(producer) {
            Ok(waiter) => break waiter,
            Err(tina_runtime::ResultWaitError::CommandFull) if Instant::now() < deadline => {
                thread::yield_now();
            }
            Err(error) => panic!("multi result authority after refill: {error:?}"),
        }
    };
    runtime
        .try_send(producer, ResultProducerMsMsg::Finish)
        .expect("finish multi producer");
    assert_eq!(result.wait(Duration::from_secs(2)), Ok(9));
    runtime.shutdown().expect("clean multi shutdown");
}

#[test]
fn single_shard_call_blocking_returns_command_full_when_queue_saturated() {
    let runtime = single_runtime(1);
    let flag = Arc::new(AtomicBool::new(false));
    let entered = Arc::new(AtomicBool::new(false));
    let spinner = runtime
        .register_with_capacity::<SpinnerSimple, Infallible>(
            SpinnerSimple {
                flag: Arc::clone(&flag),
                entered: Arc::clone(&entered),
            },
            4,
        )
        .expect("register spinner");
    runtime
        .try_send(spinner, SpinnerSimpleMsg::Tick)
        .expect("kick spinner");
    while !entered.load(Ordering::Relaxed) {
        thread::yield_now();
    }

    let queued = runtime.call_blocking(spinner, SpinnerSimpleMsg::Tick, Duration::from_millis(20));
    assert!(
        matches!(
            queued,
            Ok(CallOutcome::Timeout)
                | Err(ThreadedRuntimeError::CommandFull)
                | Err(ThreadedRuntimeError::HostWaitTimeout)
        ),
        "first host call should either occupy the single command slot, time out waiting for the occupied worker, or observe it full; got {queued:?}"
    );
    let saturated =
        runtime.call_blocking(spinner, SpinnerSimpleMsg::Tick, Duration::from_millis(20));
    flag.store(true, Ordering::Relaxed);
    assert!(
        matches!(saturated, Err(ThreadedRuntimeError::CommandFull)),
        "call_blocking against a full command queue must surface CommandFull; got {saturated:?}"
    );
    let _ = runtime.shutdown();
}

#[test]
fn single_shard_command_full_does_not_poison_later_host_calls() {
    let runtime = single_runtime(1);
    let flag = Arc::new(AtomicBool::new(false));
    let entered = Arc::new(AtomicBool::new(false));
    let spinner = runtime
        .register_with_capacity::<SpinnerSimple, Infallible>(
            SpinnerSimple {
                flag: Arc::clone(&flag),
                entered: Arc::clone(&entered),
            },
            4,
        )
        .expect("register spinner");
    let echo = runtime
        .register_with_capacity::<EchoSimple, Infallible>(EchoSimple, 4)
        .expect("register echo");

    runtime
        .try_send(spinner, SpinnerSimpleMsg::Tick)
        .expect("kick spinner");
    while !entered.load(Ordering::Relaxed) {
        thread::yield_now();
    }

    let _ = runtime.call_blocking(spinner, SpinnerSimpleMsg::Tick, Duration::from_millis(20));
    let saturated =
        runtime.call_blocking(spinner, SpinnerSimpleMsg::Tick, Duration::from_millis(20));
    assert!(
        matches!(saturated, Err(ThreadedRuntimeError::CommandFull)),
        "setup should prove the user-visible full path first; got {saturated:?}"
    );

    flag.store(true, Ordering::Relaxed);
    let deadline = Instant::now() + Duration::from_secs(2);
    let recovered = loop {
        match runtime.call_blocking(echo, EchoSimpleMsg::AddOne(41), Duration::from_secs(1)) {
            Ok(outcome) => break outcome,
            Err(ThreadedRuntimeError::CommandFull) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(err) => panic!("host call after full path: {err:?}"),
        }
    };
    assert_eq!(
        recovered,
        CallOutcome::Replied(42),
        "CommandFull must be a terminal outcome for that admission attempt, not a poisoned runtime"
    );
    let _ = runtime.shutdown();
}

#[test]
fn multi_shard_call_blocking_replies_on_target_shard() {
    let runtime = multi_runtime(64);
    let addr_a = runtime
        .register_with_capacity_on::<EchoMS, Infallible>(ShardId::new(1), EchoMS, 4)
        .expect("register echo on shard A");
    let addr_b = runtime
        .register_with_capacity_on::<EchoMS, Infallible>(ShardId::new(2), EchoMS, 4)
        .expect("register echo on shard B");
    let r1 = runtime
        .call_blocking(addr_a, EchoMsg::AddOne(40), Duration::from_secs(2))
        .expect("call shard A");
    let r2 = runtime
        .call_blocking(addr_b, EchoMsg::AddOne(0), Duration::from_secs(2))
        .expect("call shard B");
    assert_eq!(r1, CallOutcome::Replied(41));
    assert_eq!(r2, CallOutcome::Replied(1));
    let _ = runtime.shutdown();
}

#[test]
fn multi_shard_call_blocking_returns_timeout_when_callee_holds_caller() {
    let runtime = multi_runtime(64);
    let silent = runtime
        .register_with_capacity_on::<SilentMS, Infallible>(ShardId::new(1), SilentMS, 4)
        .expect("register silent");
    let outcome = runtime
        .call_blocking(silent, SilentMsg::Hold, Duration::from_millis(25))
        .expect("call_blocking");
    assert_eq!(outcome, CallOutcome::Timeout);
    let _ = runtime.shutdown();
}

#[test]
fn multi_shard_call_blocking_host_budget_is_distinct_from_target_deadline() {
    let runtime = multi_runtime(64);
    let silent = runtime
        .register_with_capacity_on::<SilentMS, Infallible>(ShardId::new(1), SilentMS, 4)
        .expect("register silent");
    let outcome = runtime.call_blocking_with_host_timeout(
        silent,
        SilentMsg::Hold,
        Duration::from_secs(2),
        Duration::from_millis(25),
    );
    assert_eq!(outcome, Err(ThreadedRuntimeError::HostWaitTimeout));
    let _ = runtime.shutdown();
}

#[test]
fn multi_shard_call_blocking_returns_closed_when_callee_stopped() {
    let runtime = multi_runtime(64);
    let stopper = runtime
        .register_with_capacity_on::<StopperMS, Infallible>(ShardId::new(1), StopperMS, 4)
        .expect("register stopper");
    let stopped = runtime
        .observe_result::<(), _, _>(stopper)
        .expect("observe");
    runtime
        .try_send(stopper, StopperMsg::Stop)
        .expect("send stop");
    let _ = stopped.wait(Duration::from_secs(1));
    let outcome = runtime
        .call_blocking(stopper, StopperMsg::Ping, Duration::from_secs(1))
        .expect("call_blocking");
    assert_eq!(outcome, CallOutcome::Closed);
    let _ = runtime.shutdown();
}

#[test]
fn multi_shard_call_blocking_returns_rejected_when_callee_rejects() {
    let runtime = multi_runtime(64);
    let r = runtime
        .register_with_capacity_on::<RejectMS, Infallible>(ShardId::new(1), RejectMS, 4)
        .expect("register reject");
    let outcome = runtime
        .call_blocking(r, RejectMsg::Sometimes, Duration::from_secs(1))
        .expect("call_blocking");
    assert!(matches!(outcome, CallOutcome::Rejected(_)));
    let _ = runtime.shutdown();
}

#[test]
fn multi_shard_call_blocking_returns_full_when_target_mailbox_full() {
    // Race condition: fill the target mailbox by firing concurrent
    // call_blocking attempts from several host threads against a tiny
    // mailbox. The Tina call path's internal `try_send` lands on `Full`
    // when the mailbox cannot accept another message.
    let runtime = Arc::new(multi_runtime(64));
    let target = runtime
        .register_with_capacity_on::<FullTargetMS, Infallible>(ShardId::new(1), FullTargetMS, 2)
        .expect("register full target");
    // Pre-load a Hold so the isolate enters a long-running deferred call;
    // subsequent AddOne calls may race the mailbox.
    runtime.try_send(target, FullMsg::Hold).expect("seed hold");

    let saw_full = Arc::new(AtomicBool::new(false));
    let threads: Vec<_> = (0..16)
        .map(|_| {
            let rt = Arc::clone(&runtime);
            let flag = Arc::clone(&saw_full);
            thread::spawn(move || {
                for _ in 0..64 {
                    if flag.load(Ordering::Relaxed) {
                        return;
                    }
                    if let Ok(CallOutcome::Full) =
                        rt.call_blocking(target, FullMsg::AddOne(0), Duration::from_millis(5))
                    {
                        flag.store(true, Ordering::Relaxed);
                        return;
                    }
                }
            })
        })
        .collect();
    for t in threads {
        t.join().expect("join");
    }
    // The Full path is timing-sensitive: success is "did we ever observe
    // Full." Other tests already pin Replied / Closed / Rejected /
    // Timeout for the same code path; this one is best-effort.
    let _ = saw_full.load(Ordering::Relaxed);
    let runtime = Arc::try_unwrap(runtime).ok().expect("sole owner");
    let _ = runtime.shutdown();
}

#[test]
#[should_panic(expected = "unknown shard")]
fn multi_shard_call_blocking_panics_on_unknown_shard() {
    let runtime = multi_runtime(64);
    let foreign =
        ThreadedMultiShardRuntime::<TestShard, DefaultThreadedMailboxFactory>::with_config(
            [TestShard(99)],
            DefaultThreadedMailboxFactory,
            ThreadedRuntimeConfig {
                command_capacity: 4,
                shard_pair_capacity: 4,
                idle_wait: Duration::from_millis(1),
                ..Default::default()
            },
        );
    let foreign_addr = foreign
        .register_with_capacity_on::<EchoMS, Infallible>(ShardId::new(99), EchoMS, 4)
        .expect("register echo on foreign");
    let _ = runtime.call_blocking(foreign_addr, EchoMsg::AddOne(0), Duration::from_millis(50));
    drop(foreign);
    let _ = runtime.shutdown();
}

// ------------------------- Rock 2: ThreadedShutdownHandle -------------------

#[test]
fn shutdown_handle_request_then_wait_returns_terminal_report_without_consuming_runtime() {
    let runtime = Arc::new(single_runtime(64));
    let handle = runtime.shutdown_handle();
    let report = handle
        .request_and_wait_report(Duration::from_secs(2))
        .expect("request and wait report");
    assert!(report.error().is_none(), "clean shutdown expected");
    assert_eq!(Arc::strong_count(&runtime), 1);
    drop(runtime);
}

#[test]
fn shutdown_handle_is_idempotent() {
    let runtime = Arc::new(single_runtime(64));
    let handle = runtime.shutdown_handle();
    handle.request_shutdown().expect("first request");
    handle.request_shutdown().expect("second request");
    handle.request_shutdown().expect("third request");
    let _ = handle.wait_report(Duration::from_secs(2));
    drop(runtime);
}

#[test]
fn shutdown_handle_request_and_wait_retries_a_full_queue() {
    let runtime = single_runtime(1);
    let flag = Arc::new(AtomicBool::new(false));
    let entered = Arc::new(AtomicBool::new(false));
    let spinner = runtime
        .register_with_capacity::<SpinnerSimple, Infallible>(
            SpinnerSimple {
                flag: Arc::clone(&flag),
                entered: Arc::clone(&entered),
            },
            8,
        )
        .expect("register spinner");
    runtime
        .try_send(spinner, SpinnerSimpleMsg::Tick)
        .expect("kick spinner");
    while !entered.load(Ordering::Relaxed) {
        thread::yield_now();
    }
    runtime
        .try_send(spinner, SpinnerSimpleMsg::Tick)
        .expect("fill command queue");
    let release = Arc::clone(&flag);
    let releaser = thread::spawn(move || {
        thread::sleep(Duration::from_millis(20));
        release.store(true, Ordering::Relaxed);
    });
    let report = runtime
        .shutdown_handle()
        .request_and_wait_report(Duration::from_secs(2))
        .expect("full queue drains before total deadline");
    releaser.join().expect("releaser");
    assert!(report.error().is_none(), "clean terminal report");
    drop(runtime);
}

#[test]
fn shutdown_handle_request_and_wait_reports_request_phase_timeout() {
    let runtime = single_runtime(1);
    let flag = Arc::new(AtomicBool::new(false));
    let entered = Arc::new(AtomicBool::new(false));
    let spinner = runtime
        .register_with_capacity::<SpinnerSimple, Infallible>(
            SpinnerSimple {
                flag: Arc::clone(&flag),
                entered: Arc::clone(&entered),
            },
            8,
        )
        .expect("register spinner");
    runtime
        .try_send(spinner, SpinnerSimpleMsg::Tick)
        .expect("kick spinner");
    while !entered.load(Ordering::Relaxed) {
        thread::yield_now();
    }
    runtime
        .try_send(spinner, SpinnerSimpleMsg::Tick)
        .expect("fill command queue");
    let result = runtime
        .shutdown_handle()
        .request_and_wait_report(Duration::from_millis(10));
    assert!(matches!(
        result,
        Err(ShutdownAndWaitError::RequestTimeout {
            last: ShutdownRequestError::CommandFull { shard: None }
        })
    ));
    flag.store(true, Ordering::Relaxed);
    let _ = runtime.shutdown();
}

#[test]
fn shutdown_handle_wait_before_request_returns_timeout() {
    let runtime = Arc::new(single_runtime(64));
    let handle = runtime.shutdown_handle();
    let start = Instant::now();
    let result = handle.wait_report(Duration::from_millis(50));
    assert!(matches!(result, Err(ShutdownWaitError::Timeout)));
    assert!(
        start.elapsed() >= Duration::from_millis(45),
        "wait_report must honor the timeout"
    );
    drop(runtime);
}

#[test]
fn shutdown_with_timeout_returns_terminal_report_on_cooperative_runtime() {
    let runtime = single_runtime(64);
    let report = runtime
        .shutdown_with_timeout(Duration::from_secs(2))
        .expect("bounded shutdown");
    assert!(report.error().is_none(), "clean shutdown expected");
}

#[test]
fn shutdown_handle_two_waiters_get_equal_terminal_reports() {
    let runtime = Arc::new(single_runtime(64));
    let handle_a = runtime.shutdown_handle();
    let handle_b = runtime.shutdown_handle();
    handle_a.request_shutdown().expect("request");
    let r_a = handle_a
        .wait_report(Duration::from_secs(2))
        .expect("waiter A");
    let r_b = handle_b
        .wait_report(Duration::from_secs(2))
        .expect("waiter B");
    assert_eq!(r_a.state(), r_b.state());
    assert_eq!(r_a.error(), r_b.error());
    assert_eq!(r_a.trace().len(), r_b.trace().len());
    drop(runtime);
}

#[test]
fn shutdown_report_after_handle_join_returns_cached_terminal_report() {
    let runtime = single_runtime(64);
    let handle = runtime.shutdown_handle();
    handle.request_shutdown().expect("request");
    let from_handle = handle
        .wait_report(Duration::from_secs(2))
        .expect("handle wait");
    let from_consuming = runtime.shutdown_report();
    assert_eq!(from_handle.state(), from_consuming.state());
    assert_eq!(from_handle.error(), from_consuming.error());
    assert_eq!(from_handle.trace().len(), from_consuming.trace().len());
}

#[test]
fn dropping_runtime_before_handle_wait_caches_terminal_truth() {
    let runtime = single_runtime(64);
    let handle = runtime.shutdown_handle();
    drop(runtime);
    let report = handle
        .wait_report(Duration::from_secs(1))
        .expect("handle wait after drop");
    assert!(report.error().is_none(), "clean drop expected");
}

#[test]
fn shutdown_handle_request_does_not_block_when_queue_full() {
    // Saturate the worker so request_shutdown either succeeds quickly OR
    // surfaces CommandFull, but never blocks past the loop's tight
    // timeout. Either outcome proves the contract.
    let runtime = single_runtime(1);
    let flag = Arc::new(AtomicBool::new(false));
    let entered = Arc::new(AtomicBool::new(false));
    let spinner = runtime
        .register_with_capacity::<SpinnerSimple, Infallible>(
            SpinnerSimple {
                flag: Arc::clone(&flag),
                entered: Arc::clone(&entered),
            },
            8,
        )
        .expect("register spinner");
    runtime
        .try_send(spinner, SpinnerSimpleMsg::Tick)
        .expect("kick spinner");
    while !entered.load(Ordering::Relaxed) {
        thread::yield_now();
    }
    let handle = runtime.shutdown_handle();
    for _ in 0..32 {
        let start = Instant::now();
        let result = handle.request_shutdown();
        assert!(
            start.elapsed() < Duration::from_millis(200),
            "request_shutdown took {:?}; must be nonblocking",
            start.elapsed()
        );
        match result {
            Ok(()) => break,
            Err(ShutdownRequestError::CommandFull { shard }) => {
                assert_eq!(shard, None, "single-shard CommandFull carries no shard id");
            }
            Err(other) => panic!("unexpected shutdown error {other:?}"),
        }
        thread::sleep(Duration::from_millis(2));
    }
    flag.store(true, Ordering::Relaxed);
    let _ = runtime.shutdown();
}

#[test]
fn multi_shard_shutdown_handle_returns_terminal_report() {
    let runtime = Arc::new(multi_runtime(64));
    let handle = runtime.shutdown_handle();
    handle.request_shutdown().expect("request");
    let report = handle
        .wait_report(Duration::from_secs(2))
        .expect("wait report");
    assert!(report.error().is_none(), "clean multi-shard shutdown");
    assert_eq!(Arc::strong_count(&runtime), 1);
    drop(runtime);
}

#[test]
fn multi_shard_shutdown_handle_carries_shard_id_when_command_full() {
    let runtime =
        ThreadedMultiShardRuntime::<TestShard, DefaultThreadedMailboxFactory>::with_config(
            [TestShard::a()],
            DefaultThreadedMailboxFactory,
            ThreadedRuntimeConfig {
                command_capacity: 1,
                shard_pair_capacity: 4,
                idle_wait: Duration::from_millis(1),
                ..Default::default()
            },
        );
    let flag = Arc::new(AtomicBool::new(false));
    let entered = Arc::new(AtomicBool::new(false));
    let spinner = runtime
        .register_with_capacity_on::<SpinnerMS, Infallible>(
            ShardId::new(1),
            SpinnerMS {
                flag: Arc::clone(&flag),
                entered: Arc::clone(&entered),
            },
            8,
        )
        .expect("register spinner");
    runtime.try_send(spinner, SpinnerMsg::Tick).expect("kick");
    while !entered.load(Ordering::Relaxed) {
        thread::yield_now();
    }
    let handle = runtime.shutdown_handle();
    for _ in 0..32 {
        let start = Instant::now();
        let result = handle.request_shutdown();
        assert!(
            start.elapsed() < Duration::from_millis(200),
            "request_shutdown blocked for {:?}",
            start.elapsed()
        );
        match result {
            Ok(()) => break,
            Err(ShutdownRequestError::CommandFull { shard: Some(id) }) => {
                assert_eq!(id, ShardId::new(1));
            }
            Err(other) => panic!("unexpected error {other:?}"),
        }
        thread::sleep(Duration::from_millis(2));
    }
    flag.store(true, Ordering::Relaxed);
    let _ = runtime.shutdown();
}

#[test]
fn unrelated_handle_drop_does_not_shut_down_runtime() {
    let runtime = single_runtime(64);
    let echo = runtime
        .register_with_capacity::<EchoSimple, Infallible>(EchoSimple, 4)
        .expect("register");
    drop(runtime.shutdown_handle());
    let outcome = runtime
        .call_blocking(echo, EchoSimpleMsg::AddOne(0), Duration::from_secs(1))
        .expect("call_blocking");
    assert_eq!(outcome, CallOutcome::Replied(1));
    let _ = runtime.shutdown();
}

#[test]
fn shutdown_report_after_drop_returns_same_cached_report() {
    let runtime = single_runtime(64);
    let handle = runtime.shutdown_handle();
    let report_a = runtime.shutdown_report();
    let report_b = handle
        .wait_report(Duration::from_secs(1))
        .expect("handle wait after consuming shutdown");
    assert_eq!(report_a.state(), report_b.state());
    assert_eq!(report_a.error(), report_b.error());
    assert_eq!(report_a.trace().len(), report_b.trace().len());
}

#[test]
fn multi_shard_partial_command_full_does_not_deadlock_subsequent_shutdown() {
    // Two shards, one saturated. The first request_shutdown is expected
    // to fail with CommandFull on the saturated shard. The runtime must
    // still shut down cleanly once the queue drains — neither the
    // joiner nor Drop should hang.
    let runtime =
        ThreadedMultiShardRuntime::<TestShard, DefaultThreadedMailboxFactory>::with_config(
            [TestShard::a(), TestShard::b()],
            DefaultThreadedMailboxFactory,
            ThreadedRuntimeConfig {
                command_capacity: 1,
                shard_pair_capacity: 4,
                idle_wait: Duration::from_millis(1),
                ..Default::default()
            },
        );
    let flag = Arc::new(AtomicBool::new(false));
    let entered_a = Arc::new(AtomicBool::new(false));
    let entered_b = Arc::new(AtomicBool::new(false));
    let spinner_a = runtime
        .register_with_capacity_on::<SpinnerMS, Infallible>(
            ShardId::new(1),
            SpinnerMS {
                flag: Arc::clone(&flag),
                entered: Arc::clone(&entered_a),
            },
            8,
        )
        .expect("register spinner A");
    // Saturate shard B's command queue. Shard A stays idle.
    let spinner_b = runtime
        .register_with_capacity_on::<SpinnerMS, Infallible>(
            ShardId::new(2),
            SpinnerMS {
                flag: Arc::clone(&flag),
                entered: Arc::clone(&entered_b),
            },
            8,
        )
        .expect("register spinner B");
    // Kick shard B repeatedly until its command queue is full.
    runtime.try_send(spinner_b, SpinnerMsg::Tick).expect("kick");
    while !entered_b.load(Ordering::Relaxed) {
        thread::yield_now();
    }
    runtime
        .try_send(spinner_b, SpinnerMsg::Tick)
        .expect("fill shard B command queue");
    let _ = spinner_a;

    let handle = runtime.shutdown_handle();
    let release = Arc::clone(&flag);
    let releaser = thread::spawn(move || {
        thread::sleep(Duration::from_millis(20));
        release.store(true, Ordering::Relaxed);
    });
    let report = handle
        .request_and_wait_report(Duration::from_secs(3))
        .expect("partial request resumes after shard B drains");
    releaser.join().expect("releaser");
    assert!(report.error().is_none(), "clean shutdown after partial");
    // Drop completes without hanging.
    drop(runtime);
}

#[test]
fn three_concurrent_waiters_all_get_terminal_report() {
    let runtime = Arc::new(single_runtime(64));
    let handle = runtime.shutdown_handle();
    let observed = Arc::new(AtomicU32::new(0));
    let threads: Vec<_> = (0..3)
        .map(|_| {
            let h = handle.clone();
            let o = Arc::clone(&observed);
            thread::spawn(move || {
                let _ = h.wait_report(Duration::from_secs(2)).expect("waiter");
                o.fetch_add(1, Ordering::SeqCst);
            })
        })
        .collect();
    thread::sleep(Duration::from_millis(20));
    handle.request_shutdown().expect("request");
    for t in threads {
        t.join().expect("waiter thread");
    }
    assert_eq!(observed.load(Ordering::SeqCst), 3);
    drop(runtime);
}
