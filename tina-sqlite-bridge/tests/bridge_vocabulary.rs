//! Proofs that the SQLite bridge aligns to the shared
//! `tina_runtime::bridge` vocabulary. The serial-bridge case is the
//! interesting one: capacity is `1`, not "absent" — pressure reports
//! still surface that honestly.

use tina_runtime::CallOutcome;
use tina_runtime::bridge::{BridgeFatal, BridgeOutcomeClass, BridgeRetryable, BridgeUnavailable};
use tina_sqlite_bridge::{
    BridgePressure, SQLITE_BRIDGE_SURFACE, SqliteError, SqlitePressureReport, sqlite_bridge_class,
};

#[test]
fn closed_is_unavailable_not_retryable() {
    let closed: CallOutcome<Result<(), SqliteError>> =
        CallOutcome::Replied(Err(SqliteError::Closed));
    assert_eq!(
        sqlite_bridge_class(&closed),
        BridgeOutcomeClass::Unavailable(BridgeUnavailable::BridgeClosed)
    );
    let bridge_closed: CallOutcome<Result<(), SqliteError>> = CallOutcome::Closed;
    assert_eq!(
        sqlite_bridge_class(&bridge_closed),
        BridgeOutcomeClass::Unavailable(BridgeUnavailable::BridgeClosed)
    );
}

#[test]
fn busy_is_retryable_service_throttled() {
    // `SQLITE_BUSY` is SQLite's "back off" signal — it lands in the
    // retryable arm. The caller decides if their idempotency rules
    // allow a retry.
    let busy: CallOutcome<Result<(), SqliteError>> = CallOutcome::Replied(Err(SqliteError::Busy));
    assert_eq!(
        sqlite_bridge_class(&busy),
        BridgeOutcomeClass::Retryable(BridgeRetryable::ServiceThrottled)
    );
}

#[test]
fn constraint_is_fatal_invalid_request() {
    let c: CallOutcome<Result<(), SqliteError>> =
        CallOutcome::Replied(Err(SqliteError::Constraint("uniq".into())));
    assert_eq!(
        sqlite_bridge_class(&c),
        BridgeOutcomeClass::Fatal(BridgeFatal::InvalidRequest)
    );
}

#[test]
fn unknown_sqlite_error_is_fatal_sdk_unknown() {
    let s: CallOutcome<Result<(), SqliteError>> =
        CallOutcome::Replied(Err(SqliteError::Sqlite("?".into())));
    assert_eq!(
        sqlite_bridge_class(&s),
        BridgeOutcomeClass::Fatal(BridgeFatal::SdkUnknown)
    );
}

#[test]
fn full_is_retryable_bridge_full() {
    let f: CallOutcome<Result<(), SqliteError>> = CallOutcome::Full;
    assert_eq!(
        sqlite_bridge_class(&f),
        BridgeOutcomeClass::Retryable(BridgeRetryable::BridgeFull)
    );
}

#[test]
fn caller_timeout_is_retryable_caller_timeout() {
    let t: CallOutcome<Result<(), SqliteError>> = CallOutcome::Timeout;
    assert_eq!(
        sqlite_bridge_class(&t),
        BridgeOutcomeClass::Retryable(BridgeRetryable::CallerTimeout)
    );
}

#[test]
fn serial_pressure_reports_capacity_one_not_unavailable() {
    // Serial admission is still bounded admission — capacity 1, not
    // "absent". This is the test the plan calls out explicitly:
    // SQLite serial pressure maps to one bridge surface with capacity
    // `1`.
    let r = SqlitePressureReport {
        capacity: 1,
        available: 1,
        leased: 0,
        waiters: 0,
        max_waiters: 0,
        full_count: 0,
        closed_count: 0,
        timeout_count: 0,
        busy_count: 2,
        constraint_count: 0,
        late_result_count: 0,
        high_water: 1,
    };
    let bp: BridgePressure = r.into();
    assert_eq!(bp.name(), SQLITE_BRIDGE_SURFACE);
    assert_eq!(bp.capacity(), 1);
    assert_eq!(bp.current(), 0);
    assert_eq!(bp.high_water(), 1);
}

#[test]
fn pressure_round_trip_preserves_late_result_count() {
    let r = SqlitePressureReport {
        capacity: 1,
        available: 0,
        leased: 1,
        waiters: 0,
        max_waiters: 0,
        full_count: 3,
        closed_count: 1,
        timeout_count: 2,
        busy_count: 0,
        constraint_count: 0,
        late_result_count: 5,
        high_water: 1,
    };
    let bp: BridgePressure = r.into();
    assert_eq!(bp.late_result_count(), 5);
    assert_eq!(bp.worker_terminal_count(), 5);
    assert_eq!(bp.timeout_count(), 2);
    assert_eq!(bp.closed_count(), 1);
    assert_eq!(bp.full_count(), 3);
}
