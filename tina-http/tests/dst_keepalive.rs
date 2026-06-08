//! Isolate-level DST for [`tina_http::KeepaliveConnection`]. Drives
//! the keepalive client through `tina_sim::Simulator` with a scripted
//! TCP listener. Asserts:
//!
//! - same scripted config → byte-identical trace fingerprint across
//!   two independent runs (state machine is deterministic).
//! - the driver observes a successful response on each scripted run.
//!
//! The keepalive isolate uses generation-tagged deadlines and a
//! lazy-connect / always-Reuse protocol; if any of those introduce
//! non-determinism (timer ordering, hashmap iteration, etc.) the
//! trace fingerprint diverges and this test fails.

#![allow(dead_code)]

use std::cell::RefCell;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::Duration;

use http::StatusCode;
use tina::ShardId;
use tina::prelude::*;
use tina_http::{
    HttpClientConfig, HttpRequest, HttpTarget, KeepaliveConnection, KeepaliveConnectionMsg,
    KeepaliveOutcome,
};
use tina_runtime::{CallOutcome, RuntimeCall, call, stable_trace_hash};
use tina_sim::{
    ScriptedListenerConfig, ScriptedPeerConfig, ScriptedTcpConfig, Simulator, SimulatorConfig,
};

const SIM_SHARD_ID: u32 = 121;

#[derive(Debug, Default)]
struct SimShard;

impl Shard for SimShard {
    fn id(&self) -> ShardId {
        ShardId::new(SIM_SHARD_ID)
    }
}

#[derive(Debug, Clone)]
enum DriverMsg {
    Begin {
        conn: Address<KeepaliveConnectionMsg, KeepaliveOutcome>,
        request: HttpRequest,
        timeout: Duration,
    },
    Returned(CallOutcome<KeepaliveOutcome>),
}

struct Driver {
    observed: Rc<RefCell<Option<KeepaliveOutcome>>>,
}

impl Isolate for Driver {
    tina::isolate_types! {
        message: DriverMsg,
        reply: (),
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<DriverMsg>,
        shard: SimShard,
    }

    fn handle(
        &mut self,
        msg: DriverMsg,
        _ctx: &mut Context<'_, SimShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            DriverMsg::Begin {
                conn,
                request,
                timeout,
            } => call(
                conn,
                KeepaliveConnectionMsg::request(request, timeout),
                timeout,
            )
            .then(DriverMsg::Returned),
            DriverMsg::Returned(outcome) => {
                if let CallOutcome::Replied(outcome) = outcome {
                    *self.observed.borrow_mut() = Some(outcome);
                }
                noop()
            }
        }
    }
}

fn target_addr() -> SocketAddr {
    "127.0.0.1:18080".parse().unwrap()
}

fn canned_response() -> Vec<u8> {
    b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello".to_vec()
}

fn build_config() -> SimulatorConfig {
    let addr = target_addr();
    SimulatorConfig {
        seed: 0xDEAD_BEEF_DEAD_BEEF,
        tcp: ScriptedTcpConfig {
            pending_completion_capacity: 16,
            listeners: vec![ScriptedListenerConfig {
                bind_addr: addr,
                local_addr: addr,
                backlog_capacity: 4,
                peers: vec![ScriptedPeerConfig {
                    accept_after_step: 0,
                    peer_addr: "127.0.0.2:54321".parse().unwrap(),
                    inbound_chunks: vec![canned_response()],
                    inbound_capacity: 4096,
                    read_chunk_cap: None,
                    write_cap: 4096,
                    output_capacity: 4096,
                }],
            }],
        },
        ..Default::default()
    }
}

fn run_pass() -> (u64, usize, KeepaliveOutcome) {
    let config = build_config();
    let target = HttpTarget::http_with_host(target_addr(), "x");
    let request = HttpRequest::get("/").build();

    let mut sim = Simulator::new(SimShard, config);
    let conn_addr = sim.register_with_mailbox_capacity(
        KeepaliveConnection::<SimShard>::new(target, HttpClientConfig::dev()),
        16,
    );
    let observed = Rc::new(RefCell::new(None));
    let driver = sim.register_with_mailbox_capacity(
        Driver {
            observed: Rc::clone(&observed),
        },
        8,
    );
    sim.try_send(
        driver,
        DriverMsg::Begin {
            conn: conn_addr,
            request,
            timeout: Duration::from_secs(5),
        },
    )
    .expect("send Begin");
    sim.run_until_quiescent();

    let trace = sim.trace();
    let hash = stable_trace_hash(trace.iter());
    let len = trace.len();
    let outcome = observed
        .borrow_mut()
        .take()
        .expect("driver observed an outcome");
    (hash, len, outcome)
}

#[test]
fn keepalive_connection_replays_byte_identical() {
    let (hash_a, len_a, outcome_a) = run_pass();
    let (hash_b, len_b, _outcome_b) = run_pass();
    assert_eq!(
        hash_a, hash_b,
        "two passes against the same scripted config must produce identical trace fingerprints"
    );
    assert_eq!(len_a, len_b, "trace event count must match across replays");

    match outcome_a {
        KeepaliveOutcome::Request {
            result: Ok(response),
            must_retire,
        } => {
            assert_eq!(response.status, StatusCode::OK);
            assert!(!must_retire, "scripted server did not signal close");
            let body = response.body.as_buffered().unwrap_or_default().to_vec();
            assert_eq!(body, b"hello");
        }
        other => panic!("expected successful request reply, got {other:?}"),
    }
}
