use std::error::Error;
use std::fmt;
use std::thread;
use std::time::Duration;

#[cfg(test)]
use std::sync::{Arc, Mutex};

use tina::prelude::*;
use tina_runtime::{
    CallError, CallOutcome, ConcurrencyParkError, ConcurrencyParkTicket, ConcurrencyPendingReplies,
    DefaultThreadedMailboxFactory, LocalSystem, ReportedWorkloadError, RunToShutdownError,
    SleepReply, StartupError, request_effect_after_concurrency_park, sleep,
};

const MAX_CALLERS: usize = 10_000;
const MAX_LANE_IN_FLIGHT: usize = 10_000;
const MAX_LANE_MAILBOX: usize = 100_000;
const MAX_WORK_MS: u64 = 60_000;
const MAX_CALL_TIMEOUT_MS: u64 = 60_000;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    pub fn from_env() -> Result<Self, RunConfigError> {
        let mut config = Self::default();
        config.callers = env_usize("OBJECT_LANE_CALLERS")?.unwrap_or(config.callers);
        config.lane_in_flight =
            env_usize("OBJECT_LANE_IN_FLIGHT")?.unwrap_or(config.lane_in_flight);
        config.lane_mailbox = env_usize("OBJECT_LANE_MAILBOX")?.unwrap_or(config.lane_mailbox);
        config.work_ms = env_u64("OBJECT_LANE_WORK_MS")?.unwrap_or(config.work_ms);
        config.call_timeout_ms =
            env_u64("OBJECT_LANE_CALL_TIMEOUT_MS")?.unwrap_or(config.call_timeout_ms);
        config.validate()?;
        Ok(config)
    }

    pub fn validate(self) -> Result<Self, RunConfigError> {
        validate_cap("callers", self.callers, 1, MAX_CALLERS)?;
        validate_cap("lane_in_flight", self.lane_in_flight, 1, MAX_LANE_IN_FLIGHT)?;
        // Zero is useful for deterministic mailbox-full tests.
        validate_cap("lane_mailbox", self.lane_mailbox, 0, MAX_LANE_MAILBOX)?;
        validate_duration("work_ms", self.work_ms, false, MAX_WORK_MS)?;
        validate_duration(
            "call_timeout_ms",
            self.call_timeout_ms,
            true,
            MAX_CALL_TIMEOUT_MS,
        )?;
        // Barrier participants = callers + 1 must not overflow.
        self.callers
            .checked_add(1)
            .ok_or(RunConfigError::OutOfRange {
                field: "callers",
                value: self.callers,
                min: 1,
                max: MAX_CALLERS,
            })?;
        Ok(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunConfigError {
    InvalidEnvironment {
        name: &'static str,
        value: String,
    },
    OutOfRange {
        field: &'static str,
        value: usize,
        min: usize,
        max: usize,
    },
    ZeroDuration(&'static str),
    DurationTooLarge {
        field: &'static str,
        value_ms: u128,
        max_ms: u64,
    },
    EmptyS3Bucket,
}

impl fmt::Display for RunConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEnvironment { name, value } => {
                write!(f, "{name} is not a valid integer: {value:?}")
            }
            Self::OutOfRange {
                field,
                value,
                min,
                max,
            } => write!(f, "{field}={value} is outside {min}..={max}"),
            Self::ZeroDuration(field) => write!(f, "{field} must be non-zero"),
            Self::DurationTooLarge {
                field,
                value_ms,
                max_ms,
            } => write!(f, "{field}={value_ms}ms exceeds maximum {max_ms}ms"),
            Self::EmptyS3Bucket => f.write_str("S3 bucket must not be empty"),
        }
    }
}

impl std::error::Error for RunConfigError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunReport {
    pub callers: usize,
    pub stored: usize,
    pub busy: usize,
    pub failed: usize,
    /// Exact worker-side failures, in caller observation order.
    pub work_failures: Vec<WorkFailure>,
    pub full: usize,
    pub closed: usize,
    pub timeout: usize,
    pub rejected: usize,
    pub rejection_reasons: Vec<tina::CallRejectedReason>,
    pub stats: LaneStats,
    pub dropped_permits: u64,
}

/// Successful real-S3 run with mandatory application and bridge-drain truth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3RunReport {
    /// Object-lane workload outcomes.
    pub workload: RunReport,
    /// Successful bridge close-and-drain result captured before facade shutdown.
    pub drain: tina_aws_bridge::S3DrainReport,
}

/// Workload-side failures for [`run_against_s3`].
#[derive(Debug)]
pub enum S3WorkloadError {
    /// The bridge config, client, runtime, or Tina registration failed.
    Install(tina_aws_bridge::InstallError),
    /// The object-lane application failed after bridge installation.
    Application(anyhow::Error),
    /// The application succeeded but accepted SDK work did not drain.
    Drain(tina_aws_bridge::S3DrainReport),
    /// Both application work and bridge drain failed; neither is discarded.
    ApplicationAndDrain {
        application: anyhow::Error,
        drain: tina_aws_bridge::S3DrainReport,
    },
}

impl fmt::Display for S3WorkloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Install(error) => write!(f, "install S3 bridge: {error}"),
            Self::Application(error) => write!(f, "object-lane workload failed: {error}"),
            Self::Drain(report) => write!(
                f,
                "S3 bridge did not drain: remaining={} kinds={:?}",
                report.in_flight_remaining, report.in_flight_kinds
            ),
            Self::ApplicationAndDrain { application, drain } => write!(
                f,
                "object-lane workload failed ({application}) and S3 bridge did not drain: \
                 remaining={} kinds={:?}",
                drain.in_flight_remaining, drain.in_flight_kinds
            ),
        }
    }
}

impl Error for S3WorkloadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Install(error) => Some(error),
            Self::Application(error) | Self::ApplicationAndDrain { application: error, .. } => {
                Some(error.as_ref())
            }
            Self::Drain(_) => None,
        }
    }
}

impl AsRef<dyn Error + Send + Sync + 'static> for S3WorkloadError {
    fn as_ref(&self) -> &(dyn Error + Send + Sync + 'static) {
        self
    }
}

/// Terminal S3 runner failure preserving workload and facade shutdown truth.
pub type S3TerminalError = RunToShutdownError<ReportedWorkloadError<S3WorkloadError>>;

/// Complete failure surface for [`run_against_s3`].
#[derive(Debug)]
pub enum S3RunError {
    /// Inputs were rejected before runtime construction.
    InvalidConfig(RunConfigError),
    /// The local runtime could not start.
    Startup(StartupError),
    /// Workload, bridge drain, facade shutdown, or a preserved combination failed.
    Terminal(Box<S3TerminalError>),
}

impl fmt::Display for S3RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(error) => write!(f, "invalid S3 lane configuration: {error}"),
            Self::Startup(error) => write!(f, "S3 lane startup failed: {error}"),
            Self::Terminal(error) => write!(f, "S3 lane run failed: {error}"),
        }
    }
}

impl Error for S3RunError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidConfig(error) => Some(error),
            Self::Startup(error) => Some(error),
            Self::Terminal(error) => Some(error.as_ref()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneStats {
    pub accepted: usize,
    pub busy: usize,
    pub work_completed: usize,
    pub completed: u64,
    pub current: usize,
    pub retired: u64,
    pub caller_gone: u64,
    pub counts_agree: bool,
    pub settlements_agree: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneReply {
    Stored(String),
    Busy { in_flight: usize, cap: usize },
    Failed(WorkFailure),
    Stats(LaneStats),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkFailure {
    Timer(CallError),
    Bridge(BridgeFailure),
    S3(tina_aws_bridge::S3Error),
    UnexpectedS3Response(tina_aws_bridge::S3Response),
    Protocol(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeFailure {
    Full,
    Closed,
    Timeout,
    Rejected(tina::CallRejectedReason),
}

#[derive(Debug)]
enum LaneEvent {
    PutFinished {
        ticket: ConcurrencyParkTicket<u64>,
        key: String,
        result: Result<(), WorkFailure>,
    },
    #[cfg(test)]
    Stop,
}

#[derive(Debug)]
enum LaneRequest {
    Put { key: String },
    Stats,
}

enum WorkBackend {
    FakeSleep {
        work: Duration,
    },
    #[allow(dead_code)]
    AwsS3 {
        address: tina_aws_bridge::S3Address,
        bucket: String,
        key_prefix: String,
        timeout: Duration,
    },
}

struct ObjectLane {
    pending: ConcurrencyPendingReplies<u64, LaneReply>,
    backend: WorkBackend,
    next_id: u64,
    accepted: usize,
    busy: usize,
    work_completed: usize,
    #[cfg(test)]
    lifecycle_audit: Option<Arc<Mutex<Option<LaneStats>>>>,
}

#[tina_runtime::isolate(event = LaneEvent, request = LaneRequest, reply = LaneReply)]
impl ObjectLane {
    fn handle_event(
        &mut self,
        event: LaneEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            LaneEvent::PutFinished {
                ticket,
                key,
                result,
            } => {
                let reply = match result {
                    Ok(()) => {
                        self.work_completed += 1;
                        LaneReply::Stored(key)
                    }
                    Err(error) => LaneReply::Failed(error),
                };
                self.pending
                    .reply_ticket::<Self>(ticket, reply)
                    .unwrap_or_else(|_| noop())
            }
            #[cfg(test)]
            LaneEvent::Stop => stop(),
        }
    }

    fn handle_request(
        &mut self,
        request: LaneRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            LaneRequest::Put { key } => self.put(key, call),
            LaneRequest::Stats => {
                let report = self.pending.report();
                call.reply(LaneReply::Stats(LaneStats {
                    accepted: self.accepted,
                    busy: self.busy,
                    work_completed: self.work_completed,
                    completed: report.completed_count,
                    current: report.admission.current,
                    retired: report.retired_count,
                    caller_gone: report.caller_gone_count,
                    counts_agree: report.counts_agree(),
                    settlements_agree: self.accepted as u64
                        == report
                            .completed_count
                            .saturating_add(report.retired_count)
                            .saturating_add(report.admission.current as u64),
                }))
            }
        }
    }
}

impl Drop for ObjectLane {
    fn drop(&mut self) {
        let _ = self.pending.drain();
        #[cfg(test)]
        if let Some(audit) = &self.lifecycle_audit {
            let report = self.pending.report();
            let stats = LaneStats {
                accepted: self.accepted,
                busy: self.busy,
                work_completed: self.work_completed,
                completed: report.completed_count,
                current: report.admission.current,
                retired: report.retired_count,
                caller_gone: report.caller_gone_count,
                counts_agree: report.counts_agree(),
                settlements_agree: self.accepted as u64
                    == report
                        .completed_count
                        .saturating_add(report.retired_count)
                        .saturating_add(report.admission.current as u64),
            };
            *audit.lock().expect("lifecycle audit lock is not poisoned") = Some(stats);
        }
    }
}

impl ObjectLane {
    fn put(&mut self, key: String, call: RequestCall<'_, Self>) -> RequestEffect<Self> {
        let id = self.next_id;
        let Some(next_id) = id.checked_add(1) else {
            return call.reply(LaneReply::Failed(WorkFailure::Protocol(
                "operation id space exhausted",
            )));
        };
        let (ticket, effect_permit) = match self.pending.park_request(id, call) {
            Ok(parked) => parked,
            Err(ConcurrencyParkError::Admission { call, failure, .. }) => {
                self.busy += 1;
                let report = failure.report();
                return call.reply(LaneReply::Busy {
                    in_flight: report.current,
                    cap: report.capacity,
                });
            }
            Err(ConcurrencyParkError::DuplicateKey { call, .. }) => {
                return call.reply(LaneReply::Failed(WorkFailure::Protocol(
                    "duplicate operation id",
                )));
            }
            Err(ConcurrencyParkError::PendingFull { call, .. }) => {
                return call.reply(LaneReply::Failed(WorkFailure::Protocol(
                    "pending replies full after admission",
                )));
            }
        };
        self.next_id = next_id;
        self.accepted += 1;

        let effect = match &self.backend {
            WorkBackend::FakeSleep { work } => {
                sleep(*work).then_service_event(move |result| LaneEvent::PutFinished {
                    ticket,
                    key,
                    result: sleep_to_work_result(result),
                })
            }
            WorkBackend::AwsS3 {
                address,
                bucket,
                key_prefix,
                timeout,
            } => {
                let full_key = format!("{key_prefix}{key}");
                tina_aws_bridge::send_s3(
                    *address,
                    tina_aws_bridge::S3Request::PutObject(tina_aws_bridge::S3PutObject {
                        bucket: bucket.clone(),
                        key: full_key,
                        body: key.clone().into_bytes(),
                        content_type: Some("application/octet-stream".into()),
                    }),
                    *timeout,
                )
                .then_service_event(move |outcome| LaneEvent::PutFinished {
                    ticket,
                    key,
                    result: s3_outcome_to_work_result(outcome),
                })
            }
        };
        request_effect_after_concurrency_park(effect_permit, effect)
    }
}

/// Runs the lane against an S3 worker installed into the same `LocalSystem`.
///
/// Lane admission remains local and bounded. Bridge admission, delivery,
/// rejection, worker, and unexpected-response failures retain their typed
/// variants in [`WorkFailure`].
pub fn run_against_s3(
    config: RunConfig,
    s3_config: tina_aws_bridge::S3Config,
    bucket: String,
    key_prefix: String,
    bridge_timeout: Duration,
) -> Result<S3RunReport, S3RunError> {
    let config = config.validate().map_err(S3RunError::InvalidConfig)?;
    if bucket.trim().is_empty() {
        return Err(S3RunError::InvalidConfig(RunConfigError::EmptyS3Bucket));
    }
    if bridge_timeout > Duration::from_millis(MAX_CALL_TIMEOUT_MS) {
        return Err(S3RunError::InvalidConfig(
            RunConfigError::DurationTooLarge {
                field: "bridge_timeout",
                value_ms: duration_millis_ceil(bridge_timeout),
                max_ms: MAX_CALL_TIMEOUT_MS,
            },
        ));
    }
    let dropped_before = tina_runtime::dropped_permit_count();
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory)
        .try_build()
        .map_err(S3RunError::Startup)?;
    let mut report = app
        .run_to_shutdown_reported(SHUTDOWN_TIMEOUT, |app| {
            let bridge = tina_aws_bridge::install_s3_local(app, s3_config)
                .map_err(S3WorkloadError::Install)?;
            let workload = register_lane(
                app,
                config,
                WorkBackend::AwsS3 {
                    address: bridge.address,
                    bucket,
                    key_prefix,
                    timeout: bridge_timeout,
                },
            )
            .and_then(|lane| drive_callers(app, lane.requests, config, true));
            let drain = bridge.closer.close_and_drain(SHUTDOWN_TIMEOUT);
            finish_s3_workload(workload, drain)
        })
        .map_err(|error| S3RunError::Terminal(Box::new(error)))?;
    report.workload.dropped_permits =
        tina_runtime::dropped_permit_count().saturating_sub(dropped_before);
    Ok(report)
}

fn duration_millis_ceil(duration: Duration) -> u128 {
    duration.as_nanos().div_ceil(1_000_000)
}

fn finish_s3_workload(
    workload: anyhow::Result<RunReport>,
    drain: tina_aws_bridge::S3DrainReport,
) -> Result<S3RunReport, S3WorkloadError> {
    match (workload, drain.drained) {
        (Ok(workload), true) => Ok(S3RunReport { workload, drain }),
        (Err(error), true) => Err(S3WorkloadError::Application(error)),
        (Ok(_), false) => Err(S3WorkloadError::Drain(drain)),
        (Err(application), false) => Err(S3WorkloadError::ApplicationAndDrain {
            application,
            drain,
        }),
    }
}

pub fn run(config: RunConfig) -> anyhow::Result<RunReport> {
    let config = config.validate()?;
    let dropped_before = tina_runtime::dropped_permit_count();
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;
    let mut report = app
        .run_to_shutdown_reported(SHUTDOWN_TIMEOUT, |app| {
            let lane = register_lane(
                app,
                config,
                WorkBackend::FakeSleep {
                    work: Duration::from_millis(config.work_ms),
                },
            )?;
            drive_callers(app, lane.requests, config, true)
        })
        .map_err(anyhow::Error::from)?;
    report.dropped_permits = tina_runtime::dropped_permit_count().saturating_sub(dropped_before);
    Ok(report)
}

/// Drive Puts only and keep exact host terminals without requiring Stats.
///
/// Used to prove mailbox `Full` (and other host outcomes) stay distinct from
/// application `Busy` when the mailbox itself cannot accept work.
pub fn run_put_terminals(config: RunConfig) -> anyhow::Result<RunReport> {
    let config = config.validate()?;
    let dropped_before = tina_runtime::dropped_permit_count();
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;
    let mut report = app
        .run_to_shutdown_reported(SHUTDOWN_TIMEOUT, |app| {
            let lane = register_lane(
                app,
                config,
                WorkBackend::FakeSleep {
                    work: Duration::from_millis(config.work_ms),
                },
            )?;
            drive_callers(app, lane.requests, config, false)
        })
        .map_err(anyhow::Error::from)?;
    report.dropped_permits = tina_runtime::dropped_permit_count().saturating_sub(dropped_before);
    Ok(report)
}

fn register_lane(
    app: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
    config: RunConfig,
    backend: WorkBackend,
) -> anyhow::Result<tina_runtime::SplitServiceHandle<LaneEvent, LaneRequest, LaneReply>> {
    app.register_split_service::<ObjectLane, LaneEvent, LaneRequest, std::convert::Infallible>(
        ObjectLane {
            pending: ConcurrencyPendingReplies::with_capacity(
                "system_bounded_object_lane.pending",
                config.lane_in_flight,
            ),
            backend,
            next_id: 1,
            accepted: 0,
            busy: 0,
            work_completed: 0,
            #[cfg(test)]
            lifecycle_audit: None,
        },
        config.lane_mailbox,
    )
    .map_err(|error| anyhow::anyhow!("register lane: {error:?}"))
}

fn drive_callers(
    app: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
    lane: tina::ServiceRequestAddress<LaneEvent, LaneRequest, LaneReply>,
    config: RunConfig,
    require_stats: bool,
) -> anyhow::Result<RunReport> {
    let participants = config
        .callers
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("callers + 1 overflowed"))?;
    let barrier = std::sync::Barrier::new(participants);
    let call_timeout = Duration::from_millis(config.call_timeout_ms);
    let outcomes = thread::scope(|scope| {
        let mut threads = Vec::with_capacity(config.callers);
        for n in 0..config.callers {
            let barrier = &barrier;
            threads.push(scope.spawn(move || {
                barrier.wait();
                app.call_blocking_request(
                    lane,
                    LaneRequest::Put {
                        key: format!("object-{n}"),
                    },
                    call_timeout,
                )
            }));
        }
        barrier.wait();
        threads
            .into_iter()
            .map(|thread| {
                thread
                    .join()
                    .map_err(|_| anyhow::anyhow!("object-lane caller thread panicked"))
            })
            .collect::<anyhow::Result<Vec<_>>>()
    })?;

    let mut stored = 0;
    let mut busy = 0;
    let mut failed = 0;
    let mut work_failures = Vec::new();
    let mut full = 0;
    let mut closed = 0;
    let mut timeout = 0;
    let mut rejected = 0;
    let mut rejection_reasons = Vec::new();
    for outcome in outcomes {
        match outcome? {
            CallOutcome::Replied(LaneReply::Stored(_)) => stored += 1,
            CallOutcome::Replied(LaneReply::Busy { .. }) => busy += 1,
            CallOutcome::Replied(LaneReply::Failed(error)) => {
                failed += 1;
                work_failures.push(error);
            }
            CallOutcome::Replied(LaneReply::Stats(stats)) => {
                return Err(anyhow::anyhow!(HostFailure::UnexpectedPutReply(
                    LaneReply::Stats(stats),
                )));
            }
            CallOutcome::Full => full += 1,
            CallOutcome::Closed => closed += 1,
            CallOutcome::Timeout => timeout += 1,
            CallOutcome::Rejected(reason) => {
                rejected += 1;
                rejection_reasons.push(reason);
            }
        }
    }

    let stats = if require_stats {
        match app.call_blocking_request(lane, LaneRequest::Stats, Duration::from_secs(1))? {
            CallOutcome::Replied(LaneReply::Stats(stats)) => stats,
            CallOutcome::Replied(other) => {
                return Err(anyhow::anyhow!(HostFailure::UnexpectedStatsReply(other)));
            }
            CallOutcome::Full => return Err(anyhow::anyhow!(HostFailure::StatsFull)),
            CallOutcome::Closed => return Err(anyhow::anyhow!(HostFailure::StatsClosed)),
            CallOutcome::Timeout => return Err(anyhow::anyhow!(HostFailure::StatsTimeout)),
            CallOutcome::Rejected(reason) => {
                return Err(anyhow::anyhow!(HostFailure::StatsRejected(reason)));
            }
        }
    } else {
        // Stats were not observed. Do not claim agreement — Put terminals are
        // the only load-bearing fields on this path.
        LaneStats {
            accepted: 0,
            busy: 0,
            work_completed: 0,
            completed: 0,
            current: 0,
            retired: 0,
            caller_gone: 0,
            counts_agree: false,
            settlements_agree: false,
        }
    };

    Ok(RunReport {
        callers: config.callers,
        stored,
        busy,
        failed,
        work_failures,
        full,
        closed,
        timeout,
        rejected,
        rejection_reasons,
        stats,
        dropped_permits: 0,
    })
}

#[derive(Debug)]
enum HostFailure {
    UnexpectedPutReply(LaneReply),
    UnexpectedStatsReply(LaneReply),
    StatsFull,
    StatsClosed,
    StatsTimeout,
    StatsRejected(tina::CallRejectedReason),
}

impl fmt::Display for HostFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedPutReply(reply) => {
                write!(f, "put call returned unexpected reply: {reply:?}")
            }
            Self::UnexpectedStatsReply(reply) => write!(f, "unexpected stats reply: {reply:?}"),
            Self::StatsFull => f.write_str("stats call mailbox was full"),
            Self::StatsClosed => f.write_str("stats service was closed"),
            Self::StatsTimeout => f.write_str("stats call timed out"),
            Self::StatsRejected(reason) => write!(f, "stats call was rejected: {reason:?}"),
        }
    }
}

impl std::error::Error for HostFailure {}

fn sleep_to_work_result(result: SleepReply) -> Result<(), WorkFailure> {
    result.map_err(WorkFailure::Timer)
}

fn s3_outcome_to_work_result(
    outcome: CallOutcome<Result<tina_aws_bridge::S3Response, tina_aws_bridge::S3Error>>,
) -> Result<(), WorkFailure> {
    match outcome {
        CallOutcome::Replied(Ok(tina_aws_bridge::S3Response::PutObject(_))) => Ok(()),
        CallOutcome::Replied(Ok(other)) => Err(WorkFailure::UnexpectedS3Response(other)),
        CallOutcome::Replied(Err(error)) => Err(WorkFailure::S3(error)),
        CallOutcome::Full => Err(WorkFailure::Bridge(BridgeFailure::Full)),
        CallOutcome::Closed => Err(WorkFailure::Bridge(BridgeFailure::Closed)),
        CallOutcome::Timeout => Err(WorkFailure::Bridge(BridgeFailure::Timeout)),
        CallOutcome::Rejected(reason) => Err(WorkFailure::Bridge(BridgeFailure::Rejected(reason))),
    }
}

fn validate_cap(
    field: &'static str,
    value: usize,
    min: usize,
    max: usize,
) -> Result<(), RunConfigError> {
    if (min..=max).contains(&value) {
        Ok(())
    } else {
        Err(RunConfigError::OutOfRange {
            field,
            value,
            min,
            max,
        })
    }
}

fn validate_duration(
    field: &'static str,
    value_ms: u64,
    require_nonzero: bool,
    max_ms: u64,
) -> Result<(), RunConfigError> {
    if require_nonzero && value_ms == 0 {
        Err(RunConfigError::ZeroDuration(field))
    } else if value_ms > max_ms {
        Err(RunConfigError::DurationTooLarge {
            field,
            value_ms: u128::from(value_ms),
            max_ms,
        })
    } else {
        Ok(())
    }
}

fn env_usize(name: &'static str) -> Result<Option<usize>, RunConfigError> {
    env_parse(name)
}

fn env_u64(name: &'static str) -> Result<Option<u64>, RunConfigError> {
    env_parse(name)
}

fn env_parse<T>(name: &'static str) -> Result<Option<T>, RunConfigError>
where
    T: std::str::FromStr,
{
    let Some(value) = std::env::var_os(name) else {
        return Ok(None);
    };
    let value = value.to_string_lossy().into_owned();
    value
        .parse()
        .map(Some)
        .map_err(|_| RunConfigError::InvalidEnvironment { name, value })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn typed_timer_and_bridge_failures_are_preserved() {
        assert_eq!(
            sleep_to_work_result(Err(CallError::TimerFull)),
            Err(WorkFailure::Timer(CallError::TimerFull))
        );
        assert_eq!(
            s3_outcome_to_work_result(CallOutcome::Rejected(
                tina::CallRejectedReason::HandlerPanicked
            )),
            Err(WorkFailure::Bridge(BridgeFailure::Rejected(
                tina::CallRejectedReason::HandlerPanicked
            )))
        );
        assert_eq!(
            s3_outcome_to_work_result(CallOutcome::Replied(Err(
                tina_aws_bridge::S3Error::RequestTooLarge
            ))),
            Err(WorkFailure::S3(tina_aws_bridge::S3Error::RequestTooLarge))
        );
        assert_eq!(
            s3_outcome_to_work_result(CallOutcome::Full),
            Err(WorkFailure::Bridge(BridgeFailure::Full))
        );
        assert_eq!(
            s3_outcome_to_work_result(CallOutcome::Closed),
            Err(WorkFailure::Bridge(BridgeFailure::Closed))
        );
        assert_eq!(
            s3_outcome_to_work_result(CallOutcome::Timeout),
            Err(WorkFailure::Bridge(BridgeFailure::Timeout))
        );
        let unexpected =
            tina_aws_bridge::S3Response::DeletedObject(tina_aws_bridge::S3DeletedObject {
                version_id: Some("v1".into()),
                delete_marker: Some(true),
            });
        assert_eq!(
            s3_outcome_to_work_result(CallOutcome::Replied(Ok(unexpected.clone()))),
            Err(WorkFailure::UnexpectedS3Response(unexpected))
        );
    }

    #[test]
    fn s3_lifecycle_retains_workload_and_drain_failures_together() {
        let drain = tina_aws_bridge::S3DrainReport {
            closed: true,
            drained: false,
            in_flight_remaining: 1,
            in_flight_kinds: vec![("put_object", 1)],
        };
        let error = finish_s3_workload(Err(anyhow::anyhow!("workload")), drain)
            .expect_err("both failures must remain visible");
        assert!(matches!(
            error,
            S3WorkloadError::ApplicationAndDrain {
                application,
                drain: tina_aws_bridge::S3DrainReport {
                    drained: false,
                    in_flight_remaining: 1,
                    ..
                },
            } if application.to_string() == "workload"
        ));
    }

    #[test]
    fn invalid_dimensions_are_typed_and_do_not_panic() {
        assert!(matches!(
            RunConfig {
                callers: 0,
                ..RunConfig::default()
            }
            .validate(),
            Err(RunConfigError::OutOfRange {
                field: "callers",
                ..
            })
        ));
        assert!(matches!(
            RunConfig {
                lane_in_flight: 0,
                ..RunConfig::default()
            }
            .validate(),
            Err(RunConfigError::OutOfRange {
                field: "lane_in_flight",
                ..
            })
        ));
        assert!(matches!(
            RunConfig {
                lane_in_flight: MAX_LANE_IN_FLIGHT + 1,
                ..RunConfig::default()
            }
            .validate(),
            Err(RunConfigError::OutOfRange {
                field: "lane_in_flight",
                ..
            })
        ));
        assert!(
            RunConfig {
                lane_mailbox: 0,
                ..RunConfig::default()
            }
            .validate()
            .is_ok()
        );
        assert!(matches!(
            RunConfig {
                work_ms: MAX_WORK_MS + 1,
                ..RunConfig::default()
            }
            .validate(),
            Err(RunConfigError::DurationTooLarge {
                field: "work_ms",
                ..
            })
        ));
        assert!(matches!(
            RunConfig {
                call_timeout_ms: MAX_CALL_TIMEOUT_MS + 1,
                ..RunConfig::default()
            }
            .validate(),
            Err(RunConfigError::DurationTooLarge {
                field: "call_timeout_ms",
                ..
            })
        ));
    }

    #[test]
    fn caller_gone_retires_and_second_wave_refills() {
        let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory)
            .try_build()
            .expect("build app");
        let stats = app
            .run_to_shutdown_reported(SHUTDOWN_TIMEOUT, |app| -> anyhow::Result<LaneStats> {
                let config = RunConfig {
                    callers: 1,
                    lane_in_flight: 1,
                    lane_mailbox: 8,
                    work_ms: 40,
                    call_timeout_ms: 5,
                };
                let lane = register_lane(
                    app,
                    config,
                    WorkBackend::FakeSleep {
                        work: Duration::from_millis(config.work_ms),
                    },
                )?;
                assert_eq!(
                    app.call_blocking_request(
                        lane.requests,
                        LaneRequest::Put {
                            key: "first".into()
                        },
                        Duration::from_millis(5),
                    )?,
                    CallOutcome::Timeout
                );
                thread::sleep(Duration::from_millis(80));
                assert!(matches!(
                    app.call_blocking_request(
                        lane.requests,
                        LaneRequest::Put { key: "second".into() },
                        Duration::from_millis(200),
                    )?,
                    CallOutcome::Replied(LaneReply::Stored(key)) if key == "second"
                ));
                match app.call_blocking_request(
                    lane.requests,
                    LaneRequest::Stats,
                    Duration::from_secs(1),
                )? {
                    CallOutcome::Replied(LaneReply::Stats(stats)) => Ok(stats),
                    other => anyhow::bail!("unexpected stats outcome: {other:?}"),
                }
            })
            .expect("workload and shutdown succeed");

        assert_eq!(stats.current, 0);
        assert_eq!(stats.work_completed, 2);
        assert_eq!(stats.completed, 1);
        assert_eq!(stats.retired, 1);
        assert_eq!(stats.caller_gone, 1);
        assert!(stats.counts_agree);
        assert!(stats.settlements_agree);
    }

    #[test]
    fn owner_stop_retires_mid_flight_authority() {
        let dropped_before = tina_runtime::dropped_permit_count();
        let audit = Arc::new(Mutex::new(None));
        let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory)
            .try_build()
            .expect("build app");
        app.run_to_shutdown_reported(SHUTDOWN_TIMEOUT, |app| -> anyhow::Result<()> {
            let lane = app.register_split_service::<
                ObjectLane,
                LaneEvent,
                LaneRequest,
                std::convert::Infallible,
            >(
                ObjectLane {
                    pending: ConcurrencyPendingReplies::with_capacity("owner-stop", 1),
                    backend: WorkBackend::FakeSleep {
                        work: Duration::from_secs(1),
                    },
                    next_id: 1,
                    accepted: 0,
                    busy: 0,
                    work_completed: 0,
                    lifecycle_audit: Some(Arc::clone(&audit)),
                },
                8,
            )?;

            thread::scope(|scope| -> anyhow::Result<()> {
                let caller = scope.spawn(|| {
                    app.call_blocking_request(
                        lane.requests,
                        LaneRequest::Put { key: "held".into() },
                        Duration::from_secs(2),
                    )
                });

                let deadline = Instant::now() + Duration::from_secs(1);
                loop {
                    match app.call_blocking_request(
                        lane.requests,
                        LaneRequest::Stats,
                        Duration::from_millis(50),
                    )? {
                        CallOutcome::Replied(LaneReply::Stats(stats)) if stats.current == 1 => {
                            break;
                        }
                        _ if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
                        other => anyhow::bail!("request was not admitted before stop: {other:?}"),
                    }
                }

                app.send_event_observed_until(
                    lane.events,
                    Instant::now() + Duration::from_secs(1),
                    Duration::from_millis(5),
                    || LaneEvent::Stop,
                )?;
                let outcome = caller
                    .join()
                    .map_err(|_| anyhow::anyhow!("caller thread panicked"))??;
                assert!(matches!(
                    outcome,
                    CallOutcome::Closed | CallOutcome::Rejected(_)
                ));
                Ok(())
            })
        })
        .expect("workload and shutdown succeed");

        let stats = audit
            .lock()
            .expect("audit lock")
            .clone()
            .expect("owner drop published audit");
        assert_eq!(stats.current, 0);
        assert_eq!(stats.completed, 0);
        assert_eq!(stats.retired, 1);
        assert!(stats.counts_agree);
        assert!(stats.settlements_agree);
        assert_eq!(tina_runtime::dropped_permit_count(), dropped_before);
    }
}
