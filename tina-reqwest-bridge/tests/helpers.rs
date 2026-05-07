//! Tests for the polish helpers: type aliases, send_request, flatten_outcome.

use tina_reqwest_bridge::{
    BridgeFailure, ReqwestCallError, ReqwestCallOutcome, ReqwestError, ReqwestResponse,
    flatten_outcome,
};
use tina_runtime::CallOutcome;

fn ok_response() -> ReqwestResponse {
    ReqwestResponse {
        status: http::StatusCode::OK,
        headers: http::HeaderMap::new(),
        body: b"hi".to_vec(),
    }
}

#[test]
fn flatten_replied_ok_passes_through_response() {
    let outcome: ReqwestCallOutcome = CallOutcome::Replied(Ok(ok_response()));
    match flatten_outcome(outcome) {
        Ok(response) => assert_eq!(response.body, b"hi"),
        other => panic!("expected Ok, got {other:?}"),
    }
}

#[test]
fn flatten_replied_err_becomes_worker_layer() {
    let outcome: ReqwestCallOutcome = CallOutcome::Replied(Err(ReqwestError::Timeout));
    match flatten_outcome(outcome) {
        Err(ReqwestCallError::Worker(ReqwestError::Timeout)) => {}
        other => panic!("expected Err(Worker(Timeout)), got {other:?}"),
    }
}

#[test]
fn flatten_call_outcome_full_becomes_bridge_layer_full() {
    let outcome: ReqwestCallOutcome = CallOutcome::Full;
    match flatten_outcome(outcome) {
        Err(ReqwestCallError::Bridge(BridgeFailure::Full)) => {}
        other => panic!("expected Err(Bridge(Full)), got {other:?}"),
    }
}

#[test]
fn flatten_call_outcome_closed_becomes_bridge_layer_closed() {
    let outcome: ReqwestCallOutcome = CallOutcome::Closed;
    match flatten_outcome(outcome) {
        Err(ReqwestCallError::Bridge(BridgeFailure::Closed)) => {}
        other => panic!("expected Err(Bridge(Closed)), got {other:?}"),
    }
}

#[test]
fn flatten_call_outcome_timeout_becomes_bridge_layer_timeout() {
    let outcome: ReqwestCallOutcome = CallOutcome::Timeout;
    match flatten_outcome(outcome) {
        Err(ReqwestCallError::Bridge(BridgeFailure::Timeout)) => {}
        other => panic!("expected Err(Bridge(Timeout)), got {other:?}"),
    }
}

#[test]
fn bridge_full_and_worker_full_have_distinguishable_match_arms() {
    // The crucial property: a caller pattern-matching on the flat
    // error must be able to tell bridge-layer Full apart from
    // worker-layer Full. Different match arms, different log lines.
    let bridge_flat = flatten_outcome(CallOutcome::Full).unwrap_err();
    let worker_flat = flatten_outcome(CallOutcome::Replied(Err(ReqwestError::Full))).unwrap_err();

    assert!(matches!(
        bridge_flat,
        ReqwestCallError::Bridge(BridgeFailure::Full)
    ));
    assert!(matches!(
        worker_flat,
        ReqwestCallError::Worker(ReqwestError::Full)
    ));

    // Display strings keep the layer name so log lines stay readable.
    let bridge_display = format!("{bridge_flat}");
    let worker_display = format!("{worker_flat}");
    assert!(bridge_display.contains("bridge"), "{bridge_display}");
    assert!(worker_display.contains("worker"), "{worker_display}");
    assert_ne!(bridge_display, worker_display);
}

#[test]
fn bridge_timeout_and_worker_timeout_have_distinguishable_match_arms() {
    let bridge_flat = flatten_outcome(CallOutcome::Timeout).unwrap_err();
    let worker_flat =
        flatten_outcome(CallOutcome::Replied(Err(ReqwestError::Timeout))).unwrap_err();
    assert!(matches!(
        bridge_flat,
        ReqwestCallError::Bridge(BridgeFailure::Timeout)
    ));
    assert!(matches!(
        worker_flat,
        ReqwestCallError::Worker(ReqwestError::Timeout)
    ));
}

#[test]
fn bridge_closed_and_worker_closed_have_distinguishable_match_arms() {
    let bridge_flat = flatten_outcome(CallOutcome::Closed).unwrap_err();
    let worker_flat = flatten_outcome(CallOutcome::Replied(Err(ReqwestError::Closed))).unwrap_err();
    assert!(matches!(
        bridge_flat,
        ReqwestCallError::Bridge(BridgeFailure::Closed)
    ));
    assert!(matches!(
        worker_flat,
        ReqwestCallError::Worker(ReqwestError::Closed)
    ));
}

#[test]
fn reqwest_call_error_implements_display_and_error() {
    let err = ReqwestCallError::Bridge(BridgeFailure::Timeout);
    let display = format!("{err}");
    assert!(display.contains("bridge"), "Display: {display}");
    assert!(
        !display.contains("worker"),
        "bridge layer Display must not name the worker layer: {display}"
    );

    let err = ReqwestCallError::Worker(ReqwestError::ResponseTooLarge);
    let display = format!("{err}");
    assert!(display.contains("worker"), "Display: {display}");
    assert!(
        !display.contains("bridge:"),
        "worker layer Display must not double-prefix with bridge: {display}"
    );

    let err: ReqwestCallError = ReqwestError::Timeout.into();
    let _: &dyn std::error::Error = &err;
    let _: Box<dyn std::error::Error + Send + Sync> = Box::new(err);
}

#[test]
fn from_bridge_failure_into_reqwest_call_error() {
    let err: ReqwestCallError = BridgeFailure::Closed.into();
    assert!(matches!(
        err,
        ReqwestCallError::Bridge(BridgeFailure::Closed)
    ));
}

#[test]
fn from_reqwest_error_into_reqwest_call_error() {
    let err: ReqwestCallError = ReqwestError::ResponseTooLarge.into();
    assert!(matches!(
        err,
        ReqwestCallError::Worker(ReqwestError::ResponseTooLarge)
    ));
}
