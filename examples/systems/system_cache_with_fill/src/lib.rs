use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, LocalSystem, SharedWork, SharedWorkError,
    SleepReply, SplitServiceHandle, request_effect_after_shared_wait, sleep,
};

#[derive(Debug, Clone, Copy)]
pub struct RunConfig {
    pub callers: usize,
    pub pending_capacity: usize,
    pub cache_mailbox: usize,
    pub fill_ms: u64,
    pub call_timeout_ms: u64,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            callers: 8,
            pending_capacity: 5,
            cache_mailbox: 64,
            fill_ms: 120,
            call_timeout_ms: 2_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunReport {
    pub single_flight: SingleFlightReport,
    pub stale_invalidation: StaleInvalidationReport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SingleFlightReport {
    pub callers: usize,
    pub filled: usize,
    pub busy: usize,
    pub failed: usize,
    pub hit_after_fill: bool,
    pub stats: CacheStats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleInvalidationReport {
    pub first_get_stale: bool,
    pub replacement_filled: bool,
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheReply {
    Value {
        key: String,
        value: String,
        source: ValueSource,
    },
    Busy,
    Stale,
    Invalidated,
    Failed(String),
    Stats(CacheStats),
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
    waiters: SharedWork<String, CacheReply>,
    entries: HashMap<String, CacheEntry>,
    fills_started: usize,
    fills_completed: usize,
    stale_completions: usize,
    invalidations: usize,
    hits: usize,
    busy_replies: usize,
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
    fn new(pending_capacity: usize, fill_delay: Duration) -> Self {
        Self {
            fill_delay,
            waiters: SharedWork::with_capacity(pending_capacity)
                .named("system_cache_with_fill.waiters"),
            entries: HashMap::new(),
            fills_started: 0,
            fills_completed: 0,
            stale_completions: 0,
            invalidations: 0,
            hits: 0,
            busy_replies: 0,
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
                call.reply(CacheReply::Busy)
            }
            Err(SharedWorkError::KeyFull { call, .. }) => {
                self.busy_replies += 1;
                call.reply(CacheReply::Busy)
            }
        }
    }

    fn invalidate(&mut self, key: String, call: RequestCall<'_, Self>) -> RequestEffect<Self> {
        self.invalidations += 1;
        let entry = self.entries.entry(key.clone()).or_default();
        entry.generation += 1;
        entry.value = None;

        let Some(_fill) = entry.filling.take() else {
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
        let Some(entry) = self.entries.get_mut(&key) else {
            self.stale_completions += 1;
            return noop();
        };

        let Some(fill) = entry.filling.take() else {
            self.stale_completions += 1;
            return noop();
        };

        if fill.generation != generation || entry.generation != generation {
            self.stale_completions += 1;
            return noop();
        }

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
                let message = format!("{error:?}");
                Effect::Batch(
                    self.waiters
                        .reply_all_with::<Self, _>(&key, || CacheReply::Failed(message.clone())),
                )
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
        }
    }
}

pub fn run(config: RunConfig) -> anyhow::Result<RunReport> {
    Ok(RunReport {
        single_flight: run_single_flight(config)?,
        stale_invalidation: run_stale_invalidation(config)?,
    })
}

pub fn run_single_flight(config: RunConfig) -> anyhow::Result<SingleFlightReport> {
    let runtime = Arc::new(
        LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?,
    );
    let shutdown = runtime.shutdown_handle();
    let cache = register_cache(&runtime, config)?;

    let barrier = Arc::new(Barrier::new(config.callers + 1));
    let outcomes = Arc::new(Mutex::new(Vec::with_capacity(config.callers)));
    let timeout = Duration::from_millis(config.call_timeout_ms);
    let mut threads = Vec::with_capacity(config.callers);

    for _ in 0..config.callers {
        let rt = Arc::clone(&runtime);
        let gate = Arc::clone(&barrier);
        let out = Arc::clone(&outcomes);
        threads.push(thread::spawn(move || {
            gate.wait();
            let outcome = rt.call_blocking_request(
                cache.requests,
                CacheRequest::Get {
                    key: "shared".into(),
                },
                timeout,
            );
            out.lock().expect("outcomes lock").push(outcome);
        }));
    }

    barrier.wait();
    for thread in threads {
        thread.join().expect("caller thread panicked");
    }

    let mut filled = 0;
    let mut busy = 0;
    let mut failed = 0;
    for outcome in outcomes.lock().expect("outcomes lock").iter() {
        match outcome {
            Ok(CallOutcome::Replied(CacheReply::Value {
                source: ValueSource::Fill,
                ..
            })) => filled += 1,
            Ok(CallOutcome::Replied(CacheReply::Busy)) => busy += 1,
            _ => failed += 1,
        }
    }

    let hit_after_fill = matches!(
        runtime.call_blocking_request(
            cache.requests,
            CacheRequest::Get {
                key: "shared".into()
            },
            timeout
        )?,
        CallOutcome::Replied(CacheReply::Value {
            source: ValueSource::Hit,
            ..
        })
    );
    let stats = stats(&runtime, cache.requests)?;
    shutdown_runtime(shutdown, runtime)?;

    Ok(SingleFlightReport {
        callers: config.callers,
        filled,
        busy,
        failed,
        hit_after_fill,
        stats,
    })
}

pub fn run_stale_invalidation(config: RunConfig) -> anyhow::Result<StaleInvalidationReport> {
    let runtime = Arc::new(
        LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?,
    );
    let shutdown = runtime.shutdown_handle();
    let cache = register_cache(&runtime, config)?;
    let timeout = Duration::from_millis(config.call_timeout_ms);

    let rt = Arc::clone(&runtime);
    let cache_call = cache.requests;
    let first = thread::spawn(move || {
        rt.call_blocking_request(
            cache_call,
            CacheRequest::Get {
                key: "invalidate-me".into(),
            },
            timeout,
        )
    });

    thread::sleep(Duration::from_millis((config.fill_ms / 4).max(1)));
    let _ = runtime.call_blocking_request(
        cache.requests,
        CacheRequest::Invalidate {
            key: "invalidate-me".into(),
        },
        timeout,
    )?;

    let first_get_stale = matches!(
        first.join().expect("get thread panicked")?,
        CallOutcome::Replied(CacheReply::Stale)
    );

    thread::sleep(Duration::from_millis(config.fill_ms + 20));
    let replacement_filled = matches!(
        runtime.call_blocking_request(
            cache.requests,
            CacheRequest::Get {
                key: "invalidate-me".into()
            },
            timeout
        )?,
        CallOutcome::Replied(CacheReply::Value {
            source: ValueSource::Fill,
            ..
        })
    );
    let stats = stats(&runtime, cache.requests)?;
    shutdown_runtime(shutdown, runtime)?;

    Ok(StaleInvalidationReport {
        first_get_stale,
        replacement_filled,
        stats,
    })
}

fn register_cache(
    runtime: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
    config: RunConfig,
) -> anyhow::Result<SplitServiceHandle<CacheEvent, CacheRequest, CacheReply>> {
    runtime
        .register_split_service::<Cache, CacheEvent, CacheRequest, Infallible>(
            Cache::new(
                config.pending_capacity,
                Duration::from_millis(config.fill_ms),
            ),
            config.cache_mailbox,
        )
        .map_err(|e| anyhow::anyhow!("register cache: {e:?}"))
}

fn stats(
    runtime: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
    cache: tina::ServiceRequestAddress<CacheEvent, CacheRequest, CacheReply>,
) -> anyhow::Result<CacheStats> {
    match runtime.call_blocking_request(cache, CacheRequest::Stats, Duration::from_secs(1))? {
        CallOutcome::Replied(CacheReply::Stats(stats)) => Ok(stats),
        other => anyhow::bail!("stats failed: {other:?}"),
    }
}

fn shutdown_runtime(
    shutdown: tina_runtime::ThreadedShutdownHandle,
    runtime: Arc<LocalSystem<SingleShard, DefaultThreadedMailboxFactory>>,
) -> anyhow::Result<()> {
    let terminal = shutdown.request_and_wait_report(Duration::from_secs(5))?;
    drop(runtime);
    terminal.ensure_clean()?;
    Ok(())
}
