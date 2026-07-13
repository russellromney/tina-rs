use system_job_queue::{
    JobOutcome, QueueReply, RunConfig, RunConfigError, run_caller_gone, run_cancel_in_flight,
    run_overflow, run_poison_crash, run_respawn_then_admit,
};

fn config() -> RunConfig {
    RunConfig {
        workers: 2,
        queue_mailbox: 64,
        worker_mailbox: 8,
        job_sleep_ms: 80,
        call_timeout_ms: 5_000,
    }
}

#[test]
fn overflow_burst_admits_workers_and_rest_get_busy() {
    // Admission cap = workers (2). Burst = workers + 3 = 5.
    let report = run_overflow(config()).expect("overflow run");
    assert_eq!(report.completed, 2);
    assert_eq!(report.busy, 3);
    assert_eq!(report.full, 0);
    assert_eq!(report.closed, 0);
    assert_eq!(report.timeout, 0);
    assert_eq!(report.rejected, 0);
    assert!(report.rejection_reasons.is_empty());
    assert_eq!(report.stats.jobs_admitted, 2);
    assert_eq!(report.stats.jobs_busy_rejected, 3);
    assert_eq!(report.stats.jobs_completed, 2);
    assert_eq!(report.stats.workers_alive, 2);
    assert_eq!(report.stats.worker_crashes, 0);
}

#[test]
fn cancel_in_flight_replies_immediately_to_parked_caller() {
    let report = run_cancel_in_flight(config()).expect("cancel run");
    // Submit caller gets Cancelled (not Completed, not Failed).
    match report.submit_outcome {
        JobOutcome::Cancelled { .. } => {}
        other => panic!("expected Cancelled submit outcome, got {other:?}"),
    }
    // Cancel API caller gets Cancelled.
    match report.cancel_reply {
        QueueReply::Cancelled(_) => {}
        other => panic!("expected Cancelled cancel reply, got {other:?}"),
    }
    let JobOutcome::Completed { value, .. } = report.refill_outcome else {
        panic!("expected completed refill, got {:?}", report.refill_outcome);
    };
    assert_eq!(value, 42);
    // Both parked callers settle exactly once and the cancelled worker slot
    // admits new work immediately after its cancel event releases the worker.
    assert_eq!(report.stats.jobs_cancelled, 1);
    assert_eq!(report.stats.jobs_completed, 1);
    assert_eq!(report.stats.jobs_failed, 0);
    assert_eq!(report.stats.jobs_admitted, 2);
    assert_eq!(report.stats.in_flight, 0);
    assert_eq!(report.stats.workers_alive, 2);
}

#[test]
fn caller_timeout_does_not_strand_pending_authority() {
    let report = run_caller_gone(config()).expect("caller-gone run");
    assert!(matches!(
        report.submit_outcome,
        tina_runtime::CallOutcome::Timeout
    ));
    assert_eq!(report.stats.jobs_admitted, 1);
    assert_eq!(report.stats.jobs_completed, 1);
    assert_eq!(report.stats.jobs_cancelled, 0);
    assert_eq!(report.stats.jobs_failed, 0);
    assert_eq!(report.stats.in_flight, 0);
}

#[test]
fn poison_marks_failed_and_respawns_worker() {
    let report = run_poison_crash(config()).expect("poison run");
    let JobOutcome::Failed { reason, .. } = report.failed_outcome else {
        panic!("expected Failed outcome, got {:?}", report.failed_outcome);
    };
    assert!(
        reason.contains("Closed") || reason.contains("Rejected"),
        "expected Closed/Rejected reason, got {reason:?}",
    );
    assert_eq!(report.stats.jobs_failed, 1);
    assert_eq!(report.stats.worker_crashes, 1);
    assert_eq!(report.stats.worker_respawns, 1);
    assert_eq!(report.stats.workers_alive, config().workers);
}

#[test]
fn admission_recovers_after_respawn() {
    let report = run_respawn_then_admit(config()).expect("respawn-then-admit run");
    // Poison call fails as expected.
    matches!(report.poison_outcome, JobOutcome::Failed { .. });
    // Follow-up call lands on the respawned worker and completes.
    let JobOutcome::Completed { value, .. } = report.follow_up_outcome else {
        panic!(
            "expected Completed follow-up, got {:?}",
            report.follow_up_outcome
        );
    };
    assert_eq!(value, 42); // 21 * 2
    assert_eq!(report.stats.jobs_completed, 1);
    assert_eq!(report.stats.jobs_failed, 1);
    assert_eq!(report.stats.worker_crashes, 1);
    assert_eq!(report.stats.worker_respawns, 1);
    assert_eq!(report.stats.workers_alive, config().workers);
}

#[test]
fn run_config_rejects_all_unbounded_allocation_and_wait_inputs() {
    let base = config();
    for (field, invalid) in [
        ("workers", RunConfig { workers: 0, ..base }),
        (
            "queue_mailbox",
            RunConfig {
                queue_mailbox: 0,
                ..base
            },
        ),
        (
            "worker_mailbox",
            RunConfig {
                worker_mailbox: 0,
                ..base
            },
        ),
    ] {
        assert_eq!(invalid.validate(), Err(RunConfigError::Zero(field)));
    }
    for (field, invalid) in [
        (
            "job_sleep_ms",
            RunConfig {
                job_sleep_ms: 0,
                ..base
            },
        ),
        (
            "call_timeout_ms",
            RunConfig {
                call_timeout_ms: 0,
                ..base
            },
        ),
    ] {
        assert_eq!(invalid.validate(), Err(RunConfigError::Zero(field)));
    }

    assert!(matches!(
        RunConfig {
            workers: usize::MAX,
            ..base
        }
        .validate(),
        Err(RunConfigError::TooLarge {
            field: "workers",
            ..
        })
    ));
    assert!(matches!(
        RunConfig {
            job_sleep_ms: u64::MAX,
            ..base
        }
        .validate(),
        Err(RunConfigError::DurationTooLarge {
            field: "job_sleep_ms",
            ..
        })
    ));
}
