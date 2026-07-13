//! Smoke tests: each side runs the script through the supervisor
//! and produces the same expected counts (every Work was processed,
//! every Poison panicked + restarted).

use specimen_supervised_worker::{Job, Report, job_script, tina_impl, tokio_impl};

fn expected() -> Report {
    let script = job_script();
    let work = script.iter().filter(|j| matches!(j, Job::Work(_))).count() as u32;
    let poison = script.iter().filter(|j| matches!(j, Job::Poison)).count() as u32;
    Report {
        processed: work,
        poisoned: poison,
        restarts: poison,
        exit_clean: true,
    }
}

#[test]
fn tokio_smoke() {
    let report = tokio_impl::run().expect("tokio side ran");
    assert_eq!(report, expected());
}

#[test]
fn tina_smoke() {
    let report = tina_impl::run().expect("tina side ran");
    assert_eq!(report, expected());
}

#[test]
fn tina_restart_handoff_is_race_stable() {
    for attempt in 0..20 {
        let report = tina_impl::run()
            .unwrap_or_else(|error| panic!("tina restart attempt {attempt} failed: {error:#}"));
        assert_eq!(report, expected(), "restart attempt {attempt}");
    }
}
