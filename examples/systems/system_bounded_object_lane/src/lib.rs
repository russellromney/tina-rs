#[cfg(feature = "real-s3")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "real-s3")]
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
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
    #[cfg(feature = "real-s3")]
    RealS3 {
        jobs: SyncSender<S3BridgeJob>,
        body_bytes: usize,
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
        ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            LaneMsg::Put { key } => {
                if self.in_flight >= self.max_in_flight {
                    self.busy += 1;
                    return reply(LaneReply::Busy {
                        in_flight: self.in_flight,
                        cap: self.max_in_flight,
                    });
                }

                let Ok(request) = ctx.take_request_context() else {
                    return reply(LaneReply::Failed("put arrived without caller".into()));
                };

                self.in_flight += 1;
                self.accepted += 1;
                match &self.backend {
                    WorkBackend::FakeSleep { work } => {
                        sleep(*work).reply_with_request(request, move |request, result| {
                            LaneMsg::PutFinished {
                                request,
                                key,
                                result: sleep_to_work_result(result),
                            }
                        })
                    }
                    #[cfg(feature = "real-s3")]
                    WorkBackend::RealS3 { jobs, body_bytes } => {
                        let job = S3Job {
                            request,
                            key: key.clone(),
                            body: vec![b'x'; *body_bytes],
                        };
                        match jobs.try_send(S3BridgeJob { job }) {
                            Ok(()) => noop(),
                            Err(TrySendError::Full(S3BridgeJob { job })) => {
                                self.in_flight = self.in_flight.saturating_sub(1);
                                self.busy += 1;
                                reply_to_request(
                                    job.request,
                                    LaneReply::Busy {
                                        in_flight: self.in_flight,
                                        cap: self.max_in_flight,
                                    },
                                )
                            }
                            Err(TrySendError::Disconnected(S3BridgeJob { job })) => {
                                self.in_flight = self.in_flight.saturating_sub(1);
                                reply_to_request(
                                    job.request,
                                    LaneReply::Failed("s3 bridge is closed".into()),
                                )
                            }
                        }
                    }
                }
            }
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
                    Err(error) => {
                        reply_to_request(request, LaneReply::Failed(format!("{error:?}")))
                    }
                }
            }
            LaneMsg::Stats => reply(LaneReply::Stats(LaneStats {
                accepted: self.accepted,
                busy: self.busy,
                completed: self.completed,
                in_flight: self.in_flight,
            })),
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

#[cfg(feature = "real-s3")]
#[derive(Debug, Clone)]
pub struct RealS3Config {
    pub bucket: String,
    pub prefix: String,
    pub region: Option<String>,
    pub endpoint_url: Option<String>,
    pub force_path_style: bool,
    pub body_bytes: usize,
    pub operation_timeout_ms: u64,
}

#[cfg(feature = "real-s3")]
impl RealS3Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let bucket = std::env::var("OBJECT_LANE_S3_BUCKET")
            .map_err(|_| anyhow::anyhow!("OBJECT_LANE_S3_BUCKET is required for real S3 mode"))?;
        Ok(Self {
            bucket,
            prefix: std::env::var("OBJECT_LANE_S3_PREFIX")
                .unwrap_or_else(|_| "tina-object-lane/".to_string()),
            region: std::env::var("OBJECT_LANE_S3_REGION").ok(),
            endpoint_url: std::env::var("OBJECT_LANE_S3_ENDPOINT_URL").ok(),
            force_path_style: env_bool("OBJECT_LANE_S3_FORCE_PATH_STYLE").unwrap_or(false),
            body_bytes: env_usize("OBJECT_LANE_S3_BODY_BYTES").unwrap_or(16),
            operation_timeout_ms: env_u64("OBJECT_LANE_S3_OPERATION_TIMEOUT_MS").unwrap_or(10_000),
        })
    }
}

#[cfg(feature = "real-s3")]
pub fn run_real_s3(config: RunConfig, s3: RealS3Config) -> anyhow::Result<RunReport> {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));
    let (jobs, job_rx) = sync_channel(config.lane_in_flight.max(1));
    let lane = runtime
        .register_with_capacity::<_, std::convert::Infallible>(
            ObjectLane {
                max_in_flight: config.lane_in_flight,
                backend: WorkBackend::RealS3 {
                    jobs: jobs.clone(),
                    body_bytes: s3.body_bytes,
                },
                accepted: 0,
                busy: 0,
                completed: 0,
                in_flight: 0,
            },
            config.lane_mailbox,
        )
        .map_err(|e| anyhow::anyhow!("register lane: {e:?}"))?;

    let bridge = spawn_s3_bridge(Arc::clone(&runtime), lane, job_rx, s3);
    let report = drive_callers(&runtime, lane, config)?;
    drop(jobs);
    bridge.shutdown_and_join()?;

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

#[cfg(feature = "real-s3")]
fn env_bool(name: &str) -> Option<bool> {
    match std::env::var(name).ok()?.as_str() {
        "1" | "true" | "TRUE" | "yes" | "YES" => Some(true),
        "0" | "false" | "FALSE" | "no" | "NO" => Some(false),
        _ => None,
    }
}

#[cfg(feature = "real-s3")]
struct S3Job {
    request: RequestContext<LaneReply>,
    key: String,
    body: Vec<u8>,
}

#[cfg(feature = "real-s3")]
struct S3BridgeJob {
    job: S3Job,
}

#[cfg(feature = "real-s3")]
struct S3BridgeHandle {
    shutdown: Arc<AtomicBool>,
    handle: thread::JoinHandle<anyhow::Result<()>>,
}

#[cfg(feature = "real-s3")]
impl S3BridgeHandle {
    fn shutdown_and_join(self) -> anyhow::Result<()> {
        self.shutdown.store(true, Ordering::Relaxed);
        self.handle
            .join()
            .map_err(|_| anyhow::anyhow!("s3 bridge thread panicked"))?
    }
}

#[cfg(feature = "real-s3")]
fn spawn_s3_bridge(
    runtime: Arc<ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>>,
    lane: Address<LaneMsg, LaneReply>,
    jobs: Receiver<S3BridgeJob>,
    config: RealS3Config,
) -> S3BridgeHandle {
    let shutdown = Arc::new(AtomicBool::new(false));
    let bridge_shutdown = Arc::clone(&shutdown);
    let handle = thread::Builder::new()
        .name("object-lane-s3-bridge".into())
        .spawn(move || run_s3_bridge(runtime, lane, jobs, config, bridge_shutdown))
        .expect("spawn s3 bridge");
    S3BridgeHandle { shutdown, handle }
}

#[cfg(feature = "real-s3")]
fn run_s3_bridge(
    runtime: Arc<ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>>,
    lane: Address<LaneMsg, LaneReply>,
    jobs: Receiver<S3BridgeJob>,
    config: RealS3Config,
    shutdown: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let tokio = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    let client = tokio.block_on(build_s3_client(&config))?;

    while !shutdown.load(Ordering::Relaxed) {
        match jobs.recv_timeout(Duration::from_millis(50)) {
            Ok(S3BridgeJob { job }) => {
                let key = object_key(&config.prefix, &job.key);
                let result = tokio.block_on(put_object(&client, &config, &key, job.body));
                let completion = LaneMsg::PutFinished {
                    request: job.request,
                    key,
                    result,
                };
                let _ = runtime.try_send(lane, completion);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    Ok(())
}

#[cfg(feature = "real-s3")]
async fn build_s3_client(config: &RealS3Config) -> anyhow::Result<aws_sdk_s3::Client> {
    let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
    if let Some(region) = &config.region {
        loader = loader.region(aws_sdk_s3::config::Region::new(region.clone()));
    }
    let shared_config = loader.load().await;
    let mut builder = aws_sdk_s3::config::Builder::from(&shared_config);
    if let Some(endpoint_url) = &config.endpoint_url {
        builder = builder.endpoint_url(endpoint_url.clone());
    }
    builder = builder.force_path_style(config.force_path_style);
    Ok(aws_sdk_s3::Client::from_conf(builder.build()))
}

#[cfg(feature = "real-s3")]
async fn put_object(
    client: &aws_sdk_s3::Client,
    config: &RealS3Config,
    key: &str,
    body: Vec<u8>,
) -> WorkResult {
    let timeout = Duration::from_millis(config.operation_timeout_ms);
    let send = client
        .put_object()
        .bucket(&config.bucket)
        .key(key)
        .body(aws_sdk_s3::primitives::ByteStream::from(body))
        .send();

    match tokio::time::timeout(timeout, send).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(format!("{error:?}")),
        Err(_) => Err(format!("s3 put timed out after {timeout:?}")),
    }
}

#[cfg(feature = "real-s3")]
fn object_key(prefix: &str, key: &str) -> String {
    if prefix.is_empty() || prefix.ends_with('/') {
        format!("{prefix}{key}")
    } else {
        format!("{prefix}/{key}")
    }
}
