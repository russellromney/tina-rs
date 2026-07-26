use std::time::Duration;

use tina::prelude::Shard;
use tina_http::{
    InstalledKeepalivePool, KeepaliveCloseAndDrain, KeepalivePendingCounts,
    KeepalivePoolDrainOutcome, KeepalivePoolSettledReport,
};
use tina_runtime::lifecycle::{CloseAdmission, CloseOutcome, ResourceCloseReport, ResourceKind};
use tina_runtime::{LocalSystemState, LocalSystemTerminalReport, MailboxFactory};

const POST_OWNER_SETTLE_TIMEOUT: Duration = Duration::from_millis(100);

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
    pub pending: Option<KeepalivePendingCounts>,
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
             outbound.stop_already_closed={} outbound.stop_failures={} \
             outbound.pending={:?}",
            self.drain,
            self.requested,
            self.stopped,
            self.timed_out,
            self.rejected,
            self.already_closed,
            self.failures,
            self.pending,
        )
    }
}

/// Why an outbound close attempt retained the linear installation authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RetainedKeepaliveReason {
    TimedOut(KeepalivePendingCounts),
    OwnerFailed {
        error: tina_runtime::ThreadedRuntimeError,
        pending: KeepalivePendingCounts,
    },
}

/// Linear authority returned by a keepalive close that did not settle.
#[must_use = "retained keepalive authority must be retried or settled by owner shutdown"]
pub(crate) struct RetainedKeepaliveAuthority<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    pool: InstalledKeepalivePool<S, F>,
    reason: RetainedKeepaliveReason,
}

/// Convert an owned [`KeepaliveCloseAndDrain`] into a terminal summary plus a
/// typed resource-close report for the shutdown choreography.
pub(crate) fn keepalive_close_report<S, F>(
    outcome: KeepaliveCloseAndDrain<S, F>,
    elapsed: Duration,
) -> (
    OutboundDrainSummary,
    ResourceCloseReport,
    Option<RetainedKeepaliveAuthority<S, F>>,
)
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
                pending: None,
            };
            let close = pool_settled_to_close_report("outbound.pool", &report, elapsed);
            (summary, close, None)
        }
        KeepaliveCloseAndDrain::TimedOut { pool, pending } => {
            let summary = OutboundDrainSummary {
                drain: KeepalivePoolDrainOutcome::TimedOut {
                    leased: pending.leased,
                },
                requested: 0,
                stopped: 0,
                timed_out: 0,
                rejected: 0,
                already_closed: 0,
                failures: 0,
                pending: Some(pending),
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
            (
                summary,
                close,
                Some(RetainedKeepaliveAuthority {
                    pool,
                    reason: RetainedKeepaliveReason::TimedOut(pending),
                }),
            )
        }
        KeepaliveCloseAndDrain::OwnerFailed {
            pool,
            error,
            pending,
        } => {
            let summary = OutboundDrainSummary {
                drain: KeepalivePoolDrainOutcome::NotRequested,
                requested: 0,
                stopped: 0,
                timed_out: 0,
                rejected: 0,
                already_closed: 0,
                failures: 0,
                pending: Some(pending),
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
            (
                summary,
                close,
                Some(RetainedKeepaliveAuthority {
                    pool,
                    reason: RetainedKeepaliveReason::OwnerFailed { error, pending },
                }),
            )
        }
        KeepaliveCloseAndDrain::Shutdown(settlement) => {
            let summary = OutboundDrainSummary {
                drain: settlement.drain,
                requested: 0,
                stopped: 0,
                timed_out: 0,
                rejected: 0,
                already_closed: 0,
                failures: 0,
                pending: Some(settlement.pending),
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
            (summary, close, None)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PostOwnerKeepaliveSettlement {
    Shutdown {
        retained_reason: RetainedKeepaliveReason,
        settlement: tina_http::KeepaliveShutdownSettlement,
    },
    Drained {
        retained_reason: RetainedKeepaliveReason,
        report: KeepalivePoolSettledReport,
    },
    /// The framework still returned retained authority after the owner had
    /// already terminated. Owner shutdown is the terminal resource proof.
    OwnerShutdownFallback {
        retained_reason: RetainedKeepaliveReason,
        final_reason: RetainedKeepaliveReason,
    },
}

/// Evidence that the local owner reached one of its documented terminal states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OwnerTerminalProof(());

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OwnerNotTerminal(LocalSystemState);

impl std::fmt::Display for OwnerNotTerminal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "owner report is not terminal: {:?}", self.0)
    }
}

impl std::error::Error for OwnerNotTerminal {}

pub(crate) fn prove_owner_terminal(
    report: &LocalSystemTerminalReport,
) -> Result<OwnerTerminalProof, OwnerNotTerminal> {
    match report.state() {
        LocalSystemState::Closed | LocalSystemState::Failed => Ok(OwnerTerminalProof(())),
        state => Err(OwnerNotTerminal(state)),
    }
}

pub(crate) fn settle_after_owner_shutdown<S, F>(
    authority: RetainedKeepaliveAuthority<S, F>,
    _owner_terminal: OwnerTerminalProof,
) -> PostOwnerKeepaliveSettlement
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    let RetainedKeepaliveAuthority { pool, reason } = authority;
    match pool.close_and_drain(POST_OWNER_SETTLE_TIMEOUT) {
        KeepaliveCloseAndDrain::Shutdown(settlement) => PostOwnerKeepaliveSettlement::Shutdown {
            retained_reason: reason,
            settlement,
        },
        KeepaliveCloseAndDrain::Drained(report) => PostOwnerKeepaliveSettlement::Drained {
            retained_reason: reason,
            report,
        },
        KeepaliveCloseAndDrain::TimedOut { pool, pending } => {
            // The owner has already terminated, so its shutdown is the final
            // resource settlement even if this stale host view times out.
            drop(pool);
            PostOwnerKeepaliveSettlement::OwnerShutdownFallback {
                retained_reason: reason,
                final_reason: RetainedKeepaliveReason::TimedOut(pending),
            }
        }
        KeepaliveCloseAndDrain::OwnerFailed {
            pool,
            error,
            pending,
        } => {
            drop(pool);
            PostOwnerKeepaliveSettlement::OwnerShutdownFallback {
                retained_reason: reason,
                final_reason: RetainedKeepaliveReason::OwnerFailed { error, pending },
            }
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
        KeepalivePoolDrainOutcome::Drained if pool.requested == pool.stopped => CloseOutcome::Clean,
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
    use tina::pool::PoolConfig;
    use tina_http::{
        HttpClientConfig, HttpTarget, InstallKeepalivePool, KeepaliveConnectionStopFailure,
        KeepaliveConnectionStopOutcome, KeepalivePoolCloseOutcome, KeepalivePoolInstallConfig,
        KeepalivePoolShutdownReport, KeepaliveShutdownSettlement,
    };
    use tina_runtime::{DefaultThreadedMailboxFactory, LocalSystem, ThreadedRuntimeError};

    fn installed_pool(
        port: u16,
    ) -> (
        LocalSystem<tina::SingleShard, DefaultThreadedMailboxFactory>,
        InstalledKeepalivePool<tina::SingleShard>,
    ) {
        let app = LocalSystem::single_shard(tina::SingleShard, DefaultThreadedMailboxFactory)
            .try_build()
            .expect("system");
        let pool = app
            .install_keepalive_pool(KeepalivePoolInstallConfig::new(
                HttpTarget::http(format!("127.0.0.1:{port}").parse().unwrap()),
                HttpClientConfig::pressure(),
                PoolConfig::new(1, 0),
                8,
                8,
            ))
            .expect("install");
        (app, pool)
    }

    fn stopped_owner_settles(
        app: LocalSystem<tina::SingleShard, DefaultThreadedMailboxFactory>,
        authority: RetainedKeepaliveAuthority<tina::SingleShard, DefaultThreadedMailboxFactory>,
    ) -> PostOwnerKeepaliveSettlement {
        let terminal = app
            .shutdown_handle()
            .request_and_wait_report(Duration::from_secs(2))
            .expect("owner shutdown");
        let proof = prove_owner_terminal(&terminal).expect("terminal proof");
        let settlement = settle_after_owner_shutdown(authority, proof);
        app.shutdown().join().expect("owner join remains clean");
        settlement
    }

    #[test]
    fn close_adapter_exhaustively_retains_and_settles_every_terminal_shape() {
        let drained = KeepalivePoolSettledReport {
            pool_close: KeepalivePoolCloseOutcome::Closed,
            drain: KeepalivePoolDrainOutcome::Drained,
            requested: 1,
            stopped: 1,
            already_closed: 0,
        };
        let (_, _, retained) = keepalive_close_report::<
            tina::SingleShard,
            DefaultThreadedMailboxFactory,
        >(KeepaliveCloseAndDrain::Drained(drained), Duration::ZERO);
        assert!(retained.is_none());

        let nonterminal = LocalSystemTerminalReport::new(LocalSystemState::Accepting, Vec::new());
        assert_eq!(
            prove_owner_terminal(&nonterminal),
            Err(OwnerNotTerminal(LocalSystemState::Accepting))
        );

        let shutdown = KeepaliveShutdownSettlement {
            pool_close: KeepalivePoolCloseOutcome::AlreadyClosed,
            drain: KeepalivePoolDrainOutcome::PoolAlreadyClosed,
            pending: KeepalivePendingCounts {
                leased: None,
                connections_live: 0,
                admission_closed: true,
            },
        };
        let (_, _, retained) = keepalive_close_report::<
            tina::SingleShard,
            DefaultThreadedMailboxFactory,
        >(KeepaliveCloseAndDrain::Shutdown(shutdown), Duration::ZERO);
        assert!(retained.is_none());

        let pending = KeepalivePendingCounts {
            leased: Some(1),
            connections_live: 1,
            admission_closed: true,
        };
        let (app, pool) = installed_pool(9);
        let (summary, close, timed_out) = keepalive_close_report(
            KeepaliveCloseAndDrain::TimedOut { pool, pending },
            Duration::from_millis(1),
        );
        assert_eq!(summary.requested, 0);
        assert_eq!(summary.timed_out, 0);
        assert_eq!(summary.pending, Some(pending));
        assert!(close.details.contains("connections_live=1"));
        let timed_out = timed_out.expect("timeout retains authority");
        assert!(matches!(
            &timed_out.reason,
            RetainedKeepaliveReason::TimedOut(actual) if *actual == pending
        ));
        assert!(matches!(
            stopped_owner_settles(app, timed_out),
            PostOwnerKeepaliveSettlement::Shutdown { .. }
        ));

        let (app, pool) = installed_pool(10);
        let (summary, close, owner_failed) = keepalive_close_report(
            KeepaliveCloseAndDrain::OwnerFailed {
                pool,
                error: ThreadedRuntimeError::CommandFull,
                pending,
            },
            Duration::from_millis(1),
        );
        assert_eq!(summary.requested, 0);
        assert_eq!(summary.rejected, 0);
        assert_eq!(summary.failures, 0);
        assert_eq!(summary.pending, Some(pending));
        assert!(close.details.contains("connections_live: 1"));
        let owner_failed = owner_failed.expect("owner failure retains authority");
        assert!(matches!(
            &owner_failed.reason,
            RetainedKeepaliveReason::OwnerFailed {
                error: ThreadedRuntimeError::CommandFull,
                pending: actual,
            } if *actual == pending
        ));
        assert!(matches!(
            stopped_owner_settles(app, owner_failed),
            PostOwnerKeepaliveSettlement::Shutdown { .. }
        ));
    }

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
