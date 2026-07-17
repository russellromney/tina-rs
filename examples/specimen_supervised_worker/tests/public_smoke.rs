use specimen_supervised_worker::{Job, Report, job_script, tina_impl};
use tina::SpawnObservedError;
use tina_impl::{FactoryPanicObserved, FactoryPanicPhase};

fn expected() -> Report {
    let script = job_script();
    Report {
        processed: script
            .iter()
            .filter(|job| matches!(job, Job::Work(_)))
            .count() as u32,
        poisoned: script
            .iter()
            .filter(|job| matches!(job, Job::Poison))
            .count() as u32,
        restarts: script
            .iter()
            .filter(|job| matches!(job, Job::Poison))
            .count() as u32,
        exit_clean: true,
    }
}

#[test]
fn public_smoke() {
    assert_eq!(tina_impl::run().expect("Tina public runner"), expected());
}

#[test]
fn public_characterization() {
    assert_eq!(tina_impl::run().expect("supervision behavior"), expected());
}

#[test]
fn public_correction() {
    assert_eq!(
        tina_impl::run_factory_panic_correction(FactoryPanicPhase::Initial)
            .expect("initial factory panic"),
        FactoryPanicObserved::Initial(SpawnObservedError::FactoryPanicked)
    );
    assert_eq!(
        tina_impl::run_factory_panic_correction(FactoryPanicPhase::Replacement)
            .expect("replacement factory panic"),
        FactoryPanicObserved::Replacement {
            skipped_factory_panicked: 1
        }
    );
}
