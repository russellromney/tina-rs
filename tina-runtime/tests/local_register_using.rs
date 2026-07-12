use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use tina::AddressGeneration;
use tina::prelude::*;
use tina_runtime::{
    CallOutcome, DefaultMailboxFactory, DefaultThreadedMailboxFactory, IngressSendError,
    LocalSystem, MultiShardRuntime, ThreadedMultiShardRuntime, ThreadedRuntime,
    ThreadedRuntimeConfig, ThreadedRuntimeError,
};

#[derive(Debug, Clone, Copy)]
struct AppShard(u32);

impl Shard for AppShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

#[derive(Debug)]
enum WhoMsg {
    Who,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Identity {
    shard: ShardId,
    isolate: IsolateId,
    generation: AddressGeneration,
}

struct WhoAmI {
    identity: Identity,
}

#[tina_runtime::isolate(message = WhoMsg, reply = Identity, shard = AppShard)]
impl WhoAmI {
    fn handle(
        &mut self,
        _message: WhoMsg,
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        noop()
    }

    fn handle_call(&mut self, _message: WhoMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        call.reply(self.identity)
    }
}

fn identity(address: Address<WhoMsg, Identity>) -> Identity {
    Identity {
        shard: address.shard(),
        isolate: address.isolate(),
        generation: address.generation(),
    }
}

#[test]
fn local_system_register_root_using_supports_typed_host_calls() {
    let app = LocalSystem::single_shard(AppShard(7), DefaultThreadedMailboxFactory)
        .try_build()
        .expect("start local system");
    let address = app
        .register_root_using(4, |address| WhoAmI {
            identity: identity(address),
        })
        .expect("register address-aware root");

    assert_eq!(address.system(), app.system_incarnation());
    assert_eq!(
        app.call_blocking(address, WhoMsg::Who, Duration::from_secs(1)),
        Ok(CallOutcome::Replied(identity(address)))
    );
    app.shutdown()
        .drain()
        .join_report()
        .ensure_clean()
        .expect("clean shutdown");
}

#[test]
fn local_multi_shard_register_root_using_on_preserves_owner_and_typed_call() {
    let app = LocalSystem::multi_shard(DefaultThreadedMailboxFactory)
        .shard(AppShard(3))
        .shard(AppShard(9))
        .try_build()
        .expect("start multi-shard local system");
    let address = app
        .register_root_using_on(ShardId::new(9), 4, |address| WhoAmI {
            identity: identity(address),
        })
        .expect("register on chosen owner");

    assert_eq!(address.system(), app.system_incarnation());
    assert_eq!(address.shard(), ShardId::new(9));
    assert_eq!(
        app.call_blocking(address, WhoMsg::Who, Duration::from_secs(1)),
        Ok(CallOutcome::Replied(identity(address)))
    );
    app.shutdown()
        .drain()
        .join_report()
        .ensure_clean()
        .expect("clean shutdown");
}

#[test]
fn explicit_multi_shard_constructor_panic_publishes_no_entry_and_does_not_reuse_id() {
    let mut runtime = MultiShardRuntime::new([AppShard(3), AppShard(9)], DefaultMailboxFactory);
    let leaked = Arc::new(std::sync::Mutex::new(None));
    let leaked_from_ctor = Arc::clone(&leaked);
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.register_with_capacity_using_on::<WhoAmI, Infallible, _>(
            ShardId::new(9),
            4,
            move |address| -> WhoAmI {
                *leaked_from_ctor.lock().expect("capture leaked address") = Some(address);
                panic!("constructor failed")
            },
        )
    }));
    assert!(panicked.is_err());

    let leaked = leaked
        .lock()
        .expect("read leaked address")
        .expect("captured");
    let next = runtime.register_with_capacity_using_on::<WhoAmI, Infallible, _>(
        ShardId::new(9),
        4,
        |address| WhoAmI {
            identity: identity(address),
        },
    );
    assert_eq!(leaked.shard(), ShardId::new(9));
    assert_eq!(leaked.generation(), AddressGeneration::new(0));
    assert_eq!(next.shard(), leaked.shard());
    assert_eq!(next.isolate().get(), leaked.isolate().get() + 1);
    assert_eq!(next.generation(), AddressGeneration::new(0));
    assert!(matches!(
        runtime.try_send(leaked, WhoMsg::Who),
        Err(IngressSendError::Closed(WhoMsg::Who))
    ));
}

#[test]
fn local_multi_shard_unknown_owner_does_not_run_constructor() {
    let app = LocalSystem::multi_shard(DefaultThreadedMailboxFactory)
        .shard(AppShard(3))
        .try_build()
        .expect("start local system");
    let constructed = Arc::new(AtomicU32::new(0));
    let constructed_in_ctor = Arc::clone(&constructed);

    assert!(matches!(
        app.register_root_using_on::<WhoAmI, Infallible, _>(
            ShardId::new(99),
            4,
            move |address| {
                constructed_in_ctor.fetch_add(1, Ordering::AcqRel);
                WhoAmI {
                    identity: identity(address),
                }
            },
        ),
        Err(ThreadedRuntimeError::UnknownShard(shard)) if shard == ShardId::new(99)
    ));
    assert_eq!(constructed.load(Ordering::Acquire), 0);
    app.shutdown()
        .drain()
        .join_report()
        .ensure_clean()
        .expect("clean shutdown");
}

#[test]
fn local_system_closed_owner_does_not_run_constructor() {
    let app = Arc::new(
        LocalSystem::single_shard(AppShard(7), DefaultThreadedMailboxFactory)
            .try_build()
            .expect("start local system"),
    );
    app.shutdown_handle()
        .request_and_wait_report(Duration::from_secs(1))
        .expect("stop worker");
    let constructed = Arc::new(AtomicU32::new(0));
    let constructed_in_ctor = Arc::clone(&constructed);

    assert!(matches!(
        app.register_root_using::<WhoAmI, Infallible, _>(4, move |address| {
            constructed_in_ctor.fetch_add(1, Ordering::AcqRel);
            WhoAmI {
                identity: identity(address),
            }
        }),
        Err(ThreadedRuntimeError::WorkerStopped)
    ));
    assert_eq!(constructed.load(Ordering::Acquire), 0);
}

#[test]
fn accepted_single_constructor_timeout_can_publish_later() {
    let timeout = Duration::from_millis(30);
    let runtime = ThreadedRuntime::with_config(
        AppShard(7),
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            control_call_timeout: timeout,
            ..ThreadedRuntimeConfig::default()
        },
    );
    let leaked = Arc::new(std::sync::Mutex::new(None));
    let leaked_in_ctor = Arc::clone(&leaked);
    let release = Arc::new(AtomicBool::new(false));
    let release_in_ctor = Arc::clone(&release);
    let published = Arc::new(AtomicBool::new(false));
    let published_in_ctor = Arc::clone(&published);

    let started = Instant::now();
    let result = runtime.register_with_capacity_using::<WhoAmI, Infallible, _>(4, move |address| {
        *leaked_in_ctor.lock().expect("capture constructor address") = Some(address);
        while !release_in_ctor.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        published_in_ctor.store(true, Ordering::Release);
        WhoAmI {
            identity: identity(address),
        }
    });
    release.store(true, Ordering::Release);

    assert!(matches!(
        result,
        Err(ThreadedRuntimeError::WorkerUnresponsive)
    ));
    assert!(started.elapsed() >= timeout);
    let address = leaked
        .lock()
        .expect("read constructor address")
        .expect("accepted constructor ran");
    let deadline = Instant::now() + Duration::from_secs(2);
    while !published.load(Ordering::Acquire) && Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert!(published.load(Ordering::Acquire));
    assert_eq!(address.system(), runtime.system_incarnation());
    assert_eq!(
        runtime.call_blocking(address, WhoMsg::Who, Duration::from_secs(1)),
        Ok(CallOutcome::Replied(identity(address)))
    );
}

#[test]
fn accepted_multi_constructor_timeout_can_publish_later_on_selected_owner() {
    let timeout = Duration::from_millis(30);
    let runtime = ThreadedMultiShardRuntime::with_config(
        [AppShard(3), AppShard(9)],
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            control_call_timeout: timeout,
            ..ThreadedRuntimeConfig::default()
        },
    );
    let leaked = Arc::new(std::sync::Mutex::new(None));
    let leaked_in_ctor = Arc::clone(&leaked);
    let release = Arc::new(AtomicBool::new(false));
    let release_in_ctor = Arc::clone(&release);
    let published = Arc::new(AtomicBool::new(false));
    let published_in_ctor = Arc::clone(&published);

    let result = runtime.register_with_capacity_using_on::<WhoAmI, Infallible, _>(
        ShardId::new(9),
        4,
        move |address| {
            *leaked_in_ctor.lock().expect("capture constructor address") = Some(address);
            while !release_in_ctor.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            published_in_ctor.store(true, Ordering::Release);
            WhoAmI {
                identity: identity(address),
            }
        },
    );
    release.store(true, Ordering::Release);

    assert!(matches!(
        result,
        Err(ThreadedRuntimeError::WorkerUnresponsive)
    ));
    let address = leaked
        .lock()
        .expect("read constructor address")
        .expect("accepted constructor ran");
    let deadline = Instant::now() + Duration::from_secs(2);
    while !published.load(Ordering::Acquire) && Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert!(published.load(Ordering::Acquire));
    assert_eq!(address.system(), runtime.system_incarnation());
    assert_eq!(address.shard(), ShardId::new(9));
    assert_eq!(
        runtime.call_blocking(address, WhoMsg::Who, Duration::from_secs(1)),
        Ok(CallOutcome::Replied(identity(address)))
    );
}

#[test]
fn local_system_constructor_panic_surfaces_worker_failure() {
    let app = Arc::new(
        LocalSystem::single_shard(AppShard(7), DefaultThreadedMailboxFactory)
            .try_build()
            .expect("start local system"),
    );
    assert!(matches!(
        app.register_root_using::<WhoAmI, Infallible, _>(4, |_address| -> WhoAmI {
            panic!("constructor failed")
        }),
        Err(ThreadedRuntimeError::WorkerStopped)
    ));
    assert!(matches!(
        app.register_root_using::<WhoAmI, Infallible, _>(4, |address| WhoAmI {
            identity: identity(address),
        }),
        Err(ThreadedRuntimeError::WorkerStopped)
    ));
}

#[test]
fn local_multi_shard_constructor_panic_fails_only_the_selected_owner() {
    let app = Arc::new(
        LocalSystem::multi_shard(DefaultThreadedMailboxFactory)
            .shard(AppShard(3))
            .shard(AppShard(9))
            .try_build()
            .expect("start local system"),
    );
    assert!(matches!(
        app.register_root_using_on::<WhoAmI, Infallible, _>(
            ShardId::new(3),
            4,
            |_address| -> WhoAmI { panic!("constructor failed") },
        ),
        Err(ThreadedRuntimeError::WorkerStopped)
    ));

    let healthy = app
        .register_root_using_on::<WhoAmI, Infallible, _>(ShardId::new(9), 4, |address| WhoAmI {
            identity: identity(address),
        })
        .expect("other owner remains accepting");
    assert_eq!(
        app.call_blocking(healthy, WhoMsg::Who, Duration::from_secs(1)),
        Ok(CallOutcome::Replied(identity(healthy)))
    );
    let report = app
        .shutdown_handle()
        .request_and_wait_report(Duration::from_secs(1))
        .expect("collect failed terminal report");
    assert_eq!(report.error(), Some(ThreadedRuntimeError::WorkerStopped));
}
