use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::rc::Rc;

use tina::{Address, Mailbox, TrySendError, prelude::*};
use tina_runtime::{
    MailboxFactory, MultiShardRuntime, MultiShardRuntimeConfig, RuntimeEvent, RuntimeEventKind,
    SendRejectedReason,
};

#[derive(Debug, Clone, Copy)]
struct WorkShard(u32);

impl Shard for WorkShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

struct TestMailbox<T> {
    capacity: usize,
    queue: Rc<RefCell<VecDeque<T>>>,
    closed: Rc<Cell<bool>>,
}

impl<T> Clone for TestMailbox<T> {
    fn clone(&self) -> Self {
        Self {
            capacity: self.capacity,
            queue: Rc::clone(&self.queue),
            closed: Rc::clone(&self.closed),
        }
    }
}

impl<T> TestMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: Rc::new(RefCell::new(VecDeque::new())),
            closed: Rc::new(Cell::new(false)),
        }
    }
}

impl<T> Mailbox<T> for TestMailbox<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }

    fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        if self.closed.get() {
            return Err(TrySendError::Closed(message));
        }
        let mut queue = self.queue.borrow_mut();
        if queue.len() >= self.capacity {
            return Err(TrySendError::Full(message));
        }
        queue.push_back(message);
        Ok(())
    }

    fn recv(&self) -> Option<T> {
        self.queue.borrow_mut().pop_front()
    }

    fn close(&self) {
        self.closed.set(true);
    }
}

#[derive(Debug, Clone, Copy)]
struct TestMailboxFactory;

impl MailboxFactory for TestMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(TestMailbox::new(capacity))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoordinatorEvent {
    Submit {
        job_id: u64,
        value: u64,
    },
    SubmitPair {
        first_job: u64,
        first_value: u64,
        second_job: u64,
        second_value: u64,
    },
    JobCompleted {
        job_id: u64,
        doubled: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerEvent {
    Run {
        job_id: u64,
        value: u64,
        reply_to: Address<CoordinatorEvent>,
    },
}

#[derive(Debug)]
struct Coordinator {
    worker: Address<WorkerEvent>,
    completed: Rc<RefCell<Vec<(u64, u64)>>>,
}

impl Isolate for Coordinator {
    tina::isolate_types! {
        message: CoordinatorEvent,
        reply: (),
        send: Outbound<WorkerEvent>,
        spawn: Infallible,
        call: Infallible,
        shard: WorkShard,
    }

    fn handle(&mut self, msg: Self::Message, ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            CoordinatorEvent::Submit { job_id, value } => send(
                self.worker,
                WorkerEvent::Run {
                    job_id,
                    value,
                    reply_to: ctx.me(),
                },
            ),
            CoordinatorEvent::SubmitPair {
                first_job,
                first_value,
                second_job,
                second_value,
            } => batch([
                send(
                    self.worker,
                    WorkerEvent::Run {
                        job_id: first_job,
                        value: first_value,
                        reply_to: ctx.me(),
                    },
                ),
                send(
                    self.worker,
                    WorkerEvent::Run {
                        job_id: second_job,
                        value: second_value,
                        reply_to: ctx.me(),
                    },
                ),
            ]),
            CoordinatorEvent::JobCompleted { job_id, doubled } => {
                self.completed.borrow_mut().push((job_id, doubled));
                noop()
            }
        }
    }
}

#[derive(Debug)]
struct Worker;

impl Isolate for Worker {
    tina::isolate_types! {
        message: WorkerEvent,
        reply: (),
        send: Outbound<CoordinatorEvent>,
        spawn: Infallible,
        call: Infallible,
        shard: WorkShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            WorkerEvent::Run {
                job_id,
                value,
                reply_to,
            } => send(
                reply_to,
                CoordinatorEvent::JobCompleted {
                    job_id,
                    doubled: value * 2,
                },
            ),
        }
    }
}

fn run_until_idle(runtime: &mut MultiShardRuntime<WorkShard, TestMailboxFactory>) -> usize {
    let mut total = 0;
    for _ in 0..32 {
        let delivered = runtime.step();
        total += delivered;
        if delivered == 0 && !runtime.has_in_flight_calls() {
            return total;
        }
    }
    panic!("multi-shard runtime did not quiesce within 32 rounds");
}

fn event_id(trace: &[RuntimeEvent], predicate: impl Fn(&RuntimeEvent) -> bool) -> u64 {
    trace
        .iter()
        .find(|event| predicate(event))
        .unwrap_or_else(|| panic!("expected matching event in trace"))
        .id()
        .get()
}

fn run_dispatcher_workload(
    config: MultiShardRuntimeConfig,
    event: CoordinatorEvent,
) -> (Vec<(u64, u64)>, Vec<RuntimeEvent>) {
    let mut runtime =
        MultiShardRuntime::with_config([WorkShard(11), WorkShard(22)], TestMailboxFactory, config);
    let completed = Rc::new(RefCell::new(Vec::new()));

    let worker =
        runtime.register_with_capacity_on::<Worker, CoordinatorEvent>(ShardId::new(22), Worker, 4);
    let coordinator = runtime.register_with_capacity_on::<Coordinator, WorkerEvent>(
        ShardId::new(11),
        Coordinator {
            worker,
            completed: Rc::clone(&completed),
        },
        4,
    );

    runtime.try_send(coordinator, event).unwrap();
    assert!(run_until_idle(&mut runtime) > 0);

    (completed.borrow().clone(), runtime.trace())
}

#[test]
fn dispatcher_worker_workload_preserves_cross_shard_request_reply_causality() {
    let mut runtime = MultiShardRuntime::new([WorkShard(11), WorkShard(22)], TestMailboxFactory);
    let completed = Rc::new(RefCell::new(Vec::new()));

    let worker =
        runtime.register_with_capacity_on::<Worker, CoordinatorEvent>(ShardId::new(22), Worker, 4);
    let coordinator = runtime.register_with_capacity_on::<Coordinator, WorkerEvent>(
        ShardId::new(11),
        Coordinator {
            worker,
            completed: Rc::clone(&completed),
        },
        4,
    );

    runtime
        .try_send(
            coordinator,
            CoordinatorEvent::Submit {
                job_id: 7,
                value: 21,
            },
        )
        .unwrap();

    assert!(run_until_idle(&mut runtime) > 0);
    assert_eq!(&*completed.borrow(), &[(7, 42)]);

    let trace = runtime.trace();
    let request_attempt = event_id(&trace, |event| {
        event.shard() == ShardId::new(11)
            && matches!(
                event.kind(),
                RuntimeEventKind::SendDispatchAttempted {
                    target_shard,
                    target_isolate,
                    ..
                } if target_shard == ShardId::new(22) && target_isolate == worker.isolate()
            )
    });
    let request_accept = event_id(&trace, |event| {
        event.shard() == ShardId::new(11)
            && matches!(
                event.kind(),
                RuntimeEventKind::SendAccepted {
                    target_shard,
                    target_isolate,
                    ..
                } if target_shard == ShardId::new(22) && target_isolate == worker.isolate()
            )
    });
    let worker_mailbox = event_id(&trace, |event| {
        event.shard() == ShardId::new(22)
            && matches!(event.kind(), RuntimeEventKind::MailboxAccepted)
            && event
                .cause()
                .is_some_and(|cause| cause.event().get() == request_attempt)
    });
    let reply_attempt = event_id(&trace, |event| {
        event.shard() == ShardId::new(22)
            && matches!(
                event.kind(),
                RuntimeEventKind::SendDispatchAttempted {
                    target_shard,
                    target_isolate,
                    ..
                } if target_shard == ShardId::new(11) && target_isolate == coordinator.isolate()
            )
    });
    let coordinator_mailbox = event_id(&trace, |event| {
        event.shard() == ShardId::new(11)
            && matches!(event.kind(), RuntimeEventKind::MailboxAccepted)
            && event
                .cause()
                .is_some_and(|cause| cause.event().get() == reply_attempt)
    });

    assert!(request_attempt < request_accept);
    assert!(request_accept < worker_mailbox);
    assert!(worker_mailbox < reply_attempt);
    assert!(reply_attempt < coordinator_mailbox);
}

#[test]
fn dispatcher_worker_workload_surfaces_source_time_full_rejection() {
    let (completed, trace) = run_dispatcher_workload(
        MultiShardRuntimeConfig {
            shard_pair_capacity: 1,
        },
        CoordinatorEvent::SubmitPair {
            first_job: 1,
            first_value: 10,
            second_job: 2,
            second_value: 20,
        },
    );
    assert_eq!(completed, vec![(1, 20)]);

    let full_rejections = trace
        .iter()
        .filter(|event| {
            event.shard() == ShardId::new(11)
                && matches!(
                    event.kind(),
                    RuntimeEventKind::SendRejected {
                        reason: SendRejectedReason::Full,
                        ..
                    }
                )
        })
        .count();
    assert_eq!(full_rejections, 1);
}

#[test]
fn dispatcher_worker_workload_is_deterministic_across_identical_runs() {
    let config = MultiShardRuntimeConfig {
        shard_pair_capacity: 4,
    };
    let event = CoordinatorEvent::Submit {
        job_id: 11,
        value: 5,
    };

    let first = run_dispatcher_workload(config, event);
    let second = run_dispatcher_workload(config, event);

    assert_eq!(first.0, second.0);
    assert_eq!(first.1, second.1);
}
