pub(crate) mod tina_impl;
pub(crate) mod tokio_impl;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Job {
    Work(u32),
    Poison,
}

pub(crate) fn job_script() -> Vec<Job> {
    vec![
        Job::Work(1),
        Job::Work(2),
        Job::Poison,
        Job::Work(3),
        Job::Poison,
        Job::Work(4),
        Job::Work(5),
    ]
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SideReport {
    pub processed: u32,
    pub poisoned: u32,
    pub restarts: u32,
    pub exit_clean: bool,
}

pub(crate) fn print_report(side: &str, report: &SideReport) {
    println!(
        "side={side} processed={} poisoned={} restarts={} exit_clean={}",
        report.processed, report.poisoned, report.restarts, report.exit_clean
    );
}

pub(crate) fn assert_equivalent(tokio: &SideReport, tina: &SideReport) {
    let script = job_script();
    let expected_work = script.iter().filter(|j| matches!(j, Job::Work(_))).count() as u32;
    let expected_poison = script.iter().filter(|j| matches!(j, Job::Poison)).count() as u32;

    assert_eq!(
        tokio.processed, expected_work,
        "tokio processed every Work job"
    );
    assert_eq!(
        tina.processed, expected_work,
        "tina processed every Work job"
    );
    assert_eq!(
        tokio.poisoned, expected_poison,
        "tokio observed every Poison",
    );
    assert_eq!(tina.poisoned, expected_poison, "tina observed every Poison",);
    assert_eq!(tokio.restarts, expected_poison, "tokio respawned each time");
    assert_eq!(
        tina.restarts, expected_poison,
        "tina supervisor restarted each time"
    );
    assert!(tokio.exit_clean, "tokio side exited cleanly");
    assert!(tina.exit_clean, "tina side exited cleanly");
}
