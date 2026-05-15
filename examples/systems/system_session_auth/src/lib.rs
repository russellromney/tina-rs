//! `system_session_auth` — sharded session table with a recurring expiry
//! sweep.
//!
//! What this specimen pulls on:
//!
//! - `ShardPlacement` to route sessions into one of N buckets. The buckets
//!   live inside a single isolate (`ThreadedMultiShardRuntime` is missing
//!   a host `call_blocking`; see Findings).
//! - Runtime-owned `sleep_then` for the periodic expiry sweep. The sweep
//!   reschedules itself on every tick — that is the "recurring timer."
//! - `CallContext` for `Login` / `Touch` / `Logout` / `Stats` caller
//!   authority.
//! - `ThreadedRuntime::call_blocking` for host-driven scenarios.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use tina::{CallContext, prelude::*};
use tina_runtime::sharded::ShardPlacement;
use tina_runtime::{CallOutcome, DefaultThreadedMailboxFactory, ThreadedRuntime, sleep_then};

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

/// Operational snapshot. Every cap is reported so a smoke test can assert
/// on it without reaching into internals.
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

/// Opaque session handle returned by `Login`. Callers pass this back on
/// `Touch` / `Logout`. The string is deterministic for tests.
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
    /// The owning shard bucket was at capacity. Caller must retry later.
    Full,
    Stats(SessionStats),
}

#[derive(Debug)]
pub enum SessionAuthMsg {
    /// One-shot kick from the host to start the recurring sweep timer.
    /// Sent automatically by `register_auth`.
    Bootstrap,
    Login {
        user_id: String,
    },
    Touch {
        token: SessionToken,
    },
    Logout {
        token: SessionToken,
    },
    Stats,
    /// Internal sweep tick. Hosts should not send this directly; it lands
    /// here via `sleep_then` and reschedules itself.
    Sweep,
}

#[derive(Debug)]
struct SessionRow {
    #[allow(dead_code)] // carried so the table is realistic, not used by stats yet.
    user_id: String,
    last_touched_at: Instant,
}

struct SessionAuth {
    config: RunConfig,
    placement: ShardPlacement,
    shard_to_idx: HashMap<ShardId, usize>,
    buckets: Vec<HashMap<SessionToken, SessionRow>>,
    next_id: u64,
    sweeping: bool,
    admitted: u64,
    logged_out: u64,
    idle_expired: u64,
    touch_ok: u64,
    touch_not_found: u64,
    full_rejects: u64,
    sweeps_run: u64,
    per_shard_admitted: Vec<u64>,
    per_shard_high_water: Vec<u64>,
}

#[tina_runtime::isolate(message = SessionAuthMsg, reply = SessionAuthReply)]
impl SessionAuth {
    fn handle(
        &mut self,
        msg: SessionAuthMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            SessionAuthMsg::Bootstrap => self.start_sweep(),
            SessionAuthMsg::Sweep => self.sweep(),
            // Request/reply variants are answered in `handle_call`. If a
            // caller mis-routes them through `try_send` we drop them.
            SessionAuthMsg::Login { .. }
            | SessionAuthMsg::Touch { .. }
            | SessionAuthMsg::Logout { .. }
            | SessionAuthMsg::Stats => noop(),
        }
    }

    fn handle_call(&mut self, msg: SessionAuthMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            SessionAuthMsg::Login { user_id } => self.login(user_id, call),
            SessionAuthMsg::Touch { token } => self.touch(token, call),
            SessionAuthMsg::Logout { token } => self.logout(token, call),
            SessionAuthMsg::Stats => call.reply(SessionAuthReply::Stats(self.stats())),
            SessionAuthMsg::Bootstrap | SessionAuthMsg::Sweep => {
                call.reject(tina::CallRejectedReason::UnsupportedMessage)
            }
        }
    }
}

impl SessionAuth {
    fn new(config: RunConfig) -> anyhow::Result<Self> {
        if config.shards == 0 {
            anyhow::bail!("shards must be >= 1");
        }
        let shard_ids: Vec<ShardId> = (0..config.shards as u32).map(ShardId::new).collect();
        let placement = ShardPlacement::new("system_session_auth.placement", shard_ids.clone())
            .map_err(|e| anyhow::anyhow!("placement: {e:?}"))?;
        let shard_to_idx = shard_ids
            .iter()
            .copied()
            .enumerate()
            .map(|(i, s)| (s, i))
            .collect();
        Ok(Self {
            config,
            placement,
            shard_to_idx,
            buckets: (0..config.shards).map(|_| HashMap::new()).collect(),
            next_id: 1,
            sweeping: false,
            admitted: 0,
            logged_out: 0,
            idle_expired: 0,
            touch_ok: 0,
            touch_not_found: 0,
            full_rejects: 0,
            sweeps_run: 0,
            per_shard_admitted: vec![0; config.shards],
            per_shard_high_water: vec![0; config.shards],
        })
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
            SessionAuthMsg::Sweep,
        )
    }

    fn bucket_for(&self, token: &SessionToken) -> usize {
        let owner = self.placement.owner_for_str(&token.0);
        *self
            .shard_to_idx
            .get(&owner)
            .expect("placement returned a shard id we registered")
    }

    fn login(&mut self, user_id: String, call: CallContext<'_, Self>) -> Effect<Self> {
        let token = SessionToken(format!("s-{}", self.next_id));
        self.next_id += 1;
        let idx = self.bucket_for(&token);
        let bucket = &mut self.buckets[idx];
        if bucket.len() >= self.config.max_sessions_per_shard {
            self.full_rejects += 1;
            return call.reply(SessionAuthReply::Full);
        }
        bucket.insert(
            token.clone(),
            SessionRow {
                user_id,
                last_touched_at: Instant::now(),
            },
        );
        self.admitted += 1;
        self.per_shard_admitted[idx] += 1;
        let depth = bucket.len() as u64;
        if depth > self.per_shard_high_water[idx] {
            self.per_shard_high_water[idx] = depth;
        }
        call.reply(SessionAuthReply::Admitted { token })
    }

    fn touch(&mut self, token: SessionToken, call: CallContext<'_, Self>) -> Effect<Self> {
        let idx = self.bucket_for(&token);
        let bucket = &mut self.buckets[idx];
        match bucket.get_mut(&token) {
            Some(row) => {
                row.last_touched_at = Instant::now();
                self.touch_ok += 1;
                call.reply(SessionAuthReply::Touched)
            }
            None => {
                self.touch_not_found += 1;
                call.reply(SessionAuthReply::NotFound)
            }
        }
    }

    fn logout(&mut self, token: SessionToken, call: CallContext<'_, Self>) -> Effect<Self> {
        let idx = self.bucket_for(&token);
        let bucket = &mut self.buckets[idx];
        if bucket.remove(&token).is_some() {
            self.logged_out += 1;
            call.reply(SessionAuthReply::LoggedOut)
        } else {
            call.reply(SessionAuthReply::NotFound)
        }
    }

    fn sweep(&mut self) -> Effect<Self> {
        self.sweeps_run += 1;
        let now = Instant::now();
        let idle = Duration::from_millis(self.config.idle_timeout_ms);
        for bucket in &mut self.buckets {
            let expired: Vec<SessionToken> = bucket
                .iter()
                .filter(|(_, row)| now.duration_since(row.last_touched_at) >= idle)
                .map(|(t, _)| t.clone())
                .collect();
            for token in expired {
                bucket.remove(&token);
                self.idle_expired += 1;
            }
        }
        self.schedule_sweep()
    }

    fn stats(&self) -> SessionStats {
        let per_shard_active: Vec<u64> = self.buckets.iter().map(|b| b.len() as u64).collect();
        let active = per_shard_active.iter().copied().sum::<u64>() as usize;
        SessionStats {
            active,
            admitted: self.admitted,
            logged_out: self.logged_out,
            idle_expired: self.idle_expired,
            touch_ok: self.touch_ok,
            touch_not_found: self.touch_not_found,
            full_rejects: self.full_rejects,
            sweeps_run: self.sweeps_run,
            per_shard_active,
            per_shard_admitted: self.per_shard_admitted.clone(),
            per_shard_high_water: self.per_shard_high_water.clone(),
        }
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
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));
    let auth = register_auth(&runtime, config)?;
    let timeout = Duration::from_millis(config.call_timeout_ms);

    let token = match runtime.call_blocking(
        auth,
        SessionAuthMsg::Login {
            user_id: "alice".into(),
        },
        timeout,
    )? {
        CallOutcome::Replied(SessionAuthReply::Admitted { token }) => token,
        other => anyhow::bail!("login: {other:?}"),
    };

    let touch_ok = matches!(
        runtime.call_blocking(
            auth,
            SessionAuthMsg::Touch {
                token: token.clone(),
            },
            timeout,
        )?,
        CallOutcome::Replied(SessionAuthReply::Touched)
    );
    let logout_ok = matches!(
        runtime.call_blocking(
            auth,
            SessionAuthMsg::Logout {
                token: token.clone(),
            },
            timeout,
        )?,
        CallOutcome::Replied(SessionAuthReply::LoggedOut)
    );
    let touch_after_logout = matches!(
        runtime.call_blocking(auth, SessionAuthMsg::Touch { token }, timeout)?,
        CallOutcome::Replied(SessionAuthReply::NotFound)
    );

    let stats = stats(&runtime, auth)?;
    shutdown(runtime);
    Ok(LoginTouchLogoutReport {
        login_ok: true,
        touch_ok,
        logout_ok,
        touch_after_logout_not_found: touch_after_logout,
        stats,
    })
}

pub fn run_idle_expiry(config: RunConfig) -> anyhow::Result<IdleExpiryReport> {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));
    let auth = register_auth(&runtime, config)?;
    let timeout = Duration::from_millis(config.call_timeout_ms);

    let token = match runtime.call_blocking(
        auth,
        SessionAuthMsg::Login {
            user_id: "bob".into(),
        },
        timeout,
    )? {
        CallOutcome::Replied(SessionAuthReply::Admitted { token }) => token,
        other => anyhow::bail!("login: {other:?}"),
    };

    // Sleep long enough that idle_timeout has elapsed AND the recurring
    // sweep has run at least twice past that point.
    let idle_wait =
        Duration::from_millis(config.idle_timeout_ms + (3 * config.sweep_interval_ms) + 40);
    thread::sleep(idle_wait);

    let not_found = matches!(
        runtime.call_blocking(auth, SessionAuthMsg::Touch { token }, timeout)?,
        CallOutcome::Replied(SessionAuthReply::NotFound)
    );
    let stats = stats(&runtime, auth)?;
    shutdown(runtime);
    Ok(IdleExpiryReport {
        touch_after_idle_not_found: not_found,
        stats,
    })
}

pub fn run_overflow(config: RunConfig) -> anyhow::Result<OverflowReport> {
    // Collapse the cap so overflow is deterministic regardless of how
    // tokens hash across buckets. The cap-reporting contract is the same.
    let mut config = config;
    config.shards = 1;
    config.max_sessions_per_shard = 4;

    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));
    let auth = register_auth(&runtime, config)?;
    let timeout = Duration::from_millis(config.call_timeout_ms);

    let burst = config.max_sessions_per_shard + 5;
    let mut admitted = 0;
    let mut full = 0;
    for i in 0..burst {
        let outcome = runtime.call_blocking(
            auth,
            SessionAuthMsg::Login {
                user_id: format!("u-{i}"),
            },
            timeout,
        )?;
        match outcome {
            CallOutcome::Replied(SessionAuthReply::Admitted { .. }) => admitted += 1,
            CallOutcome::Replied(SessionAuthReply::Full) => full += 1,
            other => anyhow::bail!("login: {other:?}"),
        }
    }

    let stats = stats(&runtime, auth)?;
    shutdown(runtime);
    Ok(OverflowReport {
        admitted,
        full,
        stats,
    })
}

fn register_auth(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
    config: RunConfig,
) -> anyhow::Result<Address<SessionAuthMsg, SessionAuthReply>> {
    let address = runtime
        .register_with_capacity::<SessionAuth, Infallible>(
            SessionAuth::new(config)?,
            config.session_mailbox,
        )
        .map_err(|e| anyhow::anyhow!("register: {e:?}"))?;
    runtime
        .try_send(address, SessionAuthMsg::Bootstrap)
        .map_err(|e| anyhow::anyhow!("bootstrap: {e:?}"))?;
    Ok(address)
}

fn stats(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
    auth: Address<SessionAuthMsg, SessionAuthReply>,
) -> anyhow::Result<SessionStats> {
    match runtime.call_blocking(auth, SessionAuthMsg::Stats, Duration::from_secs(1))? {
        CallOutcome::Replied(SessionAuthReply::Stats(s)) => Ok(s),
        other => anyhow::bail!("stats failed: {other:?}"),
    }
}

fn shutdown(runtime: Arc<ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>>) {
    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}
