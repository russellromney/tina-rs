//! Simulator parity for the README bounded-mailbox workflow.

use std::convert::Infallible;

#[allow(dead_code)]
#[path = "../../tina-runtime/examples/bounded_mailbox.rs"]
mod bounded_mailbox;

use bounded_mailbox::{Job, ScenarioReport, Worker};
use tina::prelude::*;
use tina_sim::{Simulator, SimulatorConfig};

fn run(seed: u64) -> (ScenarioReport, Vec<tina_runtime::RuntimeEvent>) {
    let mut sim = Simulator::new(
        SingleShard,
        SimulatorConfig {
            seed,
            ..SimulatorConfig::default()
        },
    );
    let worker = sim.register_with_mailbox_capacity::<Worker, Job, Infallible>(Worker, 2);

    sim.try_send(worker, Job::Run(1)).expect("job 1 fits");
    sim.try_send(worker, Job::Run(2)).expect("job 2 fits");
    let rejected = match sim.try_send(worker, Job::Run(3)) {
        Err(tina_runtime::IngressSendError::Full(job)) => job,
        Err(tina_runtime::IngressSendError::ForeignSystem { .. }) => {
            panic!("worker became foreign")
        }
        other => panic!("expected Full, got {other:?}"),
    };
    assert_eq!(rejected, Job::Run(3), "Full returns the attempted job");

    assert_eq!(sim.step(), 1);
    sim.try_send(worker, rejected)
        .expect("retry fits after one step");
    sim.run_until_quiescent();

    sim.try_send(worker, Job::Stop).expect("stop fits");
    assert_eq!(sim.step(), 1);
    let closed = match sim.try_send(worker, Job::Run(4)) {
        Err(tina_runtime::IngressSendError::Closed(job)) => job,
        Err(tina_runtime::IngressSendError::ForeignSystem { .. }) => {
            panic!("worker became foreign")
        }
        other => panic!("expected Closed, got {other:?}"),
    };
    assert_eq!(closed, Job::Run(4), "Closed returns the attempted job");

    (
        ScenarioReport {
            rejected,
            retried: rejected,
            closed,
        },
        sim.trace().to_vec(),
    )
}

#[test]
fn simulator_matches_full_retry_and_closed_behavior() {
    let (report, _) = run(7);
    assert_eq!(
        report,
        ScenarioReport {
            rejected: Job::Run(3),
            retried: Job::Run(3),
            closed: Job::Run(4),
        }
    );
}

#[test]
fn default_fault_config_is_seed_inert_for_this_workflow() {
    let (first_report, first_trace) = run(7);
    let (second_report, second_trace) = run(99);
    assert_eq!(first_report, second_report);
    assert_eq!(first_report.rejected, Job::Run(3));
    assert_eq!(first_report.closed, Job::Run(4));
    assert_eq!(
        tina_runtime::stable_trace_hash(first_trace.iter()),
        tina_runtime::stable_trace_hash(second_trace.iter())
    );
}
