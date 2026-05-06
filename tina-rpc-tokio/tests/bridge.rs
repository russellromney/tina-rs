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
struct EiffelShard;

impl tina::Shard for EiffelShard {
    fn id(&self) -> ShardId {
        ShardId::new(95)
    }
}

// Tiny VecDeque mailbox factory matching the eiffel example pattern.
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
}

impl Isolate for ClientStub {
    type Message = ClientMsg;
    type Reply = ();
    type Send = Outbound<ClientResultMsg>;
    type Spawn = Infallible;
    type Call = RuntimeCall<ClientMsg>;
    type Shard = EiffelShard;

    fn handle(&mut self, msg: ClientMsg, _ctx: &mut Context<'_, EiffelShard>) -> Effect<Self> {
        match msg {
            ClientMsg::Request(req) => {
                let result = match self.behavior {
                    StubBehavior::Echo => ClientResult::Ok(req.payload.clone()),
                    StubBehavior::AlwaysFull => ClientResult::Full,
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

fn build_runtime() -> Arc<ThreadedRuntime<EiffelShard, EFactory>> {
    Arc::new(ThreadedRuntime::with_config(
        EiffelShard,
        EFactory,
        ThreadedRuntimeConfig {
            command_capacity: 32,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    ))
}

fn register_stub(
    runtime: &ThreadedRuntime<EiffelShard, EFactory>,
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
    let bridge = BridgeClient::<EiffelShard>::new(Arc::clone(&runtime), stub, 64).unwrap();

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
    let bridge = BridgeClient::<EiffelShard>::new(Arc::clone(&runtime), stub, 16).unwrap();

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
    let bridge = BridgeClient::<EiffelShard>::new(Arc::clone(&runtime), stub, 16).unwrap();

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
async fn parallel_calls_demux_correctly() {
    let runtime = build_runtime();
    let stub = register_stub(&runtime, StubBehavior::Echo);
    let bridge = BridgeClient::<EiffelShard>::new(Arc::clone(&runtime), stub, 64).unwrap();

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
