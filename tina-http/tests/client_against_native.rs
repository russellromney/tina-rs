//! End-to-end integration: service-shaped [`HttpClient`] talking to a
//! native [`HttpListener`] on the same runtime process.
//!
//! Proves the call-shaped client composes with the native server: no
//! Tokio, no stdlib reference, just two isolate trees over real
//! loopback. Drives the same Counter scenario `specimen_native_http`
//! uses scripted-stdlib for, but every TCP byte on both sides is
//! Tina-owned.

mod common;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::Duration;

use http::StatusCode;
use tina::prelude::*;
use tina_http::{
    HttpClient, HttpClientConfig, HttpClientError, HttpClientMsg, HttpRequest, HttpResponse,
};
use tina_runtime::{CallOutcome, DefaultThreadedMailboxFactory, ThreadedRuntime};

use common::TestShard;

fn map_outcome(
    outcome: CallOutcome<Result<HttpResponse, HttpClientError>>,
) -> Result<HttpResponse, HttpClientError> {
    match outcome {
        CallOutcome::Replied(inner) => inner,
        CallOutcome::Full => Err(HttpClientError::Busy),
        CallOutcome::Closed => Err(HttpClientError::Closed),
        CallOutcome::Timeout => Err(HttpClientError::Timeout),
        CallOutcome::Rejected(_) => Err(HttpClientError::Closed),
    }
}

fn run_request(
    runtime: &ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory>,
    client: Address<HttpClientMsg, Result<HttpResponse, HttpClientError>>,
    target: SocketAddr,
    request: HttpRequest,
) -> Result<HttpResponse, HttpClientError> {
    map_outcome(
        runtime
            .call_blocking(
                client,
                HttpClientMsg::call(target, request),
                Duration::from_secs(5),
            )
            .expect("call runs"),
    )
}

#[test]
fn native_client_against_native_server_drives_counter() {
    let harness = common::TestHarness::start();
    let target = harness.addr;
    let runtime = harness.runtime_handle();

    let client = runtime
        .register_with_capacity::<HttpClient<TestShard>, Infallible>(
            HttpClient::<TestShard>::new(HttpClientConfig::dev()),
            16,
        )
        .expect("register client");

    let r0 = run_request(
        runtime,
        client,
        target,
        HttpRequest::get("/counter").header("Host", "x").build(),
    )
    .expect("initial GET");
    assert_eq!(r0.status, StatusCode::OK);
    assert_eq!(r0.body, b"0");

    for expected in 1..=3 {
        let r = run_request(
            runtime,
            client,
            target,
            HttpRequest::post("/counter").header("Host", "x").build(),
        )
        .expect("POST");
        assert_eq!(r.status, StatusCode::OK);
        assert_eq!(
            r.body,
            expected.to_string().into_bytes(),
            "POST #{expected} should return {expected}"
        );
    }

    let r4 = run_request(
        runtime,
        client,
        target,
        HttpRequest::get("/counter").header("Host", "x").build(),
    )
    .expect("final GET");
    assert_eq!(r4.body, b"3");

    let r5 = run_request(
        runtime,
        client,
        target,
        HttpRequest::get("/missing").header("Host", "x").build(),
    )
    .expect("404 GET");
    assert_eq!(r5.status, StatusCode::NOT_FOUND);

    harness.shutdown();
}
