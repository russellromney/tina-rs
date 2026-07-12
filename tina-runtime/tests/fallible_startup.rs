use std::collections::VecDeque;
use std::error::Error;
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tina::{Mailbox, Shard, ShardId, SingleShard, SystemIncarnation, TrySendError};
use tina_runtime::{
    DefaultThreadedMailboxFactory, LocalSystem, LocalSystemConfigError, MailboxFactory,
    StartupError, ThreadedMultiShardRuntime, ThreadedRuntime, ThreadedRuntimeConfig,
    ThreadedRuntimeConfigError,
};

#[derive(Debug)]
struct TestMailbox<T> {
    capacity: usize,
    queue: Mutex<VecDeque<T>>,
}

impl<T> TestMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: Mutex::new(VecDeque::with_capacity(capacity)),
        }
    }
}

impl<T> Mailbox<T> for TestMailbox<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }

    fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        let mut queue = self.queue.lock().expect("mailbox lock");
        if queue.len() == self.capacity {
            return Err(TrySendError::Full(message));
        }
        queue.push_back(message);
        Ok(())
    }

    fn recv(&self) -> Option<T> {
        self.queue.lock().expect("mailbox lock").pop_front()
    }

    fn is_empty(&self) -> bool {
        self.queue.lock().expect("mailbox lock").is_empty()
    }

    fn close(&self) {}
}

#[derive(Debug)]
struct PanickingFactory;

impl MailboxFactory for PanickingFactory {
    fn create<T: 'static>(&self, _capacity: usize) -> Box<dyn Mailbox<T>> {
        panic!("injected mailbox factory failure")
    }
}

#[test]
fn threaded_config_validation_is_typed_and_precedes_worker_start() {
    let error = ThreadedRuntime::try_with_config(
        SingleShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 0,
            ..ThreadedRuntimeConfig::default()
        },
    )
    .err()
    .expect("zero command capacity must fail");

    assert!(matches!(
        error,
        StartupError::InvalidThreadedConfig(ThreadedRuntimeConfigError::ZeroCommandCapacity)
    ));
}

#[test]
fn unscoped_threaded_system_incarnation_is_a_typed_startup_error() {
    let error = ThreadedRuntime::try_with_config(
        SingleShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            system_incarnation: Some(SystemIncarnation::DEFAULT),
            ..ThreadedRuntimeConfig::default()
        },
    )
    .err()
    .expect("unscoped system incarnation must fail");

    assert!(matches!(
        error,
        StartupError::InvalidThreadedConfig(ThreadedRuntimeConfigError::UnscopedSystemIncarnation)
    ));
}

#[test]
fn io_loop_initialization_error_keeps_platform_source() {
    let error = ThreadedRuntime::try_with_config_and_io_loop_factory(
        SingleShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig::default(),
        || {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected I/O failure",
            ))
        },
    )
    .err()
    .expect("I/O-loop failure must fail startup");

    match error {
        StartupError::IoLoopInitialization { shard, source } => {
            assert_eq!(shard, ShardId::new(0));
            assert_eq!(source.kind(), io::ErrorKind::PermissionDenied);
            assert_eq!(source.to_string(), "injected I/O failure");
        }
        other => panic!("unexpected startup error: {other:?}"),
    }
}

#[test]
fn startup_error_exposes_the_platform_error_as_its_source() {
    let error = ThreadedRuntime::try_with_config_and_io_loop_factory(
        SingleShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig::default(),
        || Err(io::Error::other("injected source-chain failure")),
    )
    .err()
    .expect("I/O-loop failure must fail startup");

    let source = error
        .source()
        .expect("startup error must retain its source");
    assert_eq!(source.to_string(), "injected source-chain failure");
}

#[test]
fn mailbox_factory_panic_is_captured_at_the_ready_handshake() {
    let error = ThreadedRuntime::try_new(SingleShard, PanickingFactory)
        .err()
        .expect("mailbox panic must fail startup");

    assert!(matches!(
        error,
        StartupError::WorkerStartupPanicked { shard, ref message }
            if shard == ShardId::new(0)
                && message.contains("injected mailbox factory failure")
    ));
}

#[test]
fn local_system_try_build_returns_local_config_error() {
    let error = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory)
        .ingress_capacity(0)
        .try_build()
        .err()
        .expect("invalid local config must fail");

    assert!(matches!(
        error,
        StartupError::InvalidLocalSystemConfig(LocalSystemConfigError::ZeroIngressCapacity)
    ));
}

#[test]
fn local_system_unscoped_incarnation_is_a_typed_startup_error() {
    let error = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory)
        .config(tina_runtime::LocalSystemConfig {
            system_incarnation: Some(SystemIncarnation::DEFAULT),
            ..tina_runtime::LocalSystemConfig::default()
        })
        .try_build()
        .err()
        .expect("unscoped system incarnation must fail");

    assert!(matches!(
        error,
        StartupError::InvalidLocalSystemConfig(LocalSystemConfigError::UnscopedSystemIncarnation)
    ));
}

#[derive(Debug)]
struct DropShard {
    id: u32,
    drops: Arc<AtomicUsize>,
}

impl Shard for DropShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.id)
    }
}

impl Drop for DropShard {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Debug)]
struct PanicOnSecondCloneFactory {
    clones: Arc<AtomicUsize>,
}

impl Clone for PanicOnSecondCloneFactory {
    fn clone(&self) -> Self {
        if self.clones.fetch_add(1, Ordering::SeqCst) == 1 {
            panic!("injected second-shard clone failure");
        }
        Self {
            clones: Arc::clone(&self.clones),
        }
    }
}

impl MailboxFactory for PanicOnSecondCloneFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(TestMailbox::new(capacity))
    }
}

#[test]
fn multi_shard_failure_drops_the_partially_started_topology() {
    let drops = Arc::new(AtomicUsize::new(0));
    let shards = [
        DropShard {
            id: 1,
            drops: Arc::clone(&drops),
        },
        DropShard {
            id: 2,
            drops: Arc::clone(&drops),
        },
    ];
    let error = ThreadedMultiShardRuntime::try_new(
        shards,
        PanicOnSecondCloneFactory {
            clones: Arc::new(AtomicUsize::new(0)),
        },
    )
    .err()
    .expect("second worker startup must fail");
    assert!(matches!(
        error,
        StartupError::WorkerStartupPanicked { shard, ref message }
            if shard == ShardId::new(2)
                && message.contains("injected second-shard clone failure")
    ));

    let deadline = Instant::now() + Duration::from_secs(2);
    while drops.load(Ordering::SeqCst) != 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(drops.load(Ordering::SeqCst), 2);
}

#[test]
fn multi_shard_topology_errors_are_typed() {
    let no_shards =
        ThreadedMultiShardRuntime::<SingleShard, _>::try_new([], DefaultThreadedMailboxFactory)
            .err()
            .expect("empty topology must fail");
    assert!(matches!(no_shards, StartupError::NoShards));

    let duplicate = ThreadedMultiShardRuntime::try_new(
        [SingleShard, SingleShard],
        DefaultThreadedMailboxFactory,
    )
    .err()
    .expect("duplicate topology must fail");
    assert!(matches!(
        duplicate,
        StartupError::DuplicateShard(shard) if shard == ShardId::new(0)
    ));
}

#[derive(Debug, Clone, Copy)]
struct IdShard(u32);

impl Shard for IdShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

#[test]
fn multi_shard_core_assignment_overflow_is_typed_before_start() {
    let error = ThreadedMultiShardRuntime::try_with_config(
        [IdShard(1), IdShard(2)],
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            configured_core: Some(usize::MAX),
            ..ThreadedRuntimeConfig::default()
        },
    )
    .err()
    .expect("second core assignment must overflow");

    assert!(matches!(
        error,
        StartupError::ConfiguredCoreOverflow {
            base: usize::MAX,
            ordinal: 1
        }
    ));
}
