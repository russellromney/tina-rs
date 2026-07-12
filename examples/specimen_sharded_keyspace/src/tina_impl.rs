//! Tina-shaped sharded keyspace.
//!
//! - One `Store` isolate per shard. Each owns its own `BTreeMap` —
//!   there is no shared mutex, no inner-shard concurrency, and the
//!   only way to read or write a shard's state is to send it a
//!   message.
//! - `Store::handle` always re-checks
//!   `placement.owner_for_str(&key) == ctx.shard_id()` before
//!   touching keyed state and replies with a typed
//!   [`tina_runtime::sharded::WrongShard`] on mismatch — a routing bug
//!   does not look like data loss.
//! - `Driver` walks the script one command at a time using
//!   `call(addr, StoreMsg, timeout).then(...)`. The
//!   [`ShardServiceTable`] hands it the right per-shard address for
//!   every key without exposing a hidden registry.
//! - `SUM` fans out sequentially across `placement.shards()`. The
//!   richer scatter/gather contract types (with `send_observed` and a
//!   bridge isolate) live in
//!   `tina-runtime/tests/sharded_primitives.rs`; this example uses
//!   the simpler sequential form because it reads top-to-bottom.
//!
//! Compare to `tokio_impl.rs`: same script, same FNV-1a placement,
//! same final [`Report`]. The model difference is structural — owners
//! enforced, no mutex, typed errors on misroute.

use std::collections::{BTreeMap, VecDeque};
use std::convert::Infallible;
use std::time::Duration;

use anyhow::Context;
use tina::prelude::*;
use tina_runtime::sharded::{ShardPlacement, ShardServiceTable, WrongShard};
use tina_runtime::{CallOutcome, DefaultThreadedMailboxFactory, ThreadedMultiShardRuntime, call};

use crate::{Command, Report, SCRIPT, SHARD_RAW_IDS, parse_commands};

#[derive(Debug, Clone, Copy)]
struct AppShard(u32);

impl Shard for AppShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

// ---------- Per-shard `Store` isolate ----------

#[derive(Debug, Clone)]
pub enum StoreMsg {
    Set { key: String, value: String },
    Get { key: String },
    Del { key: String },
    Count,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreReply {
    /// `SET k v` accepted on the right shard.
    Ok,
    /// `GET k` returned a value.
    Value(String),
    /// `GET k` saw the key absent on its owner shard.
    Miss,
    /// `DEL k` removed an entry.
    Deleted,
    /// `COUNT` reply: number of entries in this shard's map.
    Count(u64),
    /// The request landed on the wrong shard — the typed routing-error
    /// outcome. No silent fallback to "missing".
    WrongShard(WrongShard),
}

struct Store {
    placement: ShardPlacement,
    values: BTreeMap<String, String>,
}

#[tina_runtime::isolate(message = StoreMsg, reply = StoreReply, shard = AppShard)]
impl Store {
    fn handle(
        &mut self,
        msg: StoreMsg,
        ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        reply(self.apply(msg, ctx.shard_id()))
    }

    fn handle_call(&mut self, msg: StoreMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        let shard_id = call.shard_id();
        call.reply(self.apply(msg, shard_id))
    }
}

impl Store {
    fn apply(&mut self, msg: StoreMsg, shard_id: ShardId) -> StoreReply {
        match msg {
            StoreMsg::Set { key, value } => {
                if let Err(w) = self.placement.require_owner_str(&key, shard_id) {
                    return StoreReply::WrongShard(w);
                }
                self.values.insert(key, value);
                StoreReply::Ok
            }
            StoreMsg::Get { key } => {
                if let Err(w) = self.placement.require_owner_str(&key, shard_id) {
                    return StoreReply::WrongShard(w);
                }
                match self.values.get(&key) {
                    Some(value) => StoreReply::Value(value.clone()),
                    None => StoreReply::Miss,
                }
            }
            StoreMsg::Del { key } => {
                if let Err(w) = self.placement.require_owner_str(&key, shard_id) {
                    return StoreReply::WrongShard(w);
                }
                if self.values.remove(&key).is_some() {
                    StoreReply::Deleted
                } else {
                    StoreReply::Miss
                }
            }
            StoreMsg::Count => StoreReply::Count(self.values.len() as u64),
        }
    }
}

// ---------- `Driver` isolate: walks the script ----------

#[derive(Debug, Clone)]
pub enum DriverMsg {
    /// Kick off the first command.
    Begin,
    /// Continuation for each `call(...)` to a `Store`.
    StoreReturned(CallOutcome<StoreReply>),
}

struct Driver {
    table: ShardServiceTable<StoreMsg, StoreReply>,
    placement: ShardPlacement,
    commands: VecDeque<Command>,
    report: Report,
    /// SUM state: shards still owing a count, and the running total.
    sum_pending: VecDeque<ShardId>,
    sum_total: u64,
}

#[tina_runtime::isolate(message = DriverMsg, shard = AppShard)]
impl Driver {
    fn handle(
        &mut self,
        msg: DriverMsg,
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            DriverMsg::Begin => self.next_step(),
            DriverMsg::StoreReturned(outcome) => {
                self.absorb_store_reply(outcome);
                self.next_step()
            }
        }
    }
}

impl Driver {
    fn next_step(&mut self) -> Effect<Self> {
        // SUM in progress: keep fanning out until every shard has
        // contributed. Sequential fanout is the simplest form;
        // `tina-runtime/tests/sharded_primitives.rs` shows the parallel
        // `send_observed` + bridge pattern.
        if let Some(shard) = self.sum_pending.pop_front() {
            let address = self
                .table
                .address_for(shard)
                .expect("shard came from placement.shards()");
            return call(address, StoreMsg::Count, Duration::from_millis(200))
                .then(DriverMsg::StoreReturned);
        }

        match self.commands.pop_front() {
            Some(Command::Set { key, value }) => {
                let address = self.table.address_for_str(&key);
                call(
                    address,
                    StoreMsg::Set { key, value },
                    Duration::from_millis(200),
                )
                .then(DriverMsg::StoreReturned)
            }
            Some(Command::Get { key }) => {
                let address = self.table.address_for_str(&key);
                call(address, StoreMsg::Get { key }, Duration::from_millis(200))
                    .then(DriverMsg::StoreReturned)
            }
            Some(Command::Del { key }) => {
                let address = self.table.address_for_str(&key);
                call(address, StoreMsg::Del { key }, Duration::from_millis(200))
                    .then(DriverMsg::StoreReturned)
            }
            Some(Command::Sum) => {
                self.sum_total = 0;
                self.sum_pending = self.placement.shards().iter().copied().collect();
                self.next_step()
            }
            // The host reads `self.report` via
            // `runtime.observe_result::<Report>` — no shared mutex.
            Some(Command::Quit) | None => stop_with(self.report),
        }
    }

    fn absorb_store_reply(&mut self, outcome: CallOutcome<StoreReply>) {
        match outcome.into_result() {
            Ok(StoreReply::Ok) => self.report.ok += 1,
            Ok(StoreReply::Value(_)) => self.report.values += 1,
            Ok(StoreReply::Miss) => self.report.misses += 1,
            Ok(StoreReply::Deleted) => self.report.deleted += 1,
            Ok(StoreReply::Count(n)) => {
                self.sum_total += n;
                if self.sum_pending.is_empty() {
                    self.report.sum = self.sum_total;
                }
            }
            Ok(StoreReply::WrongShard(w)) => panic!(
                "driver routed to the wrong shard: expected {}, actual {}",
                w.expected.get(),
                w.actual.get()
            ),
            Err(e) => panic!("call to store failed: {e:?}"),
        }
    }
}

// ---------- Run ----------

pub fn run() -> anyhow::Result<Report> {
    let runtime = ThreadedMultiShardRuntime::try_new(
        SHARD_RAW_IDS.iter().copied().map(AppShard),
        DefaultThreadedMailboxFactory,
    )?;
    let placement = ShardPlacement::new(
        "specimen-sharded-keyspace",
        SHARD_RAW_IDS.iter().copied().map(ShardId::new).collect(),
    )
    .context("placement")?;

    // One `Store` per shard, table built over the same ordered shard
    // list. `try_from_placement` runs the registration closure for each
    // shard and folds any per-shard error into a typed table-build error.
    // The error retains the successfully registered prefix for explicit
    // settlement and composes with `.context(...)`.
    let placement_for_register = placement.clone();
    let table = ShardServiceTable::try_from_placement(placement.clone(), |shard| {
        runtime.register_with_capacity_on::<Store, Infallible>(
            shard,
            Store {
                placement: placement_for_register.clone(),
                values: BTreeMap::new(),
            },
            32,
        )
    })
    .context("register stores per shard")?;

    // Driver lives on the first shard; it walks the script one
    // command at a time via `call(...).then(...)`.
    let driver = runtime
        .register_with_capacity_on::<Driver, Infallible>(
            ShardId::new(SHARD_RAW_IDS[0]),
            Driver {
                table: table.clone(),
                placement,
                commands: parse_commands(SCRIPT.as_bytes())?.into(),
                report: Report::default(),
                sum_pending: VecDeque::new(),
                sum_total: 0,
            },
            64,
        )
        .map_err(|e| anyhow::anyhow!("register driver: {e:?}"))?;

    // Typed result waiter on the multi-shard runtime. No
    // `Arc<Mutex<Option<Report>>>` polling.
    let waiter = runtime
        .observe_result::<Report, _, _>(driver)
        .map_err(|e| anyhow::anyhow!("observe_result: {e:?}"))?;

    runtime
        .try_send(driver, DriverMsg::Begin)
        .map_err(|e| anyhow::anyhow!("send Begin: {e:?}"))?;

    let report = waiter
        .wait(Duration::from_secs(5))
        .map_err(|e| anyhow::anyhow!("driver did not produce a report: {e:?}"))?;
    runtime.shutdown_report().ensure_clean()?;
    Ok(report)
}
