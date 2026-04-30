use std::cell::RefCell;
use std::convert::Infallible;
use std::rc::Rc;
use std::time::Duration;

use tina::{Context, Effect, Isolate, SendMessage, Shard, ShardId};
use tina_runtime_current::{CallKind, CallRequest, CurrentCall, RuntimeEvent, RuntimeEventKind};
use tina_sim::{ReplayArtifact, Simulator, SimulatorConfig};

#[derive(Debug, Default)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(52)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryMsg {
    Attempt,
    RetryNow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryObservation {
    Attempted(usize),
    Failed(usize),
    BackoffElapsed,
    Succeeded(usize),
}

#[derive(Debug)]
struct RetryWorker {
    backoff: Duration,
    attempts: usize,
    observations: Rc<RefCell<Vec<RetryObservation>>>,
}

impl Isolate for RetryWorker {
    type Message = RetryMsg;
    type Reply = ();
    type Send = SendMessage<RetryMsg>;
    type Spawn = Infallible;
    type Call = CurrentCall<RetryMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            RetryMsg::Attempt => {
                self.attempts += 1;
                self.observations
                    .borrow_mut()
                    .push(RetryObservation::Attempted(self.attempts));
                if self.attempts == 1 {
                    self.observations
                        .borrow_mut()
                        .push(RetryObservation::Failed(self.attempts));
                    Effect::Call(CurrentCall::new(
                        CallRequest::Sleep {
                            after: self.backoff,
                        },
                        |_| RetryMsg::RetryNow,
                    ))
                } else {
                    self.observations
                        .borrow_mut()
                        .push(RetryObservation::Succeeded(self.attempts));
                    Effect::Noop
                }
            }
            RetryMsg::RetryNow => {
                self.observations
                    .borrow_mut()
                    .push(RetryObservation::BackoffElapsed);
                Effect::Send(SendMessage::new(
                    ctx.current_address::<RetryMsg>(),
                    RetryMsg::Attempt,
                ))
            }
        }
    }
}

fn count_call_dispatch_attempted(trace: &[RuntimeEvent], kind: CallKind) -> usize {
    trace
        .iter()
        .filter(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallDispatchAttempted { call_kind, .. } if call_kind == kind
            )
        })
        .count()
}

fn count_call_completed(trace: &[RuntimeEvent], kind: CallKind) -> usize {
    trace
        .iter()
        .filter(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallCompleted { call_kind, .. } if call_kind == kind
            )
        })
        .count()
}

fn run_retry_workload(config: SimulatorConfig) -> (Vec<RetryObservation>, ReplayArtifact) {
    let observations = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(TestShard, config);
    let worker = sim.register(RetryWorker {
        backoff: Duration::from_millis(25),
        attempts: 0,
        observations: Rc::clone(&observations),
    });
    sim.try_send(worker, RetryMsg::Attempt).unwrap();
    sim.run_until_quiescent();
    (observations.borrow().clone(), sim.replay_artifact())
}

#[test]
fn retry_backoff_retries_after_virtual_timer_wake() {
    let (observations, artifact) = run_retry_workload(SimulatorConfig { seed: 77 });

    assert_eq!(
        observations,
        vec![
            RetryObservation::Attempted(1),
            RetryObservation::Failed(1),
            RetryObservation::BackoffElapsed,
            RetryObservation::Attempted(2),
            RetryObservation::Succeeded(2),
        ]
    );

    assert_eq!(
        count_call_dispatch_attempted(artifact.event_record(), CallKind::Sleep),
        1
    );
    assert_eq!(
        count_call_completed(artifact.event_record(), CallKind::Sleep),
        1
    );
    assert!(
        artifact.event_record().iter().any(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallCompleted {
                    call_kind: CallKind::Sleep,
                    ..
                }
            )
        }),
        "retry workload should prove that the delayed path came through simulated Sleep"
    );
    assert_eq!(artifact.final_time(), Duration::from_millis(25));
}

#[test]
fn replay_artifact_reproduces_same_event_record() {
    let (_, artifact) = run_retry_workload(SimulatorConfig { seed: 99 });
    let (_, replayed) = run_retry_workload(artifact.config());
    assert_eq!(artifact.event_record(), replayed.event_record());
    assert_eq!(artifact.final_time(), replayed.final_time());
}
