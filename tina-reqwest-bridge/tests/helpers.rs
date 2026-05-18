//! Tests for the polish helpers: type aliases, send_request, flatten_outcome.

use tina_reqwest_bridge::{
    BridgeFailure, ReqwestCallError, ReqwestCallOutcome, ReqwestError, ReqwestFatalReason,
    ReqwestOutcomeClass, ReqwestOutcomeExt, ReqwestResponse, ReqwestTransientReason,
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

// ---------- classify (Phase 062 Rock 6) ----------

fn response_with_status(status: http::StatusCode) -> ReqwestResponse {
    ReqwestResponse {
        status,
        headers: http::HeaderMap::new(),
        body: Vec::new(),
    }
}

#[test]
fn classify_2xx_is_succeeded() {
    let outcome: ReqwestCallOutcome =
        CallOutcome::Replied(Ok(response_with_status(http::StatusCode::OK)));
    match outcome.classify() {
        ReqwestOutcomeClass::Succeeded(resp) => assert_eq!(resp.status, http::StatusCode::OK),
        other => panic!("expected Succeeded, got {other:?}"),
    }
}

#[test]
fn classify_503_is_transient_upstream_server() {
    let outcome: ReqwestCallOutcome = CallOutcome::Replied(Ok(response_with_status(
        http::StatusCode::SERVICE_UNAVAILABLE,
    )));
    match outcome.classify() {
        ReqwestOutcomeClass::Transient(ReqwestTransientReason::UpstreamServer { status }) => {
            assert_eq!(status, http::StatusCode::SERVICE_UNAVAILABLE);
        }
        other => panic!("expected Transient(UpstreamServer), got {other:?}"),
    }
}

#[test]
fn classify_404_is_fatal_upstream_client() {
    let outcome: ReqwestCallOutcome =
        CallOutcome::Replied(Ok(response_with_status(http::StatusCode::NOT_FOUND)));
    match outcome.classify() {
        ReqwestOutcomeClass::Fatal(ReqwestFatalReason::UpstreamClient { status }) => {
            assert_eq!(status, http::StatusCode::NOT_FOUND);
        }
        other => panic!("expected Fatal(UpstreamClient), got {other:?}"),
    }
}

#[test]
fn classify_bridge_timeout_distinct_from_worker_timeout() {
    let bridge: ReqwestCallOutcome = CallOutcome::Timeout;
    let worker: ReqwestCallOutcome = CallOutcome::Replied(Err(ReqwestError::Timeout));
    assert!(matches!(
        bridge.classify(),
        ReqwestOutcomeClass::Transient(ReqwestTransientReason::BridgeTimeout)
    ));
    assert!(matches!(
        worker.classify(),
        ReqwestOutcomeClass::Transient(ReqwestTransientReason::WorkerTimeout)
    ));
}

#[test]
fn classify_reqwest_transport_error_preserves_underlying_message() {
    let outcome: ReqwestCallOutcome =
        CallOutcome::Replied(Err(ReqwestError::Reqwest("connect refused".into())));
    match outcome.classify() {
        ReqwestOutcomeClass::Transient(ReqwestTransientReason::WorkerTransport(msg)) => {
            assert_eq!(msg, "connect refused");
        }
        other => panic!("expected Transient(WorkerTransport(..)), got {other:?}"),
    }
}

#[test]
fn classify_worker_internal_is_fatal() {
    let outcome: ReqwestCallOutcome =
        CallOutcome::Replied(Err(ReqwestError::Internal("task vanished".into())));
    match outcome.classify() {
        ReqwestOutcomeClass::Fatal(ReqwestFatalReason::WorkerInternal(msg)) => {
            assert_eq!(msg, "task vanished");
        }
        other => panic!("expected Fatal(WorkerInternal(..)), got {other:?}"),
    }
}

#[test]
fn classify_bridge_full_and_closed_are_fatal() {
    assert!(matches!(
        CallOutcome::<Result<ReqwestResponse, ReqwestError>>::Full.classify(),
        ReqwestOutcomeClass::Fatal(ReqwestFatalReason::BridgeFull)
    ));
    assert!(matches!(
        CallOutcome::<Result<ReqwestResponse, ReqwestError>>::Closed.classify(),
        ReqwestOutcomeClass::Fatal(ReqwestFatalReason::BridgeClosed)
    ));
}

#[test]
fn classify_worker_full_and_closed_are_fatal() {
    let full: ReqwestCallOutcome = CallOutcome::Replied(Err(ReqwestError::Full));
    let closed: ReqwestCallOutcome = CallOutcome::Replied(Err(ReqwestError::Closed));
    assert!(matches!(
        full.classify(),
        ReqwestOutcomeClass::Fatal(ReqwestFatalReason::WorkerFull)
    ));
    assert!(matches!(
        closed.classify(),
        ReqwestOutcomeClass::Fatal(ReqwestFatalReason::WorkerClosed)
    ));
}

#[test]
fn classify_size_caps_and_invalid_request_are_fatal() {
    let req_large: ReqwestCallOutcome = CallOutcome::Replied(Err(ReqwestError::RequestTooLarge));
    let resp_large: ReqwestCallOutcome = CallOutcome::Replied(Err(ReqwestError::ResponseTooLarge));
    let invalid: ReqwestCallOutcome =
        CallOutcome::Replied(Err(ReqwestError::InvalidRequest("bad url".into())));

    assert!(matches!(
        req_large.classify(),
        ReqwestOutcomeClass::Fatal(ReqwestFatalReason::RequestTooLarge)
    ));
    assert!(matches!(
        resp_large.classify(),
        ReqwestOutcomeClass::Fatal(ReqwestFatalReason::ResponseTooLarge)
    ));
    match invalid.classify() {
        ReqwestOutcomeClass::Fatal(ReqwestFatalReason::InvalidRequest(msg)) => {
            assert_eq!(msg, "bad url");
        }
        other => panic!("expected Fatal(InvalidRequest(..)), got {other:?}"),
    }
}

#[test]
fn classify_transient_reason_debug_includes_underlying_detail() {
    // The principal log shape is `tracing::warn!("{reason:?}")` —
    // make sure the Debug output isn't a bare unit-variant name when
    // the underlying error carried a message.
    let outcome: ReqwestCallOutcome =
        CallOutcome::Replied(Err(ReqwestError::Reqwest("dns lookup failed".into())));
    let debug = format!("{:?}", outcome.classify());
    assert!(
        debug.contains("dns lookup failed"),
        "Debug must surface the underlying transport message: {debug}"
    );
}
