use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallError, CallOutcome, DefaultThreadedMailboxFactory, LocalPermitGate, LocalPermitName,
    Permit, SleepReply, ThreadedRuntime, sleep,
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
        permit: Permit,
        result: WorkResult,
    },
    Stats,
}

type WorkResult = Result<(), String>;

enum WorkBackend {
    FakeSleep {
        work: Duration,
    },
    /// Real AWS S3 path via `tina-aws-bridge`. The lane uses a real
    /// `S3Address` for `PutObject` calls. Bridge-level pressure shows
    /// up as a typed `Failed`; lane-level admission still flows through
    /// the same `LocalPermitGate` so the user sees one consistent
    /// `Busy` shape.
    #[allow(dead_code)]
    AwsS3 {
        address: tina_aws_bridge::S3Address,
        bucket: String,
        key_prefix: String,
        timeout: Duration,
    },
}

struct ObjectLane {
    /// Fixed-capacity admission gate for the lane. "Local" here means: not a
    /// pool, no waiters queue, no auto-release; the permit travels with the
    /// continuation and the release point is structural.
    gate: LocalPermitGate,
    backend: WorkBackend,
    accepted: usize,
    busy: usize,
    completed: usize,
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
                permit,
                result,
            } => {
                let _ = self
                    .gate
                    .release(permit)
                    .expect("permit released exactly once by PutFinished");
                match result {
                    Ok(()) => {
                        self.completed += 1;
                        reply_to(request, LaneReply::Stored(key))
                    }
                    Err(error) => {
                        reply_to(request, LaneReply::Failed(format!("{error:?}")))
                    }
                }
            }
        }
    }

    fn handle_call(&mut self, msg: LaneMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            LaneMsg::Put { key } => match self.gate.try_admit() {
                Err(full) => {
                    self.busy += 1;
                    let report = full.report();
                    call.reply(LaneReply::Busy {
                        in_flight: report.current,
                        cap: report.capacity,
                    })
                }
                Ok(permit) => {
                    self.accepted += 1;
                    match &self.backend {
                        WorkBackend::FakeSleep { work } => {
                            call.defer(sleep(*work)).reply(move |request, result| {
                                LaneMsg::PutFinished {
                                    request,
                                    key,
                                    permit,
                                    result: sleep_to_work_result(result),
                                }
                            })
                        }
                        WorkBackend::AwsS3 {
                            address,
                            bucket,
                            key_prefix,
                            timeout,
                        } => {
                            let full_key = format!("{key_prefix}{key}");
                            let issued = tina_aws_bridge::send_s3(
                                *address,
                                tina_aws_bridge::S3Request::PutObject(
                                    tina_aws_bridge::S3PutObject {
                                        bucket: bucket.clone(),
                                        key: full_key,
                                        body: key.clone().into_bytes(),
                                        content_type: Some("application/octet-stream".into()),
                                    },
                                ),
                                *timeout,
                            );
                            call.defer(issued)
                                .reply(move |request, outcome| LaneMsg::PutFinished {
                                    request,
                                    key,
                                    permit,
                                    result: s3_outcome_to_work_result(outcome),
                                })
                        }
                    }
                }
            },
            LaneMsg::Stats => {
                let report = self.gate.report();
                call.reply(LaneReply::Stats(LaneStats {
                    accepted: self.accepted,
                    busy: self.busy,
                    completed: self.completed,
                    in_flight: report.current,
                }))
            }
            LaneMsg::PutFinished { .. } => {
                call.reject(tina::CallRejectedReason::UnsupportedMessage)
            }
        }
    }
}

/// Run the lane against a real `tina-aws-bridge` S3 worker.
///
/// The caller is responsible for installing the bridge and shutting it
/// down. The lane uses `tina_aws_bridge::send_s3(...).then(...)` for
/// each admitted call. Bridge-level pressure (`max_in_flight`,
/// `RequestTooLarge`, SDK errors) becomes a typed `Failed` reply with
/// the layer named in the message; lane-level admission still uses the
/// existing `LocalPermitGate`.
///
/// Hermetic tests should still use [`run`] (the FakeSleep backend); the
/// AWS-S3-shaped backend is exercised against a fake S3 HTTP server in
/// `tina-aws-bridge`'s integration tests.
pub fn run_against_s3(
    config: RunConfig,
    s3_address: tina_aws_bridge::S3Address,
    bucket: String,
    key_prefix: String,
    bridge_timeout: Duration,
) -> anyhow::Result<RunReport> {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));
    let lane = runtime
        .register_with_capacity::<_, std::convert::Infallible>(
            ObjectLane {
                gate: LocalPermitGate::with_capacity(config.lane_in_flight)
                    .named(LocalPermitName("object_lane")),
                backend: WorkBackend::AwsS3 {
                    address: s3_address,
                    bucket,
                    key_prefix,
                    timeout: bridge_timeout,
                },
                accepted: 0,
                busy: 0,
                completed: 0,
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

pub fn run(config: RunConfig) -> anyhow::Result<RunReport> {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));
    let lane = runtime
        .register_with_capacity::<_, std::convert::Infallible>(
            ObjectLane {
                gate: LocalPermitGate::with_capacity(config.lane_in_flight)
                    .named(LocalPermitName("object_lane")),
                backend: WorkBackend::FakeSleep {
                    work: Duration::from_millis(config.work_ms),
                },
                accepted: 0,
                busy: 0,
                completed: 0,
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
        let rt = Arc::clone(runtime);
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

/// Maps the AWS S3 bridge's two-layer outcome into the lane's flat
/// `Result<(), String>`. The lane treats bridge-level `Full/Closed/Timeout`
/// and worker-level `S3Error` as failures with the typed name visible
/// in the error string.
fn s3_outcome_to_work_result(
    outcome: tina_runtime::CallOutcome<
        Result<tina_aws_bridge::S3Response, tina_aws_bridge::S3Error>,
    >,
) -> WorkResult {
    use tina_aws_bridge::S3Error;
    use tina_runtime::CallOutcome;
    match outcome {
        CallOutcome::Replied(Ok(_)) => Ok(()),
        CallOutcome::Replied(Err(S3Error::Closed)) => Err("s3:closed".into()),
        CallOutcome::Replied(Err(S3Error::Full)) => Err("s3:full".into()),
        CallOutcome::Replied(Err(S3Error::Timeout)) => Err("s3:timeout".into()),
        CallOutcome::Replied(Err(other)) => Err(format!("s3:{other}")),
        CallOutcome::Full => Err("bridge:full".into()),
        CallOutcome::Closed => Err("bridge:closed".into()),
        CallOutcome::Timeout => Err("bridge:timeout".into()),
        CallOutcome::Rejected(r) => Err(format!("bridge:rejected:{r:?}")),
    }
}

fn env_usize(name: &str) -> Option<usize> {
    std::env::var(name).ok()?.parse().ok()
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.parse().ok()
}
