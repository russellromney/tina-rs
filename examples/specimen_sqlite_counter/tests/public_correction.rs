//! Public adversarial certification for exact SQLite counter terminals.

use specimen_sqlite_counter::{
    CounterFailure, CounterProtocolFailure,
    tina_impl::{CorrectionOutcome, CorrectionScenario, run_correction},
};
use tina_runtime::ResultWaitError;
use tina_sqlite_bridge::{SqliteError, SqliteMetrics};

fn metrics(
    admitted: u64,
    closed: u64,
    timeouts: u64,
    worker_rows: u64,
    worker_sqlite: u64,
    late_results: u64,
) -> SqliteMetrics {
    SqliteMetrics {
        admitted,
        closed,
        timeouts,
        worker_rows,
        worker_sqlite,
        late_results,
        current_in_flight: 0,
        in_flight_high_water: u64::from(admitted != 0),
        ..SqliteMetrics::default()
    }
}

#[test]
fn malformed_sql_is_a_typed_database_terminal_without_a_report() {
    let correction = run_correction(CorrectionScenario::MalformedSql).expect("clean shutdown");
    match correction.outcome {
        CorrectionOutcome::Counter(CounterFailure::Sqlite(SqliteError::Sqlite(message))) => {
            assert!(message.contains("syntax error"), "{message}");
        }
        other => panic!("unexpected terminal: {other:?}"),
    }
    assert!(correction.bridge_closed);
    assert_eq!(correction.metrics, metrics(1, 0, 0, 0, 1, 0));
}

#[test]
fn closed_bridge_is_exact_and_never_reaches_the_worker() {
    let correction = run_correction(CorrectionScenario::ClosedBridge).expect("clean shutdown");
    assert_eq!(
        correction.outcome,
        CorrectionOutcome::Counter(CounterFailure::Sqlite(SqliteError::Closed))
    );
    assert!(correction.bridge_closed);
    assert_eq!(correction.metrics, metrics(0, 1, 0, 0, 0, 0));
}

#[test]
fn timeout_preserves_the_terminal_and_late_completion_truth() {
    let correction = run_correction(CorrectionScenario::WorkerTimeout).expect("clean shutdown");
    assert_eq!(
        correction.outcome,
        CorrectionOutcome::Counter(CounterFailure::Sqlite(SqliteError::Timeout))
    );
    assert!(correction.bridge_closed);
    assert_eq!(correction.metrics, metrics(1, 0, 1, 1, 0, 1));
}

#[test]
fn successful_sql_with_the_wrong_value_type_is_a_protocol_terminal() {
    let correction = run_correction(CorrectionScenario::ProtocolValueType).expect("clean shutdown");
    assert_eq!(
        correction.outcome,
        CorrectionOutcome::Counter(CounterFailure::Protocol(
            CounterProtocolFailure::UnexpectedValueKind {
                actual: specimen_sqlite_counter::SqliteValueKind::Text,
            }
        ))
    );
    assert!(correction.bridge_closed);
    assert_eq!(correction.metrics, metrics(1, 0, 0, 1, 0, 0));
}

#[test]
fn observer_type_mismatch_remains_an_exact_observer_failure() {
    let correction =
        run_correction(CorrectionScenario::ObserverTypeMismatch).expect("clean shutdown");
    assert_eq!(
        correction.outcome,
        CorrectionOutcome::Observer(ResultWaitError::TypeMismatch)
    );
    assert!(correction.bridge_closed);
    assert_eq!(correction.metrics, metrics(1, 0, 0, 1, 0, 0));
}
