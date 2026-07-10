//! `system_session_auth` — sharded session table with a recurring expiry
//! sweep, hosted on a real `ThreadedMultiShardRuntime`.
//!
//! What this specimen pulls on:
//!
//! - One per-shard `SessionBucket` isolate per shard. No router or
//!   tracker isolate; the host routes by `ShardPlacement` and calls each
//!   bucket directly through `ThreadedMultiShardRuntime::call_blocking_request`.
//! - Runtime-owned `sleep_then` for the periodic expiry sweep. The sweep
//!   reschedules itself on every tick — that is the "recurring timer."
//! - `CallContext` for `Login` / `Touch` / `Logout` / `Stats` caller
//!   authority.
//! - `ThreadedMultiShardRuntime::call_blocking_request` for host-driven
//!   scenarios on a chosen live shard (no per-test driver isolate).

use std::collections::{BTreeMap, HashMap};
use std::convert::Infallible;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use tina::prelude::*;
use tina_runtime::sharded::ShardPlacement;
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, ThreadedMultiShardRuntime, sleep_then,
};

/// Tunables for one specimen run.
#[derive(Debug, Clone, Copy)]
pub struct RunConfig {
    pub shards: usize,
    pub max_sessions_per_shard: usize,
    pub idle_timeout_ms: u64,
    pub sweep_interval_ms: u64,
    pub session_mailbox: usize,
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

/// Aggregate report for one specimen run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunReport {
    pub login_touch_logout: LoginTouchLogoutReport,
    pub idle_expiry: IdleExpiryReport,
    pub overflow: OverflowReport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginTouchLogoutReport {
    pub login_ok: bool,
    pub touch_ok: bool,
    pub logout_ok: bool,
    pub touch_after_logout_not_found: bool,
    pub stats: SessionStats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdleExpiryReport {
    pub touch_after_idle_not_found: bool,
    pub stats: SessionStats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverflowReport {
    pub admitted: usize,
    pub full: usize,
    pub stats: SessionStats,
}

/// Operational snapshot, aggregated across every shard the host owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionStats {
    pub active: usize,
    pub admitted: u64,
    pub logged_out: u64,
    pub idle_expired: u64,
    pub touch_ok: u64,
    pub touch_not_found: u64,
    pub full_rejects: u64,
    pub sweeps_run: u64,
    pub per_shard_active: Vec<u64>,
    pub per_shard_admitted: Vec<u64>,
    pub per_shard_high_water: Vec<u64>,
}

/// Opaque session handle. Encodes nothing the host couldn't compute.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct SessionToken(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionAuthReply {
    Admitted {
        token: SessionToken,
    },
    Touched,
    LoggedOut,
    /// Session was logged out, expired, or never existed. We do not leak
    /// which to the caller.
    NotFound,
    /// The owning shard bucket was at capacity. Caller must retry later
    /// (potentially on a different shard for a new login).
    Full,
    Stats(BucketStats),
}

/// One shard's slice of stats. Aggregated by the host into [`SessionStats`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BucketStats {
    pub active: u64,
    pub admitted: u64,
    pub logged_out: u64,
    pub idle_expired: u64,
    pub touch_ok: u64,
    pub touch_not_found: u64,
    pub full_rejects: u64,
    pub sweeps_run: u64,
    pub high_water: u64,
}

/// Fire-and-forget facts a bucket accepts: bootstrap and the recurring
/// sweep tick. Neither carries caller authority.
#[derive(Debug)]
pub enum SessionAuthEvent {
    /// One-shot kick from the host to start the recurring sweep timer.
    Bootstrap,
    /// Internal sweep tick. Reschedules itself.
    Sweep,
}

/// Caller-authority requests the host can ask a bucket.
#[derive(Debug)]
pub enum SessionAuthRequest {
    /// Host-supplied token; the bucket records it and replies with the
    /// admission result. The host is responsible for routing this call
    /// to the shard the token hashes to.
    Login {
        user_id: String,
        token: SessionToken,
    },
    Touch {
        token: SessionToken,
    },
    Logout {
        token: SessionToken,
    },
    Stats,
}

/// Split-service envelope for [`SessionBucket`].
pub type SessionAuthMsg = tina::ServiceMessage<SessionAuthEvent, SessionAuthRequest>;

#[derive(Debug)]
struct SessionRow {
    #[allow(dead_code)]
    user_id: String,
    last_touched_at: Instant,
}

/// One per-shard bucket. Each shard hosts exactly one of these.
struct SessionBucket {
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
        _ctx: &mut Context<'_, AuthShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            SessionAuthEvent::Bootstrap => self.start_sweep(),
            SessionAuthEvent::Sweep => self.sweep(),
        }
    }

    fn handle_request(
        &mut self,
        request: SessionAuthRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            SessionAuthRequest::Login { user_id, token } => self.login(user_id, token, call),
            SessionAuthRequest::Touch { token } => self.touch(token, call),
            SessionAuthRequest::Logout { token } => self.logout(token, call),
            SessionAuthRequest::Stats => call.reply(SessionAuthReply::Stats(self.snapshot())),
        }
    }
}

impl SessionBucket {
    fn new(config: RunConfig) -> Self {
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
        sleep_then(
            Duration::from_millis(self.config.sweep_interval_ms),
            SessionAuthMsg::Event(SessionAuthEvent::Sweep),
        )
    }

    fn login(
        &mut self,
        user_id: String,
        token: SessionToken,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        if self.rows.len() >= self.config.max_sessions_per_shard {
            self.stats.full_rejects += 1;
            return call.reply(SessionAuthReply::Full);
        }
        self.rows.insert(
            token.clone(),
            SessionRow {
                user_id,
                last_touched_at: Instant::now(),
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
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match self.rows.get_mut(&token) {
            Some(row) => {
                row.last_touched_at = Instant::now();
                self.stats.touch_ok += 1;
                call.reply(SessionAuthReply::Touched)
            }
            None => {
                self.stats.touch_not_found += 1;
                call.reply(SessionAuthReply::NotFound)
            }
        }
    }

    fn logout(
        &mut self,
        token: SessionToken,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        if self.rows.remove(&token).is_some() {
            self.stats.logged_out += 1;
            self.stats.active = self.rows.len() as u64;
            call.reply(SessionAuthReply::LoggedOut)
        } else {
            call.reply(SessionAuthReply::NotFound)
        }
    }

    fn sweep(&mut self) -> Effect<Self> {
        self.stats.sweeps_run += 1;
        let now = Instant::now();
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
}

/// Multi-shard wrapper around `AuthShard` used by
/// `ThreadedMultiShardRuntime`.
#[derive(Clone, Copy, Debug)]
pub struct AuthShard(pub u32);

impl Shard for AuthShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

type AuthRuntime = ThreadedMultiShardRuntime<AuthShard, DefaultThreadedMailboxFactory>;
type AuthAddr = tina::ServiceRequestAddress<SessionAuthEvent, SessionAuthRequest, SessionAuthReply>;

/// Host-side view. Wraps the multi-shard runtime, the placement map
/// from token text to shard id, and one bucket address per shard.
struct AuthWorld {
    runtime: AuthRuntime,
    placement: ShardPlacement,
    shard_ids: Vec<ShardId>,
    addrs_by_shard: BTreeMap<ShardId, AuthAddr>,
    next_id: AtomicU64,
    timeout: Duration,
}

impl AuthWorld {
    fn start(config: RunConfig) -> anyhow::Result<Self> {
        if config.shards == 0 {
            anyhow::bail!("shards must be >= 1");
        }
        let shard_objs: Vec<AuthShard> = (0..config.shards as u32).map(AuthShard).collect();
        let shard_ids: Vec<ShardId> = shard_objs.iter().map(|s| s.id()).collect();
        let placement = ShardPlacement::new("system_session_auth.placement", shard_ids.clone())
            .map_err(|e| anyhow::anyhow!("placement: {e:?}"))?;
        let runtime = AuthRuntime::new(shard_objs, DefaultThreadedMailboxFactory);
        let mut addrs_by_shard = BTreeMap::new();
        for shard_id in &shard_ids {
            let addr = runtime
                .register_with_capacity_and_bootstrap_on::<SessionBucket, Infallible>(
                    *shard_id,
                    SessionBucket::new(config),
                    config.session_mailbox,
                    SessionAuthMsg::Event(SessionAuthEvent::Bootstrap),
                )
                .map_err(|e| anyhow::anyhow!("register on shard {}: {e:?}", shard_id.get()))?;
            addrs_by_shard.insert(
                *shard_id,
                tina::ServiceRequestAddress::from_call_address(addr.callable()),
            );
        }
        Ok(Self {
            runtime,
            placement,
            shard_ids,
            addrs_by_shard,
            next_id: AtomicU64::new(1),
            timeout: Duration::from_millis(config.call_timeout_ms),
        })
    }

    fn mint_token(&self) -> SessionToken {
        let n = self.next_id.fetch_add(1, Ordering::Relaxed);
        SessionToken(format!("s-{n}"))
    }

    fn addr_for(&self, token: &SessionToken) -> AuthAddr {
        let shard = self.placement.owner_for_str(&token.0);
        self.addrs_by_shard[&shard]
    }

    fn login(&self, user_id: String) -> anyhow::Result<SessionAuthReply> {
        let token = self.mint_token();
        let addr = self.addr_for(&token);
        match self.runtime.call_blocking_request(
            addr,
            SessionAuthRequest::Login {
                user_id,
                token: token.clone(),
            },
            self.timeout,
        )? {
            CallOutcome::Replied(r) => Ok(r),
            other => anyhow::bail!("login outcome: {other:?}"),
        }
    }

    fn touch(&self, token: SessionToken) -> anyhow::Result<SessionAuthReply> {
        let addr = self.addr_for(&token);
        match self.runtime.call_blocking_request(
            addr,
            SessionAuthRequest::Touch { token },
            self.timeout,
        )? {
            CallOutcome::Replied(r) => Ok(r),
            other => anyhow::bail!("touch outcome: {other:?}"),
        }
    }

    fn logout(&self, token: SessionToken) -> anyhow::Result<SessionAuthReply> {
        let addr = self.addr_for(&token);
        match self.runtime.call_blocking_request(
            addr,
            SessionAuthRequest::Logout { token },
            self.timeout,
        )? {
            CallOutcome::Replied(r) => Ok(r),
            other => anyhow::bail!("logout outcome: {other:?}"),
        }
    }

    fn stats(&self) -> anyhow::Result<SessionStats> {
        let mut combined = SessionStats {
            active: 0,
            admitted: 0,
            logged_out: 0,
            idle_expired: 0,
            touch_ok: 0,
            touch_not_found: 0,
            full_rejects: 0,
            sweeps_run: 0,
            per_shard_active: vec![0; self.shard_ids.len()],
            per_shard_admitted: vec![0; self.shard_ids.len()],
            per_shard_high_water: vec![0; self.shard_ids.len()],
        };
        for (idx, shard_id) in self.shard_ids.iter().enumerate() {
            let addr = self.addrs_by_shard[shard_id];
            match self
                .runtime
                .call_blocking_request(addr, SessionAuthRequest::Stats, self.timeout)?
            {
                CallOutcome::Replied(SessionAuthReply::Stats(s)) => {
                    combined.active += s.active as usize;
                    combined.admitted += s.admitted;
                    combined.logged_out += s.logged_out;
                    combined.idle_expired += s.idle_expired;
                    combined.touch_ok += s.touch_ok;
                    combined.touch_not_found += s.touch_not_found;
                    combined.full_rejects += s.full_rejects;
                    combined.sweeps_run += s.sweeps_run;
                    combined.per_shard_active[idx] = s.active;
                    combined.per_shard_admitted[idx] = s.admitted;
                    combined.per_shard_high_water[idx] = s.high_water;
                }
                other => anyhow::bail!("stats outcome: {other:?}"),
            }
        }
        Ok(combined)
    }

    /// Tear down through the cloneable shutdown handle so this specimen
    /// does not need `Arc::try_unwrap(runtime)` ceremony.
    fn shutdown(self) -> anyhow::Result<()> {
        let _ = self.runtime.shutdown();
        Ok(())
    }
}

pub fn run(config: RunConfig) -> anyhow::Result<RunReport> {
    Ok(RunReport {
        login_touch_logout: run_login_touch_logout(config)?,
        idle_expiry: run_idle_expiry(config)?,
        overflow: run_overflow(config)?,
    })
}

pub fn run_login_touch_logout(config: RunConfig) -> anyhow::Result<LoginTouchLogoutReport> {
    let world = AuthWorld::start(config)?;
    let token = match world.login("alice".into())? {
        SessionAuthReply::Admitted { token } => token,
        other => anyhow::bail!("login: {other:?}"),
    };
    let touch_ok = matches!(world.touch(token.clone())?, SessionAuthReply::Touched);
    let logout_ok = matches!(world.logout(token.clone())?, SessionAuthReply::LoggedOut);
    let touch_after_logout = matches!(world.touch(token)?, SessionAuthReply::NotFound);

    let stats = world.stats()?;
    world.shutdown()?;
    Ok(LoginTouchLogoutReport {
        login_ok: true,
        touch_ok,
        logout_ok,
        touch_after_logout_not_found: touch_after_logout,
        stats,
    })
}

pub fn run_idle_expiry(config: RunConfig) -> anyhow::Result<IdleExpiryReport> {
    let world = AuthWorld::start(config)?;
    let token = match world.login("bob".into())? {
        SessionAuthReply::Admitted { token } => token,
        other => anyhow::bail!("login: {other:?}"),
    };
    let idle_wait =
        Duration::from_millis(config.idle_timeout_ms + (3 * config.sweep_interval_ms) + 40);
    thread::sleep(idle_wait);

    let not_found = matches!(world.touch(token)?, SessionAuthReply::NotFound);
    let stats = world.stats()?;
    world.shutdown()?;
    Ok(IdleExpiryReport {
        touch_after_idle_not_found: not_found,
        stats,
    })
}

pub fn run_overflow(config: RunConfig) -> anyhow::Result<OverflowReport> {
    let mut config = config;
    config.shards = 1;
    config.max_sessions_per_shard = 4;

    let world = AuthWorld::start(config)?;
    let burst = config.max_sessions_per_shard + 5;
    let mut admitted = 0;
    let mut full = 0;
    for i in 0..burst {
        match world.login(format!("u-{i}"))? {
            SessionAuthReply::Admitted { .. } => admitted += 1,
            SessionAuthReply::Full => full += 1,
            other => anyhow::bail!("login: {other:?}"),
        }
    }
    let stats = world.stats()?;
    world.shutdown()?;
    Ok(OverflowReport {
        admitted,
        full,
        stats,
    })
}
