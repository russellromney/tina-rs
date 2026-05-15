use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallError, CallOutcome, DefaultThreadedMailboxFactory, SleepReply, ThreadedRuntime, sleep,
};

#[derive(Debug, Clone, Copy)]
pub struct RunConfig {
    pub callers: usize,
    pub lane_in_flight: usize,
    pub lane_mailbox: usize,
    pub work_ms: u64,
    pub call_timeout_ms: u64,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            callers: 12,
            lane_in_flight: 2,
            lane_mailbox: 32,
            work_ms: 120,
            call_timeout_ms: 2_000,
        }
    }
}

impl RunConfig {
    pub fn from_env() -> Self {
        let mut config = Self::default();
        config.callers = env_usize("OBJECT_LANE_CALLERS").unwrap_or(config.callers);
        config.lane_in_flight = env_usize("OBJECT_LANE_IN_FLIGHT").unwrap_or(config.lane_in_flight);
        config.lane_mailbox = env_usize("OBJECT_LANE_MAILBOX").unwrap_or(config.lane_mailbox);
        config.work_ms = env_u64("OBJECT_LANE_WORK_MS").unwrap_or(config.work_ms);
        config.call_timeout_ms =
            env_u64("OBJECT_LANE_CALL_TIMEOUT_MS").unwrap_or(config.call_timeout_ms);
        config
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunReport {
    pub callers: usize,
    pub stored: usize,
    pub busy: usize,
    pub failed: usize,
    pub stats: LaneStats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneStats {
    pub accepted: usize,
    pub busy: usize,
    pub completed: usize,
    pub in_flight: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneReply {
    Stored(String),
    Busy { in_flight: usize, cap: usize },
    Failed(String),
    Stats(LaneStats),
}

#[derive(Debug)]
enum LaneMsg {
    Put {
        key: String,
    },
    PutFinished {
        request: RequestContext<LaneReply>,
        key: String,
        result: WorkResult,
    },
    Stats,
}

type WorkResult = Result<(), String>;

enum WorkBackend {
    FakeSleep {
        work: Duration,
    },
}

struct ObjectLane {
    max_in_flight: usize,
    backend: WorkBackend,
    accepted: usize,
    busy: usize,
    completed: usize,
    in_flight: usize,
}

#[tina_runtime::isolate(message = LaneMsg, reply = LaneReply)]
impl ObjectLane {
    fn handle(
        &mut self,
        msg: LaneMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            LaneMsg::Put { .. } | LaneMsg::Stats => noop(),
            LaneMsg::PutFinished {
                request,
                key,
                result,
            } => {
                self.in_flight = self.in_flight.saturating_sub(1);
                match result {
                    Ok(()) => {
                        self.completed += 1;
                        reply_to_request(request, LaneReply::Stored(key))
                    }
                    Err(error) => reply_to_request(request, LaneReply::Failed(format!("{error:?}"))),
                }
            }
        }
    }

    fn handle_call(&mut self, msg: LaneMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            LaneMsg::Put { key } => {
                if self.in_flight >= self.max_in_flight {
                    self.busy += 1;
                    return call.reply(LaneReply::Busy {
                        in_flight: self.in_flight,
                        cap: self.max_in_flight,
                    });
                }

                self.in_flight += 1;
                self.accepted += 1;
                match &self.backend {
                    WorkBackend::FakeSleep { work } => call.defer(sleep(*work)).reply(
                        move |request, result| LaneMsg::PutFinished {
                            request,
                            key,
                            result: sleep_to_work_result(result),
                        },
                    ),
                }
            }
            LaneMsg::Stats => call.reply(LaneReply::Stats(LaneStats {
                accepted: self.accepted,
                busy: self.busy,
                completed: self.completed,
                in_flight: self.in_flight,
            })),
            LaneMsg::PutFinished { .. } => call.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }
}

pub fn run(config: RunConfig) -> anyhow::Result<RunReport> {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));
    let lane = runtime
        .register_with_capacity::<_, std::convert::Infallible>(
            ObjectLane {
                max_in_flight: config.lane_in_flight,
                backend: WorkBackend::FakeSleep {
                    work: Duration::from_millis(config.work_ms),
                },
                accepted: 0,
                busy: 0,
                completed: 0,
                in_flight: 0,
            },
            config.lane_mailbox,
        )
        .map_err(|e| anyhow::anyhow!("register lane: {e:?}"))?;

    let report = drive_callers(&runtime, lane, config)?;

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }

    Ok(report)
}

fn drive_callers(
    runtime: &Arc<ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>>,
    lane: Address<LaneMsg, LaneReply>,
    config: RunConfig,
) -> anyhow::Result<RunReport> {
    let barrier = Arc::new(Barrier::new(config.callers + 1));
    let outcomes = Arc::new(Mutex::new(Vec::with_capacity(config.callers)));
    let mut threads = Vec::with_capacity(config.callers);
    let call_timeout = Duration::from_millis(config.call_timeout_ms);

    for n in 0..config.callers {
        let rt = Arc::clone(&runtime);
        let gate = Arc::clone(&barrier);
        let out = Arc::clone(&outcomes);
        threads.push(thread::spawn(move || {
            gate.wait();
            let outcome = rt.call_blocking(
                lane,
                LaneMsg::Put {
                    key: format!("object-{n}"),
                },
                call_timeout,
            );
            out.lock().expect("outcomes lock").push(outcome);
        }));
    }

    barrier.wait();
    for thread in threads {
        thread.join().expect("caller thread panicked");
    }

    let mut stored = 0;
    let mut busy = 0;
    let mut failed = 0;
    for outcome in outcomes.lock().expect("outcomes lock").iter() {
        match outcome {
            Ok(CallOutcome::Replied(LaneReply::Stored(_))) => stored += 1,
            Ok(CallOutcome::Replied(LaneReply::Busy { .. })) => busy += 1,
            Ok(_) | Err(_) => failed += 1,
        }
    }

    let stats = match runtime.call_blocking(lane, LaneMsg::Stats, Duration::from_secs(1))? {
        CallOutcome::Replied(LaneReply::Stats(stats)) => stats,
        other => anyhow::bail!("stats call failed: {other:?}"),
    };

    Ok(RunReport {
        callers: config.callers,
        stored,
        busy,
        failed,
        stats,
    })
}

#[allow(dead_code)]
fn _call_error_is_part_of_the_public_story(_: CallError) {}

fn sleep_to_work_result(result: SleepReply) -> WorkResult {
    result.map_err(|error| format!("{error:?}"))
}

fn env_usize(name: &str) -> Option<usize> {
    std::env::var(name).ok()?.parse().ok()
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.parse().ok()
}
