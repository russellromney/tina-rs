use std::cell::RefCell;
use std::convert::Infallible;
use std::rc::Rc;
use std::time::Duration;

use tina::{Shard, ShardId, prelude::*};
use tina_runtime::{RuntimeCall, sleep};
use tina_sim::{ReplayArtifact, Simulator, SimulatorConfig};

#[derive(Debug, Default)]
struct ConsumerShard;

impl Shard for ConsumerShard {
    fn id(&self) -> ShardId {
        ShardId::new(61)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryMsg {
    Start,
    Slept(Result<(), tina_runtime::CallError>),
    RetryNow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryObservation {
    Started,
    BackoffElapsed,
    Retried,
}

#[derive(Debug)]
struct RetryWorker {
    observations: Rc<RefCell<Vec<RetryObservation>>>,
}

impl Isolate for RetryWorker {
    type Message = RetryMsg;
    type Reply = ();
    type Send = Outbound<RetryMsg>;
    type Spawn = Infallible;
    type Call = RuntimeCall<RetryMsg>;
    type Shard = ConsumerShard;

    fn handle(&mut self, msg: Self::Message, ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            RetryMsg::Start => {
                self.observations
                    .borrow_mut()
                    .push(RetryObservation::Started);
                sleep(Duration::from_millis(25)).reply(RetryMsg::Slept)
            }
            RetryMsg::Slept(Ok(())) => {
                self.observations
                    .borrow_mut()
                    .push(RetryObservation::BackoffElapsed);
                send(ctx.current_address::<RetryMsg>(), RetryMsg::RetryNow)
            }
            RetryMsg::RetryNow => {
                self.observations
                    .borrow_mut()
                    .push(RetryObservation::Retried);
                stop()
            }
            RetryMsg::Slept(Err(_)) => stop(),
        }
    }
}

fn run_retry_workload(config: SimulatorConfig) -> (Vec<RetryObservation>, ReplayArtifact) {
    let observations = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(ConsumerShard, config);
    let worker = sim.register_with_capacity(
        RetryWorker {
            observations: Rc::clone(&observations),
        },
        8,
    );
    sim.try_send(worker, RetryMsg::Start).unwrap();
    sim.run_until_quiescent();
    (observations.borrow().clone(), sim.replay_artifact())
}

#[test]
fn downstream_consumer_can_use_simulator_with_preferred_runtime_names() {
    let (observations, artifact) = run_retry_workload(SimulatorConfig { seed: 17 });

    assert_eq!(
        observations,
        vec![
            RetryObservation::Started,
            RetryObservation::BackoffElapsed,
            RetryObservation::Retried,
        ]
    );
    assert_eq!(artifact.final_time(), Duration::from_millis(25));
}

#[test]
fn downstream_consumer_can_replay_the_same_simulated_run() {
    let (_, artifact) = run_retry_workload(SimulatorConfig { seed: 91 });
    let (_, replayed) = run_retry_workload(artifact.config());

    assert_eq!(artifact.event_record(), replayed.event_record());
    assert_eq!(artifact.final_time(), replayed.final_time());
}
