//! Public runner proof for the job-queue system.
//!
//! Characterization pins overflow admission, cancel-while-running, caller-gone
//! settlement, poison crash + respawn, and typed readiness (no host spin).
//! Public smoke exercises the documented aggregate runner.

use system_job_queue::{
    JobOutcome, QueueReply, RunConfig, run, run_caller_gone, run_cancel_in_flight, run_overflow,
    run_poison_crash, run_respawn_then_admit,
};

fn default_config() -> RunConfig {
    RunConfig {
        workers: 2,
        queue_mailbox: 64,
        worker_mailbox: 8,
        job_sleep_ms: 80,
        call_timeout_ms: 5_000,
    }
}

fn assert_queue_report(report: system_job_queue::RunReport) {
    assert_eq!(report.overflow.completed, 2);
    assert_eq!(report.overflow.busy, 3);
    assert_eq!(report.overflow.stats.workers_alive, 2);
    assert_eq!(report.overflow.stats.cancel_reconciliation_failures, 0);

    assert!(matches!(
        report.cancel_in_flight.submit_outcome,
        JobOutcome::Cancelled { .. }
    ));
    assert!(matches!(
        report.cancel_in_flight.cancel_reply,
        QueueReply::Cancelled(_)
    ));
    assert!(matches!(
        report.cancel_in_flight.refill_outcome,
        JobOutcome::Completed { value: 42, .. }
    ));

    assert!(matches!(
        report.caller_gone.submit_outcome,
        tina_runtime::CallOutcome::Timeout
    ));
    assert_eq!(report.caller_gone.stats.jobs_completed, 1);
    assert_eq!(report.caller_gone.stats.in_flight, 0);

    assert!(matches!(
        report.poison_crash.failed_outcome,
        JobOutcome::Failed { .. }
    ));
    assert_eq!(report.poison_crash.stats.worker_respawns, 1);
    assert_eq!(
        report.poison_crash.stats.workers_alive,
        default_config().workers
    );

    assert!(matches!(
        report.respawn_then_admit.poison_outcome,
        JobOutcome::Failed { .. }
    ));
    assert!(matches!(
        report.respawn_then_admit.follow_up_outcome,
        JobOutcome::Completed { value: 42, .. }
    ));
    assert_eq!(report.respawn_then_admit.stats.worker_respawns, 1);
}

/// Pins admission, cancel, caller-gone, poison, and respawn facts.
#[test]
fn public_characterization() {
    assert_queue_report(run(default_config()).expect("run succeeds"));

    // Focused scenarios still use the same typed readiness path.
    let overflow = run_overflow(default_config()).expect("overflow");
    assert_eq!(overflow.completed, 2);
    assert_eq!(overflow.busy, 3);

    let cancel = run_cancel_in_flight(default_config()).expect("cancel");
    assert!(matches!(cancel.submit_outcome, JobOutcome::Cancelled { .. }));

    let gone = run_caller_gone(default_config()).expect("caller-gone");
    assert!(matches!(
        gone.submit_outcome,
        tina_runtime::CallOutcome::Timeout
    ));

    let poison = run_poison_crash(default_config()).expect("poison");
    assert!(matches!(poison.failed_outcome, JobOutcome::Failed { .. }));
    assert_eq!(poison.stats.workers_alive, default_config().workers);

    let respawn = run_respawn_then_admit(default_config()).expect("respawn");
    assert!(matches!(
        respawn.follow_up_outcome,
        JobOutcome::Completed { value: 42, .. }
    ));
}

/// Documented public runner path: `run(RunConfig)`.
#[test]
fn public_smoke() {
    assert_queue_report(run(default_config()).expect("run succeeds"));
}
