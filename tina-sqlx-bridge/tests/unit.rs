//! Pure unit tests that do not need a running database.

use std::time::Duration;

use tina_runtime::CallOutcome;
use tina_sqlx_bridge::{
    PgCancelConfig, PgConfig, PgConfigError, PgError, PgFatalReason, PgOutcomeClass, PgOutcomeExt,
    PgPoolConfig, PgRequest, PgResponse, PgRow, PgStep, PgStepOk, PgTransactionOutcome,
    PgTransientReason, PgType, PgValue, U64TooLarge,
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
fn typed_null_builders_match_pg_type() {
    assert_eq!(PgValue::null_bool(), PgValue::TypedNull(PgType::Bool));
    assert_eq!(PgValue::null_i64(), PgValue::TypedNull(PgType::I64));
    assert_eq!(PgValue::null_f64(), PgValue::TypedNull(PgType::F64));
    assert_eq!(PgValue::null_text(), PgValue::TypedNull(PgType::Text));
    assert_eq!(PgValue::null_bytes(), PgValue::TypedNull(PgType::Bytes));
    let v: PgValue = PgType::Bool.into();
    assert_eq!(v, PgValue::TypedNull(PgType::Bool));
}

#[test]
fn typed_null_is_null() {
    assert!(PgValue::Null.is_null());
    assert!(PgValue::null_bool().is_null());
    assert!(PgValue::null_text().is_null());
    assert!(!PgValue::I64(0).is_null());
    assert!(!PgValue::Bool(false).is_null());
}

#[test]
fn typed_null_accessors_return_none() {
    // TypedNull is a NULL, not a value — typed accessors return None.
    assert_eq!(PgValue::null_bool().as_bool(), None);
    assert_eq!(PgValue::null_i64().as_i64(), None);
    assert_eq!(PgValue::null_text().as_str(), None);
}

#[cfg(feature = "uuid")]
#[test]
fn typed_null_uuid_builder_and_accessor() {
    let v = PgValue::null_uuid();
    assert_eq!(v, PgValue::TypedNull(PgType::Uuid));
    assert!(v.is_null());
    assert_eq!(v.as_uuid(), None);
}

#[cfg(feature = "json")]
#[test]
fn typed_null_json_builder_and_accessor() {
    let v = PgValue::null_json();
    assert_eq!(v, PgValue::TypedNull(PgType::Json));
    assert!(v.is_null());
}

#[cfg(feature = "numeric")]
#[test]
fn typed_null_numeric_builder_and_accessor() {
    let v = PgValue::null_numeric();
    assert_eq!(v, PgValue::TypedNull(PgType::Numeric));
    assert!(v.is_null());
}

#[cfg(feature = "time")]
#[test]
fn typed_null_temporal_builders() {
    assert!(PgValue::null_timestamp().is_null());
    assert!(PgValue::null_timestamptz().is_null());
    assert!(PgValue::null_date().is_null());
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

#[cfg(feature = "uuid")]
#[test]
fn pg_value_uuid_round_trip_through_from_and_accessor() {
    let id = uuid::Uuid::from_u128(0x1234_5678_90ab_cdef_1234_5678_90ab_cdef);
    let v: PgValue = id.into();
    assert_eq!(v, PgValue::Uuid(id));
    assert_eq!(v.as_uuid(), Some(id));
    assert_eq!(PgValue::I64(1).as_uuid(), None);
}

#[cfg(feature = "json")]
#[test]
fn pg_value_json_round_trip() {
    let payload = serde_json::json!({ "k": "v", "n": 7 });
    let v: PgValue = payload.clone().into();
    match &v {
        PgValue::Json(inner) => assert_eq!(inner, &payload),
        _ => panic!("expected Json"),
    }
    assert_eq!(v.as_json(), Some(&payload));
}

#[cfg(feature = "numeric")]
#[test]
fn pg_value_numeric_round_trip() {
    use core::str::FromStr;
    let d = rust_decimal::Decimal::from_str("3.14159").unwrap();
    let v: PgValue = d.into();
    assert_eq!(v, PgValue::Numeric(d));
    assert_eq!(v.as_numeric(), Some(d));
}

#[cfg(feature = "time")]
#[test]
fn pg_value_temporal_round_trip() {
    use time::macros::{date, datetime, time as t};
    let d = date!(2024 - 03 - 14);
    let dt = time::PrimitiveDateTime::new(d, t!(09:30:00));
    let dtz = datetime!(2024-03-14 09:30:00 UTC);
    assert_eq!(PgValue::from(d).as_date(), Some(d));
    assert_eq!(PgValue::from(dt).as_timestamp(), Some(dt));
    assert_eq!(PgValue::from(dtz).as_timestamptz(), Some(dtz));
    // Wrong-shape accessors return None.
    assert_eq!(PgValue::from(d).as_timestamp(), None);
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

#[test]
fn pg_step_builder_param_and_replace() {
    let step = PgStep::execute("UPDATE t SET v = $1 WHERE k = $2")
        .param(7_i64)
        .param("seven");
    match step {
        PgStep::Execute { params, .. } => {
            assert_eq!(
                params,
                vec![PgValue::I64(7), PgValue::String("seven".into())]
            );
        }
        _ => panic!("expected Execute"),
    }
    let replaced = PgStep::fetch_one("SELECT 1")
        .param(1)
        .params(vec![PgValue::Bool(false)]);
    match replaced {
        PgStep::FetchOne { params, .. } => {
            assert_eq!(params, vec![PgValue::Bool(false)]);
        }
        _ => panic!("expected FetchOne"),
    }
}

#[test]
fn pg_request_transaction_carries_steps() {
    let req = PgRequest::transaction(vec![
        PgStep::execute("INSERT INTO t VALUES ($1)").param(1_i64),
        PgStep::fetch_one("SELECT 1::INT8"),
    ]);
    match req {
        PgRequest::Transaction { steps } => {
            assert_eq!(steps.len(), 2);
        }
        _ => panic!("expected Transaction"),
    }
}

#[test]
fn pg_request_param_is_noop_on_transaction() {
    // `.param(...)` on a Transaction is a no-op, not a panic. Steps
    // carry their own parameters.
    let req = PgRequest::transaction(vec![PgStep::execute("SELECT 1")])
        .param(99_i64)
        .params(vec![PgValue::Bool(true)]);
    match req {
        PgRequest::Transaction { steps } => assert_eq!(steps.len(), 1),
        _ => panic!("expected Transaction"),
    }
}

#[test]
fn classify_committed_transaction_succeeds() {
    use tina_sqlx_bridge::PgTransactionCallOutcome;
    let outcome: PgTransactionCallOutcome =
        CallOutcome::Replied(Ok(PgTransactionOutcome::Committed {
            steps: vec![PgStepOk::Executed { rows_affected: 1 }],
        }));
    match outcome.classify() {
        PgOutcomeClass::Succeeded(PgTransactionOutcome::Committed { steps }) => {
            assert_eq!(steps.len(), 1);
        }
        other => panic!("expected Succeeded(Committed), got {other:?}"),
    }
}

#[test]
fn commit_ambiguous_transaction_preserves_completed_steps_and_error() {
    use tina_sqlx_bridge::PgTransactionCallOutcome;
    let outcome: PgTransactionCallOutcome =
        CallOutcome::Replied(Ok(PgTransactionOutcome::CommitAmbiguous {
            completed: vec![PgStepOk::Executed { rows_affected: 7 }],
            error: PgError::Sqlx("commit connection closed".into()),
        }));

    match outcome.classify() {
        PgOutcomeClass::Succeeded(PgTransactionOutcome::CommitAmbiguous { completed, error }) => {
            assert_eq!(completed, vec![PgStepOk::Executed { rows_affected: 7 }]);
            assert_eq!(error, PgError::Sqlx("commit connection closed".into()));
        }
        other => panic!("expected Succeeded(CommitAmbiguous), got {other:?}"),
    }
}

#[test]
fn pg_request_fetch_many_carries_max_rows_and_params() {
    let req = PgRequest::fetch_many("SELECT * FROM t WHERE k > $1", 100).param(7_i64);
    match req {
        PgRequest::FetchMany {
            sql,
            params,
            max_rows,
        } => {
            assert!(sql.contains("FROM t"));
            assert_eq!(max_rows, 100);
            assert_eq!(params, vec![PgValue::I64(7)]);
        }
        _ => panic!("expected FetchMany"),
    }
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
fn config_rejects_zero_max_response_rows() {
    let cfg = good_config().with_max_response_rows(0);
    assert_eq!(cfg.validate(), Err(PgConfigError::ZeroMaxResponseRows));
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
fn config_rejects_zero_cancel_pool_size() {
    let cfg = good_config().with_cancel(PgCancelConfig::new().with_pool_size(0));
    assert_eq!(cfg.validate(), Err(PgConfigError::ZeroCancelPoolSize));
}

#[test]
fn config_rejects_zero_cancel_acquire_timeout() {
    let cfg = good_config().with_cancel(PgCancelConfig::new().with_acquire_timeout(Duration::ZERO));
    assert_eq!(cfg.validate(), Err(PgConfigError::ZeroCancelAcquireTimeout));
}

#[test]
fn with_cancel_on_timeout_sets_default_acquire_timeout() {
    let cfg = good_config().with_cancel_on_timeout(2);
    assert!(cfg.cancel.is_some());
    let cancel = cfg.cancel.as_ref().unwrap();
    assert_eq!(cancel.pool_size, 2);
    assert!(cancel.acquire_timeout > Duration::ZERO);
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

#[test]
fn validate_tina_ignores_pool_and_cancel_settings() {
    // A caller who builds a config from a URL but then plans to pass
    // a supplied pool should not be rejected for `pool` fields the
    // bridge promises not to apply on the supplied-pool path.
    let cfg = good_config()
        .with_pool(
            PgPoolConfig::new("postgres://invalid/")
                .with_max_connections(0)
                .with_acquire_timeout(Duration::ZERO),
        )
        .with_cancel(PgCancelConfig::new().with_pool_size(0));
    assert!(cfg.validate().is_err(), "full validate should reject");
    assert!(
        cfg.validate_tina().is_ok(),
        "tina-only validate must ignore pool and cancel fields",
    );
}

#[test]
fn validate_tina_still_rejects_tina_side_zeros() {
    let cfg = good_config().with_max_in_flight(0);
    assert_eq!(cfg.validate_tina(), Err(PgConfigError::ZeroMaxInFlight),);
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
