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
use tina_rpc::{ClientMsg, ClientRequest, ClientResult, ClientResultMsg, service};
use tina_rpc_tokio::{
    BridgeBuildError, BridgeClient, BridgeError, RetryDelay, RetryPolicy, call_with_retry,
};
use tina_runtime::{MailboxFactory, RuntimeCall, ThreadedRuntime, ThreadedRuntimeConfig};

#[derive(Debug, Default, Clone)]
struct SpecimenShard(Cell<u8>);

impl tina::Shard for SpecimenShard {
    fn id(&self) -> ShardId {
        let _ = self.0.get();
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
    fn is_empty(&self) -> bool {
        self.queue.borrow().is_empty()
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

async fn wait_for_available_slots(bridge: &BridgeClient<SpecimenShard>, expected: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while bridge.available_slots() != expected {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "bridge admission did not reach {expected} available slots; observed {}",
            bridge.available_slots()
        )
    });
}

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
    /// Models a connection-close `begin_close` fan-out: emits `strays`
    /// stray `ConnectionClosed` notifications (unknown correlators, as if
    /// for other in-flight client entries) in one turn, then the live
    /// correlator's own `ConnectionClosed`. The burst is bounded by the
    /// client's in-flight cap, not the bridge's; the shim must be sized to
    /// absorb it or the live reply is dropped and the awaiter only settles
    /// via the deadline backstop (Timeout), losing its true terminal.
    CloseFanout { strays: u64 },
}

impl Isolate for ClientStub {
    type Message = ClientMsg;
    type Reply = ();
    type Send = Outbound<ClientResultMsg>;
    type Spawn = Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Io = RuntimeCall<ClientMsg>;
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
                    StubBehavior::CloseFanout { strays } => {
                        // Stray notifications for other in-flight entries,
                        // then the live correlator's own ConnectionClosed —
                        // all in one turn, like begin_close.
                        let mut effects = Vec::with_capacity(strays as usize + 1);
                        for n in 0..strays {
                            effects.push(Effect::Send(Outbound::new(
                                req.reply_to,
                                ClientResultMsg {
                                    correlator: req.correlator + 1_000 + n,
                                    result: ClientResult::ConnectionClosed,
                                },
                            )));
                        }
                        effects.push(Effect::Send(Outbound::new(
                            req.reply_to,
                            ClientResultMsg {
                                correlator: req.correlator,
                                result: ClientResult::ConnectionClosed,
                            },
                        )));
                        return Effect::Batch(effects);
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
        SpecimenShard::default(),
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

fn assert_send<T: Send>(_: T) {}

fn assert_bridge_call_future_is_send<S>(bridge: &BridgeClient<S>)
where
    S: tina::Shard + Send + 'static,
{
    assert_send(bridge.call(
        |correlator, reply_to| {
            Ok(ClientRequest {
                service: "compile-check".into(),
                method: "send-future".into(),
                payload: Vec::new(),
                deadline: Duration::from_secs(1),
                correlator,
                reply_to,
            })
        },
        |bytes| Ok(bytes.len()),
    ));
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn construction_validation_is_fallible_and_call_future_is_send() {
    let runtime = build_runtime();
    let stub = register_stub(&runtime, StubBehavior::Echo);

    assert!(matches!(
        BridgeClient::<SpecimenShard>::new(Arc::clone(&runtime), stub, 0, 1),
        Err(BridgeBuildError::ZeroMaxInFlight)
    ));
    assert!(matches!(
        BridgeClient::<SpecimenShard>::new(Arc::clone(&runtime), stub, 2, 1),
        Err(BridgeBuildError::ClientCapacityTooSmall {
            bridge_max_in_flight: 2,
            client_max_in_flight: 1,
        })
    ));
    assert!(matches!(
        BridgeClient::<SpecimenShard>::new(Arc::clone(&runtime), stub, 1, usize::MAX),
        Err(BridgeBuildError::ShimCapacityOverflow {
            bridge_max_in_flight: 1,
            client_max_in_flight: usize::MAX,
        })
    ));

    let bridge = BridgeClient::<SpecimenShard>::new(runtime, stub, 1, 1).expect("valid bridge");
    assert_bridge_call_future_is_send(&bridge);
}

#[tokio::test]
async fn typed_call_round_trips_through_bridge_and_macro() {
    let runtime = build_runtime();
    let stub = register_stub(&runtime, StubBehavior::Echo);
    let bridge = BridgeClient::<SpecimenShard>::new(Arc::clone(&runtime), stub, 64, 64).unwrap();

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
    let bridge = BridgeClient::<SpecimenShard>::new(Arc::clone(&runtime), stub, 16, 64).unwrap();

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
    let bridge = BridgeClient::<SpecimenShard>::new(Arc::clone(&runtime), stub, 16, 64).unwrap();

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
    let bridge = BridgeClient::<SpecimenShard>::new(Arc::clone(&runtime), stub, 2, 64).unwrap();

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

    // Do not infer admission from scheduler turns. Wait until both permits
    // are observably charged before asserting the next call is full.
    wait_for_available_slots(&bridge, 0).await;

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
    let bridge = BridgeClient::<SpecimenShard>::new(Arc::clone(&runtime), stub, 1, 64).unwrap();

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
    wait_for_available_slots(&bridge, 0).await;
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
    let bridge = BridgeClient::<SpecimenShard>::new(Arc::clone(&runtime), stub, 1, 64).unwrap();

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
    wait_for_available_slots(&bridge, 0).await;
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
    // client_max_in_flight = 1 → shim mailbox = 1 + 1 = 2. FloodShimThenReply
    // sends 2 strays + 1 real = 3, so even the correctly-sized shim overflows
    // here and the real reply is dropped — proving the deadline backstop still
    // settles the awaiter when a reply is genuinely lost.
    let bridge = BridgeClient::<SpecimenShard>::new(Arc::clone(&runtime), stub, 1, 1).unwrap();

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
async fn close_fanout_settles_live_call_with_true_terminal_not_timeout() {
    // The underlying client's `begin_close` fans out one notification per
    // in-flight entry at once — bounded by the *client's* in-flight cap
    // (here 16), not the bridge's (here 1). The shim mailbox must be sized
    // to that client cap, or the live reply at the tail of the burst is
    // dropped and the awaiter settles only via the deadline backstop
    // (Timeout), losing its true ConnectionClosed cause.
    let client_cap = 16;
    let runtime = build_runtime();
    let stub = register_stub(&runtime, StubBehavior::CloseFanout { strays: client_cap });
    let bridge =
        BridgeClient::<SpecimenShard>::new(Arc::clone(&runtime), stub, 1, client_cap as usize)
            .unwrap();

    let outcome = tokio::time::timeout(
        Duration::from_secs(2),
        bridge.call(
            // Generous per-call deadline so the *only* way this settles as
            // Timeout is a dropped reply, not the deadline firing first.
            |corr, rt| {
                EchoClient::say_request("live".into(), Duration::from_secs(5), corr, rt, 1024)
            },
            |bytes| EchoClient::say_decode_reply(bytes, 1024),
        ),
    )
    .await
    .expect("bridge must settle the live call without hanging");

    assert_eq!(
        outcome,
        Err(BridgeError::ConnectionClosed),
        "live reply must settle with its true terminal (ConnectionClosed), not a backstop Timeout"
    );
    assert_eq!(
        bridge.available_slots(),
        1,
        "admission slot must be returned after the call settles"
    );
}

#[tokio::test]
async fn back_to_back_calls_at_capacity_one_never_spuriously_full() {
    // User-perspective e2e liveness at the #271 capacity edge.
    //
    // Bridge capacity is exactly 1. Each call is awaited to completion, then
    // the next is issued immediately, 2000 times against the same single-slot
    // edge. This proves the settle -> release -> re-admit cycle stays live
    // end-to-end and the slot is always back after a settled call.
    //
    // NOTE: this is coverage, not the #271 ordering regression proof. With
    // this synchronous stub the worker thread runs `settle_pending` to
    // completion (including the trailing `add_permits`) before the Tokio
    // awaiter is rescheduled, so the "permit still held at the wake instant"
    // race does not manifest here -- this test passes under BOTH slot
    // orderings. The genuine, disable-provable regression guard for the
    // ordering is `settle_returns_permit_before_reply_is_observable` in the
    // crate's unit tests, which pins the permit state at the exact
    // pre-`tx.send` instant.
    let runtime = build_runtime();
    let stub = register_stub(&runtime, StubBehavior::Echo);
    let bridge = BridgeClient::<SpecimenShard>::new(Arc::clone(&runtime), stub, 1, 64).unwrap();

    for i in 0..2_000i64 {
        let outcome: Result<(i64, i64), BridgeError> = bridge
            .call(
                |corr, rt| {
                    EchoClient::add_request(i, i + 1, Duration::from_secs(5), corr, rt, 1024)
                },
                |bytes| {
                    serde_json::from_slice(bytes).map_err(|_| tina_rpc::EncodingError::Decode {
                        encoder: "json",
                        kind: tina_rpc::EncodingErrorKind::Syntax,
                    })
                },
            )
            .await;
        assert_eq!(
            outcome,
            Ok((i, i + 1)),
            "back-to-back call at capacity 1 (iteration {i}) must be admitted and succeed, \
             never spuriously Full",
        );
    }

    // After the last settle the single permit must be back.
    assert_eq!(
        bridge.available_slots(),
        1,
        "admission slot must be returned after every settled call",
    );
}

#[tokio::test]
async fn parallel_calls_demux_correctly() {
    let runtime = build_runtime();
    let stub = register_stub(&runtime, StubBehavior::Echo);
    let bridge = BridgeClient::<SpecimenShard>::new(Arc::clone(&runtime), stub, 64, 64).unwrap();

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
