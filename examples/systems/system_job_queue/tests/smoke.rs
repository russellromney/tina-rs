use system_job_queue::{
    JobOutcome, QueueReply, RunConfig, run_cancel_in_flight, run_overflow, run_poison_crash,
    run_respawn_then_admit,
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
    // Stats: exactly one cancel, no completion, no failure. The worker's
    // eventual reply is rejected by the runtime (cancel_call closed the
    // wait), so no `Completed` or `Failed` ticks here.
    assert_eq!(report.stats.jobs_cancelled, 1);
    assert_eq!(report.stats.jobs_completed, 0);
    assert_eq!(report.stats.jobs_failed, 0);
    assert_eq!(report.stats.workers_alive, 2);
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
        panic!("expected Completed follow-up, got {:?}", report.follow_up_outcome);
    };
    assert_eq!(value, 42); // 21 * 2
    assert_eq!(report.stats.jobs_completed, 1);
    assert_eq!(report.stats.jobs_failed, 1);
    assert_eq!(report.stats.worker_crashes, 1);
    assert_eq!(report.stats.worker_respawns, 1);
    assert_eq!(report.stats.workers_alive, config().workers);
}
