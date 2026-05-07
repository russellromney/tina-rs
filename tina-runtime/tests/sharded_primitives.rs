//! Phase 053: integration tests for `tina_runtime::sharded`.
//!
//! Drives the explicit-step `MultiShardRuntime` so the contracts can be
//! proved end-to-end:
//!
//! - sharded counter first form (key -> owner shard -> shard-local counter);
//! - sharded map first form (key -> owner shard -> shard-local `BTreeMap`);
//! - owner re-checks `placement.owner_for_bytes(key) == ctx.shard_id()` and
//!   returns typed `WrongShard` on mismatch;
//! - bounded scatter/gather fanout that records partial outcomes;
//! - service-table lookups surface `MissingShard` for shards not in the
//!   table.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
use std::convert::Infallible;
use std::rc::Rc;
use std::time::Duration;

use tina::{Address, Mailbox, TrySendError, prelude::*};
use tina_runtime::sharded::{
    MissingShard, ScatterGatherConfig, ScatterGatherConfigError, ScatterGatherReport,
    ScatterGatherTargetOutcome, ShardPlacement, ShardServiceTable, WrongShard,
};
use tina_runtime::{MailboxFactory, MultiShardRuntime, RuntimeCall, SendOutcome, send_observed};

#[derive(Debug, Clone, Copy)]
struct AppShard(u32);

impl Shard for AppShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

// Test mailbox + factory (deterministic, no `Send` requirement).

struct TestMailbox<T> {
    capacity: usize,
    queue: Rc<RefCell<VecDeque<T>>>,
    closed: Rc<Cell<bool>>,
}

impl<T> Clone for TestMailbox<T> {
    fn clone(&self) -> Self {
        Self {
            capacity: self.capacity,
            queue: Rc::clone(&self.queue),
            closed: Rc::clone(&self.closed),
        }
    }
}

impl<T> TestMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: Rc::new(RefCell::new(VecDeque::new())),
            closed: Rc::new(Cell::new(false)),
        }
    }
}

impl<T> Mailbox<T> for TestMailbox<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }

    fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        if self.closed.get() {
            return Err(TrySendError::Closed(message));
        }
        let mut queue = self.queue.borrow_mut();
        if queue.len() >= self.capacity {
            return Err(TrySendError::Full(message));
        }
        queue.push_back(message);
        Ok(())
    }

    fn recv(&self) -> Option<T> {
        self.queue.borrow_mut().pop_front()
    }

    fn close(&self) {
        self.closed.set(true);
    }
}

#[derive(Debug, Clone, Copy)]
struct TestMailboxFactory;

impl MailboxFactory for TestMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(TestMailbox::new(capacity))
    }
}

fn run_until_idle(runtime: &mut MultiShardRuntime<AppShard, TestMailboxFactory>) {
    for _ in 0..64 {
        let delivered = runtime.step();
        if delivered == 0 && !runtime.has_in_flight_calls() {
            return;
        }
    }
    panic!("multi-shard runtime did not quiesce within 64 rounds");
}

// ---------- Sharded counter first form ----------

#[derive(Debug, Clone)]
enum CounterMsg {
    Inc {
        key: String,
        reply_to: Address<CounterCoordMsg>,
    },
    Get {
        reply_to: Address<CounterCoordMsg>,
    },
}

#[derive(Debug, Clone)]
enum CounterCoordMsg {
    IncResult {
        shard: ShardId,
        outcome: Result<(), WrongShard>,
    },
    GetResult {
        shard: ShardId,
        value: u64,
    },
}

struct ShardedCounter {
    placement: ShardPlacement,
    value: u64,
}

impl Isolate for ShardedCounter {
    tina::isolate_types! {
        message: CounterMsg,
        reply: (),
        send: Outbound<CounterCoordMsg>,
        spawn: Infallible,
        call: Infallible,
        shard: AppShard,
    }

    fn handle(&mut self, msg: Self::Message, ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            CounterMsg::Inc { key, reply_to } => {
                let owner = self.placement.owner_for_str(&key);
                if owner != ctx.shard_id() {
                    return send(
                        reply_to,
                        CounterCoordMsg::IncResult {
                            shard: ctx.shard_id(),
                            outcome: Err(WrongShard {
                                expected: owner,
                                actual: ctx.shard_id(),
                            }),
                        },
                    );
                }
                self.value += 1;
                send(
                    reply_to,
                    CounterCoordMsg::IncResult {
                        shard: ctx.shard_id(),
                        outcome: Ok(()),
                    },
                )
            }
            CounterMsg::Get { reply_to } => send(
                reply_to,
                CounterCoordMsg::GetResult {
                    shard: ctx.shard_id(),
                    value: self.value,
                },
            ),
        }
    }
}

#[derive(Debug, Default)]
struct CounterTracker {
    inc_oks: Rc<RefCell<Vec<ShardId>>>,
    inc_wrong: Rc<RefCell<Vec<WrongShard>>>,
    get_replies: Rc<RefCell<Vec<(ShardId, u64)>>>,
}

impl Isolate for CounterTracker {
    tina::isolate_types! {
        message: CounterCoordMsg,
        reply: (),
        send: Outbound<Infallible>,
        spawn: Infallible,
        call: Infallible,
        shard: AppShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            CounterCoordMsg::IncResult { shard, outcome } => match outcome {
                Ok(()) => self.inc_oks.borrow_mut().push(shard),
                Err(wrong) => self.inc_wrong.borrow_mut().push(wrong),
            },
            CounterCoordMsg::GetResult { shard, value } => {
                self.get_replies.borrow_mut().push((shard, value));
            }
        }
        noop()
    }
}

fn register_counter_table(
    runtime: &mut MultiShardRuntime<AppShard, TestMailboxFactory>,
    placement: &ShardPlacement,
    counter_capacity: usize,
) -> ShardServiceTable<CounterMsg> {
    let mut entries = Vec::new();
    for shard in placement.shards() {
        let address = runtime.register_with_capacity_on::<ShardedCounter, CounterCoordMsg>(
            *shard,
            ShardedCounter {
                placement: placement.clone(),
                value: 0,
            },
            counter_capacity,
        );
        entries.push((*shard, address));
    }
    ShardServiceTable::new(placement.clone(), entries).unwrap()
}

#[test]
fn sharded_counter_first_form_routes_keys_to_owner_shards() {
    let mut runtime = MultiShardRuntime::new(
        [AppShard(7), AppShard(31), AppShard(91)],
        TestMailboxFactory,
    );
    let placement = ShardPlacement::new(
        "counter-keys",
        vec![ShardId::new(7), ShardId::new(31), ShardId::new(91)],
    )
    .unwrap();

    let tracker_state = CounterTracker::default();
    let inc_oks = Rc::clone(&tracker_state.inc_oks);
    let inc_wrong = Rc::clone(&tracker_state.inc_wrong);
    let get_replies = Rc::clone(&tracker_state.get_replies);
    let tracker = runtime.register_with_capacity_on::<CounterTracker, Infallible>(
        ShardId::new(7),
        tracker_state,
        64,
    );

    let table = register_counter_table(&mut runtime, &placement, 16);

    let keys = [
        "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta",
    ];
    for key in keys {
        let owner = table.address_for_str(key);
        runtime
            .try_send(
                owner,
                CounterMsg::Inc {
                    key: key.to_string(),
                    reply_to: tracker,
                },
            )
            .unwrap();
    }
    run_until_idle(&mut runtime);

    assert!(inc_wrong.borrow().is_empty(), "{:?}", inc_wrong.borrow());
    assert_eq!(inc_oks.borrow().len(), keys.len());

    let mut expected_per_shard: BTreeMap<ShardId, u64> = BTreeMap::new();
    for key in keys {
        *expected_per_shard
            .entry(placement.owner_for_str(key))
            .or_default() += 1;
    }
    let mut got_per_shard: BTreeMap<ShardId, u64> = BTreeMap::new();
    for shard in inc_oks.borrow().iter() {
        *got_per_shard.entry(*shard).or_default() += 1;
    }
    assert_eq!(got_per_shard, expected_per_shard);

    for address in table.addresses() {
        runtime
            .try_send(*address, CounterMsg::Get { reply_to: tracker })
            .unwrap();
    }
    run_until_idle(&mut runtime);

    let replies: BTreeMap<ShardId, u64> = get_replies.borrow().iter().copied().collect();
    assert_eq!(replies, expected_per_shard);
    let total: u64 = replies.values().sum();
    assert_eq!(total as usize, keys.len());
}

#[test]
fn sharded_counter_owner_recheck_returns_wrong_shard_typed() {
    let mut runtime = MultiShardRuntime::new([AppShard(7), AppShard(31)], TestMailboxFactory);
    let placement =
        ShardPlacement::new("counter-keys", vec![ShardId::new(7), ShardId::new(31)]).unwrap();

    let tracker_state = CounterTracker::default();
    let inc_oks = Rc::clone(&tracker_state.inc_oks);
    let inc_wrong = Rc::clone(&tracker_state.inc_wrong);
    let tracker = runtime.register_with_capacity_on::<CounterTracker, Infallible>(
        ShardId::new(7),
        tracker_state,
        16,
    );

    let table = register_counter_table(&mut runtime, &placement, 8);

    let key = "alpha";
    let correct_owner = placement.owner_for_str(key);
    let wrong_owner = if correct_owner == ShardId::new(7) {
        ShardId::new(31)
    } else {
        ShardId::new(7)
    };
    let wrong_address = table.address_for(wrong_owner).unwrap();
    runtime
        .try_send(
            wrong_address,
            CounterMsg::Inc {
                key: key.to_string(),
                reply_to: tracker,
            },
        )
        .unwrap();
    run_until_idle(&mut runtime);

    assert!(inc_oks.borrow().is_empty());
    assert_eq!(
        inc_wrong.borrow().as_slice(),
        &[WrongShard {
            expected: correct_owner,
            actual: wrong_owner,
        }]
    );
}

// ---------- Sharded map first form ----------

#[derive(Debug, Clone)]
enum MapMsg {
    Put {
        key: String,
        value: String,
        reply_to: Address<MapCoordMsg>,
    },
    Get {
        key: String,
        reply_to: Address<MapCoordMsg>,
    },
    Del {
        key: String,
        reply_to: Address<MapCoordMsg>,
    },
}

#[derive(Debug, Clone)]
enum MapCoordMsg {
    PutResult(Result<ShardId, WrongShard>),
    GetResult {
        shard: ShardId,
        value: Option<String>,
    },
    DelResult(Result<bool, WrongShard>),
}

struct ShardedMap {
    placement: ShardPlacement,
    values: BTreeMap<String, String>,
}

impl Isolate for ShardedMap {
    tina::isolate_types! {
        message: MapMsg,
        reply: (),
        send: Outbound<MapCoordMsg>,
        spawn: Infallible,
        call: Infallible,
        shard: AppShard,
    }

    fn handle(&mut self, msg: Self::Message, ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            MapMsg::Put {
                key,
                value,
                reply_to,
            } => {
                let owner = self.placement.owner_for_str(&key);
                if owner != ctx.shard_id() {
                    return send(
                        reply_to,
                        MapCoordMsg::PutResult(Err(WrongShard {
                            expected: owner,
                            actual: ctx.shard_id(),
                        })),
                    );
                }
                self.values.insert(key, value);
                send(reply_to, MapCoordMsg::PutResult(Ok(ctx.shard_id())))
            }
            MapMsg::Get { key, reply_to } => {
                let owner = self.placement.owner_for_str(&key);
                if owner != ctx.shard_id() {
                    return send(
                        reply_to,
                        MapCoordMsg::GetResult {
                            shard: ctx.shard_id(),
                            value: None,
                        },
                    );
                }
                let value = self.values.get(&key).cloned();
                send(
                    reply_to,
                    MapCoordMsg::GetResult {
                        shard: ctx.shard_id(),
                        value,
                    },
                )
            }
            MapMsg::Del { key, reply_to } => {
                let owner = self.placement.owner_for_str(&key);
                if owner != ctx.shard_id() {
                    return send(
                        reply_to,
                        MapCoordMsg::DelResult(Err(WrongShard {
                            expected: owner,
                            actual: ctx.shard_id(),
                        })),
                    );
                }
                let removed = self.values.remove(&key).is_some();
                send(reply_to, MapCoordMsg::DelResult(Ok(removed)))
            }
        }
    }
}

#[derive(Debug, Default)]
struct MapTracker {
    put_oks: Rc<RefCell<Vec<ShardId>>>,
    put_wrong: Rc<RefCell<Vec<WrongShard>>>,
    gets: Rc<RefCell<Vec<(ShardId, Option<String>)>>>,
    dels: Rc<RefCell<Vec<Result<bool, WrongShard>>>>,
}

impl Isolate for MapTracker {
    tina::isolate_types! {
        message: MapCoordMsg,
        reply: (),
        send: Outbound<Infallible>,
        spawn: Infallible,
        call: Infallible,
        shard: AppShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            MapCoordMsg::PutResult(Ok(shard)) => self.put_oks.borrow_mut().push(shard),
            MapCoordMsg::PutResult(Err(w)) => self.put_wrong.borrow_mut().push(w),
            MapCoordMsg::GetResult { shard, value } => {
                self.gets.borrow_mut().push((shard, value));
            }
            MapCoordMsg::DelResult(r) => self.dels.borrow_mut().push(r),
        }
        noop()
    }
}

#[test]
fn sharded_map_routes_put_get_delete_to_owner_shard() {
    let mut runtime = MultiShardRuntime::new(
        [AppShard(3), AppShard(17), AppShard(91)],
        TestMailboxFactory,
    );
    let placement = ShardPlacement::new(
        "map-keys",
        vec![ShardId::new(3), ShardId::new(17), ShardId::new(91)],
    )
    .unwrap();

    let tracker_state = MapTracker::default();
    let put_oks = Rc::clone(&tracker_state.put_oks);
    let put_wrong = Rc::clone(&tracker_state.put_wrong);
    let gets = Rc::clone(&tracker_state.gets);
    let dels = Rc::clone(&tracker_state.dels);
    let tracker = runtime.register_with_capacity_on::<MapTracker, Infallible>(
        ShardId::new(3),
        tracker_state,
        64,
    );

    let mut entries = Vec::new();
    for shard in placement.shards() {
        let address = runtime.register_with_capacity_on::<ShardedMap, MapCoordMsg>(
            *shard,
            ShardedMap {
                placement: placement.clone(),
                values: BTreeMap::new(),
            },
            16,
        );
        entries.push((*shard, address));
    }
    let table = ShardServiceTable::new(placement.clone(), entries).unwrap();

    let pairs = [
        ("alpha", "1"),
        ("beta", "2"),
        ("gamma", "3"),
        ("delta", "4"),
    ];
    for (k, v) in pairs {
        runtime
            .try_send(
                table.address_for_str(k),
                MapMsg::Put {
                    key: k.to_string(),
                    value: v.to_string(),
                    reply_to: tracker,
                },
            )
            .unwrap();
    }
    run_until_idle(&mut runtime);
    assert!(put_wrong.borrow().is_empty());
    assert_eq!(put_oks.borrow().len(), pairs.len());

    for (k, _) in pairs {
        runtime
            .try_send(
                table.address_for_str(k),
                MapMsg::Get {
                    key: k.to_string(),
                    reply_to: tracker,
                },
            )
            .unwrap();
    }
    run_until_idle(&mut runtime);
    // Match replies by (owner_shard, value) since the multi-shard runtime
    // can interleave reply order across shards. We only need to assert
    // that every expected (owner, value) appears in the gathered set.
    let mut gathered: Vec<(ShardId, Option<String>)> = gets.borrow().clone();
    gathered.sort_by_key(|(s, _)| s.get());
    let mut expected: Vec<(ShardId, Option<String>)> = pairs
        .iter()
        .map(|(k, v)| (placement.owner_for_str(k), Some(v.to_string())))
        .collect();
    expected.sort_by_key(|(s, _)| s.get());
    assert_eq!(gathered.len(), expected.len());
    for (s, v) in &expected {
        assert!(
            gathered.iter().any(|(g_s, g_v)| g_s == s && g_v == v),
            "expected ({s:?}, {v:?}) in gathered = {gathered:?}"
        );
    }

    let key = "alpha";
    let correct = placement.owner_for_str(key);
    let wrong = placement
        .shards()
        .iter()
        .copied()
        .find(|s| *s != correct)
        .unwrap();
    runtime
        .try_send(
            table.address_for(wrong).unwrap(),
            MapMsg::Put {
                key: key.to_string(),
                value: "intruder".to_string(),
                reply_to: tracker,
            },
        )
        .unwrap();
    run_until_idle(&mut runtime);
    assert_eq!(
        put_wrong.borrow().as_slice(),
        &[WrongShard {
            expected: correct,
            actual: wrong,
        }]
    );

    for (k, _) in pairs {
        runtime
            .try_send(
                table.address_for_str(k),
                MapMsg::Del {
                    key: k.to_string(),
                    reply_to: tracker,
                },
            )
            .unwrap();
    }
    run_until_idle(&mut runtime);
    for r in dels.borrow().iter() {
        assert_eq!(*r, Ok(true));
    }
}

// ---------- Byte-identical placement across crates ----------
//
// The single source of truth for the FNV-1a 64 mod-count mapping is the
// `placement_owner_mapping_is_frozen_at_version_1` unit test in
// `tina-runtime/src/sharded.rs`. The constants below are duplicated here
// (and in `tina-sim/tests/sharded_dst.rs`) only as redundant guards — if
// any of the three diverges, multiple tests fail and the discrepancy is
// hard to miss. Update all three together when bumping the hash version.

#[test]
fn placement_owner_for_str_is_byte_identical_with_sim() {
    // Frozen — must match `sharded::tests::FROZEN_DST_*` in
    // `tina-runtime/src/sharded.rs` and `KEYS`/`FROZEN_OWNERS` in
    // `tina-sim/tests/sharded_dst.rs`.
    const KEYS: &[&str] = &[
        "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "iota", "kappa",
        "lambda", "mu",
    ];
    const FROZEN_OWNERS: [u32; 12] = [11, 33, 33, 22, 33, 33, 11, 11, 11, 33, 33, 11];

    let placement = ShardPlacement::new(
        "dst-fanout",
        vec![ShardId::new(11), ShardId::new(22), ShardId::new(33)],
    )
    .unwrap();
    let owners: Vec<u32> = KEYS
        .iter()
        .map(|k| placement.owner_for_str(k).get())
        .collect();
    assert_eq!(owners, FROZEN_OWNERS.to_vec());
}

// ---------- Service-table MissingShard ----------

#[test]
fn service_table_returns_missing_shard_for_unknown_id() {
    let mut runtime = MultiShardRuntime::new(
        [AppShard(3), AppShard(17), AppShard(91)],
        TestMailboxFactory,
    );
    let placement = ShardPlacement::new(
        "fanout",
        vec![ShardId::new(3), ShardId::new(17), ShardId::new(91)],
    )
    .unwrap();
    let table = register_counter_table(&mut runtime, &placement, 8);
    assert_eq!(
        table.address_for(ShardId::new(404)).unwrap_err(),
        MissingShard(ShardId::new(404))
    );
}

// ---------- Bounded scatter/gather aggregate ----------
//
// Coordinator: on Bind, captures the bridge address; on Start, fans Get
// out to every shard in its table; on Reply, accumulates per-target
// outcomes and emits a `ScatterGatherReport` when every target has either
// replied or been marked as a non-reply outcome.
//
// ReplyBridge: same shard as coord; translates `CounterCoordMsg` into
// `ScatterCoordMsg::Reply` so typed addresses stay honest.

struct ScatterCoord {
    table: ShardServiceTable<CounterMsg>,
    bridge: Option<Address<CounterCoordMsg>>,
    config: ScatterGatherConfig,
    pending_targets: Vec<ShardId>,
    outcomes: Vec<(ShardId, ScatterGatherTargetOutcome<u64>)>,
    report_into: Rc<RefCell<Option<ScatterGatherReport<u64>>>>,
}

#[derive(Debug, Clone)]
enum ScatterCoordMsg {
    Bind { bridge: Address<CounterCoordMsg> },
    Start,
    Reply(CounterCoordMsg),
}

impl Isolate for ScatterCoord {
    tina::isolate_types! {
        message: ScatterCoordMsg,
        reply: (),
        send: Outbound<CounterMsg>,
        spawn: Infallible,
        call: Infallible,
        shard: AppShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ScatterCoordMsg::Bind { bridge } => {
                self.bridge = Some(bridge);
                noop()
            }
            ScatterCoordMsg::Start => {
                let bridge = self.bridge.expect("bind before start");
                let mut effects = Vec::new();
                let limit = self
                    .config
                    .max_targets
                    .min(self.table.placement().shards().len());
                for shard in self.table.placement().shards().iter().take(limit) {
                    self.pending_targets.push(*shard);
                    let address = self.table.address_for(*shard).expect("shard in table");
                    effects.push(send(address, CounterMsg::Get { reply_to: bridge }));
                }
                batch(effects)
            }
            ScatterCoordMsg::Reply(msg) => {
                if let CounterCoordMsg::GetResult { shard, value } = msg {
                    if let Some(pos) = self.pending_targets.iter().position(|s| *s == shard) {
                        self.pending_targets.swap_remove(pos);
                        self.outcomes
                            .push((shard, ScatterGatherTargetOutcome::Replied(value)));
                    }
                }
                if self.pending_targets.is_empty() {
                    let mut outcomes = std::mem::take(&mut self.outcomes);
                    outcomes.sort_by_key(|(s, _)| s.get());
                    let report = ScatterGatherReport {
                        config: self.config,
                        outcomes,
                    };
                    *self.report_into.borrow_mut() = Some(report);
                }
                noop()
            }
        }
    }
}

struct ReplyBridge {
    coord: Address<ScatterCoordMsg>,
}

impl Isolate for ReplyBridge {
    tina::isolate_types! {
        message: CounterCoordMsg,
        reply: (),
        send: Outbound<ScatterCoordMsg>,
        spawn: Infallible,
        call: Infallible,
        shard: AppShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        send(self.coord, ScatterCoordMsg::Reply(msg))
    }
}

#[test]
fn scatter_gather_aggregate_collects_one_reply_per_target() {
    let mut runtime = MultiShardRuntime::new(
        [AppShard(3), AppShard(17), AppShard(91)],
        TestMailboxFactory,
    );
    let placement = ShardPlacement::new(
        "fanout",
        vec![ShardId::new(3), ShardId::new(17), ShardId::new(91)],
    )
    .unwrap();
    let table = register_counter_table(&mut runtime, &placement, 16);

    let tracker_state = CounterTracker::default();
    let tracker = runtime.register_with_capacity_on::<CounterTracker, Infallible>(
        ShardId::new(3),
        tracker_state,
        32,
    );
    let preload_keys = ["a", "b", "c", "d", "e", "f", "g", "h"];
    for k in preload_keys {
        runtime
            .try_send(
                table.address_for_str(k),
                CounterMsg::Inc {
                    key: k.to_string(),
                    reply_to: tracker,
                },
            )
            .unwrap();
    }
    run_until_idle(&mut runtime);

    let mut expected: BTreeMap<ShardId, u64> = BTreeMap::new();
    for k in preload_keys {
        *expected.entry(placement.owner_for_str(k)).or_default() += 1;
    }

    let config = ScatterGatherConfig {
        max_targets: placement.shards().len(),
        collector_capacity: placement.shards().len(),
        per_target_timeout: Duration::from_millis(50),
        aggregate_timeout: Duration::from_millis(100),
    };
    config.validate().unwrap();

    let report_slot: Rc<RefCell<Option<ScatterGatherReport<u64>>>> = Rc::new(RefCell::new(None));
    let coord = runtime.register_with_capacity_on::<ScatterCoord, CounterMsg>(
        ShardId::new(3),
        ScatterCoord {
            table: table.clone(),
            bridge: None,
            config,
            pending_targets: Vec::new(),
            outcomes: Vec::with_capacity(config.max_targets),
            report_into: Rc::clone(&report_slot),
        },
        config.collector_capacity,
    );
    let bridge = runtime.register_with_capacity_on::<ReplyBridge, ScatterCoordMsg>(
        ShardId::new(3),
        ReplyBridge { coord },
        config.collector_capacity,
    );
    runtime
        .try_send(coord, ScatterCoordMsg::Bind { bridge })
        .unwrap();
    runtime.try_send(coord, ScatterCoordMsg::Start).unwrap();
    run_until_idle(&mut runtime);

    let report = report_slot
        .borrow_mut()
        .take()
        .expect("scatter coord produced a report");
    assert_eq!(report.replied_count(), placement.shards().len());
    assert_eq!(report.full_count(), 0);
    assert_eq!(report.closed_count(), 0);
    assert_eq!(report.timeout_count(), 0);
    assert_eq!(report.aggregate_timeout_count(), 0);
    let replied: BTreeMap<ShardId, u64> = report.replied().map(|(s, v)| (s, *v)).collect();
    assert_eq!(replied, expected);
    assert!(report.outcomes.len() <= report.config.max_targets);
}

// ---------- Scatter/gather config validation ----------

#[test]
fn scatter_gather_config_collector_capacity_must_match_max_targets() {
    let cfg = ScatterGatherConfig {
        max_targets: 4,
        collector_capacity: 3,
        per_target_timeout: Duration::from_millis(10),
        aggregate_timeout: Duration::from_millis(10),
    };
    assert_eq!(
        cfg.validate().unwrap_err(),
        ScatterGatherConfigError::CollectorCapacityBelowMaxTargets {
            collector_capacity: 3,
            max_targets: 4,
        }
    );
}

// ---------- Hot-key pressure: caller-owned retry loop ----------
//
// The owner shard rejects incoming Inc messages with `Full` once its
// mailbox is at capacity (small here on purpose). The caller-owned retry
// counter records first-attempt accept, first-attempt full, retry success,
// retry exhaustion via `HotKeyAttemptReport`. There is no automatic retry
// inside the placement/map/scatter helpers.

fn register_hot_key_owner(
    runtime: &mut MultiShardRuntime<AppShard, TestMailboxFactory>,
    placement: &ShardPlacement,
    mailbox_capacity: usize,
) -> Address<CounterMsg> {
    let mut entries = Vec::new();
    for shard in placement.shards() {
        let address = runtime.register_with_capacity_on::<ShardedCounter, CounterCoordMsg>(
            *shard,
            ShardedCounter {
                placement: placement.clone(),
                value: 0,
            },
            mailbox_capacity,
        );
        entries.push((*shard, address));
    }
    let table = ShardServiceTable::new(placement.clone(), entries).unwrap();
    table.address_for_str("hot-key")
}

#[test]
fn hot_key_pressure_caller_retry_loop_observes_full_and_succeeds_on_retry() {
    use tina_runtime::sharded::{HotKeyAttemptOutcome, HotKeyAttemptReport};

    let mut runtime = MultiShardRuntime::new([AppShard(7), AppShard(31)], TestMailboxFactory);
    let placement =
        ShardPlacement::new("hot-keys", vec![ShardId::new(7), ShardId::new(31)]).unwrap();
    let tracker_state = CounterTracker::default();
    let tracker = runtime.register_with_capacity_on::<CounterTracker, Infallible>(
        ShardId::new(7),
        tracker_state,
        64,
    );

    let owner = register_hot_key_owner(&mut runtime, &placement, 1);
    let make_msg = || CounterMsg::Inc {
        key: "hot-key".to_string(),
        reply_to: tracker,
    };

    let mut report = HotKeyAttemptReport::default();
    const BURST: usize = 5;
    const RETRY_BUDGET: u32 = 8;

    // First wave: BURST attempts with no stepping. cap=1 means exactly one
    // fits; the rest must come back `Full` on the very first try.
    let mut needs_retry: Vec<usize> = Vec::new();
    for i in 0..BURST {
        match runtime.try_send(owner, make_msg()) {
            Ok(()) => report.record(HotKeyAttemptOutcome::Accepted),
            Err(TrySendError::Full(_)) => {
                report.record(HotKeyAttemptOutcome::FullFirstAttempt);
                needs_retry.push(i);
            }
            Err(TrySendError::Closed(_)) => {
                report.record(HotKeyAttemptOutcome::Closed);
            }
        }
    }

    // Retry wave: caller-owned single-step backoff per retry. Once the
    // owner drains its slot, the next attempt fits.
    for _ in &needs_retry {
        let mut attempts = 0u32;
        loop {
            runtime.step(); // visible, caller-owned backoff
            attempts += 1;
            match runtime.try_send(owner, make_msg()) {
                Ok(()) => {
                    report.record(HotKeyAttemptOutcome::RetrySucceeded);
                    break;
                }
                Err(TrySendError::Full(_)) => {
                    report.record(HotKeyAttemptOutcome::FullRetry);
                    if attempts >= RETRY_BUDGET {
                        report.record(HotKeyAttemptOutcome::RetryExhausted);
                        break;
                    }
                }
                Err(TrySendError::Closed(_)) => {
                    report.record(HotKeyAttemptOutcome::Closed);
                    break;
                }
            }
        }
    }
    run_until_idle(&mut runtime);

    // Strong, exact bookkeeping. Every claim would break if `Full` were
    // not observed, or if the retry path were silently treated as a
    // single attempt.
    assert_eq!(
        report.accepted_first_attempt, 1,
        "exactly one first-attempt accept (cap=1, burst={BURST})"
    );
    assert_eq!(
        report.full_first_attempt,
        (BURST - 1) as u32,
        "every other burst entry hit Full on first attempt"
    );
    assert_eq!(
        report.retry_succeeded,
        (BURST - 1) as u32,
        "every retry-loop entry eventually succeeded"
    );
    assert_eq!(report.retry_exhausted, 0);
    assert_eq!(report.timeout, 0);
    assert_eq!(report.closed, 0);
    // Every retry happens immediately after a step that drains the
    // single-slot mailbox, so retries succeed on first try and
    // `full_retry_total` stays at zero. The cap-0 test below covers the
    // path where retries themselves observe `Full`.
    assert_eq!(report.full_retry_total, 0);
    assert_eq!(report.loop_entries(), BURST as u32);
}

#[test]
fn hot_key_pressure_retries_against_cap_zero_target_record_full_retry_and_exhaust() {
    use tina_runtime::sharded::{HotKeyAttemptOutcome, HotKeyAttemptReport};

    let mut runtime = MultiShardRuntime::new([AppShard(7), AppShard(31)], TestMailboxFactory);
    let placement =
        ShardPlacement::new("hot-keys", vec![ShardId::new(7), ShardId::new(31)]).unwrap();
    let tracker_state = CounterTracker::default();
    let tracker = runtime.register_with_capacity_on::<CounterTracker, Infallible>(
        ShardId::new(7),
        tracker_state,
        16,
    );
    // cap=0: every send to the owner returns Full, retries included.
    let owner = register_hot_key_owner(&mut runtime, &placement, 0);
    let make_msg = || CounterMsg::Inc {
        key: "hot-key".to_string(),
        reply_to: tracker,
    };

    let mut report = HotKeyAttemptReport::default();
    const RETRY_BUDGET: u32 = 3;

    let mut attempts = 0u32;
    loop {
        match runtime.try_send(owner, make_msg()) {
            Ok(()) => panic!("cap=0 mailbox must never accept a send"),
            Err(TrySendError::Full(_)) => {
                if attempts == 0 {
                    report.record(HotKeyAttemptOutcome::FullFirstAttempt);
                } else {
                    report.record(HotKeyAttemptOutcome::FullRetry);
                }
                attempts += 1;
                if attempts > RETRY_BUDGET {
                    report.record(HotKeyAttemptOutcome::RetryExhausted);
                    break;
                }
                runtime.step();
            }
            Err(TrySendError::Closed(_)) => panic!("owner is not closed"),
        }
    }

    assert_eq!(report.accepted_first_attempt, 0);
    assert_eq!(
        report.full_first_attempt, 1,
        "exactly one first-attempt Full"
    );
    assert_eq!(
        report.full_retry_total, RETRY_BUDGET,
        "every retry observed Full"
    );
    assert_eq!(report.retry_succeeded, 0);
    assert_eq!(report.retry_exhausted, 1);
    assert_eq!(report.timeout, 0);
    assert_eq!(report.closed, 0);
    assert_eq!(report.loop_entries(), 1);
}

#[test]
fn hot_key_pressure_retry_exhaustion_is_recorded_when_budget_is_zero() {
    use tina_runtime::sharded::{HotKeyAttemptOutcome, HotKeyAttemptReport};

    let mut runtime = MultiShardRuntime::new([AppShard(7), AppShard(31)], TestMailboxFactory);
    let placement =
        ShardPlacement::new("hot-keys", vec![ShardId::new(7), ShardId::new(31)]).unwrap();
    let tracker_state = CounterTracker::default();
    let tracker = runtime.register_with_capacity_on::<CounterTracker, Infallible>(
        ShardId::new(7),
        tracker_state,
        16,
    );
    let owner = register_hot_key_owner(&mut runtime, &placement, 1);
    let make_msg = || CounterMsg::Inc {
        key: "hot-key".to_string(),
        reply_to: tracker,
    };

    // Pre-fill the mailbox.
    runtime.try_send(owner, make_msg()).unwrap();

    let mut report = HotKeyAttemptReport::default();
    // No retries: budget=0 means the FullFirstAttempt is also terminal.
    match runtime.try_send(owner, make_msg()) {
        Err(TrySendError::Full(_)) => {
            report.record(HotKeyAttemptOutcome::FullFirstAttempt);
            report.record(HotKeyAttemptOutcome::RetryExhausted);
        }
        other => panic!("expected Full, got {other:?}"),
    }
    run_until_idle(&mut runtime);

    assert_eq!(report.accepted_first_attempt, 0);
    assert_eq!(report.full_first_attempt, 1);
    assert_eq!(report.retry_succeeded, 0);
    assert_eq!(report.retry_exhausted, 1);
    assert_eq!(report.full_retry_total, 0);
    assert_eq!(report.loop_entries(), 1);
}

// ---------- Scatter/gather partial-failure ----------
//
// Richer coordinator that uses `send_observed` to surface per-target
// `Full`/`Closed` and supports an explicit target list so a caller can
// name a shard that is not in the service table (which then surfaces as
// `MissingShard`). This is the canonical first-form scatter/gather:
// every non-reply outcome is named in the report.

struct ScatterCoordObs {
    table: ShardServiceTable<CounterMsg>,
    bridge: Option<Address<CounterCoordMsg>>,
    config: ScatterGatherConfig,
    targets: Vec<ShardId>,
    pending_replies: Vec<ShardId>,
    outcomes: Vec<(ShardId, ScatterGatherTargetOutcome<u64>)>,
    report_into: Rc<RefCell<Option<ScatterGatherReport<u64>>>>,
}

#[derive(Debug, Clone)]
enum ScatterCoordObsMsg {
    Bind {
        bridge: Address<CounterCoordMsg>,
    },
    Start,
    SendOutcome {
        shard: ShardId,
        outcome: SendOutcome,
    },
    Reply(CounterCoordMsg),
}

impl Isolate for ScatterCoordObs {
    tina::isolate_types! {
        message: ScatterCoordObsMsg,
        reply: (),
        send: Outbound<CounterMsg>,
        spawn: Infallible,
        call: RuntimeCall<ScatterCoordObsMsg>,
        shard: AppShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ScatterCoordObsMsg::Bind { bridge } => {
                self.bridge = Some(bridge);
                noop()
            }
            ScatterCoordObsMsg::Start => {
                let bridge = self.bridge.expect("bind before start");
                let mut effects: Vec<Effect<Self>> = Vec::new();
                let limit = self.config.max_targets.min(self.targets.len());
                for shard in self.targets.iter().copied().take(limit) {
                    match self.table.address_for(shard) {
                        Ok(address) => {
                            self.pending_replies.push(shard);
                            effects.push(
                                send_observed(address, CounterMsg::Get { reply_to: bridge }).reply(
                                    move |outcome| ScatterCoordObsMsg::SendOutcome {
                                        shard,
                                        outcome,
                                    },
                                ),
                            );
                        }
                        Err(MissingShard(s)) => {
                            self.outcomes
                                .push((s, ScatterGatherTargetOutcome::MissingShard));
                        }
                    }
                }
                self.maybe_emit_report();
                batch(effects)
            }
            ScatterCoordObsMsg::SendOutcome { shard, outcome } => {
                match outcome {
                    SendOutcome::Accepted => {
                        // Reply still pending; nothing to record yet.
                    }
                    SendOutcome::Full => {
                        self.cancel_pending(shard, ScatterGatherTargetOutcome::Full);
                    }
                    SendOutcome::Closed => {
                        self.cancel_pending(shard, ScatterGatherTargetOutcome::Closed);
                    }
                }
                self.maybe_emit_report();
                noop()
            }
            ScatterCoordObsMsg::Reply(reply) => {
                if let CounterCoordMsg::GetResult { shard, value } = reply {
                    if let Some(pos) = self.pending_replies.iter().position(|s| *s == shard) {
                        self.pending_replies.swap_remove(pos);
                        self.outcomes
                            .push((shard, ScatterGatherTargetOutcome::Replied(value)));
                    }
                }
                self.maybe_emit_report();
                noop()
            }
        }
    }
}

impl ScatterCoordObs {
    fn cancel_pending(&mut self, shard: ShardId, outcome: ScatterGatherTargetOutcome<u64>) {
        if let Some(pos) = self.pending_replies.iter().position(|s| *s == shard) {
            self.pending_replies.swap_remove(pos);
            self.outcomes.push((shard, outcome));
        }
    }

    fn maybe_emit_report(&mut self) {
        if !self.pending_replies.is_empty() {
            return;
        }
        if self.report_into.borrow().is_some() {
            return;
        }
        let mut outcomes = std::mem::take(&mut self.outcomes);
        outcomes.sort_by_key(|(s, _)| s.get());
        let report = ScatterGatherReport {
            config: self.config,
            outcomes,
        };
        *self.report_into.borrow_mut() = Some(report);
    }
}

struct ObsBridge {
    coord: Address<ScatterCoordObsMsg>,
}

impl Isolate for ObsBridge {
    tina::isolate_types! {
        message: CounterCoordMsg,
        reply: (),
        send: Outbound<ScatterCoordObsMsg>,
        spawn: Infallible,
        call: Infallible,
        shard: AppShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        send(self.coord, ScatterCoordObsMsg::Reply(msg))
    }
}

fn run_scatter_obs(
    runtime: &mut MultiShardRuntime<AppShard, TestMailboxFactory>,
    table: ShardServiceTable<CounterMsg>,
    targets: Vec<ShardId>,
    config: ScatterGatherConfig,
    coord_shard: ShardId,
) -> ScatterGatherReport<u64> {
    let report_slot: Rc<RefCell<Option<ScatterGatherReport<u64>>>> = Rc::new(RefCell::new(None));
    let coord = runtime.register_with_capacity_on::<ScatterCoordObs, CounterMsg>(
        coord_shard,
        ScatterCoordObs {
            table,
            bridge: None,
            config,
            targets,
            pending_replies: Vec::new(),
            outcomes: Vec::with_capacity(config.max_targets),
            report_into: Rc::clone(&report_slot),
        },
        config.collector_capacity,
    );
    let bridge = runtime.register_with_capacity_on::<ObsBridge, ScatterCoordObsMsg>(
        coord_shard,
        ObsBridge { coord },
        config.collector_capacity,
    );
    runtime
        .try_send(coord, ScatterCoordObsMsg::Bind { bridge })
        .unwrap();
    runtime.try_send(coord, ScatterCoordObsMsg::Start).unwrap();
    run_until_idle(runtime);
    report_slot
        .borrow_mut()
        .take()
        .expect("scatter coord produced a report")
}

#[test]
fn scatter_gather_records_missing_shard_for_target_outside_table() {
    let mut runtime = MultiShardRuntime::new(
        [AppShard(3), AppShard(17), AppShard(91)],
        TestMailboxFactory,
    );
    let placement = ShardPlacement::new(
        "fanout",
        vec![ShardId::new(3), ShardId::new(17), ShardId::new(91)],
    )
    .unwrap();
    let table = register_counter_table(&mut runtime, &placement, 16);

    // Target list mixes two known shards with one unknown id (404).
    // The known shards must reply; 404 must surface as MissingShard.
    let targets = vec![ShardId::new(3), ShardId::new(17), ShardId::new(404)];
    let config = ScatterGatherConfig {
        max_targets: targets.len(),
        collector_capacity: targets.len(),
        per_target_timeout: Duration::from_millis(50),
        aggregate_timeout: Duration::from_millis(100),
    };
    config.validate().unwrap();

    let report = run_scatter_obs(&mut runtime, table, targets, config, ShardId::new(3));
    assert_eq!(report.replied_count(), 2);
    assert_eq!(report.missing_shard_count(), 1);
    assert_eq!(report.full_count(), 0);
    assert_eq!(report.closed_count(), 0);
    assert_eq!(report.timeout_count(), 0);
    assert_eq!(report.aggregate_timeout_count(), 0);
    assert!(
        report.outcomes.iter().any(|(s, o)| *s == ShardId::new(404)
            && matches!(o, ScatterGatherTargetOutcome::MissingShard))
    );
    assert_eq!(report.outcomes.len(), 3);
}

#[test]
fn scatter_gather_records_full_when_target_mailbox_is_saturated() {
    // Same pattern as `consumer_api::downstream_consumer_can_observe_send_full`:
    // give the target counter a 0-capacity mailbox so every send to it
    // returns `SendOutcome::Full`. The other two counters get normal
    // capacity and reply.
    let mut runtime = MultiShardRuntime::new(
        [AppShard(3), AppShard(17), AppShard(91)],
        TestMailboxFactory,
    );
    let placement = ShardPlacement::new(
        "fanout",
        vec![ShardId::new(3), ShardId::new(17), ShardId::new(91)],
    )
    .unwrap();

    let mut entries = Vec::new();
    for shard in placement.shards() {
        let cap = if *shard == ShardId::new(3) { 0 } else { 8 };
        let address = runtime.register_with_capacity_on::<ShardedCounter, CounterCoordMsg>(
            *shard,
            ShardedCounter {
                placement: placement.clone(),
                value: 0,
            },
            cap,
        );
        entries.push((*shard, address));
    }
    let table = ShardServiceTable::new(placement.clone(), entries).unwrap();

    let targets: Vec<ShardId> = placement.shards().to_vec();
    let config = ScatterGatherConfig {
        max_targets: targets.len(),
        collector_capacity: targets.len(),
        per_target_timeout: Duration::from_millis(50),
        aggregate_timeout: Duration::from_millis(100),
    };
    config.validate().unwrap();

    // Put coord on the same shard as the saturated target so the
    // SendOutcome::Full is observed at source-time (local mailbox check).
    let report = run_scatter_obs(&mut runtime, table, targets, config, ShardId::new(3));
    // Shard 3's counter has cap=0, so the coord's send_observed gets
    // Full; shards 17 and 91 reply.
    assert_eq!(report.full_count(), 1, "report = {report:?}");
    assert_eq!(report.replied_count(), 2);
    assert_eq!(report.closed_count(), 0);
    assert_eq!(report.timeout_count(), 0);
    assert_eq!(report.missing_shard_count(), 0);
    assert!(
        report
            .outcomes
            .iter()
            .any(|(s, o)| *s == ShardId::new(3) && matches!(o, ScatterGatherTargetOutcome::Full)),
        "expected Full for shard 3, got {:?}",
        report.outcomes
    );
    assert_eq!(report.outcomes.len(), 3);
}
