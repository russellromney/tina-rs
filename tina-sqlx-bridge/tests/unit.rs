//! Pure unit tests that do not need a running database.

use std::time::Duration;

use tina_runtime::CallOutcome;
use tina_sqlx_bridge::{
    PgConfig, PgConfigError, PgError, PgFatalReason, PgOutcomeClass, PgOutcomeExt, PgPoolConfig,
    PgRequest, PgResponse, PgRow, PgTransientReason, PgValue, U64TooLarge,
};

// ---------------------------------------------------------------------------
// Value conversion
// ---------------------------------------------------------------------------

#[test]
fn pg_value_signed_integers_widen_to_i64() {
    assert_eq!(PgValue::from(7_i8), PgValue::I64(7));
    assert_eq!(PgValue::from(7_i16), PgValue::I64(7));
    assert_eq!(PgValue::from(7_i32), PgValue::I64(7));
    assert_eq!(PgValue::from(-7_i64), PgValue::I64(-7));
}

#[test]
fn pg_value_small_unsigned_integers_widen_to_i64() {
    assert_eq!(PgValue::from(7_u8), PgValue::I64(7));
    assert_eq!(PgValue::from(7_u16), PgValue::I64(7));
    assert_eq!(PgValue::from(7_u32), PgValue::I64(7));
}

#[test]
fn pg_value_u64_in_range_succeeds() {
    let v: PgValue = (i64::MAX as u64).try_into().expect("in range");
    assert_eq!(v, PgValue::I64(i64::MAX));
}

#[test]
fn pg_value_u64_out_of_range_errors() {
    let huge = i64::MAX as u64 + 1;
    let err: U64TooLarge = PgValue::try_from(huge).expect_err("must reject");
    assert_eq!(err, U64TooLarge(huge));
    assert!(format!("{err}").contains("exceeds i64::MAX"));
}

#[test]
fn pg_value_floats() {
    assert_eq!(PgValue::from(1.5_f32), PgValue::F64(1.5));
    assert_eq!(PgValue::from(2.25_f64), PgValue::F64(2.25));
}

#[test]
fn pg_value_strings_and_bytes() {
    assert_eq!(PgValue::from("hi"), PgValue::String("hi".into()));
    assert_eq!(
        PgValue::from(String::from("hi")),
        PgValue::String("hi".into())
    );
    assert_eq!(PgValue::from(&b"abc"[..]), PgValue::Bytes(b"abc".to_vec()));
    let owned: Vec<u8> = vec![1, 2, 3];
    assert_eq!(PgValue::from(owned.clone()), PgValue::Bytes(owned));
}

#[test]
fn pg_value_option_some_and_none() {
    let some: PgValue = Some(7_i64).into();
    let none: PgValue = Option::<i64>::None.into();
    assert_eq!(some, PgValue::I64(7));
    assert_eq!(none, PgValue::Null);
}

#[test]
fn pg_value_accessors_match_variant() {
    assert_eq!(PgValue::I64(7).as_i64(), Some(7));
    assert_eq!(PgValue::Bool(true).as_bool(), Some(true));
    assert_eq!(PgValue::F64(1.0).as_f64(), Some(1.0));
    assert_eq!(PgValue::String("hi".into()).as_str(), Some("hi"));
    assert_eq!(PgValue::Bytes(vec![1, 2]).as_bytes(), Some(&[1u8, 2u8][..]));
    assert!(PgValue::Null.is_null());
    // Wrong-shape accessors return None rather than coerce.
    assert_eq!(PgValue::String("x".into()).as_i64(), None);
    assert_eq!(PgValue::I64(0).as_str(), None);
}

#[test]
fn pg_value_into_consumes_without_clone() {
    let s = PgValue::String("owned".into());
    assert_eq!(s.into_string().as_deref(), Some("owned"));
    let b = PgValue::Bytes(vec![9, 9]);
    assert_eq!(b.into_bytes(), Some(vec![9, 9]));
    assert_eq!(PgValue::I64(1).into_string(), None);
}

// ---------------------------------------------------------------------------
// Row accessors
// ---------------------------------------------------------------------------

fn sample_row() -> PgRow {
    PgRow {
        columns: vec!["id".into(), "name".into(), "active".into()],
        cells: vec![
            PgValue::I64(42),
            PgValue::String("ada".into()),
            PgValue::Bool(true),
        ],
    }
}

#[test]
fn pg_row_by_index_and_name() {
    let row = sample_row();
    assert_eq!(row.len(), 3);
    assert_eq!(row.get_i64(0), Some(42));
    assert_eq!(row.get_text(1), Some("ada"));
    assert_eq!(row.get_bool(2), Some(true));
    assert_eq!(row.by_name("name").and_then(PgValue::as_str), Some("ada"));
    assert_eq!(row.by_name("missing"), None);
    assert_eq!(row.col(99), None);
}

#[test]
fn pg_row_wrong_type_accessor_returns_none() {
    let row = sample_row();
    assert_eq!(row.get_text(0), None);
    assert_eq!(row.get_bool(1), None);
}

// ---------------------------------------------------------------------------
// Request builder
// ---------------------------------------------------------------------------

#[test]
fn pg_request_builder_param_appends_in_order() {
    let req = PgRequest::execute("INSERT INTO t (k, v) VALUES ($1, $2)")
        .param(7_i64)
        .param("seven");
    let PgRequest::Execute { params, sql } = req else {
        panic!("expected Execute");
    };
    assert!(sql.contains("INSERT"));
    assert_eq!(
        params,
        vec![PgValue::I64(7), PgValue::String("seven".into())]
    );
}

#[test]
fn pg_request_builder_params_replaces_vector() {
    let req = PgRequest::fetch_one("SELECT 1")
        .param(1)
        .params(vec![PgValue::Bool(true), PgValue::Null]);
    let PgRequest::FetchOne { params, .. } = req else {
        panic!("expected FetchOne");
    };
    assert_eq!(params, vec![PgValue::Bool(true), PgValue::Null]);
}

// ---------------------------------------------------------------------------
// Config validation
// ---------------------------------------------------------------------------

fn good_config() -> PgConfig {
    PgConfig::from_url("postgres://invalid/")
        .with_default_timeout(Duration::from_secs(1))
        .with_poll_interval(Duration::from_millis(1))
}

#[test]
fn config_default_validates() {
    assert!(good_config().validate().is_ok());
}

#[test]
fn config_rejects_zero_mailbox() {
    let cfg = good_config().with_mailbox_capacity(0);
    assert_eq!(cfg.validate(), Err(PgConfigError::ZeroMailboxCapacity));
}

#[test]
fn config_rejects_max_in_flight_zero() {
    let cfg = good_config().with_max_in_flight(0);
    assert_eq!(cfg.validate(), Err(PgConfigError::ZeroMaxInFlight));
}

#[test]
fn config_rejects_zero_default_timeout() {
    let cfg = good_config().with_default_timeout(Duration::ZERO);
    assert_eq!(cfg.validate(), Err(PgConfigError::ZeroDefaultTimeout));
}

#[test]
fn config_rejects_zero_poll_interval() {
    let cfg = good_config().with_poll_interval(Duration::ZERO);
    assert_eq!(cfg.validate(), Err(PgConfigError::ZeroPollInterval));
}

#[test]
fn config_rejects_zero_max_request_params() {
    let cfg = good_config().with_max_request_params(0);
    assert_eq!(cfg.validate(), Err(PgConfigError::ZeroMaxRequestParams));
}

#[test]
fn config_rejects_zero_pool_max_connections() {
    let cfg =
        good_config().with_pool(PgPoolConfig::new("postgres://invalid/").with_max_connections(0));
    assert_eq!(cfg.validate(), Err(PgConfigError::ZeroPoolMaxConnections));
}

#[test]
fn config_rejects_zero_pool_acquire_timeout() {
    let cfg = good_config()
        .with_pool(PgPoolConfig::new("postgres://invalid/").with_acquire_timeout(Duration::ZERO));
    assert_eq!(cfg.validate(), Err(PgConfigError::ZeroPoolAcquireTimeout));
}

#[test]
fn config_oversized_mailbox_rejects_with_cap() {
    let cfg = good_config().with_mailbox_capacity(usize::MAX);
    let err = cfg.validate().expect_err("must reject");
    matches!(err, PgConfigError::MailboxCapacityTooLarge { .. });
}

#[test]
fn bridge_only_config_has_no_pool() {
    let cfg = PgConfig::bridge_only();
    assert!(cfg.pool.is_none());
    assert!(cfg.validate().is_ok(), "bridge-only must validate");
}

// ---------------------------------------------------------------------------
// Classifier
// ---------------------------------------------------------------------------

#[test]
fn classify_replied_executed_succeeds() {
    let outcome: CallOutcome<Result<PgResponse, PgError>> =
        CallOutcome::Replied(Ok(PgResponse::Executed { rows_affected: 3 }));
    match outcome.classify() {
        PgOutcomeClass::Succeeded(PgResponse::Executed { rows_affected }) => {
            assert_eq!(rows_affected, 3);
        }
        other => panic!("expected Succeeded, got {other:?}"),
    }
}

#[test]
fn classify_worker_timeout_is_transient() {
    let outcome: CallOutcome<Result<PgResponse, PgError>> =
        CallOutcome::Replied(Err(PgError::Timeout));
    assert!(matches!(
        outcome.classify(),
        PgOutcomeClass::Transient(PgTransientReason::WorkerTimeout)
    ));
}

#[test]
fn classify_pool_acquire_timeout_is_transient() {
    let outcome: CallOutcome<Result<PgResponse, PgError>> =
        CallOutcome::Replied(Err(PgError::PoolAcquireTimeout));
    assert!(matches!(
        outcome.classify(),
        PgOutcomeClass::Transient(PgTransientReason::PoolAcquireTimeout)
    ));
}

#[test]
fn classify_bridge_timeout_is_transient() {
    let outcome: CallOutcome<Result<PgResponse, PgError>> = CallOutcome::Timeout;
    assert!(matches!(
        outcome.classify(),
        PgOutcomeClass::Transient(PgTransientReason::BridgeTimeout)
    ));
}

#[test]
fn classify_bridge_full_and_closed_are_fatal() {
    let outcome_full: CallOutcome<Result<PgResponse, PgError>> = CallOutcome::Full;
    let outcome_closed: CallOutcome<Result<PgResponse, PgError>> = CallOutcome::Closed;
    assert!(matches!(
        outcome_full.classify(),
        PgOutcomeClass::Fatal(PgFatalReason::BridgeFull)
    ));
    assert!(matches!(
        outcome_closed.classify(),
        PgOutcomeClass::Fatal(PgFatalReason::BridgeClosed)
    ));
}

#[test]
fn classify_too_many_rows_is_fatal() {
    let outcome: CallOutcome<Result<PgResponse, PgError>> =
        CallOutcome::Replied(Err(PgError::TooManyRows));
    assert!(matches!(
        outcome.classify(),
        PgOutcomeClass::Fatal(PgFatalReason::TooManyRows)
    ));
}

#[test]
fn classify_typed_executed_outcome_uses_u64_carrier() {
    use tina_sqlx_bridge::PgExecutedOutcome;
    let outcome: PgExecutedOutcome = CallOutcome::Replied(Ok(11));
    match outcome.classify() {
        PgOutcomeClass::Succeeded(rows_affected) => assert_eq!(rows_affected, 11),
        other => panic!("expected Succeeded, got {other:?}"),
    }
}

#[test]
fn classify_typed_fetch_one_outcome_uses_option_row_carrier() {
    use tina_sqlx_bridge::PgFetchOneOutcome;
    let outcome: PgFetchOneOutcome = CallOutcome::Replied(Ok(None));
    match outcome.classify() {
        PgOutcomeClass::Succeeded(None) => {}
        other => panic!("expected Succeeded(None), got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Display
// ---------------------------------------------------------------------------

#[test]
fn errors_are_displayable_and_named_clearly() {
    assert!(format!("{}", PgError::Full).contains("ingress full"));
    assert!(format!("{}", PgError::PoolAcquireTimeout).contains("pool"));
    assert!(format!("{}", PgError::Timeout).contains("timeout"));
    assert!(format!("{}", PgError::TooManyRows).contains("more than one"));
}
