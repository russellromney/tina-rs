use std::time::Duration;

use tina::prelude::Shard;
use tina_http::{
    KeepaliveCloseAndDrain, KeepalivePoolDrainOutcome, KeepalivePoolSettledReport,
};
use tina_runtime::lifecycle::{
    CloseAdmission, CloseOutcome, ResourceCloseReport, ResourceKind,
};
use tina_runtime::MailboxFactory;

/// Summary of an owned keepalive pool `close_and_drain` for terminal lines.
#[derive(Debug, Clone)]
pub(crate) struct OutboundDrainSummary {
    pub drain: KeepalivePoolDrainOutcome,
    pub requested: usize,
    pub stopped: usize,
    pub timed_out: usize,
    pub rejected: usize,
    pub already_closed: usize,
    pub failures: usize,
}

impl OutboundDrainSummary {
    pub fn clean(&self) -> bool {
        matches!(self.drain, KeepalivePoolDrainOutcome::Drained)
            && self.requested == self.stopped
            && self.timed_out == 0
            && self.rejected == 0
            && self.already_closed == 0
            && self.failures == 0
    }

    pub fn terminal_fragment(&self) -> String {
        format!(
            "outbound.drain={:?} outbound.stop_requested={} outbound.stop_stopped={} \
             outbound.stop_timed_out={} outbound.stop_rejected={} \
             outbound.stop_already_closed={} outbound.stop_failures={}",
            self.drain,
            self.requested,
            self.stopped,
            self.timed_out,
            self.rejected,
            self.already_closed,
            self.failures,
        )
    }
}

/// Convert an owned [`KeepaliveCloseAndDrain`] into a terminal summary plus a
/// typed resource-close report for the shutdown choreography.
pub(crate) fn keepalive_close_report<S, F>(
    outcome: KeepaliveCloseAndDrain<S, F>,
    elapsed: Duration,
) -> (OutboundDrainSummary, ResourceCloseReport)
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    match outcome {
        KeepaliveCloseAndDrain::Drained(report) => {
            let summary = OutboundDrainSummary {
                drain: report.drain,
                requested: report.requested,
                stopped: report.stopped,
                timed_out: 0,
                rejected: 0,
                already_closed: report.already_closed,
                failures: 0,
            };
            let close = pool_settled_to_close_report("outbound.pool", &report, elapsed);
            (summary, close)
        }
        KeepaliveCloseAndDrain::TimedOut { pool, pending } => {
            let connections = pool.connections().len();
            drop(pool);
            let summary = OutboundDrainSummary {
                drain: KeepalivePoolDrainOutcome::TimedOut {
                    leased: pending.leased,
                },
                requested: connections,
                stopped: 0,
                timed_out: 1,
                rejected: 0,
                already_closed: 0,
                failures: 0,
            };
            let close = ResourceCloseReport {
                name: "outbound.pool".to_owned(),
                kind: ResourceKind::Pool,
                admission: CloseAdmission::Drain,
                outcome: CloseOutcome::TimedOut { waited: elapsed },
                elapsed,
                details: format!(
                    "drain={:?} leased={:?} connections_live={} admission_closed={}",
                    summary.drain,
                    pending.leased,
                    pending.connections_live,
                    pending.admission_closed,
                ),
            };
            (summary, close)
        }
        KeepaliveCloseAndDrain::OwnerFailed {
            pool,
            error,
            pending,
        } => {
            let connections = pool.connections().len();
            drop(pool);
            let summary = OutboundDrainSummary {
                drain: KeepalivePoolDrainOutcome::NotRequested,
                requested: connections,
                stopped: 0,
                timed_out: 0,
                rejected: 1,
                already_closed: 0,
                failures: 1,
            };
            let close = ResourceCloseReport {
                name: "outbound.pool".to_owned(),
                kind: ResourceKind::Pool,
                admission: CloseAdmission::Drain,
                outcome: CloseOutcome::Failed {
                    reason: format!("owner failed: {error:?}; pending={pending:?}"),
                },
                elapsed,
                details: format!("owner_failed={error:?} pending={pending:?}"),
            };
            (summary, close)
        }
        KeepaliveCloseAndDrain::Shutdown(settlement) => {
            let summary = OutboundDrainSummary {
                drain: settlement.drain,
                requested: settlement.pending.connections_live,
                stopped: 0,
                timed_out: 0,
                rejected: 0,
                already_closed: 1,
                failures: 0,
            };
            let close = ResourceCloseReport {
                name: "outbound.pool".to_owned(),
                kind: ResourceKind::Pool,
                admission: CloseAdmission::Drain,
                outcome: CloseOutcome::AlreadyClosed,
                elapsed,
                details: format!(
                    "shutdown_settlement drain={:?} pending={:?}",
                    settlement.drain, settlement.pending
                ),
            };
            (summary, close)
        }
    }
}

/// Wrap a settled keepalive report in the typed [`ResourceCloseReport`] vocabulary.
pub(crate) fn pool_settled_to_close_report(
    name: &'static str,
    pool: &KeepalivePoolSettledReport,
    elapsed: Duration,
) -> ResourceCloseReport {
    let outcome = match pool.drain {
        KeepalivePoolDrainOutcome::Drained if pool.requested == pool.stopped => {
            CloseOutcome::Clean
        }
        KeepalivePoolDrainOutcome::TimedOut { .. } => CloseOutcome::TimedOut { waited: elapsed },
        KeepalivePoolDrainOutcome::PoolAlreadyClosed => CloseOutcome::AlreadyClosed,
        _ => CloseOutcome::Failed {
            reason: format!(
                "drain={:?} requested={} stopped={} already_closed={}",
                pool.drain, pool.requested, pool.stopped, pool.already_closed,
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
            "drain={:?} requested={} stopped={} already_closed={}",
            pool.drain, pool.requested, pool.stopped, pool.already_closed,
        ),
    }
}

/// Wrap a legacy `KeepalivePoolShutdownReport` in the typed
/// [`ResourceCloseReport`] vocabulary so unit conversion tests keep working.
#[cfg(test)]
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

