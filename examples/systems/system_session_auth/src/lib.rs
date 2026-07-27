//! `system_session_auth` — sharded session table with a recurring expiry
//! sweep, hosted on `LocalMultiShardSystem`.
//!
//! What this specimen pulls on:
//!
//! - One per-shard `SessionBucket` split service. No router isolate; the host
//!   routes by `ShardPlacement` and calls each bucket through
//!   `LocalMultiShardSystem::call_blocking_request`.
//! - Owner-provided time: login/touch stamp `call.now()`, sweep compares
//!   against `ctx.now()`. Live and simulator share the same clock rail.
//! - Runtime-owned `sleep(...).then_service_event` for the recurring sweep.
//! - Checked shard conversion and bounded `RunConfig::validate` before any
//!   worker, mailbox, or placement is built.

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use std::convert::Infallible;
use std::error::Error;
use std::fmt;
use std::thread;
use std::time::{Duration, Instant};

use tina::prelude::*;
use tina_runtime::sharded::{ShardPlacement, ShardPlacementError};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, LocalMultiShardSystem, LocalSystem,
    ReportedWorkloadError, RunToShutdownError, SleepReply, StartupError,
    ThreadedRegisterBootstrapError, ThreadedRuntimeError, sleep,
};

/// Hard public ceilings. Validation rejects above these before startup.
pub const MAX_SHARDS: usize = 64;
/// Per-shard session table capacity ceiling.
pub const MAX_SESSIONS_PER_SHARD: usize = 4_096;
/// Per-bucket mailbox capacity ceiling.
pub const MAX_SESSION_MAILBOX: usize = 65_536;
/// Idle timeout ceiling in milliseconds.
pub const MAX_IDLE_TIMEOUT_MS: u64 = 60_000;
/// Sweep interval ceiling in milliseconds.
pub const MAX_SWEEP_INTERVAL_MS: u64 = 60_000;
/// Host call timeout ceiling in milliseconds.
pub const MAX_CALL_TIMEOUT_MS: u64 = 60_000;
/// Maximum UTF-8 bytes in a user id before mailbox admission.
pub const MAX_USER_ID_BYTES: usize = 256;
/// Maximum UTF-8 bytes in a session token before mailbox admission.
pub const MAX_SESSION_TOKEN_BYTES: usize = 128;
/// Consuming shutdown observation budget for public runners.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Tunables for one specimen run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunConfig {
    /// Number of live shards (must convert cleanly to `u32` shard ids).
    pub shards: usize,
    /// Hard cap on concurrent sessions per shard bucket.
    pub max_sessions_per_shard: usize,
    /// Idle lifetime before the sweep may expire a session.
    pub idle_timeout_ms: u64,
    /// Recurring sweep cadence.
    pub sweep_interval_ms: u64,
    /// Per-bucket mailbox capacity.
    pub session_mailbox: usize,
    /// Host call timeout for login/touch/logout/stats.
    pub call_timeout_ms: u64,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            shards: 4,
            max_sessions_per_shard: 16,
            idle_timeout_ms: 80,
            sweep_interval_ms: 20,
            session_mailbox: 128,
            call_timeout_ms: 2_000,
        }
    }
}

/// Typed preflight failure. Nothing is started when this returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunConfigError {
    /// A required positive field was zero.
    Zero {
        /// Config field name.
        field: &'static str,
    },
    /// A field exceeded its public maximum.
    TooLarge {
        /// Config field name.
        field: &'static str,
        /// Requested value.
        requested: u128,
        /// Allowed maximum.
        max: u128,
    },
    /// Shard count cannot convert to a `u32` shard id space.
    ShardConversion {
        /// Requested shard count.
        requested: usize,
    },
}

impl fmt::Display for RunConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Zero { field } => write!(f, "{field} must be greater than zero"),
            Self::TooLarge {
                field,
                requested,
                max,
            } => write!(f, "{field} {requested} exceeds maximum {max}"),
            Self::ShardConversion { requested } => {
                write!(f, "shards {requested} cannot convert to a u32 shard id")
            }
        }
    }
}

impl Error for RunConfigError {}

impl RunConfig {
    /// Validate all panic and allocation bounds before starting Tina.
    pub fn validate(self) -> Result<Self, RunConfigError> {
        validate_usize("shards", self.shards, MAX_SHARDS)?;
        if u32::try_from(self.shards).is_err() {
            return Err(RunConfigError::ShardConversion {
                requested: self.shards,
            });
        }
        validate_usize(
            "max_sessions_per_shard",
            self.max_sessions_per_shard,
            MAX_SESSIONS_PER_SHARD,
        )?;
        validate_u64("idle_timeout_ms", self.idle_timeout_ms, MAX_IDLE_TIMEOUT_MS)?;
        validate_u64(
            "sweep_interval_ms",
            self.sweep_interval_ms,
            MAX_SWEEP_INTERVAL_MS,
        )?;
        validate_usize("session_mailbox", self.session_mailbox, MAX_SESSION_MAILBOX)?;
        validate_u64("call_timeout_ms", self.call_timeout_ms, MAX_CALL_TIMEOUT_MS)?;
        Ok(self)
    }

    /// Checked conversion of the shard count into a `u32` id space.
    pub fn shard_count_u32(self) -> Result<u32, RunConfigError> {
        let config = self.validate()?;
        u32::try_from(config.shards).map_err(|_| RunConfigError::ShardConversion {
            requested: config.shards,
        })
    }
}

fn validate_usize(field: &'static str, value: usize, max: usize) -> Result<(), RunConfigError> {
    if value == 0 {
        Err(RunConfigError::Zero { field })
    } else if value > max {
        Err(RunConfigError::TooLarge {
            field,
            requested: value as u128,
            max: max as u128,
        })
    } else {
        Ok(())
    }
}

fn validate_u64(field: &'static str, value: u64, max: u64) -> Result<(), RunConfigError> {
    if value == 0 {
        Err(RunConfigError::Zero { field })
    } else if value > max {
        Err(RunConfigError::TooLarge {
            field,
            requested: u128::from(value),
            max: u128::from(max),
        })
    } else {
        Ok(())
    }
}

/// Aggregate report for one specimen run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunReport {
    /// Login → touch → logout walk.
    pub login_touch_logout: LoginTouchLogoutReport,
    /// Idle expiry walk.
    pub idle_expiry: IdleExpiryReport,
    /// Per-bucket capacity overflow walk.
    pub overflow: OverflowReport,
}

/// Login/touch/logout scenario outcomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginTouchLogoutReport {
    /// Login produced an admitted token.
    pub login_ok: bool,
    /// Touch found the live session.
    pub touch_ok: bool,
    /// Logout released the session.
    pub logout_ok: bool,
    /// Touch after logout returned NotFound.
    pub touch_after_logout_not_found: bool,
    /// Aggregated bucket stats after the walk.
    pub stats: SessionStats,
}

/// Idle-expiry scenario outcomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdleExpiryReport {
    /// Touch after idle wait returned NotFound.
    pub touch_after_idle_not_found: bool,
    /// Aggregated bucket stats after the walk.
    pub stats: SessionStats,
}

/// Overflow scenario outcomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverflowReport {
    /// Login replies that admitted a session.
    pub admitted: usize,
    /// Login replies that hit the per-bucket cap.
    pub full: usize,
    /// Aggregated bucket stats after the walk.
    pub stats: SessionStats,
}

/// Operational snapshot, aggregated across every shard the host owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionStats {
    /// Live sessions across all shards.
    pub active: usize,
    /// Cumulative admitted logins.
    pub admitted: u64,
    /// Cumulative voluntary logouts.
    pub logged_out: u64,
    /// Cumulative idle expiries.
    pub idle_expired: u64,
    /// Cumulative successful touches.
    pub touch_ok: u64,
    /// Cumulative touch misses.
    pub touch_not_found: u64,
    /// Cumulative capacity rejects.
    pub full_rejects: u64,
    /// Cumulative duplicate-token rejects.
    pub duplicate_rejects: u64,
    /// Cumulative successful sweep ticks.
    pub sweeps_run: u64,
    /// Cumulative timer dependency failures observed by the sweep.
    pub timer_errors: u64,
    /// Per-shard live session counts.
    pub per_shard_active: Vec<u64>,
    /// Per-shard admitted counts.
    pub per_shard_admitted: Vec<u64>,
    /// Per-shard high-water marks.
    pub per_shard_high_water: Vec<u64>,
}

/// Why a session input was rejected before mailbox admission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionInputError {
    /// Empty identities and tokens are not meaningful.
    Empty { field: &'static str },
    /// UTF-8 byte length exceeded the public bound.
    TooLong {
        /// Input field name.
        field: &'static str,
        /// Actual UTF-8 byte length.
        actual_bytes: usize,
        /// Maximum accepted UTF-8 byte length.
        max_bytes: usize,
    },
}

impl fmt::Display for SessionInputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty { field } => write!(f, "{field} must not be empty"),
            Self::TooLong {
                field,
                actual_bytes,
                max_bytes,
            } => {
                write!(
                    f,
                    "{field} UTF-8 byte length {actual_bytes} exceeds maximum {max_bytes}"
                )
            }
        }
    }
}

impl Error for SessionInputError {}

fn validate_input(field: &'static str, value: &str, max: usize) -> Result<(), SessionInputError> {
    if value.is_empty() {
        Err(SessionInputError::Empty { field })
    } else if value.len() > max {
        Err(SessionInputError::TooLong {
            field,
            actual_bytes: value.len(),
            max_bytes: max,
        })
    } else {
        Ok(())
    }
}

/// Bounded application user identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserId(String);

impl UserId {
    /// Validate UTF-8 byte length before allocating owned storage.
    pub fn try_new(value: impl AsRef<str>) -> Result<Self, SessionInputError> {
        let value = value.as_ref();
        validate_input("user_id", value, MAX_USER_ID_BYTES)?;
        Ok(Self(value.to_owned()))
    }

    /// Borrow the validated identity as UTF-8.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for UserId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

/// Opaque, bounded session handle. Encodes nothing the host couldn't compute.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct SessionToken(String);

impl SessionToken {
    /// Validate UTF-8 byte length before allocating owned storage.
    pub fn try_new(value: impl AsRef<str>) -> Result<Self, SessionInputError> {
        let value = value.as_ref();
        validate_input("session_token", value, MAX_SESSION_TOKEN_BYTES)?;
        Ok(Self(value.to_owned()))
    }

    /// Borrow the validated opaque token as UTF-8.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for SessionToken {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

/// Typed reply vocabulary for session operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionAuthReply {
    /// Login admitted a new session for this token.
    Admitted {
        /// Token bound to the admitted session.
        token: SessionToken,
    },
    /// Touch refreshed the idle deadline.
    Touched,
    /// Logout released the session.
    LoggedOut,
    /// Session was logged out, expired, or never existed. We do not leak which.
    NotFound,
    /// The owning shard bucket was at capacity.
    Full,
    /// The token already names a live session; no row or admission count changed.
    AlreadyExists,
    /// One shard's operational snapshot.
    Stats(BucketStats),
}

/// One shard's slice of stats. Aggregated by the host into [`SessionStats`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BucketStats {
    /// Live sessions on this shard.
    pub active: u64,
    /// Cumulative admitted logins.
    pub admitted: u64,
    /// Cumulative voluntary logouts.
    pub logged_out: u64,
    /// Cumulative idle expiries.
    pub idle_expired: u64,
    /// Cumulative successful touches.
    pub touch_ok: u64,
    /// Cumulative touch misses.
    pub touch_not_found: u64,
    /// Cumulative capacity rejects.
    pub full_rejects: u64,
    /// Cumulative duplicate-token rejects.
    pub duplicate_rejects: u64,
    /// Cumulative successful sweep ticks.
    pub sweeps_run: u64,
    /// Cumulative timer dependency failures.
    pub timer_errors: u64,
    /// Peak concurrent sessions on this shard.
    pub high_water: u64,
}

/// Fire-and-forget facts a bucket accepts: bootstrap and the recurring sweep.
#[derive(Debug)]
pub enum SessionAuthEvent {
    /// One-shot kick from registration bootstrap to start the recurring sweep.
    Bootstrap,
    /// Internal sweep tick carrying the timer dependency result.
    Sweep {
        /// Timer completion or typed timer failure.
        result: SleepReply,
    },
}

/// Caller-authority requests the host can ask a bucket.
#[derive(Debug)]
pub enum SessionAuthRequest {
    /// Host-supplied token; the bucket records it and replies with admission.
    Login {
        /// Application user identity.
        user_id: UserId,
        /// Host-minted token already routed to this shard.
        token: SessionToken,
    },
    /// Refresh idle deadline for an existing session.
    Touch {
        /// Session to refresh.
        token: SessionToken,
    },
    /// Release an existing session.
    Logout {
        /// Session to release.
        token: SessionToken,
    },
    /// Read one shard's operational snapshot.
    Stats,
}

#[derive(Debug)]
struct SessionRow {
    #[allow(dead_code)]
    user_id: UserId,
    last_touched_at: Instant,
}

/// One per-shard bucket. Each shard hosts exactly one of these.
pub struct SessionBucket {
    config: RunConfig,
    rows: HashMap<SessionToken, SessionRow>,
    sweeping: bool,
    stats: BucketStats,
}

#[tina_runtime::isolate(
    event = SessionAuthEvent,
    request = SessionAuthRequest,
    reply = SessionAuthReply,
    shard = AuthShard
)]
impl SessionBucket {
    fn handle_event(
        &mut self,
        event: SessionAuthEvent,
        ctx: &mut Context<'_, AuthShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            SessionAuthEvent::Bootstrap => self.start_sweep(),
            SessionAuthEvent::Sweep { result } => self.sweep(result, ctx.now()),
        }
    }

    fn handle_request(
        &mut self,
        request: SessionAuthRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            SessionAuthRequest::Login { user_id, token } => {
                self.login(user_id, token, call.now(), call)
            }
            SessionAuthRequest::Touch { token } => self.touch(token, call.now(), call),
            SessionAuthRequest::Logout { token } => self.logout(token, call),
            SessionAuthRequest::Stats => call.reply(SessionAuthReply::Stats(self.snapshot())),
        }
    }
}

impl SessionBucket {
    /// Build an empty bucket with the validated run config.
    pub fn new(config: RunConfig) -> Self {
        Self {
            config,
            rows: HashMap::new(),
            sweeping: false,
            stats: BucketStats::default(),
        }
    }

    fn start_sweep(&mut self) -> Effect<Self> {
        if self.sweeping {
            return noop();
        }
        self.sweeping = true;
        self.schedule_sweep()
    }

    fn schedule_sweep(&self) -> Effect<Self> {
        let interval = Duration::from_millis(self.config.sweep_interval_ms);
        sleep(interval).then_service_event(|result| SessionAuthEvent::Sweep { result })
    }

    fn login(
        &mut self,
        user_id: UserId,
        token: SessionToken,
        now: Instant,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        if self.rows.contains_key(&token) {
            self.stats.duplicate_rejects += 1;
            return call.reply(SessionAuthReply::AlreadyExists);
        }
        if self.rows.len() >= self.config.max_sessions_per_shard {
            self.stats.full_rejects += 1;
            return call.reply(SessionAuthReply::Full);
        }
        self.rows.insert(
            token.clone(),
            SessionRow {
                user_id,
                last_touched_at: now,
            },
        );
        self.stats.admitted += 1;
        self.stats.active = self.rows.len() as u64;
        if self.stats.active > self.stats.high_water {
            self.stats.high_water = self.stats.active;
        }
        call.reply(SessionAuthReply::Admitted { token })
    }

    fn touch(
        &mut self,
        token: SessionToken,
        now: Instant,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match self.rows.get_mut(&token) {
            Some(row) => {
                row.last_touched_at = now;
                self.stats.touch_ok += 1;
                call.reply(SessionAuthReply::Touched)
            }
            None => {
                self.stats.touch_not_found += 1;
                call.reply(SessionAuthReply::NotFound)
            }
        }
    }

    fn logout(&mut self, token: SessionToken, call: RequestCall<'_, Self>) -> RequestEffect<Self> {
        if self.rows.remove(&token).is_some() {
            self.stats.logged_out += 1;
            self.stats.active = self.rows.len() as u64;
            call.reply(SessionAuthReply::LoggedOut)
        } else {
            call.reply(SessionAuthReply::NotFound)
        }
    }

    fn sweep(&mut self, result: SleepReply, now: Instant) -> Effect<Self> {
        if let Err(error) = result {
            self.stats.timer_errors += 1;
            let _ = error;
            // Dependency failed; re-arm so the owner keeps a live sweep.
            return self.schedule_sweep();
        }
        self.stats.sweeps_run += 1;
        let idle = Duration::from_millis(self.config.idle_timeout_ms);
        let expired: Vec<SessionToken> = self
            .rows
            .iter()
            .filter(|(_, row)| now.duration_since(row.last_touched_at) >= idle)
            .map(|(t, _)| t.clone())
            .collect();
        for token in expired {
            self.rows.remove(&token);
            self.stats.idle_expired += 1;
        }
        self.stats.active = self.rows.len() as u64;
        self.schedule_sweep()
    }

    fn snapshot(&self) -> BucketStats {
        BucketStats {
            active: self.rows.len() as u64,
            ..self.stats
        }
    }

    /// Test-only view of private counters for owner-time unit proof.
    #[cfg(test)]
    fn stats_for_test(&self) -> BucketStats {
        self.snapshot()
    }

    /// Test-only direct login using a supplied owner time.
    #[cfg(test)]
    fn login_at_for_test(
        &mut self,
        user_id: UserId,
        token: SessionToken,
        now: Instant,
    ) -> SessionAuthReply {
        if self.rows.contains_key(&token) {
            self.stats.duplicate_rejects += 1;
            return SessionAuthReply::AlreadyExists;
        }
        if self.rows.len() >= self.config.max_sessions_per_shard {
            self.stats.full_rejects += 1;
            return SessionAuthReply::Full;
        }
        self.rows.insert(
            token.clone(),
            SessionRow {
                user_id,
                last_touched_at: now,
            },
        );
        self.stats.admitted += 1;
        self.stats.active = self.rows.len() as u64;
        if self.stats.active > self.stats.high_water {
            self.stats.high_water = self.stats.active;
        }
        SessionAuthReply::Admitted { token }
    }

    /// Test-only direct touch using a supplied owner time.
    #[cfg(test)]
    fn touch_at_for_test(&mut self, token: SessionToken, now: Instant) -> SessionAuthReply {
        match self.rows.get_mut(&token) {
            Some(row) => {
                row.last_touched_at = now;
                self.stats.touch_ok += 1;
                SessionAuthReply::Touched
            }
            None => {
                self.stats.touch_not_found += 1;
                SessionAuthReply::NotFound
            }
        }
    }

    /// Test-only direct sweep using a supplied owner time and timer result.
    #[cfg(test)]
    fn sweep_at_for_test(&mut self, result: SleepReply, now: Instant) {
        let _ = self.sweep(result, now);
    }
}

/// Multi-shard placement key used by `LocalMultiShardSystem`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AuthShard(pub u32);

impl Shard for AuthShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

type AuthApp = LocalMultiShardSystem<AuthShard, DefaultThreadedMailboxFactory>;
type AuthHandle =
    tina_runtime::SplitServiceHandle<SessionAuthEvent, SessionAuthRequest, SessionAuthReply>;
type AuthRequestAddr =
    tina::ServiceRequestAddress<SessionAuthEvent, SessionAuthRequest, SessionAuthReply>;

/// Host-side view. Wraps the multi-shard facade, placement, and per-shard handles.
struct AuthWorld<'a> {
    app: &'a AuthApp,
    placement: ShardPlacement,
    shard_ids: Vec<ShardId>,
    addrs_by_shard: BTreeMap<ShardId, AuthRequestAddr>,
    next_id: Cell<u64>,
    timeout: Duration,
}

/// Workload failure retaining exact host/runtime terminals.
#[derive(Debug)]
pub enum WorkloadError {
    /// Shard placement construction failed.
    Placement(ShardPlacementError),
    /// A per-shard bucket registration/bootstrap failed.
    Registration {
        /// Shard that failed to register.
        shard: u32,
        /// Exact bootstrap/registration failure.
        source: ThreadedRegisterBootstrapError<SessionAuthEvent>,
    },
    /// The host could not complete a runtime call operation.
    HostCall {
        /// Workload phase issuing the call.
        phase: &'static str,
        /// Exact runtime/control-plane failure.
        source: ThreadedRuntimeError,
    },
    /// The call completed with a domain-terminal outcome this phase cannot use.
    UnexpectedOutcome {
        /// Workload phase issuing the call.
        phase: &'static str,
        /// Complete Tina terminal outcome, including rejection reason.
        outcome: CallOutcome<SessionAuthReply>,
    },
    /// Domain reply did not match the phase expectation.
    UnexpectedReply {
        /// Workload phase issuing the call.
        phase: &'static str,
        /// Exact domain reply.
        reply: SessionAuthReply,
    },
}

impl fmt::Display for WorkloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Placement(error) => write!(f, "placement failed: {error}"),
            Self::Registration { shard, source } => {
                write!(f, "register bucket on shard {shard} failed: {source}")
            }
            Self::HostCall { phase, source } => write!(f, "{phase} host call failed: {source}"),
            Self::UnexpectedOutcome { phase, outcome } => {
                write!(f, "{phase} returned unexpected outcome {outcome:?}")
            }
            Self::UnexpectedReply { phase, reply } => {
                write!(f, "{phase} returned unexpected reply {reply:?}")
            }
        }
    }
}

impl Error for WorkloadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Placement(error) => Some(error),
            Self::Registration { source, .. } => Some(source),
            Self::HostCall { source, .. } => Some(source),
            Self::UnexpectedOutcome { .. } | Self::UnexpectedReply { .. } => None,
        }
    }
}

impl AsRef<dyn Error + Send + Sync + 'static> for WorkloadError {
    fn as_ref(&self) -> &(dyn Error + Send + Sync + 'static) {
        self
    }
}

/// Terminal error retained by consuming multi-shard shutdown.
pub type TerminalError = RunToShutdownError<ReportedWorkloadError<WorkloadError>>;

/// Top-level run failure with configuration, startup, and terminal truth intact.
#[derive(Debug)]
pub enum RunError {
    /// Configuration failed bounded preflight validation.
    InvalidConfig(RunConfigError),
    /// The multi-shard local system could not start.
    Startup(StartupError),
    /// Workload failure, shutdown failure, or both.
    Terminal(Box<TerminalError>),
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(error) => write!(f, "invalid session auth config: {error}"),
            Self::Startup(error) => write!(f, "session auth startup failed: {error}"),
            Self::Terminal(error) => write!(f, "session auth run failed: {error}"),
        }
    }
}

impl Error for RunError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidConfig(error) => Some(error),
            Self::Startup(error) => Some(error),
            Self::Terminal(error) => Some(error.as_ref()),
        }
    }
}

/// Build checked shard objects for the validated config.
pub fn auth_shards(config: RunConfig) -> Result<Vec<AuthShard>, RunConfigError> {
    let count = config.shard_count_u32()?;
    Ok((0..count).map(AuthShard).collect())
}

fn start_app(config: RunConfig) -> Result<AuthApp, RunError> {
    let config = config.validate().map_err(RunError::InvalidConfig)?;
    let shards = auth_shards(config).map_err(RunError::InvalidConfig)?;
    let mut builder = LocalSystem::multi_shard(DefaultThreadedMailboxFactory);
    for shard in shards {
        builder = builder.shard(shard);
    }
    builder.try_build().map_err(RunError::Startup)
}

fn run_local<T>(
    config: RunConfig,
    workload: impl FnOnce(&AuthApp, RunConfig) -> Result<T, WorkloadError>,
) -> Result<T, RunError> {
    let config = config.validate().map_err(RunError::InvalidConfig)?;
    let app = start_app(config)?;
    app.run_to_shutdown_reported(SHUTDOWN_TIMEOUT, |app| workload(app, config))
        .map_err(|error| RunError::Terminal(Box::new(error)))
}

impl<'a> AuthWorld<'a> {
    fn start(app: &'a AuthApp, config: RunConfig) -> Result<Self, WorkloadError> {
        let shard_count = config
            .shard_count_u32()
            .expect("run paths validate config before AuthWorld::start");
        let shard_ids: Vec<ShardId> = (0..shard_count).map(ShardId::new).collect();
        let placement = ShardPlacement::new("system_session_auth.placement", shard_ids.clone())
            .map_err(WorkloadError::Placement)?;
        let mut addrs_by_shard = BTreeMap::new();
        for shard_id in &shard_ids {
            let handle = register_bucket(app, *shard_id, config)?;
            addrs_by_shard.insert(*shard_id, handle.requests);
        }
        Ok(Self {
            app,
            placement,
            shard_ids,
            addrs_by_shard,
            next_id: Cell::new(1),
            timeout: Duration::from_millis(config.call_timeout_ms),
        })
    }

    fn mint_token(&self) -> SessionToken {
        let n = self.next_id.get();
        self.next_id.set(n + 1);
        SessionToken::try_new(format!("s-{n}")).expect("minted token is bounded")
    }

    fn addr_for(&self, token: &SessionToken) -> AuthRequestAddr {
        let shard = self.placement.owner_for_str(token.as_str());
        self.addrs_by_shard[&shard]
    }

    fn login(&self, user_id: UserId) -> Result<SessionAuthReply, WorkloadError> {
        let token = self.mint_token();
        let addr = self.addr_for(&token);
        expect_reply(
            "login",
            self.app.call_blocking_request(
                addr,
                SessionAuthRequest::Login {
                    user_id,
                    token: token.clone(),
                },
                self.timeout,
            ),
        )
    }

    fn touch(&self, token: SessionToken) -> Result<SessionAuthReply, WorkloadError> {
        let addr = self.addr_for(&token);
        expect_reply(
            "touch",
            self.app
                .call_blocking_request(addr, SessionAuthRequest::Touch { token }, self.timeout),
        )
    }

    fn logout(&self, token: SessionToken) -> Result<SessionAuthReply, WorkloadError> {
        let addr = self.addr_for(&token);
        expect_reply(
            "logout",
            self.app.call_blocking_request(
                addr,
                SessionAuthRequest::Logout { token },
                self.timeout,
            ),
        )
    }

    fn stats(&self) -> Result<SessionStats, WorkloadError> {
        let mut combined = SessionStats {
            active: 0,
            admitted: 0,
            logged_out: 0,
            idle_expired: 0,
            touch_ok: 0,
            touch_not_found: 0,
            full_rejects: 0,
            duplicate_rejects: 0,
            sweeps_run: 0,
            timer_errors: 0,
            per_shard_active: vec![0; self.shard_ids.len()],
            per_shard_admitted: vec![0; self.shard_ids.len()],
            per_shard_high_water: vec![0; self.shard_ids.len()],
        };
        for (idx, shard_id) in self.shard_ids.iter().enumerate() {
            let addr = self.addrs_by_shard[shard_id];
            let reply = expect_reply(
                "stats",
                self.app
                    .call_blocking_request(addr, SessionAuthRequest::Stats, self.timeout),
            )?;
            match reply {
                SessionAuthReply::Stats(s) => {
                    combined.active += s.active as usize;
                    combined.admitted += s.admitted;
                    combined.logged_out += s.logged_out;
                    combined.idle_expired += s.idle_expired;
                    combined.touch_ok += s.touch_ok;
                    combined.touch_not_found += s.touch_not_found;
                    combined.full_rejects += s.full_rejects;
                    combined.duplicate_rejects += s.duplicate_rejects;
                    combined.sweeps_run += s.sweeps_run;
                    combined.timer_errors += s.timer_errors;
                    combined.per_shard_active[idx] = s.active;
                    combined.per_shard_admitted[idx] = s.admitted;
                    combined.per_shard_high_water[idx] = s.high_water;
                }
                other => {
                    return Err(WorkloadError::UnexpectedReply {
                        phase: "stats",
                        reply: other,
                    });
                }
            }
        }
        Ok(combined)
    }
}

fn register_bucket(
    app: &AuthApp,
    shard: ShardId,
    config: RunConfig,
) -> Result<AuthHandle, WorkloadError> {
    app.register_split_service_with_bootstrap_on::<SessionBucket, SessionAuthEvent, SessionAuthRequest, Infallible>(
        shard,
        SessionBucket::new(config),
        config.session_mailbox,
        SessionAuthEvent::Bootstrap,
    )
    .map_err(|source| WorkloadError::Registration {
        shard: shard.get(),
        source,
    })
}

/// Map a host call result without collapsing terminal vocabulary.
pub fn expect_reply(
    phase: &'static str,
    result: Result<CallOutcome<SessionAuthReply>, ThreadedRuntimeError>,
) -> Result<SessionAuthReply, WorkloadError> {
    match result {
        Ok(CallOutcome::Replied(reply)) => Ok(reply),
        Ok(outcome) => Err(WorkloadError::UnexpectedOutcome { phase, outcome }),
        Err(source) => Err(WorkloadError::HostCall { phase, source }),
    }
}

/// Public runner: login/touch/logout, idle expiry, and overflow walks.
pub fn run(config: RunConfig) -> Result<RunReport, RunError> {
    Ok(RunReport {
        login_touch_logout: run_login_touch_logout(config)?,
        idle_expiry: run_idle_expiry(config)?,
        overflow: run_overflow(config)?,
    })
}

/// Login admits, touch refreshes, logout releases, follow-up touch is NotFound.
pub fn run_login_touch_logout(config: RunConfig) -> Result<LoginTouchLogoutReport, RunError> {
    run_local(config, |app, config| {
        let world = AuthWorld::start(app, config)?;
        let token = match world.login(UserId::try_new("alice").expect("script user is bounded"))? {
            SessionAuthReply::Admitted { token } => token,
            reply => {
                return Err(WorkloadError::UnexpectedReply {
                    phase: "login",
                    reply,
                });
            }
        };
        let touch_ok = matches!(world.touch(token.clone())?, SessionAuthReply::Touched);
        let logout_ok = matches!(world.logout(token.clone())?, SessionAuthReply::LoggedOut);
        let touch_after_logout = matches!(world.touch(token)?, SessionAuthReply::NotFound);
        let stats = world.stats()?;
        Ok(LoginTouchLogoutReport {
            login_ok: true,
            touch_ok,
            logout_ok,
            touch_after_logout_not_found: touch_after_logout,
            stats,
        })
    })
}

/// One admitted session idles out under the owner clock and sweep timer.
pub fn run_idle_expiry(config: RunConfig) -> Result<IdleExpiryReport, RunError> {
    run_local(config, |app, config| {
        let world = AuthWorld::start(app, config)?;
        let token = match world.login(UserId::try_new("bob").expect("script user is bounded"))? {
            SessionAuthReply::Admitted { token } => token,
            reply => {
                return Err(WorkloadError::UnexpectedReply {
                    phase: "login",
                    reply,
                });
            }
        };
        let idle_wait =
            Duration::from_millis(config.idle_timeout_ms + (3 * config.sweep_interval_ms) + 40);
        thread::sleep(idle_wait);

        let not_found = matches!(world.touch(token)?, SessionAuthReply::NotFound);
        let stats = world.stats()?;
        Ok(IdleExpiryReport {
            touch_after_idle_not_found: not_found,
            stats,
        })
    })
}

/// Per-bucket capacity rejects excess logins with exact `Full`.
pub fn run_overflow(config: RunConfig) -> Result<OverflowReport, RunError> {
    let mut config = config;
    config.shards = 1;
    config.max_sessions_per_shard = 4;
    run_local(config, |app, config| {
        let world = AuthWorld::start(app, config)?;
        let burst = config.max_sessions_per_shard + 5;
        let mut admitted = 0;
        let mut full = 0;
        for i in 0..burst {
            let user_id = UserId::try_new(format!("u-{i}")).expect("script user is bounded");
            match world.login(user_id)? {
                SessionAuthReply::Admitted { .. } => admitted += 1,
                SessionAuthReply::Full => full += 1,
                reply => {
                    return Err(WorkloadError::UnexpectedReply {
                        phase: "overflow_login",
                        reply,
                    });
                }
            }
        }
        let stats = world.stats()?;
        Ok(OverflowReport {
            admitted,
            full,
            stats,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tina::CallRejectedReason;
    use tina_runtime::CallError;

    #[test]
    fn validate_rejects_zero_and_oversized_before_startup() {
        assert!(matches!(
            RunConfig {
                shards: 0,
                ..RunConfig::default()
            }
            .validate(),
            Err(RunConfigError::Zero { field: "shards" })
        ));
        assert!(matches!(
            RunConfig {
                shards: MAX_SHARDS + 1,
                ..RunConfig::default()
            }
            .validate(),
            Err(RunConfigError::TooLarge {
                field: "shards",
                ..
            })
        ));
        assert!(matches!(
            RunConfig {
                max_sessions_per_shard: 0,
                ..RunConfig::default()
            }
            .validate(),
            Err(RunConfigError::Zero {
                field: "max_sessions_per_shard"
            })
        ));
        assert!(matches!(
            RunConfig {
                session_mailbox: MAX_SESSION_MAILBOX + 1,
                ..RunConfig::default()
            }
            .validate(),
            Err(RunConfigError::TooLarge {
                field: "session_mailbox",
                ..
            })
        ));
        assert!(matches!(
            RunConfig {
                call_timeout_ms: 0,
                ..RunConfig::default()
            }
            .validate(),
            Err(RunConfigError::Zero {
                field: "call_timeout_ms"
            })
        ));
    }

    #[test]
    fn checked_shard_conversion_builds_contiguous_ids() {
        let shards = auth_shards(RunConfig {
            shards: 3,
            ..RunConfig::default()
        })
        .expect("valid");
        assert_eq!(
            shards.iter().map(|s| s.id().get()).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert!(matches!(
            RunConfig {
                shards: usize::MAX,
                ..RunConfig::default()
            }
            .shard_count_u32(),
            Err(RunConfigError::TooLarge {
                field: "shards",
                ..
            }) | Err(RunConfigError::ShardConversion { .. })
        ));
    }

    #[test]
    fn owner_provided_time_drives_idle_expiry_without_wall_clock() {
        let now = Instant::now();
        let mut bucket = SessionBucket::new(RunConfig {
            shards: 1,
            max_sessions_per_shard: 4,
            idle_timeout_ms: 100,
            sweep_interval_ms: 10,
            session_mailbox: 8,
            call_timeout_ms: 1_000,
        });
        let token = SessionToken::try_new("t-1").unwrap();
        assert!(matches!(
            bucket.login_at_for_test(UserId::try_new("alice").unwrap(), token.clone(), now),
            SessionAuthReply::Admitted { .. }
        ));
        // Still inside idle window.
        bucket.sweep_at_for_test(Ok(()), now + Duration::from_millis(50));
        assert!(matches!(
            bucket.touch_at_for_test(token.clone(), now + Duration::from_millis(50)),
            SessionAuthReply::Touched
        ));
        // Past idle window under owner time.
        bucket.sweep_at_for_test(Ok(()), now + Duration::from_millis(200));
        assert!(matches!(
            bucket.touch_at_for_test(token, now + Duration::from_millis(200)),
            SessionAuthReply::NotFound
        ));
        let stats = bucket.stats_for_test();
        assert_eq!(stats.admitted, 1);
        assert_eq!(stats.idle_expired, 1);
        assert_eq!(stats.touch_ok, 1);
        assert_eq!(stats.touch_not_found, 1);
        assert_eq!(stats.sweeps_run, 2);
        assert_eq!(stats.timer_errors, 0);
    }

    #[test]
    fn timer_dependency_failure_is_counted_and_does_not_expire_rows() {
        let now = Instant::now();
        let mut bucket = SessionBucket::new(RunConfig {
            shards: 1,
            max_sessions_per_shard: 4,
            idle_timeout_ms: 10,
            sweep_interval_ms: 10,
            session_mailbox: 8,
            call_timeout_ms: 1_000,
        });
        let token = SessionToken::try_new("t-dep").unwrap();
        assert!(matches!(
            bucket.login_at_for_test(UserId::try_new("bob").unwrap(), token.clone(), now),
            SessionAuthReply::Admitted { .. }
        ));
        bucket.sweep_at_for_test(Err(CallError::TimerFull), now + Duration::from_secs(1));
        assert!(matches!(
            bucket.touch_at_for_test(token, now + Duration::from_secs(1)),
            SessionAuthReply::Touched
        ));
        let stats = bucket.stats_for_test();
        assert_eq!(stats.timer_errors, 1);
        assert_eq!(stats.idle_expired, 0);
        assert_eq!(stats.sweeps_run, 0);
    }

    #[test]
    fn host_terminal_vocabulary_is_retained_without_collapse() {
        let outcomes = [
            CallOutcome::Full,
            CallOutcome::Closed,
            CallOutcome::Timeout,
            CallOutcome::Rejected(CallRejectedReason::UnsupportedMessage),
        ];
        for outcome in outcomes {
            let error =
                expect_reply("probe", Ok(outcome.clone())).expect_err("must retain terminal");
            match error {
                WorkloadError::UnexpectedOutcome {
                    phase: "probe",
                    outcome: actual,
                } => match actual {
                    CallOutcome::Full
                    | CallOutcome::Closed
                    | CallOutcome::Timeout
                    | CallOutcome::Rejected(CallRejectedReason::UnsupportedMessage) => {}
                    other => panic!("collapsed outcome: {other:?}"),
                },
                other => panic!("wrong error: {other:?}"),
            }
        }

        let error = expect_reply("probe", Err(ThreadedRuntimeError::WorkerUnresponsive))
            .expect_err("host failure");
        assert!(matches!(
            error,
            WorkloadError::HostCall {
                phase: "probe",
                source: ThreadedRuntimeError::WorkerUnresponsive,
            }
        ));
    }

    #[test]
    fn capacity_full_is_exact_domain_overload() {
        let now = Instant::now();
        let mut bucket = SessionBucket::new(RunConfig {
            shards: 1,
            max_sessions_per_shard: 1,
            idle_timeout_ms: 1_000,
            sweep_interval_ms: 100,
            session_mailbox: 8,
            call_timeout_ms: 1_000,
        });
        assert!(matches!(
            bucket.login_at_for_test(
                UserId::try_new("a").unwrap(),
                SessionToken::try_new("1").unwrap(),
                now,
            ),
            SessionAuthReply::Admitted { .. }
        ));
        assert_eq!(
            bucket.login_at_for_test(
                UserId::try_new("b").unwrap(),
                SessionToken::try_new("2").unwrap(),
                now,
            ),
            SessionAuthReply::Full
        );
        assert_eq!(bucket.stats_for_test().full_rejects, 1);
        assert_eq!(bucket.stats_for_test().active, 1);
    }

    #[test]
    fn request_identities_are_bounded_before_mailbox_construction() {
        assert!(matches!(
            UserId::try_new(""),
            Err(SessionInputError::Empty { field: "user_id" })
        ));
        assert!(UserId::try_new("u".repeat(MAX_USER_ID_BYTES)).is_ok());
        assert!(matches!(
            UserId::try_new("u".repeat(MAX_USER_ID_BYTES + 1)),
            Err(SessionInputError::TooLong {
                field: "user_id",
                actual_bytes,
                max_bytes: MAX_USER_ID_BYTES,
            }) if actual_bytes == MAX_USER_ID_BYTES + 1
        ));
        assert!(SessionToken::try_new("t".repeat(MAX_SESSION_TOKEN_BYTES)).is_ok());
        assert!(matches!(
            SessionToken::try_new("t".repeat(MAX_SESSION_TOKEN_BYTES + 1)),
            Err(SessionInputError::TooLong {
                field: "session_token",
                actual_bytes,
                max_bytes: MAX_SESSION_TOKEN_BYTES,
            }) if actual_bytes == MAX_SESSION_TOKEN_BYTES + 1
        ));

        let user_boundary = "é".repeat(MAX_USER_ID_BYTES / "é".len());
        let user = UserId::try_new(&user_boundary).expect("exact UTF-8 byte boundary");
        assert_eq!(user.as_str(), user_boundary);
        assert!(matches!(
            UserId::try_new(format!("{user_boundary}x")),
            Err(SessionInputError::TooLong {
                actual_bytes,
                max_bytes: MAX_USER_ID_BYTES,
                ..
            }) if actual_bytes == MAX_USER_ID_BYTES + 1
        ));

        let token_boundary = "é".repeat(MAX_SESSION_TOKEN_BYTES / "é".len());
        let token = SessionToken::try_new(&token_boundary).expect("exact UTF-8 byte boundary");
        assert_eq!(token.as_str(), token_boundary);
        assert!(matches!(
            SessionToken::try_new(format!("{token_boundary}x")),
            Err(SessionInputError::TooLong {
                actual_bytes,
                max_bytes: MAX_SESSION_TOKEN_BYTES,
                ..
            }) if actual_bytes == MAX_SESSION_TOKEN_BYTES + 1
        ));
    }

    #[test]
    fn duplicate_token_never_overwrites_or_increments_admission() {
        let now = Instant::now();
        let mut bucket = SessionBucket::new(RunConfig {
            shards: 1,
            max_sessions_per_shard: 1,
            ..RunConfig::default()
        });
        let token = SessionToken::try_new("same").unwrap();
        assert!(matches!(
            bucket.login_at_for_test(UserId::try_new("alice").unwrap(), token.clone(), now),
            SessionAuthReply::Admitted { .. }
        ));
        assert_eq!(
            bucket.login_at_for_test(
                UserId::try_new("mallory").unwrap(),
                token,
                now + Duration::from_secs(1),
            ),
            SessionAuthReply::AlreadyExists
        );
        assert_eq!(
            bucket.login_at_for_test(
                UserId::try_new("bob").unwrap(),
                SessionToken::try_new("new").unwrap(),
                now + Duration::from_secs(1),
            ),
            SessionAuthReply::Full
        );
        let stats = bucket.stats_for_test();
        assert_eq!(stats.active, 1);
        assert_eq!(stats.admitted, 1);
        assert_eq!(stats.full_rejects, 1);
        assert_eq!(stats.duplicate_rejects, 1);
    }
}
