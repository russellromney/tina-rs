//! Proofs that the Postgres bridge aligns to the shared
//! `tina_runtime::bridge` vocabulary.
//!
//! The richer typed `PgOutcomeClass` is retained for callers that want
//! the typed success payload; this suite proves that the bridge_class
//! projection lands in the right shared arm — Closed/PoolClosed are
//! `Unavailable`, unknown SQLx errors are `Fatal(SdkUnknown)`, pool
//! acquire timeout is `Retryable(PoolAcquireTimeout)`, etc.

use tina_runtime::CallOutcome;
use tina_runtime::bridge::{BridgeFatal, BridgeOutcomeClass, BridgeRetryable, BridgeUnavailable};
use tina_sqlx_bridge::{
    BridgePressure, PG_BRIDGE_SURFACE, PgError, PgPressureReport, bridge_class,
};

#[test]
fn closed_is_unavailable_not_retryable() {
    let closed: CallOutcome<Result<(), PgError>> = CallOutcome::Replied(Err(PgError::Closed));
    assert_eq!(
        bridge_class(&closed),
        BridgeOutcomeClass::Unavailable(BridgeUnavailable::BridgeClosed)
    );
    let bridge_closed: CallOutcome<Result<(), PgError>> = CallOutcome::Closed;
    assert_eq!(
        bridge_class(&bridge_closed),
        BridgeOutcomeClass::Unavailable(BridgeUnavailable::BridgeClosed)
    );
}

#[test]
fn pool_closed_is_unavailable_not_retryable() {
    let pc: CallOutcome<Result<(), PgError>> = CallOutcome::Replied(Err(PgError::PoolClosed));
    assert_eq!(
        bridge_class(&pc),
        BridgeOutcomeClass::Unavailable(BridgeUnavailable::PoolClosed)
    );
}

#[test]
fn pool_acquire_timeout_is_retryable_pool_acquire_timeout() {
    let pat: CallOutcome<Result<(), PgError>> =
        CallOutcome::Replied(Err(PgError::PoolAcquireTimeout));
    assert_eq!(
        bridge_class(&pat),
        BridgeOutcomeClass::Retryable(BridgeRetryable::PoolAcquireTimeout)
    );
}

#[test]
fn unknown_sqlx_error_is_fatal_sdk_unknown() {
    let sqlx: CallOutcome<Result<(), PgError>> =
        CallOutcome::Replied(Err(PgError::Sqlx("syntax".into())));
    assert_eq!(
        bridge_class(&sqlx),
        BridgeOutcomeClass::Fatal(BridgeFatal::SdkUnknown)
    );
}

#[test]
fn decode_is_fatal_decode() {
    let dec: CallOutcome<Result<(), PgError>> =
        CallOutcome::Replied(Err(PgError::Decode("bad".into())));
    assert_eq!(
        bridge_class(&dec),
        BridgeOutcomeClass::Fatal(BridgeFatal::Decode)
    );
}

#[test]
fn full_classifies_as_retryable_not_fatal_in_shared_vocab() {
    // The local PgFatalReason::BridgeFull historically classifies as
    // Fatal; the shared bridge vocabulary classifies `Full` as
    // Retryable(BridgeFull) — the caller may back off and try again.
    // The two views coexist; the shared view is the cross-bridge one.
    let full: CallOutcome<Result<(), PgError>> = CallOutcome::Full;
    assert_eq!(
        bridge_class(&full),
        BridgeOutcomeClass::Retryable(BridgeRetryable::BridgeFull)
    );
}

#[test]
fn pressure_report_renders_into_shared_bridge_pressure_with_installed_capacity() {
    let r = PgPressureReport {
        capacity: 8,
        available: 5,
        leased: 3,
        waiters: 0,
        max_waiters: 0,
        full_count: 1,
        closed_count: 0,
        timeout_count: 2,
        pool_acquire_timeout_count: 1,
        sql_error_count: 0,
        decode_error_count: 0,
        late_result_count: 4,
        high_water: 7,
    };
    let bp: BridgePressure = r.into();
    assert_eq!(bp.name(), PG_BRIDGE_SURFACE);
    assert_eq!(bp.capacity(), 8);
    assert_eq!(bp.current(), 3);
    assert_eq!(bp.high_water(), 7);
    assert_eq!(bp.full_count(), 1);
    // timeout counters fold together: bridge + pool acquire.
    assert_eq!(bp.timeout_count(), 3);
    assert_eq!(bp.late_result_count(), 4);
}

#[test]
fn pressure_unavailable_carries_explicit_reason() {
    // The plan says missing surfaces are reported, never silently
    // omitted. The shared vocab's `unavailable` constructor enforces
    // this — a bad name returns Err rather than letting the dashboard
    // drift.
    let s = BridgePressure::unavailable(PG_BRIDGE_SURFACE, "no client supplied").unwrap();
    assert_eq!(s.name, PG_BRIDGE_SURFACE);
    let line = s.discovery_line();
    assert!(line.contains("state=unavailable"));
    assert!(line.contains("reason=\"no client supplied\""));
}

// ---------------------------------------------------------------------
// Plan-required exhaustive coverage of remaining categories.
// ---------------------------------------------------------------------

#[test]
fn invalid_request_is_fatal_invalid_request() {
    let bad: CallOutcome<Result<(), PgError>> =
        CallOutcome::Replied(Err(PgError::InvalidRequest("empty sql".into())));
    assert_eq!(
        bridge_class(&bad),
        BridgeOutcomeClass::Fatal(BridgeFatal::InvalidRequest)
    );
}

#[test]
fn too_many_rows_is_fatal_invalid_request() {
    // FetchOne with multiple rows is a caller mistake; same shared
    // category as InvalidRequest.
    let tmr: CallOutcome<Result<(), PgError>> = CallOutcome::Replied(Err(PgError::TooManyRows));
    assert_eq!(
        bridge_class(&tmr),
        BridgeOutcomeClass::Fatal(BridgeFatal::InvalidRequest)
    );
}

#[test]
fn internal_error_is_fatal_internal() {
    let i: CallOutcome<Result<(), PgError>> =
        CallOutcome::Replied(Err(PgError::Internal("worker bug".into())));
    assert_eq!(
        bridge_class(&i),
        BridgeOutcomeClass::Fatal(BridgeFatal::Internal)
    );
}

#[test]
fn caller_timeout_is_retryable_caller_timeout() {
    let t: CallOutcome<Result<(), PgError>> = CallOutcome::Timeout;
    assert_eq!(
        bridge_class(&t),
        BridgeOutcomeClass::Retryable(BridgeRetryable::CallerTimeout)
    );
}

#[test]
fn worker_timeout_is_retryable_bridge_timeout() {
    let wt: CallOutcome<Result<(), PgError>> = CallOutcome::Replied(Err(PgError::Timeout));
    assert_eq!(
        bridge_class(&wt),
        BridgeOutcomeClass::Retryable(BridgeRetryable::BridgeTimeout)
    );
}

#[test]
fn rejected_is_fatal_invalid_request() {
    use tina::CallRejectedReason;
    let r: CallOutcome<Result<(), PgError>> =
        CallOutcome::Rejected(CallRejectedReason::UnsupportedMessage);
    assert_eq!(
        bridge_class(&r),
        BridgeOutcomeClass::Fatal(BridgeFatal::InvalidRequest)
    );
}

#[test]
fn succeeded_is_succeeded_for_each_typed_payload_outcome() {
    // bridge_class is generic over the inner Ok payload. Verify that
    // each typed reply-projection (PgExecutedOutcome → u64,
    // PgFetchOneOutcome → Option<PgRow>, etc.) lands at Succeeded.
    let executed: CallOutcome<Result<u64, PgError>> = CallOutcome::Replied(Ok(42));
    assert_eq!(bridge_class(&executed), BridgeOutcomeClass::Succeeded);

    let fetched_some: CallOutcome<Result<Option<()>, PgError>> = CallOutcome::Replied(Ok(Some(())));
    assert_eq!(bridge_class(&fetched_some), BridgeOutcomeClass::Succeeded);

    let fetched_none: CallOutcome<Result<Option<()>, PgError>> = CallOutcome::Replied(Ok(None));
    assert_eq!(bridge_class(&fetched_none), BridgeOutcomeClass::Succeeded);
}

#[test]
fn ext_trait_bridge_class_matches_free_function() {
    // The PgOutcomeExt::bridge_class() method must agree with the
    // free function; both go through `bridge_class()` under the hood.
    use tina_sqlx_bridge::PgOutcomeExt;
    let outcome: CallOutcome<Result<(), PgError>> = CallOutcome::Replied(Err(PgError::PoolClosed));
    // Trait method on PgExecutedOutcome shape — proves the projection
    // works through the typed extension trait too.
    let executed_pc: CallOutcome<Result<u64, PgError>> =
        CallOutcome::Replied(Err(PgError::PoolClosed));
    assert_eq!(bridge_class(&outcome), executed_pc.bridge_class());
}
