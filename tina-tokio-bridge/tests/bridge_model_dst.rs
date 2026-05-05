use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelOp {
    Submit(u64),
    TimeoutOldest,
    WorkerStep,
    Close,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ModelEvent {
    Accepted(u64),
    Full(u64),
    TimedOut(u64),
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

fn run_model(ops: &[ModelOp]) -> Vec<ModelEvent> {
    let mut model = BridgeModel::new(3);
    for op in ops {
        match *op {
            ModelOp::Submit(id) => model.submit(id),
            ModelOp::TimeoutOldest => model.timeout_oldest(),
            ModelOp::WorkerStep => model.worker_step(),
            ModelOp::Close => model.close(),
        }
    }
    while !model.queue.is_empty() {
        model.worker_step();
    }
    model.events
}

#[test]
fn bridge_ingress_model_dst_keeps_timeout_from_mutating_service_state() {
    for seed in 0..64 {
        let ops = random_ops(seed);
        let first = run_model(&ops);
        let second = run_model(&ops);
        assert_eq!(first, second, "bridge model replay drift for seed {seed}");

        let timed_out: Vec<u64> = first
            .iter()
            .filter_map(|event| match event {
                ModelEvent::TimedOut(id) => Some(*id),
                _ => None,
            })
            .collect();
        for id in timed_out {
            assert!(
                first.contains(&ModelEvent::SkippedCancelled(id)),
                "timed-out request {id} should be skipped by worker"
            );
            assert!(
                !first.contains(&ModelEvent::Mutated(id)),
                "timed-out request {id} must not mutate service state"
            );
        }
    }
}
