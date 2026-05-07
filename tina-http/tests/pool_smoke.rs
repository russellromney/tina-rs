//! Pool primitive tests for the call-shaped HTTP pool.
//!
//! Exercises capacity-1 admission semantics: free-slot Submits forward
//! to the underlying client and reply with the response; busy-slot
//! Submits reply immediately with `PoolFull`. The reply path is the
//! same single deferred-reply pattern the client itself uses.

mod common;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::mpsc;
use std::time::Duration;

use http::StatusCode;
use tina::prelude::*;
use tina_http::{
    HttpClient, HttpClientConfig, HttpClientError, HttpConnectionPool, HttpPoolMsg, HttpRequest,
    HttpResponse, OutboundCall, PoolConfig,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, RuntimeCall, ThreadedRuntime, call,
};

use common::TestShard;

#[derive(Debug, Clone)]
enum DriverMsg {
    Begin {
        pool: Address<HttpPoolMsg, Result<HttpResponse, HttpClientError>>,
        outbound: OutboundCall,
        timeout: Duration,
    },
    Returned(CallOutcome<Result<HttpResponse, HttpClientError>>),
}

struct Driver {
    sender: mpsc::Sender<Result<HttpResponse, HttpClientError>>,
}

impl Isolate for Driver {
    tina::isolate_types! {
        message: DriverMsg,
        reply: (),
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<DriverMsg>,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        msg: DriverMsg,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            DriverMsg::Begin {
                pool,
                outbound,
                timeout,
            } => call(pool, HttpPoolMsg::Submit(outbound), timeout).reply(DriverMsg::Returned),
            DriverMsg::Returned(outcome) => {
                let result = match outcome {
                    CallOutcome::Replied(inner) => inner,
                    CallOutcome::Full => Err(HttpClientError::Busy),
                    CallOutcome::Closed => Err(HttpClientError::Closed),
                    CallOutcome::Timeout => Err(HttpClientError::Timeout),
                };
                let _ = self.sender.send(result);
                stop()
            }
        }
    }
}

fn register_pool(
    runtime: &ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory>,
    config: PoolConfig,
) -> Address<HttpPoolMsg, Result<HttpResponse, HttpClientError>> {
    let client = runtime
        .register_with_capacity::<HttpClient<TestShard>, Infallible>(
            HttpClient::<TestShard>::new(HttpClientConfig::dev()),
            16,
        )
        .expect("register client");
    runtime
        .register_with_capacity::<HttpConnectionPool<TestShard>, Infallible>(
            HttpConnectionPool::<TestShard>::new(config, client),
            config.mailbox_capacity,
        )
        .expect("register pool")
}

fn submit_via(
    runtime: &ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory>,
    pool: Address<HttpPoolMsg, Result<HttpResponse, HttpClientError>>,
    target: SocketAddr,
    request: HttpRequest,
    sender: mpsc::Sender<Result<HttpResponse, HttpClientError>>,
) {
    let driver = runtime
        .register_with_capacity::<Driver, Infallible>(Driver { sender }, 16)
        .expect("register driver");
    runtime
        .try_send(
            driver,
            DriverMsg::Begin {
                pool,
                outbound: OutboundCall { target, request },
                timeout: Duration::from_secs(2),
            },
        )
        .expect("send Begin");
}

#[test]
#[should_panic(expected = "first-form HttpConnectionPool requires capacity = 1")]
fn pool_panics_when_capacity_not_one() {
    // The panic fires in the constructor; we never run this through a
    // runtime. Use a fake address with the correct typed parameters.
    let fake_client: Address<tina_http::HttpClientMsg, Result<HttpResponse, HttpClientError>> =
        Address::new_with_generation(
            ShardId::new(common::TEST_SHARD_ID),
            tina::IsolateId::new(0),
            tina::AddressGeneration::new(0),
        );
    let _: HttpConnectionPool<TestShard> = HttpConnectionPool::<TestShard>::new(
        PoolConfig {
            capacity: 4,
            client_call_timeout: Duration::from_secs(1),
            mailbox_capacity: 16,
        },
        fake_client,
    );
}

#[test]
fn pool_passes_request_through_when_slot_free() {
    let harness = common::TestHarness::start();
    let target = harness.addr;
    let runtime = harness.runtime_handle();

    let pool = register_pool(runtime, PoolConfig::dev());

    let (tx, rx) = mpsc::channel();
    submit_via(
        runtime,
        pool,
        target,
        HttpRequest::get("/counter").header("Host", "x").build(),
        tx,
    );

    let response = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("driver receives result")
        .expect("pool delivers response");
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body, b"0");

    harness.shutdown();
}

#[test]
fn pool_drains_two_serial_submits_correctly() {
    let harness = common::TestHarness::start();
    let target = harness.addr;
    let runtime = harness.runtime_handle();

    let pool = register_pool(runtime, PoolConfig::dev());

    // First Submit: post.
    let (tx1, rx1) = mpsc::channel();
    submit_via(
        runtime,
        pool,
        target,
        HttpRequest::post("/counter").header("Host", "x").build(),
        tx1,
    );
    let r1 = rx1
        .recv_timeout(Duration::from_secs(5))
        .expect("first reply")
        .expect("first ok");
    assert_eq!(r1.body, b"1");

    // Second Submit: post (slot is now free again).
    let (tx2, rx2) = mpsc::channel();
    submit_via(
        runtime,
        pool,
        target,
        HttpRequest::post("/counter").header("Host", "x").build(),
        tx2,
    );
    let r2 = rx2
        .recv_timeout(Duration::from_secs(5))
        .expect("second reply")
        .expect("second ok");
    assert_eq!(r2.body, b"2");

    harness.shutdown();
}

#[test]
fn pool_refuses_with_full_when_slot_busy() {
    // First Submit points at a black-hole TCP server that accepts but
    // never writes — the underlying client will tie up the pool slot
    // until its request_timeout fires. While the slot is busy, the
    // second Submit must come back as `PoolFull` immediately.
    use std::net::TcpListener as StdTcpListener;
    use std::thread;

    let black_hole = StdTcpListener::bind("127.0.0.1:0").expect("bind black-hole");
    let slow_target = black_hole.local_addr().expect("local addr");
    let _silent = thread::spawn(move || {
        if let Ok((stream, _)) = black_hole.accept() {
            // Hold the connection for a while, then drop. The pool's
            // first Submit will see Timeout (or Read on close) eventually.
            thread::sleep(Duration::from_secs(3));
            drop(stream);
        }
    });

    let harness = common::TestHarness::start();
    let runtime = harness.runtime_handle();

    let pool = register_pool(runtime, PoolConfig::dev());

    // Submit_A: against the black hole. Will hold the pool slot until
    // its client_call_timeout (10s) fires. We never observe the
    // result before our test deadline; this is fine.
    let (tx_a, _rx_a) = mpsc::channel();
    submit_via(
        runtime,
        pool,
        slow_target,
        HttpRequest::get("/").header("Host", "x").build(),
        tx_a,
    );

    // Brief pause so Submit_A reaches the pool and sets in_flight =
    // true. With single-shard scheduling this is deterministic after
    // the first handler turn — a couple of milliseconds is plenty.
    thread::sleep(Duration::from_millis(50));

    // Submit_B: now expect PoolFull.
    let (tx_b, rx_b) = mpsc::channel();
    submit_via(
        runtime,
        pool,
        harness.addr,
        HttpRequest::get("/counter").header("Host", "x").build(),
        tx_b,
    );

    let r_b = rx_b
        .recv_timeout(Duration::from_secs(2))
        .expect("Submit_B reply");
    assert!(
        matches!(r_b, Err(HttpClientError::PoolFull)),
        "expected PoolFull while slot is busy, got {r_b:?}"
    );

    harness.shutdown();
}
