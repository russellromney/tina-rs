use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use tina::prelude::*;
use tina::{AddressGeneration, Mailbox};
use tina_runtime::{
    CallOutcome, DefaultMailboxFactory, DefaultThreadedMailboxFactory, IngressSendError,
    LocalSystem, MailboxFactory, MultiShardRuntime, ThreadedMultiShardRuntime, ThreadedRuntime,
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
    system: tina::SystemIncarnation,
    shard: ShardId,
    isolate: IsolateId,
    generation: AddressGeneration,
}

struct WhoAmI {
    identity: Identity,
}

struct DropAuthority(Arc<AtomicU32>);

impl Drop for DropAuthority {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
}

#[derive(Debug, Clone, Copy)]
struct CapacityPanicMailboxFactory;

impl MailboxFactory for CapacityPanicMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        if capacity == 13 {
            panic!("intentional address-aware mailbox allocation panic");
        }
        DefaultThreadedMailboxFactory.create(capacity)
    }
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
        system: address.system(),
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
fn mailbox_panic_consumes_id_without_running_constructor_and_drops_authority_once() {
    let mut runtime = tina_runtime::Runtime::new(AppShard(7), CapacityPanicMailboxFactory);
    let constructed = Arc::new(AtomicU32::new(0));
    let drops = Arc::new(AtomicU32::new(0));
    let constructed_in_ctor = Arc::clone(&constructed);
    let authority = DropAuthority(Arc::clone(&drops));
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.register_with_capacity_using::<WhoAmI, Infallible, _>(13, move |address| {
            let _authority = authority;
            constructed_in_ctor.fetch_add(1, Ordering::AcqRel);
            WhoAmI {
                identity: identity(address),
            }
        })
    }));
    assert!(panicked.is_err());
    assert_eq!(constructed.load(Ordering::Acquire), 0);
    assert_eq!(drops.load(Ordering::Acquire), 1);

    let next = runtime.register_with_capacity_using::<WhoAmI, Infallible, _>(4, |address| WhoAmI {
        identity: identity(address),
    });
    assert_eq!(next.isolate(), IsolateId::new(2));
}

#[test]
fn zero_capacity_using_publishes_initialized_but_full_mailbox() {
    let mut runtime = tina_runtime::Runtime::new(AppShard(7), DefaultMailboxFactory);
    let constructed = Arc::new(AtomicU32::new(0));
    let constructed_in_ctor = Arc::clone(&constructed);
    let address =
        runtime.register_with_capacity_using::<WhoAmI, Infallible, _>(0, move |address| {
            constructed_in_ctor.fetch_add(1, Ordering::AcqRel);
            WhoAmI {
                identity: identity(address),
            }
        });
    assert_eq!(constructed.load(Ordering::Acquire), 1);
    assert!(matches!(
        runtime.try_send(address, WhoMsg::Who),
        Err(IngressSendError::Full(WhoMsg::Who))
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
    let drops = Arc::new(AtomicU32::new(0));
    let authority = DropAuthority(Arc::clone(&drops));

    assert!(matches!(
        app.register_root_using_on::<WhoAmI, Infallible, _>(
            ShardId::new(99),
            4,
            move |address| {
                let _authority = authority;
                constructed_in_ctor.fetch_add(1, Ordering::AcqRel);
                WhoAmI {
                    identity: identity(address),
                }
            },
        ),
        Err(ThreadedRuntimeError::UnknownShard(shard)) if shard == ShardId::new(99)
    ));
    assert_eq!(constructed.load(Ordering::Acquire), 0);
    assert_eq!(drops.load(Ordering::Acquire), 1);
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
    let drops = Arc::new(AtomicU32::new(0));
    let authority = DropAuthority(Arc::clone(&drops));

    assert!(matches!(
        app.register_root_using::<WhoAmI, Infallible, _>(4, move |address| {
            let _authority = authority;
            constructed_in_ctor.fetch_add(1, Ordering::AcqRel);
            WhoAmI {
                identity: identity(address),
            }
        }),
        Err(ThreadedRuntimeError::WorkerStopped)
    ));
    assert_eq!(constructed.load(Ordering::Acquire), 0);
    assert_eq!(drops.load(Ordering::Acquire), 1);
}

#[test]
fn accepted_single_constructor_timeout_can_publish_later() {
    let timeout = Duration::from_millis(30);
    let runtime = Arc::new(ThreadedRuntime::with_config(
        AppShard(7),
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            control_call_timeout: timeout,
            ..ThreadedRuntimeConfig::default()
        },
    ));
    let leaked = Arc::new(std::sync::Mutex::new(None));
    let leaked_in_ctor = Arc::clone(&leaked);
    let release = Arc::new(AtomicBool::new(false));
    let release_in_ctor = Arc::clone(&release);
    let runtime_for_register = Arc::clone(&runtime);
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let register =
        std::thread::spawn(move || {
            let result = runtime_for_register
                .register_with_capacity_using::<WhoAmI, Infallible, _>(4, move |address| {
                    *leaked_in_ctor.lock().expect("capture constructor address") = Some(address);
                    entered_tx.send(()).expect("report constructor entry");
                    while !release_in_ctor.load(Ordering::Acquire) {
                        std::thread::yield_now();
                    }
                    WhoAmI {
                        identity: identity(address),
                    }
                });
            result_tx.send(result).expect("report registration result");
        });

    if let Err(error) = entered_rx.recv_timeout(Duration::from_secs(2)) {
        release.store(true, Ordering::Release);
        register.join().expect("registration cleanup");
        panic!("constructor did not start: {error}");
    }
    let result = match result_rx.recv_timeout(Duration::from_secs(2)) {
        Ok(result) => result,
        Err(error) => {
            release.store(true, Ordering::Release);
            register.join().expect("registration cleanup");
            panic!("accepted constructor did not time out: {error}");
        }
    };
    release.store(true, Ordering::Release);
    register.join().expect("registration thread");

    assert!(matches!(
        result,
        Err(ThreadedRuntimeError::WorkerUnresponsive)
    ));
    let address = leaked
        .lock()
        .expect("read constructor address")
        .expect("accepted constructor ran");
    assert_eq!(address.system(), runtime.system_incarnation());
    assert_eq!(
        runtime.call_blocking(address, WhoMsg::Who, Duration::from_secs(1)),
        Ok(CallOutcome::Replied(identity(address)))
    );
}

#[test]
fn accepted_multi_constructor_timeout_can_publish_later_on_selected_owner() {
    let timeout = Duration::from_millis(30);
    let runtime = Arc::new(ThreadedMultiShardRuntime::with_config(
        [AppShard(3), AppShard(9)],
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            control_call_timeout: timeout,
            ..ThreadedRuntimeConfig::default()
        },
    ));
    let leaked = Arc::new(std::sync::Mutex::new(None));
    let leaked_in_ctor = Arc::clone(&leaked);
    let release = Arc::new(AtomicBool::new(false));
    let release_in_ctor = Arc::clone(&release);
    let runtime_for_register = Arc::clone(&runtime);
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let register = std::thread::spawn(move || {
        let result = runtime_for_register.register_with_capacity_using_on::<WhoAmI, Infallible, _>(
            ShardId::new(9),
            4,
            move |address| {
                *leaked_in_ctor.lock().expect("capture constructor address") = Some(address);
                entered_tx.send(()).expect("report constructor entry");
                while !release_in_ctor.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
                WhoAmI {
                    identity: identity(address),
                }
            },
        );
        result_tx.send(result).expect("report registration result");
    });

    if let Err(error) = entered_rx.recv_timeout(Duration::from_secs(2)) {
        release.store(true, Ordering::Release);
        register.join().expect("registration cleanup");
        panic!("constructor did not start: {error}");
    }
    let result = match result_rx.recv_timeout(Duration::from_secs(2)) {
        Ok(result) => result,
        Err(error) => {
            release.store(true, Ordering::Release);
            register.join().expect("registration cleanup");
            panic!("accepted constructor did not time out: {error}");
        }
    };
    release.store(true, Ordering::Release);
    register.join().expect("registration thread");

    assert!(matches!(
        result,
        Err(ThreadedRuntimeError::WorkerUnresponsive)
    ));
    let address = leaked
        .lock()
        .expect("read constructor address")
        .expect("accepted constructor ran");
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
fn threaded_multi_mailbox_panic_drops_constructor_and_fails_only_selected_owner() {
    let app = Arc::new(
        LocalSystem::multi_shard(CapacityPanicMailboxFactory)
            .shard(AppShard(3))
            .shard(AppShard(9))
            .try_build()
            .expect("start local system"),
    );
    let constructed = Arc::new(AtomicU32::new(0));
    let constructed_in_ctor = Arc::clone(&constructed);
    let drops = Arc::new(AtomicU32::new(0));
    let authority = DropAuthority(Arc::clone(&drops));
    assert!(matches!(
        app.register_root_using_on::<WhoAmI, Infallible, _>(ShardId::new(3), 13, move |address| {
            let _authority = authority;
            constructed_in_ctor.fetch_add(1, Ordering::AcqRel);
            WhoAmI {
                identity: identity(address),
            }
        },),
        Err(ThreadedRuntimeError::WorkerStopped)
    ));
    assert_eq!(constructed.load(Ordering::Acquire), 0);
    assert_eq!(drops.load(Ordering::Acquire), 1);

    let healthy = app
        .register_root_using_on::<WhoAmI, Infallible, _>(ShardId::new(9), 4, |address| WhoAmI {
            identity: identity(address),
        })
        .expect("peer shard remains healthy");
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
