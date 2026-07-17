//! Bounded, caller-owned retry against a flaky downstream — with
//! idempotency named in the message, not inferred by a helper.
//!
//! This is the small companion to the admission policy layer:
//! when a downstream replies `Full`, the relay does **not** retry on its
//! own. It consults an explicit [`tina_runtime::FullHandling`] budget
//! built from a [`tina::time::Backoff`], and only because the *caller*
//! stamped the request with an `idempotency_key` is re-sending the same
//! request safe. The key is a real field on the message; nothing infers
//! "this is retryable" behind the caller's back.
//!
//! What this teaches:
//!
//! - **Retry is a named budget, not a default.** `FullHandling::retry_backoff`
//!   requires an explicit `Backoff`; there is no implicit retry.
//! - **Idempotency is the caller's claim.** `Deliver { idempotency_key }`
//!   carries the key; the downstream dedups on it, so a retry is the same
//!   logical operation, never a double-charge.
//! - **Exhaustion is visible.** When the budget runs out the relay stops
//!   with `outcome = Exhausted`, never a silent give-up.
//! - **Timers are Tina-owned.** Each backoff delay is a `sleep(delay)`
//!   scheduled by the relay; `ctx.now()` drives the budget so the path is
//!   replayable.

use std::collections::HashSet;
use std::convert::Infallible;
use std::time::Duration;

use tina::prelude::*;
use tina::time::Backoff;
use tina_runtime::{
    DefaultThreadedMailboxFactory, FullDecision, FullHandling, LocalSystem, SleepReply, sleep,
};

/// How the delivery ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The downstream eventually accepted the request.
    Delivered,
    /// The retry budget ran out before the downstream accepted.
    Exhausted,
}

/// What the run observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    /// The caller-stamped idempotency key the request carried.
    pub idempotency_key: u64,
    /// How the delivery ended.
    pub outcome: Outcome,
    /// Total delivery attempts (first try + retries).
    pub attempts: u32,
    /// Retries the budget granted (attempts - 1).
    pub retries: u32,
    /// How many times the downstream actually charged. Must be exactly 1
    /// on `Delivered` (idempotent), 0 on `Exhausted`.
    pub downstream_charges: u32,
}

/// A flaky downstream: replies `Full` for the first `fail_first` attempts,
/// then accepts. Dedups on the idempotency key so a retry is never a
/// double-charge — that is what makes the caller's retry safe.
#[derive(Debug)]
struct Downstream {
    fail_first: u32,
    seen: u32,
    charged_keys: HashSet<u64>,
    charges: u32,
}

impl Downstream {
    fn new(fail_first: u32) -> Self {
        Self {
            fail_first,
            seen: 0,
            charged_keys: HashSet::new(),
            charges: 0,
        }
    }

    /// `Ok(())` = accepted, `Err(())` = Full (overloaded).
    fn try_deliver(&mut self, idempotency_key: u64) -> Result<(), ()> {
        self.seen += 1;
        if self.seen <= self.fail_first {
            return Err(());
        }
        // Idempotent commit: charge once per key even if the caller retries.
        if self.charged_keys.insert(idempotency_key) {
            self.charges += 1;
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum RelayMsg {
    /// Start delivering the request. The idempotency key is the caller's
    /// explicit claim that re-sending this exact request is safe.
    Deliver { idempotency_key: u64 },
    /// Backoff continuation: time to make the next attempt.
    RetryTick(SleepReply),
}

struct Relay {
    idempotency_key: u64,
    downstream: Downstream,
    budget: FullHandling,
    attempts: u32,
    retries: u32,
}

#[tina_runtime::isolate(message = RelayMsg)]
impl Relay {
    fn handle(
        &mut self,
        msg: RelayMsg,
        ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            RelayMsg::Deliver { idempotency_key } => {
                self.idempotency_key = idempotency_key;
                self.attempt(ctx)
            }
            RelayMsg::RetryTick(Ok(())) => self.attempt(ctx),
            RelayMsg::RetryTick(Err(_)) => {
                // The backoff sleep was cancelled (shutdown). Report what we
                // have rather than pretend success.
                stop_with(self.report(Outcome::Exhausted))
            }
        }
    }
}

impl Relay {
    fn attempt(&mut self, ctx: &mut Context<'_, SingleShard, ()>) -> Effect<Self> {
        self.attempts += 1;
        match self.downstream.try_deliver(self.idempotency_key) {
            Ok(()) => {
                // Success resets the budget for the next caller obligation.
                self.budget.record_success();
                stop_with(self.report(Outcome::Delivered))
            }
            Err(()) => {
                // The downstream is Full. We do NOT retry on our own — we
                // consult the explicit budget. Idempotency is already named
                // in the request, so a retry is safe.
                match self.budget.on_full(ctx.now(), None) {
                    FullDecision::RetryAfter { delay, .. } => {
                        self.retries += 1;
                        sleep(delay).then(RelayMsg::RetryTick)
                    }
                    FullDecision::Exhausted { .. } => stop_with(self.report(Outcome::Exhausted)),
                    FullDecision::Shed(_) => {
                        // Unreachable: this relay is built with a retry
                        // budget, never the shed-only mode.
                        stop_with(self.report(Outcome::Exhausted))
                    }
                }
            }
        }
    }

    fn report(&self, outcome: Outcome) -> Report {
        Report {
            idempotency_key: self.idempotency_key,
            outcome,
            attempts: self.attempts,
            retries: self.retries,
            downstream_charges: self.downstream.charges,
        }
    }
}

/// Run config: how many times the downstream rejects before accepting, and
/// how many retries the budget allows.
#[derive(Debug, Clone, Copy)]
pub struct RunConfig {
    /// Downstream replies `Full` this many times before accepting.
    pub downstream_fail_first: u32,
    /// Retry-attempt budget.
    pub max_retries: u64,
    /// Caller-stamped idempotency key.
    pub idempotency_key: u64,
    /// Backoff delay per retry.
    pub backoff_ms: u64,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            downstream_fail_first: 2,
            max_retries: 4,
            idempotency_key: 0xABCD,
            backoff_ms: 2,
        }
    }
}

/// Drive one delivery to completion.
pub fn run(config: RunConfig) -> anyhow::Result<Report> {
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;
    Ok(
        app.run_to_shutdown_reported(Duration::from_secs(5), move |app| {
            run_application(app, config)
        })?,
    )
}

fn run_application(
    app: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
    config: RunConfig,
) -> anyhow::Result<Report> {
    let backoff = Backoff::constant(Duration::from_millis(config.backoff_ms), config.max_retries)
        .map_err(|e| anyhow::anyhow!("backoff config: {e:?}"))?;
    let relay = Relay {
        idempotency_key: 0,
        downstream: Downstream::new(config.downstream_fail_first),
        budget: FullHandling::retry_backoff(backoff),
        attempts: 0,
        retries: 0,
    };

    let addr = app
        .register_root::<_, Infallible>(relay, 8)
        .map_err(|e| anyhow::anyhow!("register relay: {e:?}"))?;
    let waiter = app
        .observe_result::<Report, _, _>(addr)
        .map_err(|e| anyhow::anyhow!("observe_result: {e:?}"))?;

    app.try_send(
        addr,
        RelayMsg::Deliver {
            idempotency_key: config.idempotency_key,
        },
    )
    .map_err(|e| anyhow::anyhow!("send Deliver: {e:?}"))?;

    waiter
        .wait(Duration::from_secs(5))
        .map_err(|e| anyhow::anyhow!("relay did not finish: {e:?}"))
}
