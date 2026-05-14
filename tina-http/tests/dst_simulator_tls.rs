//! Isolate-level DST: HttpClient over scripted TLS. Sim scripts the
//! connect outcome (Connected with HTTP/1.1 bytes, or a typed
//! failure); HttpClient parses the bytes / classifies the error.
//! Replay determinism is pinned via `stable_trace_hash`.

use std::cell::RefCell;
use std::convert::Infallible;
use std::rc::Rc;
use std::time::Duration;

use http::{Method, StatusCode};
use tina::ShardId;
use tina::prelude::*;
use tina_http::{
    HttpClient, HttpClientConfig, HttpClientError, HttpClientMsg, HttpRequest, HttpRequestBody,
    HttpResponse, HttpResponseBody, HttpTarget, HttpTransportPhase, TlsTrustRoots, encode_request,
};
use tina_runtime::{CallError, CallOutcome, RuntimeCall, call, stable_trace_hash};
use tina_sim::{
    ScriptedTlsConfig, ScriptedTlsConnectConfig, ScriptedTlsConnectResult, ScriptedTlsReadResult,
    ScriptedTlsWriteResult, Simulator, SimulatorConfig,
};

const SIM_SHARD_ID: u32 = 119;

#[derive(Debug, Default)]
struct SimShard;

impl Shard for SimShard {
    fn id(&self) -> ShardId {
        ShardId::new(SIM_SHARD_ID)
    }
}

/// One-shot driver: stashes the call outcome into the observation slot.
#[derive(Debug, Clone)]
enum DriverMsg {
    Begin {
        client: Address<HttpClientMsg, Result<HttpResponse, HttpClientError>>,
        target: HttpTarget,
        request: HttpRequest,
        timeout: Duration,
    },
    Returned(CallOutcome<Result<HttpResponse, HttpClientError>>),
}

struct Driver {
    observed: Rc<RefCell<Option<Result<HttpResponse, HttpClientError>>>>,
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
                client,
                target,
                request,
                timeout,
            } => call(client, HttpClientMsg::call(target, request), timeout)
                .then(DriverMsg::Returned),
            DriverMsg::Returned(outcome) => {
                let result = match outcome {
                    CallOutcome::Replied(inner) => inner,
                    CallOutcome::Full => Err(HttpClientError::Busy),
                    CallOutcome::Closed => Err(HttpClientError::Closed),
                    CallOutcome::Timeout => Err(HttpClientError::Timeout),
                    CallOutcome::Rejected(_) => Err(HttpClientError::Closed),
                };
                *self.observed.borrow_mut() = Some(result);
                stop()
            }
        }
    }
}

fn https_target() -> HttpTarget {
    HttpTarget::https(
        "127.0.0.1:48200".parse().unwrap(),
        "tls.local",
        TlsTrustRoots::from_der(vec![b"unused-by-sim".to_vec()]),
    )
}

fn canned_response() -> Vec<u8> {
    b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello".to_vec()
}

fn dst_request() -> HttpRequest {
    HttpRequest {
        method: Method::GET,
        path: "/sim".to_string(),
        version: http::Version::HTTP_11,
        headers: http::HeaderMap::new(),
        body: HttpRequestBody::default(),
    }
}

fn build_sim_config(target: &HttpTarget, request: &HttpRequest) -> SimulatorConfig {
    let HttpTarget::Https {
        addr,
        server_name,
        host,
        ..
    } = target.clone()
    else {
        unreachable!("DST target is HTTPS by construction")
    };
    // Mirror the client's Host policy here so the scripted Wrote
    // count matches the actual encoded request bytes.
    let mut planned = request.clone();
    let policy_value = match host {
        tina_http::HttpHostPolicy::UseServerName => server_name.clone(),
        tina_http::HttpHostPolicy::Explicit(name) => name,
    };
    planned.headers.insert(
        http::header::HOST,
        http::HeaderValue::from_str(&policy_value).expect("ascii host"),
    );
    let request_bytes = encode_request(&planned);

    SimulatorConfig {
        tls: ScriptedTlsConfig {
            pending_completion_capacity: 4,
            connects: vec![ScriptedTlsConnectConfig {
                addr,
                server_name,
                complete_after_step: 1,
                result: ScriptedTlsConnectResult::Connected {
                    reads: vec![ScriptedTlsReadResult::Bytes(canned_response())],
                    writes: vec![ScriptedTlsWriteResult::Wrote(request_bytes.len())],
                },
            }],
        },
        ..Default::default()
    }
}

fn run_dst() -> (u64, usize, Result<HttpResponse, HttpClientError>) {
    let target = https_target();
    let request = dst_request();
    let config = build_sim_config(&target, &request);

    let mut sim = Simulator::new(SimShard, config);
    let observed = Rc::new(RefCell::new(None));
    let client = sim
        .register_with_mailbox_capacity(HttpClient::<SimShard>::new(HttpClientConfig::dev()), 16);
    let driver = sim.register_with_mailbox_capacity(
        Driver {
            observed: Rc::clone(&observed),
        },
        8,
    );

    sim.try_send(
        driver,
        DriverMsg::Begin {
            client,
            target,
            request,
            timeout: Duration::from_secs(5),
        },
    )
    .expect("send Begin");
    sim.run_until_quiescent();

    let trace = sim.trace();
    let hash = stable_trace_hash(trace.iter());
    let len = trace.len();
    let result = observed
        .borrow_mut()
        .take()
        .expect("driver must publish observed result before quiescence");
    (hash, len, result)
}

#[test]
fn https_client_replays_through_scripted_tls() {
    let (_hash_a, _len_a, result) = run_dst();
    let response = result.expect("scripted https call returns a parsed response");
    assert_eq!(response.status, StatusCode::OK);
    let body = match &response.body {
        HttpResponseBody::Buffered(b) => b.clone(),
        HttpResponseBody::Stream(_)
        | HttpResponseBody::ChunkedStream(_)
        | HttpResponseBody::WebSocket(_) => Vec::new(),
    };
    assert_eq!(body, b"hello");
}

#[test]
fn https_client_dst_replay_is_deterministic() {
    let (hash_a, len_a, _) = run_dst();
    let (hash_b, len_b, _) = run_dst();
    assert_eq!(
        hash_a, hash_b,
        "stable_trace_hash must match across DST runs (lens: {len_a} vs {len_b})"
    );
    assert_eq!(len_a, len_b, "trace event count must match across DST runs");
}

/// Sim scripts the connect outcome; returns the client's classified result.
fn run_dst_with_connect_failure(
    failure: ScriptedTlsConnectResult,
) -> Result<HttpResponse, HttpClientError> {
    let target = https_target();
    let HttpTarget::Https {
        addr, server_name, ..
    } = target.clone()
    else {
        unreachable!()
    };
    let config = SimulatorConfig {
        tls: ScriptedTlsConfig {
            pending_completion_capacity: 4,
            connects: vec![ScriptedTlsConnectConfig {
                addr,
                server_name,
                complete_after_step: 1,
                result: failure,
            }],
        },
        ..Default::default()
    };

    let mut sim = Simulator::new(SimShard, config);
    let observed = Rc::new(RefCell::new(None));
    let client = sim
        .register_with_mailbox_capacity(HttpClient::<SimShard>::new(HttpClientConfig::dev()), 16);
    let driver = sim.register_with_mailbox_capacity(
        Driver {
            observed: Rc::clone(&observed),
        },
        8,
    );
    sim.try_send(
        driver,
        DriverMsg::Begin {
            client,
            target,
            request: dst_request(),
            timeout: Duration::from_secs(5),
        },
    )
    .expect("send Begin");
    sim.run_until_quiescent();
    observed
        .borrow_mut()
        .take()
        .expect("driver observed result")
}

#[test]
fn scripted_tls_certificate_yields_typed_transport_error() {
    let result = run_dst_with_connect_failure(ScriptedTlsConnectResult::Certificate);
    match result {
        Err(HttpClientError::Transport {
            phase: HttpTransportPhase::Connect,
            source: CallError::TlsCertificate,
        }) => {}
        other => panic!("expected Transport(Connect, TlsCertificate), got {other:?}"),
    }
}

#[test]
fn scripted_tls_name_yields_typed_transport_error() {
    let result = run_dst_with_connect_failure(ScriptedTlsConnectResult::Name);
    match result {
        Err(HttpClientError::Transport {
            phase: HttpTransportPhase::Connect,
            source: CallError::TlsName,
        }) => {}
        other => panic!("expected Transport(Connect, TlsName), got {other:?}"),
    }
}

#[test]
fn scripted_tls_timeout_yields_typed_transport_error() {
    let result = run_dst_with_connect_failure(ScriptedTlsConnectResult::Timeout);
    match result {
        // Either the connect timeout or the deadline timer fires
        // first; both surface as a typed Timeout, not flat Connect.
        Err(HttpClientError::Transport {
            phase: HttpTransportPhase::Connect,
            source: CallError::Timeout,
        }) => {}
        Err(HttpClientError::Timeout) => {}
        other => panic!("expected typed Timeout, got {other:?}"),
    }
}
