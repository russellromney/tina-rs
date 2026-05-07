//! Phase 053 live smoke: sharded counter driven by `ThreadedMultiShardRuntime`.
//!
//! Cross-shard request/reply over Betelgeuse worker threads (not the
//! explicit-step in-process runtime). Drives a sharded counter to prove
//! that `ShardPlacement` + `ShardServiceTable` produce the same per-shard
//! totals against a real worker-thread cross-shard path.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tina::{Address, Mailbox, TrySendError, prelude::*};
use tina_runtime::sharded::{ShardPlacement, ShardServiceTable, WrongShard};
use tina_runtime::{MailboxFactory, RuntimeCall, ThreadedMultiShardRuntime};

#[derive(Debug, Clone, Copy)]
struct AppShard(u32);

impl Shard for AppShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

// `ThreadedMultiShardRuntime` runs each shard on its own worker thread, so
// the factory must be `Send + Clone`. The mailbox itself stays shard-local
// (the factory's `create` runs on the worker thread), so RefCell + Cell
// inside the mailbox is fine — same shape as
// `tests/betelgeuse_substrate.rs`'s `TestMailbox`.

struct WorkerMailbox<T> {
    capacity: usize,
    queue: RefCell<VecDeque<T>>,
    closed: Cell<bool>,
}

impl<T> Mailbox<T> for WorkerMailbox<T> {
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
struct WorkerMailboxFactory;

impl MailboxFactory for WorkerMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(WorkerMailbox {
            capacity,
            queue: RefCell::new(VecDeque::new()),
            closed: Cell::new(false),
        })
    }
}

// ---------- Sharded counter (Send + Mutex variant) ----------

#[derive(Debug, Clone)]
enum CounterMsg {
    Inc {
        key: String,
        reply_to: Address<TrackerMsg>,
    },
    Get {
        reply_to: Address<TrackerMsg>,
    },
}

#[derive(Debug, Clone)]
enum TrackerMsg {
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
        send: Outbound<TrackerMsg>,
        spawn: Infallible,
        call: RuntimeCall<CounterMsg>,
        shard: AppShard,
    }

    fn handle(&mut self, msg: Self::Message, ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            CounterMsg::Inc { key, reply_to } => {
                let owner = self.placement.owner_for_str(&key);
                if owner != ctx.shard_id() {
                    return send(
                        reply_to,
                        TrackerMsg::IncResult {
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
                    TrackerMsg::IncResult {
                        shard: ctx.shard_id(),
                        outcome: Ok(()),
                    },
                )
            }
            CounterMsg::Get { reply_to } => send(
                reply_to,
                TrackerMsg::GetResult {
                    shard: ctx.shard_id(),
                    value: self.value,
                },
            ),
        }
    }
}

struct Tracker {
    inc_oks: Arc<Mutex<Vec<ShardId>>>,
    inc_wrong: Arc<Mutex<Vec<WrongShard>>>,
    gets: Arc<Mutex<Vec<(ShardId, u64)>>>,
}

impl Isolate for Tracker {
    tina::isolate_types! {
        message: TrackerMsg,
        reply: (),
        send: Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<TrackerMsg>,
        shard: AppShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            TrackerMsg::IncResult { shard, outcome } => match outcome {
                Ok(()) => self.inc_oks.lock().expect("inc_oks").push(shard),
                Err(w) => self.inc_wrong.lock().expect("inc_wrong").push(w),
            },
            TrackerMsg::GetResult { shard, value } => {
                self.gets.lock().expect("gets").push((shard, value));
            }
        }
        noop()
    }
}

fn wait_until<F: FnMut() -> bool>(timeout: Duration, label: &str, mut cond: F) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("timed out waiting for {label}");
}

#[test]
fn threaded_multishard_sharded_counter_aggregates_match_placement() {
    let runtime = ThreadedMultiShardRuntime::new(
        [AppShard(11), AppShard(22), AppShard(33)],
        WorkerMailboxFactory,
    );
    let placement = ShardPlacement::new(
        "live-counter",
        vec![ShardId::new(11), ShardId::new(22), ShardId::new(33)],
    )
    .unwrap();

    let inc_oks = Arc::new(Mutex::new(Vec::new()));
    let inc_wrong = Arc::new(Mutex::new(Vec::new()));
    let gets = Arc::new(Mutex::new(Vec::new()));
    let tracker = runtime
        .register_with_capacity_on::<Tracker, _>(
            ShardId::new(11),
            Tracker {
                inc_oks: Arc::clone(&inc_oks),
                inc_wrong: Arc::clone(&inc_wrong),
                gets: Arc::clone(&gets),
            },
            128,
        )
        .expect("register tracker");

    let mut entries = Vec::new();
    for shard in placement.shards() {
        let address = runtime
            .register_with_capacity_on::<ShardedCounter, _>(
                *shard,
                ShardedCounter {
                    placement: placement.clone(),
                    value: 0,
                },
                64,
            )
            .expect("register counter");
        entries.push((*shard, address));
    }
    let table = ShardServiceTable::new(placement.clone(), entries).unwrap();

    let keys = [
        "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "iota", "kappa",
        "lambda", "mu",
    ];
    let mut expected: BTreeMap<ShardId, u64> = BTreeMap::new();
    for k in keys {
        *expected.entry(placement.owner_for_str(k)).or_default() += 1;
    }

    for k in keys {
        runtime
            .try_send(
                table.address_for_str(k),
                CounterMsg::Inc {
                    key: k.to_string(),
                    reply_to: tracker,
                },
            )
            .expect("send inc");
    }

    wait_until(Duration::from_secs(5), "all inc replies", || {
        inc_oks.lock().expect("inc_oks").len() == keys.len()
    });
    assert!(inc_wrong.lock().expect("inc_wrong").is_empty());

    for address in table.addresses() {
        runtime
            .try_send(*address, CounterMsg::Get { reply_to: tracker })
            .expect("send get");
    }
    wait_until(Duration::from_secs(5), "all get replies", || {
        gets.lock().expect("gets").len() == placement.shards().len()
    });

    let actual: BTreeMap<ShardId, u64> = gets.lock().expect("gets").iter().copied().collect();
    assert_eq!(actual, expected);
    let total: u64 = actual.values().sum();
    assert_eq!(total as usize, keys.len());

    let _ = runtime.shutdown();
}

#[test]
fn threaded_multishard_owner_recheck_returns_wrong_shard_typed() {
    let runtime =
        ThreadedMultiShardRuntime::new([AppShard(11), AppShard(22)], WorkerMailboxFactory);
    let placement =
        ShardPlacement::new("live-recheck", vec![ShardId::new(11), ShardId::new(22)]).unwrap();

    let inc_oks = Arc::new(Mutex::new(Vec::new()));
    let inc_wrong = Arc::new(Mutex::new(Vec::new()));
    let gets = Arc::new(Mutex::new(Vec::new()));
    let tracker = runtime
        .register_with_capacity_on::<Tracker, _>(
            ShardId::new(11),
            Tracker {
                inc_oks: Arc::clone(&inc_oks),
                inc_wrong: Arc::clone(&inc_wrong),
                gets: Arc::clone(&gets),
            },
            32,
        )
        .expect("register tracker");

    let mut entries = Vec::new();
    for shard in placement.shards() {
        let address = runtime
            .register_with_capacity_on::<ShardedCounter, _>(
                *shard,
                ShardedCounter {
                    placement: placement.clone(),
                    value: 0,
                },
                16,
            )
            .expect("register counter");
        entries.push((*shard, address));
    }
    let table = ShardServiceTable::new(placement.clone(), entries).unwrap();

    let key = "alpha";
    let correct = placement.owner_for_str(key);
    let wrong = if correct == ShardId::new(11) {
        ShardId::new(22)
    } else {
        ShardId::new(11)
    };
    runtime
        .try_send(
            table.address_for(wrong).unwrap(),
            CounterMsg::Inc {
                key: key.to_string(),
                reply_to: tracker,
            },
        )
        .expect("send inc to wrong shard");

    wait_until(Duration::from_secs(5), "wrong-shard reply", || {
        !inc_wrong.lock().expect("inc_wrong").is_empty()
    });
    assert!(inc_oks.lock().expect("inc_oks").is_empty());
    assert_eq!(
        inc_wrong.lock().expect("inc_wrong").as_slice(),
        &[WrongShard {
            expected: correct,
            actual: wrong,
        }]
    );

    let _ = runtime.shutdown();
}
