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
fn bridge_full_and_worker_full_are_distinct_variants() {
    // The crucial property: the flatten helper must never collapse the
    // two layers into the same variant. CallOutcome::Full (bridge) and
    // ReqwestError::Full (worker) both exist; flatten must keep them
    // distinguishable in the output.
    let bridge: ReqwestCallOutcome = CallOutcome::Full;
    let worker: ReqwestCallOutcome = CallOutcome::Replied(Err(ReqwestError::Full));

    let flat_bridge = flatten_outcome(bridge).unwrap_err();
    let flat_worker = flatten_outcome(worker).unwrap_err();

    assert_ne!(
        flat_bridge, flat_worker,
        "bridge::Full and worker::Full must remain distinguishable"
    );
    assert!(matches!(
        flat_bridge,
        ReqwestCallError::Bridge(BridgeFailure::Full)
    ));
    assert!(matches!(
        flat_worker,
        ReqwestCallError::Worker(ReqwestError::Full)
    ));
}

#[test]
fn bridge_timeout_and_worker_timeout_are_distinct_variants() {
    // Same property for the Timeout pair. The runtime's IsolateCall
    // deadline (CallOutcome::Timeout) and the worker's per-attempt
    // timeout (ReqwestError::Timeout) are different events.
    let bridge: ReqwestCallOutcome = CallOutcome::Timeout;
    let worker: ReqwestCallOutcome = CallOutcome::Replied(Err(ReqwestError::Timeout));

    let flat_bridge = flatten_outcome(bridge).unwrap_err();
    let flat_worker = flatten_outcome(worker).unwrap_err();

    assert_ne!(flat_bridge, flat_worker);
    assert!(matches!(
        flat_bridge,
        ReqwestCallError::Bridge(BridgeFailure::Timeout)
    ));
    assert!(matches!(
        flat_worker,
        ReqwestCallError::Worker(ReqwestError::Timeout)
    ));
}

#[test]
fn bridge_closed_and_worker_closed_are_distinct_variants() {
    // Same property for Closed: bridge "target gone" vs worker
    // "graceful drain via ReqwestCloser".
    let bridge: ReqwestCallOutcome = CallOutcome::Closed;
    let worker: ReqwestCallOutcome = CallOutcome::Replied(Err(ReqwestError::Closed));

    let flat_bridge = flatten_outcome(bridge).unwrap_err();
    let flat_worker = flatten_outcome(worker).unwrap_err();

    assert_ne!(flat_bridge, flat_worker);
}

#[test]
fn reqwest_call_error_implements_display_and_error() {
    let err = ReqwestCallError::Bridge(BridgeFailure::Timeout);
    let display = format!("{err}");
    assert!(display.contains("bridge"), "Display: {display}");

    let err = ReqwestCallError::Worker(ReqwestError::ResponseTooLarge);
    let display = format!("{err}");
    assert!(display.contains("worker"), "Display: {display}");

    let err: ReqwestCallError = ReqwestError::Timeout.into();
    let _: &dyn std::error::Error = &err;
    let _: Box<dyn std::error::Error + Send + Sync> = Box::new(err);
}
