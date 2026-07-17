use std::time::Duration;

use tina_http::KeepalivePoolDrainOutcome;
use tina_runtime::lifecycle::{
    CloseAdmission, CloseOutcome,
    ResourceCloseReport, ResourceKind,
};


/// Wrap a `KeepalivePoolShutdownReport` in the typed
/// [`ResourceCloseReport`] vocabulary so the service can record one
/// uniform close-line per resource while keeping the keepalive-specific
/// counters in the `details` string.
pub(crate) fn pool_shutdown_to_close_report(
    name: &'static str,
    pool: &tina_http::KeepalivePoolShutdownReport,
    elapsed: Duration,
) -> ResourceCloseReport {
    let outcome = match pool.drain {
        KeepalivePoolDrainOutcome::Drained
            if pool.requested == pool.stopped
                && pool.timed_out == 0
                && pool.rejected == 0
                && pool.connection_failures.is_empty() =>
        {
            CloseOutcome::Clean
        }
        KeepalivePoolDrainOutcome::TimedOut { .. } => CloseOutcome::TimedOut { waited: elapsed },
        KeepalivePoolDrainOutcome::PoolAlreadyClosed => CloseOutcome::AlreadyClosed,
        _ => CloseOutcome::Failed {
            reason: format!(
                "drain={:?} requested={} stopped={} timed_out={} rejected={} failures={}",
                pool.drain,
                pool.requested,
                pool.stopped,
                pool.timed_out,
                pool.rejected,
                pool.connection_failures.len(),
            ),
        },
    };
    ResourceCloseReport {
        name: name.to_owned(),
        kind: ResourceKind::Pool,
        admission: CloseAdmission::Drain,
        outcome,
        elapsed,
        details: format!(
            "drain={:?} requested={} stopped={} timed_out={} rejected={} already_closed={} failures={}",
            pool.drain,
            pool.requested,
            pool.stopped,
            pool.timed_out,
            pool.rejected,
            pool.already_closed,
            pool.connection_failures.len(),
        ),
    }
}

#[cfg(test)]
mod conversion_tests {
    use super::*;
    use tina_http::{
        KeepaliveConnectionStopFailure, KeepaliveConnectionStopOutcome, KeepalivePoolCloseOutcome,
        KeepalivePoolShutdownReport,
    };

    #[test]
    fn pool_shutdown_clean_drain_becomes_clean_close_outcome() {
        let pool = KeepalivePoolShutdownReport {
            pool_close: KeepalivePoolCloseOutcome::Closed,
            drain: KeepalivePoolDrainOutcome::Drained,
            requested: 1,
            stopped: 1,
            timed_out: 0,
            rejected: 0,
            already_closed: 0,
            connection_failures: Vec::new(),
        };
        let report =
            pool_shutdown_to_close_report("outbound.pool", &pool, Duration::from_millis(2));
        assert_eq!(report.name, "outbound.pool");
        assert_eq!(report.kind, ResourceKind::Pool);
        assert_eq!(report.admission, CloseAdmission::Drain);
        assert!(matches!(report.outcome, CloseOutcome::Clean));
        assert!(report.details.contains("requested=1 stopped=1"));
    }

    #[test]
    fn pool_shutdown_timed_out_drain_propagates_as_timed_out() {
        // Production-shape stuck-child scenario: the keepalive pool's
        // drain deadline fired while leases remained. The conversion
        // must surface this as `CloseOutcome::TimedOut`, which the
        // choreography then records as `StepOutcome::Timeout`.
        let pool = KeepalivePoolShutdownReport {
            pool_close: KeepalivePoolCloseOutcome::Closed,
            drain: KeepalivePoolDrainOutcome::TimedOut { leased: Some(1) },
            requested: 0,
            stopped: 0,
            timed_out: 0,
            rejected: 0,
            already_closed: 0,
            connection_failures: Vec::new(),
        };
        let elapsed = Duration::from_millis(2_000);
        let report = pool_shutdown_to_close_report("outbound.pool", &pool, elapsed);
        match report.outcome {
            CloseOutcome::TimedOut { waited } => assert_eq!(waited, elapsed),
            other => panic!("expected TimedOut, got {other:?}"),
        }
        assert!(report.details.contains("drain=TimedOut"));
    }

    #[test]
    fn pool_shutdown_with_connection_failures_becomes_failed_close() {
        // The pool admission closed cleanly but one of the connection
        // isolates did not stop on request. The conversion must
        // surface this as `CloseOutcome::Failed` so the choreography
        // flags `clean=false` and the failure reason is searchable.
        let pool = KeepalivePoolShutdownReport {
            pool_close: KeepalivePoolCloseOutcome::Closed,
            drain: KeepalivePoolDrainOutcome::Drained,
            requested: 2,
            stopped: 1,
            timed_out: 0,
            rejected: 1,
            already_closed: 0,
            connection_failures: vec![KeepaliveConnectionStopFailure {
                index: 1,
                outcome: KeepaliveConnectionStopOutcome::UnexpectedReply,
            }],
        };
        let report =
            pool_shutdown_to_close_report("outbound.pool", &pool, Duration::from_millis(1));
        match &report.outcome {
            CloseOutcome::Failed { reason } => {
                assert!(reason.contains("failures=1"), "{reason}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn pool_shutdown_already_closed_becomes_already_closed() {
        let pool = KeepalivePoolShutdownReport {
            pool_close: KeepalivePoolCloseOutcome::AlreadyClosed,
            drain: KeepalivePoolDrainOutcome::PoolAlreadyClosed,
            requested: 0,
            stopped: 0,
            timed_out: 0,
            rejected: 0,
            already_closed: 0,
            connection_failures: Vec::new(),
        };
        let report =
            pool_shutdown_to_close_report("outbound.pool", &pool, Duration::from_millis(0));
        assert!(matches!(report.outcome, CloseOutcome::AlreadyClosed));
    }
}

