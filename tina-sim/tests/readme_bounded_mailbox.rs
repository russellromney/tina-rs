//! Simulator parity for the README bounded-mailbox workflow.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tina::TrySendError;
use tina::prelude::*;
use tina_sim::{Simulator, SimulatorConfig};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Job {
    Run(u64),
    Stop,
}

#[derive(Debug)]
struct Worker {
    processed: Arc<AtomicUsize>,
}

#[tina_runtime::isolate(message = Job)]
impl Worker {
    fn handle(
        &mut self,
        job: Job,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match job {
            Job::Run(_) => {
                self.processed.fetch_add(1, Ordering::Relaxed);
                noop()
            }
            Job::Stop => stop(),
        }
    }
}

fn run(seed: u64) -> (usize, Vec<tina_runtime::RuntimeEvent>) {
    let processed = Arc::new(AtomicUsize::new(0));
    let mut sim = Simulator::new(
        SingleShard,
        SimulatorConfig {
            seed,
            ..SimulatorConfig::default()
        },
    );
    let worker = sim.register_with_mailbox_capacity::<Worker, Job, Infallible>(
        Worker {
            processed: Arc::clone(&processed),
        },
        2,
    );

    sim.try_send(worker, Job::Run(1)).unwrap();
    sim.try_send(worker, Job::Run(2)).unwrap();
    let rejected = match sim.try_send(worker, Job::Run(3)) {
        Err(TrySendError::Full(job)) => job,
        other => panic!("expected Full, got {other:?}"),
    };

    assert_eq!(sim.step(), 1);
    sim.try_send(worker, rejected).unwrap();
    sim.run_until_quiescent();

    sim.try_send(worker, Job::Stop).unwrap();
    assert_eq!(sim.step(), 1);
    assert_eq!(
        sim.try_send(worker, Job::Run(4)),
        Err(TrySendError::Closed(Job::Run(4)))
    );

    (processed.load(Ordering::Relaxed), sim.trace().to_vec())
}

#[test]
fn simulator_matches_full_retry_and_closed_behavior() {
    let (processed, _) = run(7);
    assert_eq!(processed, 3);
}

#[test]
fn default_fault_config_makes_the_seed_inert() {
    let (_, first) = run(7);
    let (_, second) = run(99);
    assert_eq!(
        tina_runtime::stable_trace_hash(first.iter()),
        tina_runtime::stable_trace_hash(second.iter())
    );
}
