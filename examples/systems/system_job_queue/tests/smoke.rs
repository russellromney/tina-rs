use system_job_queue::{
    JobOutcome, RunConfig, run_cancel_queued, run_overflow, run_poison_retry,
};

fn config() -> RunConfig {
    RunConfig {
        workers: 2,
        queue_capacity: 4,
        pending_capacity: 8,
        queue_mailbox: 64,
        worker_mailbox: 8,
        job_sleep_ms: 80,
        call_timeout_ms: 5_000,
    }
}

#[test]
fn overflow_burst_fills_queue_and_rest_get_busy() {
    // total admission cap = workers + queue_capacity = 6; burst = cap + 3 = 9
    let report = run_overflow(config()).expect("overflow run");
    assert_eq!(report.completed, 6);
    assert_eq!(report.busy, 3);
    assert_eq!(report.stats.jobs_admitted, 6);
    assert_eq!(report.stats.jobs_busy_rejected, 3);
    assert_eq!(report.stats.jobs_completed, 6);
    assert_eq!(report.stats.workers_alive, 2);
    assert_eq!(report.stats.worker_crashes, 0);
}

#[test]
fn cancel_replies_to_parked_queued_callers() {
    let report = run_cancel_queued(config()).expect("cancel run");
    assert!(
        report.cancelled_jobs >= config().queue_capacity,
        "expected at least queue_capacity ({}) cancellations, got {}",
        config().queue_capacity,
        report.cancelled_jobs,
    );
    assert!(
        report.completed_jobs >= config().workers,
        "in-flight jobs ({}) should still complete; got {}",
        config().workers,
        report.completed_jobs,
    );
    assert_eq!(report.stats.workers_alive, 2);
}

#[test]
fn poison_burns_retry_budget_then_marks_failed() {
    let report = run_poison_retry(config()).expect("poison run");
    let JobOutcome::Failed { attempts, .. } = report.failed_outcome else {
        panic!("expected Failed outcome, got {:?}", report.failed_outcome);
    };
    // max_retries = 2 means attempts = 3 (1 original + 2 retries).
    assert_eq!(attempts, 3);
    assert_eq!(report.stats.jobs_failed, 1);
    assert_eq!(report.stats.worker_crashes, 3);
    assert_eq!(report.stats.worker_respawns, 3);
    // After 3 crashes + 3 respawns the pool should be back to full size.
    assert_eq!(report.stats.workers_alive, config().workers);
    assert!(report.stats.retries_used >= 2, "expected retry counter to tick");
}
