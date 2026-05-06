use std::collections::VecDeque;

use tina_sim::dst::{DstRun, History, ShrinkConfig, assert_replays, delete_shrink};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelOp {
    Submit(u64),
    TimeoutOldest,
    Retry(u64),
    WorkerStep,
    Close,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ModelEvent {
    Accepted(u64),
    Full(u64),
    TimedOut(u64),
    Retried(u64),
    SkippedCancelled(u64),
    Mutated(u64),
    Closed(u64),
}

#[derive(Debug, Clone)]
struct Queued {
    id: u64,
    cancelled: bool,
}

#[derive(Debug)]
struct BridgeModel {
    capacity: usize,
    closed: bool,
    queue: VecDeque<Queued>,
    events: Vec<ModelEvent>,
}

impl BridgeModel {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            closed: false,
            queue: VecDeque::new(),
            events: Vec::new(),
        }
    }

    fn submit(&mut self, id: u64) {
        if self.closed {
            self.events.push(ModelEvent::Closed(id));
        } else if self.queue.len() == self.capacity {
            self.events.push(ModelEvent::Full(id));
        } else {
            self.queue.push_back(Queued {
                id,
                cancelled: false,
            });
            self.events.push(ModelEvent::Accepted(id));
        }
    }

    fn timeout_oldest(&mut self) {
        if let Some(request) = self.queue.iter_mut().find(|request| !request.cancelled) {
            request.cancelled = true;
            self.events.push(ModelEvent::TimedOut(request.id));
        }
    }

    fn retry(&mut self, id: u64) {
        self.events.push(ModelEvent::Retried(id));
        self.submit(id);
    }

    fn worker_step(&mut self) {
        let Some(request) = self.queue.pop_front() else {
            return;
        };
        if request.cancelled {
            self.events.push(ModelEvent::SkippedCancelled(request.id));
        } else {
            self.events.push(ModelEvent::Mutated(request.id));
        }
    }

    fn close(&mut self) {
        self.closed = true;
    }
}

fn xorshift64(mut state: u64) -> u64 {
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    state
}

fn random_ops(seed: u64) -> Vec<ModelOp> {
    let mut state = seed ^ 0xf00d_cafe_5eed_baad;
    let mut ops = Vec::with_capacity(96);
    for index in 0..96 {
        state = xorshift64(state);
        ops.push(match state % 8 {
            0..=3 => ModelOp::Submit(index),
            4 | 5 => ModelOp::WorkerStep,
            6 => ModelOp::TimeoutOldest,
            _ => ModelOp::Close,
        });
    }
    ops
}

fn run_model(history: &History<ModelOp>) -> DstRun<Vec<ModelEvent>, Vec<ModelEvent>> {
    let mut model = BridgeModel::new(3);
    for op in history.operations() {
        match *op {
            ModelOp::Submit(id) => model.submit(id),
            ModelOp::TimeoutOldest => model.timeout_oldest(),
            ModelOp::Retry(id) => model.retry(id),
            ModelOp::WorkerStep => model.worker_step(),
            ModelOp::Close => model.close(),
        }
    }
    while !model.queue.is_empty() {
        model.worker_step();
    }
    DstRun::new(model.events.clone(), model.events)
}

#[test]
fn bridge_ingress_model_dst_keeps_timeout_from_mutating_service_state() {
    for seed in 0..64 {
        let history = History::new("bridge ingress model", seed, random_ops(seed));
        let first = assert_replays(&history, run_model);

        let timed_out: Vec<u64> = first
            .output()
            .iter()
            .filter_map(|event| match event {
                ModelEvent::TimedOut(id) => Some(*id),
                _ => None,
            })
            .collect();
        for id in timed_out {
            assert!(
                first.output().contains(&ModelEvent::SkippedCancelled(id)),
                "timed-out request {id} should be skipped by worker"
            );
            assert!(
                !first.output().contains(&ModelEvent::Mutated(id)),
                "timed-out request {id} must not mutate service state"
            );
        }
    }
}

#[test]
fn baobab_bridge_model_replays_timeout_retry_and_shutdown_contract() {
    let history = History::new(
        "baobab bridge timeout retry shutdown",
        46_200,
        vec![
            ModelOp::Submit(1),
            ModelOp::TimeoutOldest,
            ModelOp::Retry(10),
            ModelOp::WorkerStep,
            ModelOp::WorkerStep,
            ModelOp::Close,
            ModelOp::Submit(99),
        ],
    );
    let run = assert_replays(&history, run_model);

    assert!(run.output().contains(&ModelEvent::TimedOut(1)));
    assert!(run.output().contains(&ModelEvent::SkippedCancelled(1)));
    assert!(run.output().contains(&ModelEvent::Retried(10)));
    assert!(run.output().contains(&ModelEvent::Mutated(10)));
    assert!(run.output().contains(&ModelEvent::Closed(99)));
    assert!(!run.output().contains(&ModelEvent::Mutated(1)));
}

#[test]
fn bridge_history_shrinks_cancelled_mutation_failure_model() {
    let history = History::new(
        "bridge cancelled mutation shrink",
        0,
        vec![
            ModelOp::Submit(1),
            ModelOp::Submit(2),
            ModelOp::TimeoutOldest,
            ModelOp::WorkerStep,
            ModelOp::WorkerStep,
            ModelOp::Close,
        ],
    );
    let still_has_cancel_skip = |candidate: &History<ModelOp>| {
        run_model(candidate)
            .output()
            .contains(&ModelEvent::SkippedCancelled(1))
    };
    assert!(still_has_cancel_skip(&history));

    let shrunk = delete_shrink(
        &history,
        ShrinkConfig::default(),
        "cancelled request still skipped",
        still_has_cancel_skip,
    );
    assert!(shrunk.shrunk().len() < history.len());
    assert!(still_has_cancel_skip(shrunk.shrunk()));
}
