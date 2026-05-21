//! End-to-end bridge tests against a stub `Client` isolate.
//!
//! The real `tina_rpc::Client` drives a TCP stream; for the bridge
//! tests we don't need wire I/O — we only need *something* sitting
//! at the `Address<ClientMsg>` that feeds a `ClientResultMsg` back
//! to the shim. A tiny "echo" stub does that, and the bridge's
//! correctness is observable without TCP.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use tina::{Address, Context, Effect, Isolate, Mailbox, Outbound, ShardId, TrySendError};
use tina_rpc::{ClientMsg, ClientResult, ClientResultMsg, service};
use tina_rpc_tokio::{BridgeClient, BridgeError, RetryDelay, RetryPolicy, call_with_retry};
use tina_runtime::{MailboxFactory, RuntimeCall, ThreadedRuntime, ThreadedRuntimeConfig};

#[derive(Debug, Default, Clone)]
struct SpecimenShard;

impl tina::Shard for SpecimenShard {
    fn id(&self) -> ShardId {
        ShardId::new(95)
    }
}

// Tiny VecDeque mailbox factory matching the specimen example pattern.
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

struct EMailbox<T> {
    capacity: usize,
    queue: Rc<RefCell<VecDeque<T>>>,
    closed: Rc<Cell<bool>>,
}

impl<T> EMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: Rc::new(RefCell::new(VecDeque::new())),
            closed: Rc::new(Cell::new(false)),
        }
    }
}

impl<T> Mailbox<T> for EMailbox<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }
    fn try_send(&self, msg: T) -> Result<(), TrySendError<T>> {
        if self.closed.get() {
            return Err(TrySendError::Closed(msg));
        }
        let mut q = self.queue.borrow_mut();
        if q.len() >= self.capacity {
            return Err(TrySendError::Full(msg));
        }
        q.push_back(msg);
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
struct EFactory;
impl MailboxFactory for EFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(EMailbox::new(capacity))
    }
}

// ---------------------------------------------------------------------------
// A typed service via the macro.
// ---------------------------------------------------------------------------

#[service]
pub trait Echo {
    fn say(&mut self, msg: String) -> String;
    fn add(&mut self, a: i64, b: i64) -> i64;
}

// ---------------------------------------------------------------------------
// Stub Client isolate: echoes every Request back as a Reply with the
// requested payload, demonstrating the typed flow without real TCP.
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct ClientStub {
    /// What to do with each incoming Request.
    behavior: StubBehavior,
}

#[derive(Debug, Clone, Copy)]
enum StubBehavior {
    /// Echo the request payload back as `ClientResult::Ok`.
    Echo,
    /// Reply with `ClientResult::Full` to every request — used to
    /// exercise the retry path.
    AlwaysFull,
    /// Hold every request without replying. Used to pin the bridge's
    /// admission limit: in-flight slots stay reserved until the
    /// bridge capacity is full, so subsequent calls must surface
    /// `BridgeError::Full` synchronously instead of hanging.
    NeverReply,
    /// Fill the reply shim mailbox with stray replies, then try to
    /// send the real reply. With bridge max_in_flight = 1 the shim
    /// mailbox has capacity 2, so the real reply is dropped unless
    /// the bridge has an independent terminal backstop.
    FloodShimThenReply,
}

impl Isolate for ClientStub {
    type Message = ClientMsg;
    type Reply = ();
    type Send = Outbound<ClientResultMsg>;
    type Spawn = Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Call = RuntimeCall<ClientMsg>;
    type Fact = Infallible;
    type Shard = SpecimenShard;

    fn handle(
        &mut self,
        msg: ClientMsg,
        _ctx: &mut Context<'_, SpecimenShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ClientMsg::Request(req) => {
                let result = match self.behavior {
                    StubBehavior::Echo => ClientResult::Ok(req.payload.clone()),
                    StubBehavior::AlwaysFull => ClientResult::Full,
                    StubBehavior::NeverReply => return Effect::Noop,
                    StubBehavior::FloodShimThenReply => {
                        return Effect::Batch(vec![
                            Effect::Send(Outbound::new(
                                req.reply_to,
                                ClientResultMsg {
                                    correlator: req.correlator + 1000,
                                    result: ClientResult::ConnectionClosed,
                                },
                            )),
                            Effect::Send(Outbound::new(
                                req.reply_to,
                                ClientResultMsg {
                                    correlator: req.correlator + 2000,
                                    result: ClientResult::ConnectionClosed,
                                },
                            )),
                            Effect::Send(Outbound::new(
                                req.reply_to,
                                ClientResultMsg {
                                    correlator: req.correlator,
                                    result: ClientResult::Ok(req.payload.clone()),
                                },
                            )),
                        ]);
                    }
                };
                Effect::Send(Outbound::new(
                    req.reply_to,
                    ClientResultMsg {
                        correlator: req.correlator,
                        result,
                    },
                ))
            }
            // Other variants are runtime-driven and not exercised
            // by the stub.
            _ => Effect::Noop,
        }
    }
}

// ---------------------------------------------------------------------------
// Test harness
// ---------------------------------------------------------------------------

fn build_runtime() -> Arc<ThreadedRuntime<SpecimenShard, EFactory>> {
    Arc::new(ThreadedRuntime::with_config(
        SpecimenShard,
        EFactory,
        ThreadedRuntimeConfig {
            command_capacity: 32,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    ))
}

fn register_stub(
    runtime: &ThreadedRuntime<SpecimenShard, EFactory>,
    behavior: StubBehavior,
) -> Address<ClientMsg> {
    let stub = ClientStub { behavior };
    runtime
        .register_with_capacity::<ClientStub, ClientResultMsg>(stub, 64)
        .expect("register stub")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn typed_call_round_trips_through_bridge_and_macro() {
    let runtime = build_runtime();
    let stub = register_stub(&runtime, StubBehavior::Echo);
    let bridge = BridgeClient::<SpecimenShard>::new(Arc::clone(&runtime), stub, 64).unwrap();

    // The stub echoes payload bytes; the macro encoded the args
    // tuple `(3, 4)` as `[3, 4]`. The macro decoder for `add` is
    // `i64`, but the stub gives back the *request* payload, not a
    // reply payload — so we read it back as a tuple.
    //
    // To prove typed round-trip correctly we use a custom decoder
    // that matches the request's tuple shape.
    let result: (i64, i64) = bridge
        .call(
            |corr, rt| EchoClient::add_request(3, 4, Duration::from_secs(1), corr, rt, 1024),
            |bytes| {
                serde_json::from_slice(bytes).map_err(|_| tina_rpc::EncodingError::Decode {
                    encoder: "json",
                    kind: tina_rpc::EncodingErrorKind::Syntax,
                })
            },
        )
        .await
        .expect("echo bridge call");
    assert_eq!(result, (3, 4));
}

#[tokio::test]
async fn server_full_surfaces_as_bridge_full() {
    let runtime = build_runtime();
    let stub = register_stub(&runtime, StubBehavior::AlwaysFull);
    let bridge = BridgeClient::<SpecimenShard>::new(Arc::clone(&runtime), stub, 16).unwrap();

    let outcome = bridge
        .call(
            |corr, rt| EchoClient::say_request("hi".into(), Duration::from_secs(1), corr, rt, 1024),
            |bytes| EchoClient::say_decode_reply(bytes, 1024),
        )
        .await;
    assert_eq!(outcome, Err(BridgeError::Full));
}

#[tokio::test]
async fn retry_policy_eventually_surfaces_persistent_full() {
    let runtime = build_runtime();
    let stub = register_stub(&runtime, StubBehavior::AlwaysFull);
    let bridge = BridgeClient::<SpecimenShard>::new(Arc::clone(&runtime), stub, 16).unwrap();

    let policy = RetryPolicy {
        attempts: 3,
        delay: RetryDelay::Fixed(Duration::ZERO),
        on_full: true,
        on_timeout: false,
    };

    let outcome: Result<String, BridgeError> = call_with_retry(&policy, || {
        let bridge = bridge.clone();
        async move {
            bridge
                .call(
                    |corr, rt| {
                        EchoClient::say_request("hi".into(), Duration::from_secs(1), corr, rt, 1024)
                    },
                    |bytes| EchoClient::say_decode_reply(bytes, 1024),
                )
                .await
        }
    })
    .await;

    // Three attempts, all `Full`, surfaces the last error.
    assert_eq!(outcome, Err(BridgeError::Full));
}

#[tokio::test]
async fn admission_full_returns_synchronously_no_hang() {
    // The bridge must surface `BridgeError::Full` synchronously when
    // the admission cap is reached, not hang on `rx.await` waiting
    // for a reply slot that will never free.
    let runtime = build_runtime();
    let stub = register_stub(&runtime, StubBehavior::NeverReply);
    let bridge = BridgeClient::<SpecimenShard>::new(Arc::clone(&runtime), stub, 2).unwrap();

    // Hold both admission slots with calls that will never complete.
    let bridge_a = bridge.clone();
    let h1 = tokio::spawn(async move {
        bridge_a
            .call(
                |corr, rt| {
                    EchoClient::say_request("a".into(), Duration::from_secs(60), corr, rt, 1024)
                },
                |bytes| EchoClient::say_decode_reply(bytes, 1024),
            )
            .await
    });
    let bridge_b = bridge.clone();
    let h2 = tokio::spawn(async move {
        bridge_b
            .call(
                |corr, rt| {
                    EchoClient::say_request("b".into(), Duration::from_secs(60), corr, rt, 1024)
                },
                |bytes| EchoClient::say_decode_reply(bytes, 1024),
            )
            .await
    });

    // Yield until both spawned tasks have synchronously claimed
    // their slots and parked on `rx.await`.
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }

    // Third call must surface `Full` immediately. Wrap in a timeout
    // so a regression hangs the test loudly instead of forever.
    let outcome = tokio::time::timeout(
        Duration::from_secs(2),
        bridge.call(
            |corr, rt| EchoClient::say_request("c".into(), Duration::from_secs(1), corr, rt, 1024),
            |bytes| EchoClient::say_decode_reply(bytes, 1024),
        ),
    )
    .await
    .expect("call past admission cap must return synchronously, not hang");

    assert_eq!(outcome, Err(BridgeError::Full));

    // Cancel the held tasks so the test exits cleanly. The slot
    // markers stay in `Some(None)` because the stub never sends a
    // reply, but that is the documented design — the slots stay
    // reserved until the underlying request completes (one way or
    // another).
    h1.abort();
    h2.abort();
    let _ = h1.await;
    let _ = h2.await;
}

#[tokio::test]
async fn cancelled_call_releases_slot_after_terminal_backstop() {
    // Dropping the awaiting future is not a terminal result for the
    // underlying RPC. The slot remains charged until the bridge
    // deadline backstop fires.
    let runtime = build_runtime();
    let stub = register_stub(&runtime, StubBehavior::NeverReply);
    let bridge = BridgeClient::<SpecimenShard>::new(Arc::clone(&runtime), stub, 1).unwrap();

    let bridge_a = bridge.clone();
    let abandoned = tokio::spawn(async move {
        bridge_a
            .call(
                |corr, rt| {
                    EchoClient::say_request("a".into(), Duration::from_millis(80), corr, rt, 1024)
                },
                |bytes| EchoClient::say_decode_reply(bytes, 1024),
            )
            .await
    });
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    abandoned.abort();
    let _ = abandoned.await;

    let before_backstop = tokio::time::timeout(
        Duration::from_millis(100),
        bridge.call(
            |corr, rt| EchoClient::say_request("b".into(), Duration::from_secs(60), corr, rt, 1024),
            |bytes| EchoClient::say_decode_reply(bytes, 1024),
        ),
    )
    .await
    .expect("capacity check should be immediate");
    assert_eq!(before_backstop, Err(BridgeError::Full));

    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(bridge.available_slots(), 1);
}

#[tokio::test]
async fn dropped_awaiter_holds_capacity_until_terminal_backstop() {
    // Dropping the awaiting future must not release admission while
    // the underlying RPC is still live. Otherwise a hot cancel/retry
    // loop can create unbounded outstanding work behind a bounded
    // bridge handle.
    let runtime = build_runtime();
    let stub = register_stub(&runtime, StubBehavior::NeverReply);
    let bridge = BridgeClient::<SpecimenShard>::new(Arc::clone(&runtime), stub, 1).unwrap();

    let bridge_a = bridge.clone();
    let abandoned = tokio::spawn(async move {
        bridge_a
            .call(
                |corr, rt| {
                    EchoClient::say_request("a".into(), Duration::from_millis(80), corr, rt, 1024)
                },
                |bytes| EchoClient::say_decode_reply(bytes, 1024),
            )
            .await
    });
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    abandoned.abort();
    let _ = abandoned.await;

    let immediate = tokio::time::timeout(
        Duration::from_millis(100),
        bridge.call(
            |corr, rt| EchoClient::say_request("b".into(), Duration::from_secs(1), corr, rt, 1024),
            |bytes| EchoClient::say_decode_reply(bytes, 1024),
        ),
    )
    .await
    .expect("capacity check should be immediate");
    assert_eq!(
        immediate,
        Err(BridgeError::Full),
        "cancelled awaiter must not free capacity before the admitted RPC settles"
    );

    tokio::time::sleep(Duration::from_millis(120)).await;

    let admitted_after_backstop = tokio::time::timeout(
        Duration::from_millis(100),
        bridge.call(
            |corr, rt| EchoClient::say_request("c".into(), Duration::from_secs(1), corr, rt, 1024),
            |bytes| EchoClient::say_decode_reply(bytes, 1024),
        ),
    )
    .await;
    assert!(
        admitted_after_backstop.is_err(),
        "after the bridge backstop releases capacity, the NeverReply call should be admitted and wait"
    );
}

#[tokio::test]
async fn dropped_shim_reply_times_out_instead_of_hanging() {
    // The client can attempt to notify the shim, but if the shim
    // mailbox is full the real reply may be dropped by the runtime.
    // The bridge still owes the awaiter a terminal result.
    let runtime = build_runtime();
    let stub = register_stub(&runtime, StubBehavior::FloodShimThenReply);
    let bridge = BridgeClient::<SpecimenShard>::new(Arc::clone(&runtime), stub, 1).unwrap();

    let outcome = tokio::time::timeout(
        Duration::from_secs(1),
        bridge.call(
            |corr, rt| {
                EchoClient::say_request("lost".into(), Duration::from_millis(50), corr, rt, 1024)
            },
            |bytes| EchoClient::say_decode_reply(bytes, 1024),
        ),
    )
    .await
    .expect("bridge must settle a dropped shim reply via its own backstop");

    assert_eq!(outcome, Err(BridgeError::Timeout));
    assert_eq!(bridge.available_slots(), 1, "backstop released admission");
}

#[tokio::test]
async fn parallel_calls_demux_correctly() {
    let runtime = build_runtime();
    let stub = register_stub(&runtime, StubBehavior::Echo);
    let bridge = BridgeClient::<SpecimenShard>::new(Arc::clone(&runtime), stub, 64).unwrap();

    // Fire many parallel calls; each gets its own correlator and
    // its own oneshot. The shim must demux every reply correctly.
    let mut handles = Vec::new();
    for i in 0..32u64 {
        let bridge = bridge.clone();
        handles.push(tokio::spawn(async move {
            let i = i as i64;
            // The stub echoes the request payload; we decode as
            // the tuple shape the request used.
            let result: (i64, i64) = bridge
                .call(
                    |corr, rt| {
                        EchoClient::add_request(i, i + 1, Duration::from_secs(1), corr, rt, 1024)
                    },
                    |bytes| {
                        serde_json::from_slice(bytes).map_err(|_| tina_rpc::EncodingError::Decode {
                            encoder: "json",
                            kind: tina_rpc::EncodingErrorKind::Syntax,
                        })
                    },
                )
                .await
                .unwrap();
            (i, result)
        }));
    }
    for handle in handles {
        let (i, (a, b)) = handle.await.unwrap();
        assert_eq!(a, i);
        assert_eq!(b, i + 1);
    }
}
