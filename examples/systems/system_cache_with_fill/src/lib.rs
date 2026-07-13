use std::collections::HashMap;
use std::convert::Infallible;
use std::error::Error;
use std::fmt;
use std::sync::Barrier;
use std::thread;
use std::time::{Duration, Instant};

use tina::prelude::*;
use tina_runtime::{
    CallError, CallOutcome, DefaultThreadedMailboxFactory, LocalSystem, SharedWork,
    SharedWorkError, SleepReply, SplitServiceHandle, ThreadedRuntimeError,
    request_effect_after_shared_wait, sleep,
};

const MAX_CALLERS: usize = 4_096;
const MAX_PENDING_CAPACITY: usize = 65_536;
const MAX_ENTRY_CAPACITY: usize = 65_536;
const MAX_CACHE_MAILBOX: usize = 65_536;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy)]
pub struct RunConfig {
    pub callers: usize,
    pub pending_capacity: usize,
    pub entry_capacity: usize,
    pub cache_mailbox: usize,
    pub fill_ms: u64,
    pub call_timeout_ms: u64,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            callers: 8,
            pending_capacity: 5,
            entry_capacity: 64,
            cache_mailbox: 64,
            fill_ms: 120,
            call_timeout_ms: 2_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigError {
    ZeroCallers,
    TooManyCallers { requested: usize, max: usize },
    ZeroPendingCapacity,
    PendingCapacityTooLarge { requested: usize, max: usize },
    ZeroEntryCapacity,
    EntryCapacityTooLarge { requested: usize, max: usize },
    ZeroCacheMailbox,
    CacheMailboxTooLarge { requested: usize, max: usize },
    ZeroFillDelay,
    ZeroCallTimeout,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroCallers => write!(f, "callers must be positive"),
            Self::TooManyCallers { requested, max } => {
                write!(f, "callers {requested} exceeds maximum {max}")
            }
            Self::ZeroPendingCapacity => write!(f, "pending capacity must be positive"),
            Self::PendingCapacityTooLarge { requested, max } => {
                write!(f, "pending capacity {requested} exceeds maximum {max}")
            }
            Self::ZeroEntryCapacity => write!(f, "entry capacity must be positive"),
            Self::EntryCapacityTooLarge { requested, max } => {
                write!(f, "entry capacity {requested} exceeds maximum {max}")
            }
            Self::ZeroCacheMailbox => write!(f, "cache mailbox must be positive"),
            Self::CacheMailboxTooLarge { requested, max } => {
                write!(f, "cache mailbox {requested} exceeds maximum {max}")
            }
            Self::ZeroFillDelay => write!(f, "fill delay must be positive"),
            Self::ZeroCallTimeout => write!(f, "call timeout must be positive"),
        }
    }
}

impl Error for ConfigError {}

#[derive(Debug)]
pub enum ScenarioError {
    Runtime {
        operation: &'static str,
        source: ThreadedRuntimeError,
    },
    Terminal {
        operation: &'static str,
        outcome: TerminalOutcome,
    },
    Reply {
        operation: &'static str,
        reply: Box<CacheReply>,
    },
    CallerPanicked,
    PendingNotObserved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalOutcome {
    Full,
    Closed,
    Timeout,
    Rejected(tina::CallRejectedReason),
}

impl fmt::Display for ScenarioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Runtime { operation, source } => write!(f, "{operation}: {source}"),
            Self::Terminal { operation, outcome } => {
                write!(f, "{operation}: terminal outcome {outcome:?}")
            }
            Self::Reply { operation, reply } => {
                write!(f, "{operation}: unexpected reply {reply:?}")
            }
            Self::CallerPanicked => write!(f, "cache caller thread panicked"),
            Self::PendingNotObserved => {
                write!(
                    f,
                    "cache fill did not publish its pending waiter before deadline"
                )
            }
        }
    }
}

impl Error for ScenarioError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Runtime { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunReport {
    pub single_flight: SingleFlightReport,
    pub stale_invalidation: StaleInvalidationReport,
    pub caller_gone: CallerGoneReport,
    pub entry_capacity: EntryCapacityReport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SingleFlightReport {
    pub callers: usize,
    pub filled: usize,
    pub busy: usize,
    pub stats: CacheStats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleInvalidationReport {
    pub stats: CacheStats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallerGoneReport {
    pub stats: CacheStats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryCapacityReport {
    pub stats: CacheStats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheStats {
    pub entries: usize,
    pub fills_started: usize,
    pub fills_completed: usize,
    pub stale_completions: usize,
    pub invalidations: usize,
    pub hits: usize,
    pub busy_replies: usize,
    pub stale_replies: usize,
    pub pending_high_water: usize,
    pub pending_full_rejects: u64,
    pub entry_full_rejects: u64,
    pub pending_current: usize,
    pub callers_gone: u64,
    pub active_fills: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheFailure {
    Fill(CallError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheReply {
    Value {
        key: String,
        value: String,
        source: ValueSource,
    },
    Rejected(CacheRejection),
    Stale,
    Invalidated,
    Failed(CacheFailure),
    Stats(CacheStats),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheRejection {
    PendingFull,
    EntryFull,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueSource {
    Hit,
    Fill,
}

#[derive(Debug)]
enum CacheEvent {
    FillDone {
        key: String,
        generation: u64,
        result: SleepReply,
    },
}

#[derive(Debug)]
enum CacheRequest {
    Get { key: String },
    Invalidate { key: String },
    Stats,
}

#[derive(Debug, Default)]
struct CacheEntry {
    generation: u64,
    value: Option<String>,
    filling: Option<FillState>,
}

#[derive(Debug)]
struct FillState {
    generation: u64,
}

struct Cache {
    fill_delay: Duration,
    entry_capacity: usize,
    waiters: SharedWork<String, CacheReply>,
    entries: HashMap<String, CacheEntry>,
    fills_started: usize,
    fills_completed: usize,
    stale_completions: usize,
    invalidations: usize,
    hits: usize,
    busy_replies: usize,
    entry_full_replies: u64,
    stale_replies: usize,
}

#[tina_runtime::isolate(event = CacheEvent, request = CacheRequest, reply = CacheReply)]
impl Cache {
    fn handle_event(
        &mut self,
        event: CacheEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            CacheEvent::FillDone {
                key,
                generation,
                result,
            } => self.fill_done(key, generation, result),
        }
    }

    fn handle_request(
        &mut self,
        request: CacheRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            CacheRequest::Get { key } => self.get(key, call),
            CacheRequest::Invalidate { key } => self.invalidate(key, call),
            CacheRequest::Stats => call.reply(CacheReply::Stats(self.stats())),
        }
    }
}

impl Cache {
    fn new(pending_capacity: usize, entry_capacity: usize, fill_delay: Duration) -> Self {
        Self {
            fill_delay,
            entry_capacity,
            waiters: SharedWork::with_capacity(pending_capacity)
                .named("system_cache_with_fill.waiters"),
            entries: HashMap::new(),
            fills_started: 0,
            fills_completed: 0,
            stale_completions: 0,
            invalidations: 0,
            hits: 0,
            busy_replies: 0,
            entry_full_replies: 0,
            stale_replies: 0,
        }
    }

    fn get(&mut self, key: String, call: RequestCall<'_, Self>) -> RequestEffect<Self> {
        if let Some(value) = self.entries.get(&key).and_then(|entry| entry.value.clone()) {
            self.hits += 1;
            return call.reply(CacheReply::Value {
                key,
                value,
                source: ValueSource::Hit,
            });
        }

        if !self.entries.contains_key(&key) && self.entries.len() >= self.entry_capacity {
            self.entry_full_replies += 1;
            return call.reply(CacheReply::Rejected(CacheRejection::EntryFull));
        }

        match self.waiters.wait(key.clone(), call) {
            Ok((_ticket, permit)) => {
                let entry = self.entries.entry(key.clone()).or_default();
                if entry.filling.is_some() {
                    return request_effect_after_shared_wait(permit, noop());
                }
                let generation = entry.generation;
                entry.filling = Some(FillState { generation });
                self.fills_started += 1;
                let fill_effect =
                    sleep(self.fill_delay).then_service_event(move |result| CacheEvent::FillDone {
                        key,
                        generation,
                        result,
                    });
                request_effect_after_shared_wait(permit, fill_effect)
            }
            Err(SharedWorkError::Full { call, .. }) => {
                self.busy_replies += 1;
                call.reply(CacheReply::Rejected(CacheRejection::PendingFull))
            }
            Err(SharedWorkError::KeyFull { call, .. }) => {
                self.busy_replies += 1;
                call.reply(CacheReply::Rejected(CacheRejection::PendingFull))
            }
        }
    }

    fn invalidate(&mut self, key: String, call: RequestCall<'_, Self>) -> RequestEffect<Self> {
        self.invalidations += 1;
        let entry = self.entries.entry(key.clone()).or_default();
        entry.generation += 1;
        entry.value = None;

        let Some(_fill) = entry.filling.take() else {
            if entry.value.is_none() {
                self.entries.remove(&key);
            }
            return call.reply(CacheReply::Invalidated);
        };

        call.capture(|request| {
            let mut effects = self
                .waiters
                .close_all_clone::<Self>(&key, CacheReply::Stale);
            self.stale_replies += effects.len();
            effects.push(reply_to::<Self>(request, CacheReply::Invalidated));
            Effect::Batch(effects)
        })
    }

    fn fill_done(&mut self, key: String, generation: u64, result: SleepReply) -> Effect<Self> {
        self.waiters.sweep();
        let Some(entry) = self.entries.get_mut(&key) else {
            self.stale_completions += 1;
            return noop();
        };

        let Some(fill) = entry.filling.as_ref() else {
            self.stale_completions += 1;
            if entry.value.is_none() {
                self.entries.remove(&key);
            }
            return noop();
        };

        if fill.generation != generation || entry.generation != generation {
            self.stale_completions += 1;
            return noop();
        }
        entry.filling = None;

        match result {
            Ok(()) => {
                let value = format!("value:{key}:g{generation}");
                entry.value = Some(value.clone());
                self.fills_completed += 1;
                Effect::Batch(
                    self.waiters
                        .reply_all_with::<Self, _>(&key, || CacheReply::Value {
                            key: key.clone(),
                            value: value.clone(),
                            source: ValueSource::Fill,
                        }),
                )
            }
            Err(error) => {
                let effects = self.waiters.reply_all_with::<Self, _>(&key, || {
                    CacheReply::Failed(CacheFailure::Fill(error))
                });
                self.entries.remove(&key);
                Effect::Batch(effects)
            }
        }
    }

    fn stats(&self) -> CacheStats {
        CacheStats {
            entries: self
                .entries
                .values()
                .filter(|entry| entry.value.is_some())
                .count(),
            fills_started: self.fills_started,
            fills_completed: self.fills_completed,
            stale_completions: self.stale_completions,
            invalidations: self.invalidations,
            hits: self.hits,
            busy_replies: self.busy_replies,
            stale_replies: self.stale_replies,
            pending_high_water: self.waiters.high_water(),
            pending_full_rejects: self.waiters.full_rejects(),
            entry_full_rejects: self.entry_full_replies,
            pending_current: self.waiters.len(),
            callers_gone: self.waiters.reclaimed(),
            active_fills: self
                .entries
                .values()
                .filter(|entry| entry.filling.is_some())
                .count(),
        }
    }
}

pub fn run(config: RunConfig) -> anyhow::Result<RunReport> {
    validate_config(config)?;
    Ok(RunReport {
        single_flight: run_single_flight(config)?,
        stale_invalidation: run_stale_invalidation(config)?,
        caller_gone: run_caller_gone(config)?,
        entry_capacity: run_entry_capacity(config)?,
    })
}

pub fn run_single_flight(config: RunConfig) -> anyhow::Result<SingleFlightReport> {
    validate_config(config)?;
    let runtime =
        LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;
    runtime
        .run_to_shutdown_reported(SHUTDOWN_TIMEOUT, |runtime| -> anyhow::Result<_> {
            let cache = register_cache(runtime, config)?;
            let participants =
                config
                    .callers
                    .checked_add(1)
                    .ok_or(ConfigError::TooManyCallers {
                        requested: config.callers,
                        max: MAX_CALLERS,
                    })?;
            let barrier = Barrier::new(participants);
            let timeout = Duration::from_millis(config.call_timeout_ms);
            let outcomes = thread::scope(|scope| {
                let mut threads = Vec::with_capacity(config.callers);
                for _ in 0..config.callers {
                    let gate = &barrier;
                    threads.push(scope.spawn(move || {
                        gate.wait();
                        runtime.call_blocking_request(
                            cache.requests,
                            CacheRequest::Get {
                                key: "shared".into(),
                            },
                            timeout,
                        )
                    }));
                }
                barrier.wait();
                threads
                    .into_iter()
                    .map(|thread| thread.join().map_err(|_| ScenarioError::CallerPanicked))
                    .collect::<Result<Vec<_>, _>>()
            })?;

            let mut filled = 0;
            let mut busy = 0;
            for outcome in outcomes {
                match classify_call("single-flight get", outcome)? {
                    CacheReply::Value {
                        source: ValueSource::Fill,
                        ..
                    } => filled += 1,
                    CacheReply::Rejected(CacheRejection::PendingFull) => busy += 1,
                    reply => {
                        return Err(ScenarioError::Reply {
                            operation: "single-flight get",
                            reply: Box::new(reply),
                        }
                        .into());
                    }
                }
            }

            match classify_call(
                "hit after fill",
                runtime.call_blocking_request(
                    cache.requests,
                    CacheRequest::Get {
                        key: "shared".into(),
                    },
                    timeout,
                ),
            )? {
                CacheReply::Value {
                    source: ValueSource::Hit,
                    ..
                } => {}
                reply => {
                    return Err(ScenarioError::Reply {
                        operation: "hit after fill",
                        reply: Box::new(reply),
                    }
                    .into());
                }
            }
            let stats = stats(runtime, cache.requests, timeout)?;

            Ok(SingleFlightReport {
                callers: config.callers,
                filled,
                busy,
                stats,
            })
        })
        .map_err(Into::into)
}

pub fn run_stale_invalidation(config: RunConfig) -> anyhow::Result<StaleInvalidationReport> {
    validate_config(config)?;
    let runtime =
        LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;
    runtime
        .run_to_shutdown_reported(SHUTDOWN_TIMEOUT, |runtime| -> anyhow::Result<_> {
            let cache = register_cache(runtime, config)?;
            let timeout = Duration::from_millis(config.call_timeout_ms);
            thread::scope(|scope| -> anyhow::Result<StaleInvalidationReport> {
                let first = scope.spawn(|| {
                    runtime.call_blocking_request(
                        cache.requests,
                        CacheRequest::Get {
                            key: "invalidate-me".into(),
                        },
                        timeout,
                    )
                });

                wait_for_stats(runtime, cache.requests, timeout, |stats| {
                    stats.pending_current == 1 && stats.active_fills == 1
                })?;
                expect_reply(
                    "invalidate active fill",
                    runtime.call_blocking_request(
                        cache.requests,
                        CacheRequest::Invalidate {
                            key: "invalidate-me".into(),
                        },
                        timeout,
                    ),
                    CacheReply::Invalidated,
                )?;
                expect_reply(
                    "invalidated caller",
                    first.join().map_err(|_| ScenarioError::CallerPanicked)?,
                    CacheReply::Stale,
                )?;

                match classify_call(
                    "replacement fill",
                    runtime.call_blocking_request(
                        cache.requests,
                        CacheRequest::Get {
                            key: "invalidate-me".into(),
                        },
                        timeout,
                    ),
                )? {
                    CacheReply::Value {
                        source: ValueSource::Fill,
                        ..
                    } => {}
                    reply => {
                        return Err(ScenarioError::Reply {
                            operation: "replacement fill",
                            reply: Box::new(reply),
                        }
                        .into());
                    }
                }
                let stats = wait_for_stats(runtime, cache.requests, timeout, |stats| {
                    stats.stale_completions == 1 && stats.active_fills == 0
                })?;
                Ok(StaleInvalidationReport { stats })
            })
        })
        .map_err(Into::into)
}

pub fn run_caller_gone(config: RunConfig) -> anyhow::Result<CallerGoneReport> {
    validate_config(config)?;
    let runtime =
        LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;
    runtime
        .run_to_shutdown_reported(SHUTDOWN_TIMEOUT, |runtime| -> anyhow::Result<_> {
            let cache = register_cache(runtime, config)?;
            let fill_delay = Duration::from_millis(config.fill_ms);
            let caller_timeout = Duration::from_millis((config.fill_ms / 4).max(1));
            match runtime.call_blocking_request(
                cache.requests,
                CacheRequest::Get {
                    key: "caller-gone".into(),
                },
                caller_timeout,
            )? {
                CallOutcome::Timeout => {}
                CallOutcome::Replied(reply) => {
                    return Err(ScenarioError::Reply {
                        operation: "caller-gone get",
                        reply: Box::new(reply),
                    }
                    .into());
                }
                CallOutcome::Full => {
                    return Err(terminal("caller-gone get", TerminalOutcome::Full).into());
                }
                CallOutcome::Closed => {
                    return Err(terminal("caller-gone get", TerminalOutcome::Closed).into());
                }
                CallOutcome::Rejected(reason) => {
                    return Err(
                        terminal("caller-gone get", TerminalOutcome::Rejected(reason)).into(),
                    );
                }
            }

            let deadline =
                Instant::now() + fill_delay + Duration::from_millis(config.call_timeout_ms);
            let stats = wait_for_stats_until(runtime, cache.requests, deadline, |stats| {
                stats.fills_completed == 1
                    && stats.callers_gone == 1
                    && stats.pending_current == 0
                    && stats.active_fills == 0
            })?;
            match classify_call(
                "hit after caller gone",
                runtime.call_blocking_request(
                    cache.requests,
                    CacheRequest::Get {
                        key: "caller-gone".into(),
                    },
                    Duration::from_millis(config.call_timeout_ms),
                ),
            )? {
                reply @ CacheReply::Value {
                    source: ValueSource::Fill,
                    ..
                } => {
                    return Err(ScenarioError::Reply {
                        operation: "hit after caller gone",
                        reply: Box::new(reply),
                    }
                    .into());
                }
                CacheReply::Value {
                    source: ValueSource::Hit,
                    ..
                } => {}
                reply => {
                    return Err(ScenarioError::Reply {
                        operation: "hit after caller gone",
                        reply: Box::new(reply),
                    }
                    .into());
                }
            }
            Ok(CallerGoneReport { stats })
        })
        .map_err(Into::into)
}

pub fn run_entry_capacity(config: RunConfig) -> anyhow::Result<EntryCapacityReport> {
    validate_config(config)?;
    let config = RunConfig {
        entry_capacity: 1,
        ..config
    };
    let runtime =
        LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;
    runtime
        .run_to_shutdown_reported(SHUTDOWN_TIMEOUT, |runtime| -> anyhow::Result<_> {
            let cache = register_cache(runtime, config)?;
            let timeout = Duration::from_millis(config.call_timeout_ms);
            thread::scope(|scope| -> anyhow::Result<EntryCapacityReport> {
                let first = scope.spawn(|| {
                    runtime.call_blocking_request(
                        cache.requests,
                        CacheRequest::Get {
                            key: "first-key".into(),
                        },
                        timeout,
                    )
                });
                wait_for_stats(runtime, cache.requests, timeout, |stats| {
                    stats.pending_current == 1 && stats.active_fills == 1
                })?;
                expect_reply(
                    "entry capacity rejection",
                    runtime.call_blocking_request(
                        cache.requests,
                        CacheRequest::Get {
                            key: "second-key".into(),
                        },
                        timeout,
                    ),
                    CacheReply::Rejected(CacheRejection::EntryFull),
                )?;
                match classify_call(
                    "entry capacity owner",
                    first.join().map_err(|_| ScenarioError::CallerPanicked)?,
                )? {
                    CacheReply::Value {
                        source: ValueSource::Fill,
                        ..
                    } => {}
                    reply => {
                        return Err(ScenarioError::Reply {
                            operation: "entry capacity owner",
                            reply: Box::new(reply),
                        }
                        .into());
                    }
                }
                let stats = stats(runtime, cache.requests, timeout)?;
                Ok(EntryCapacityReport { stats })
            })
        })
        .map_err(Into::into)
}

fn register_cache(
    runtime: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
    config: RunConfig,
) -> anyhow::Result<SplitServiceHandle<CacheEvent, CacheRequest, CacheReply>> {
    runtime
        .register_split_service::<Cache, CacheEvent, CacheRequest, Infallible>(
            Cache::new(
                config.pending_capacity,
                config.entry_capacity,
                Duration::from_millis(config.fill_ms),
            ),
            config.cache_mailbox,
        )
        .map_err(|e| anyhow::anyhow!("register cache: {e:?}"))
}

fn validate_config(config: RunConfig) -> Result<(), ConfigError> {
    if config.callers == 0 {
        return Err(ConfigError::ZeroCallers);
    }
    if config.callers > MAX_CALLERS {
        return Err(ConfigError::TooManyCallers {
            requested: config.callers,
            max: MAX_CALLERS,
        });
    }
    if config.pending_capacity == 0 {
        return Err(ConfigError::ZeroPendingCapacity);
    }
    if config.pending_capacity > MAX_PENDING_CAPACITY {
        return Err(ConfigError::PendingCapacityTooLarge {
            requested: config.pending_capacity,
            max: MAX_PENDING_CAPACITY,
        });
    }
    if config.entry_capacity == 0 {
        return Err(ConfigError::ZeroEntryCapacity);
    }
    if config.entry_capacity > MAX_ENTRY_CAPACITY {
        return Err(ConfigError::EntryCapacityTooLarge {
            requested: config.entry_capacity,
            max: MAX_ENTRY_CAPACITY,
        });
    }
    if config.cache_mailbox == 0 {
        return Err(ConfigError::ZeroCacheMailbox);
    }
    if config.cache_mailbox > MAX_CACHE_MAILBOX {
        return Err(ConfigError::CacheMailboxTooLarge {
            requested: config.cache_mailbox,
            max: MAX_CACHE_MAILBOX,
        });
    }
    if config.fill_ms == 0 {
        return Err(ConfigError::ZeroFillDelay);
    }
    if config.call_timeout_ms == 0 {
        return Err(ConfigError::ZeroCallTimeout);
    }
    Ok(())
}

fn stats(
    runtime: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
    cache: tina::ServiceRequestAddress<CacheEvent, CacheRequest, CacheReply>,
    timeout: Duration,
) -> anyhow::Result<CacheStats> {
    match classify_call(
        "cache stats",
        runtime.call_blocking_request(cache, CacheRequest::Stats, timeout),
    )? {
        CacheReply::Stats(stats) => Ok(stats),
        reply => Err(ScenarioError::Reply {
            operation: "cache stats",
            reply: Box::new(reply),
        }
        .into()),
    }
}

fn wait_for_stats(
    runtime: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
    cache: tina::ServiceRequestAddress<CacheEvent, CacheRequest, CacheReply>,
    timeout: Duration,
    ready: impl FnMut(&CacheStats) -> bool,
) -> anyhow::Result<CacheStats> {
    wait_for_stats_until(runtime, cache, Instant::now() + timeout, ready)
}

fn wait_for_stats_until(
    runtime: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
    cache: tina::ServiceRequestAddress<CacheEvent, CacheRequest, CacheReply>,
    deadline: Instant,
    mut ready: impl FnMut(&CacheStats) -> bool,
) -> anyhow::Result<CacheStats> {
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Err(ScenarioError::PendingNotObserved.into());
        }
        let snapshot = stats(runtime, cache, deadline.saturating_duration_since(now))?;
        if ready(&snapshot) {
            return Ok(snapshot);
        }
        thread::yield_now();
    }
}

fn classify_call(
    operation: &'static str,
    result: Result<CallOutcome<CacheReply>, ThreadedRuntimeError>,
) -> Result<CacheReply, ScenarioError> {
    match result {
        Ok(CallOutcome::Replied(reply)) => Ok(reply),
        Ok(CallOutcome::Full) => Err(terminal(operation, TerminalOutcome::Full)),
        Ok(CallOutcome::Closed) => Err(terminal(operation, TerminalOutcome::Closed)),
        Ok(CallOutcome::Timeout) => Err(terminal(operation, TerminalOutcome::Timeout)),
        Ok(CallOutcome::Rejected(reason)) => {
            Err(terminal(operation, TerminalOutcome::Rejected(reason)))
        }
        Err(source) => Err(ScenarioError::Runtime { operation, source }),
    }
}

fn expect_reply(
    operation: &'static str,
    result: Result<CallOutcome<CacheReply>, ThreadedRuntimeError>,
    expected: CacheReply,
) -> Result<(), ScenarioError> {
    let reply = classify_call(operation, result)?;
    if reply == expected {
        Ok(())
    } else {
        Err(ScenarioError::Reply {
            operation,
            reply: Box::new(reply),
        })
    }
}

fn terminal(operation: &'static str, outcome: TerminalOutcome) -> ScenarioError {
    ScenarioError::Terminal { operation, outcome }
}
