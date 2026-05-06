use std::collections::VecDeque;
use std::convert::Infallible;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tina::{Address, Mailbox, TrySendError, prelude::*};
use tina_runtime::{
    CallError, CallKind, CallOutcome, LocalSystem, LocalSystemState, MailboxFactory,
    RuntimeEventKind, SendOutcome, ThreadedRuntimeError, TraceRetention, call, journal_append,
    send_observed, sleep_then,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ServiceShard(u32);

impl Shard for ServiceShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

struct ServiceMailbox<T> {
    capacity: usize,
    queue: Mutex<VecDeque<T>>,
    closed: Mutex<bool>,
}

impl<T> ServiceMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: Mutex::new(VecDeque::new()),
            closed: Mutex::new(false),
        }
    }
}

impl<T> Mailbox<T> for ServiceMailbox<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }

    fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        if *self.closed.lock().expect("closed lock") {
            return Err(TrySendError::Closed(message));
        }
        let mut queue = self.queue.lock().expect("queue lock");
        if queue.len() >= self.capacity {
            return Err(TrySendError::Full(message));
        }
        queue.push_back(message);
        Ok(())
    }

    fn recv(&self) -> Option<T> {
        self.queue.lock().expect("queue lock").pop_front()
    }

    fn close(&self) {
        *self.closed.lock().expect("closed lock") = true;
    }
}

#[derive(Debug, Clone, Copy)]
struct ServiceMailboxFactory;

impl MailboxFactory for ServiceMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(ServiceMailbox::new(capacity))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ServiceRequest {
    key: u64,
    body: String,
}

#[derive(Debug, Clone)]
enum WorkerMsg {
    Work(ServiceRequest),
    Journaled(Result<(), CallError>, ServiceRequest, u64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WorkerReply {
    Stored { key: u64, body: String },
    DurableFailure(CallError),
}

#[derive(Debug)]
struct DurableWorker {
    journal_path: PathBuf,
    next_index: u64,
    observed: Arc<Mutex<Vec<ServiceObservation>>>,
}

#[tina_runtime::isolate(
    message = WorkerMsg,
    reply = WorkerReply,
    call = tina_runtime::RuntimeCall<WorkerMsg>,
    shard = ServiceShard
)]
impl DurableWorker {
    fn handle(&mut self, msg: WorkerMsg, _ctx: &mut Context<'_, ServiceShard>) -> Effect<Self> {
        match msg {
            WorkerMsg::Work(request) => {
                let index = self.next_index;
                journal_append(
                    self.journal_path.clone(),
                    index,
                    format!("{}:{}", request.key, request.body).into_bytes(),
                )
                .reply(move |result| WorkerMsg::Journaled(result, request, index))
            }
            WorkerMsg::Journaled(Ok(()), request, index) => {
                self.next_index = index + 1;
                self.observed
                    .lock()
                    .expect("worker observed lock")
                    .push(ServiceObservation::WorkerStored(request.key));
                reply(WorkerReply::Stored {
                    key: request.key,
                    body: request.body,
                })
            }
            WorkerMsg::Journaled(Err(error), _, _) => reply(WorkerReply::DurableFailure(error)),
        }
    }
}

#[derive(Debug, Clone)]
enum RouterMsg {
    Submit(ServiceRequest),
    Retry(ServiceRequest, usize),
    Routed(ServiceRequest, usize, CallOutcome<WorkerReply>),
    RouteToWrongShard(ServiceRequest, Address<WorkerMsg, WorkerReply>),
}

#[derive(Debug)]
struct Router {
    even_worker: Address<WorkerMsg, WorkerReply>,
    odd_worker: Address<WorkerMsg, WorkerReply>,
    max_retries: usize,
    observed: Arc<Mutex<Vec<ServiceObservation>>>,
}

#[tina_runtime::isolate(
    message = RouterMsg,
    call = tina_runtime::RuntimeCall<RouterMsg>,
    shard = ServiceShard
)]
impl Router {
    fn handle(&mut self, msg: RouterMsg, _ctx: &mut Context<'_, ServiceShard>) -> Effect<Self> {
        match msg {
            RouterMsg::Submit(request) => self.route(request, 0),
            RouterMsg::Retry(request, attempt) => {
                self.observed.lock().expect("router observed lock").push(
                    ServiceObservation::RetryWoke {
                        key: request.key,
                        attempt,
                    },
                );
                self.route(request, attempt)
            }
            RouterMsg::Routed(request, attempt, outcome) => match outcome {
                CallOutcome::Replied(WorkerReply::Stored { body, .. }) => {
                    self.observed.lock().expect("router observed lock").push(
                        ServiceObservation::Accepted {
                            key: request.key,
                            body,
                        },
                    );
                    noop()
                }
                CallOutcome::Replied(WorkerReply::DurableFailure(error)) => {
                    self.observed.lock().expect("router observed lock").push(
                        ServiceObservation::Rejected {
                            key: request.key,
                            reason: format!("durable:{error:?}"),
                        },
                    );
                    noop()
                }
                CallOutcome::Full if attempt < self.max_retries => {
                    self.observed.lock().expect("router observed lock").push(
                        ServiceObservation::BackoffScheduled {
                            key: request.key,
                            attempt: attempt + 1,
                        },
                    );
                    sleep_then(
                        Duration::from_millis(1),
                        RouterMsg::Retry(request, attempt + 1),
                    )
                }
                CallOutcome::Full => {
                    self.observed.lock().expect("router observed lock").push(
                        ServiceObservation::Rejected {
                            key: request.key,
                            reason: "busy".to_string(),
                        },
                    );
                    noop()
                }
                CallOutcome::Closed => {
                    self.observed.lock().expect("router observed lock").push(
                        ServiceObservation::Rejected {
                            key: request.key,
                            reason: "closed".to_string(),
                        },
                    );
                    noop()
                }
                CallOutcome::Timeout => {
                    self.observed.lock().expect("router observed lock").push(
                        ServiceObservation::Rejected {
                            key: request.key,
                            reason: "timeout".to_string(),
                        },
                    );
                    noop()
                }
            },
            RouterMsg::RouteToWrongShard(request, target) => {
                let expected = self.expected_worker(request.key);
                if target.shard() != expected.shard() {
                    self.observed.lock().expect("router observed lock").push(
                        ServiceObservation::Rejected {
                            key: request.key,
                            reason: "wrong-shard".to_string(),
                        },
                    );
                    noop()
                } else {
                    call(
                        target,
                        WorkerMsg::Work(request.clone()),
                        Duration::from_secs(1),
                    )
                    .reply(move |outcome| RouterMsg::Routed(request, 0, outcome))
                }
            }
        }
    }
}

impl Router {
    fn route(&self, request: ServiceRequest, attempt: usize) -> Effect<Self> {
        let target = self.expected_worker(request.key);
        call(
            target,
            WorkerMsg::Work(request.clone()),
            Duration::from_secs(1),
        )
        .reply(move |outcome| RouterMsg::Routed(request, attempt, outcome))
    }

    fn expected_worker(&self, key: u64) -> Address<WorkerMsg, WorkerReply> {
        if key % 2 == 0 {
            self.even_worker
        } else {
            self.odd_worker
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ServiceObservation {
    WorkerStored(u64),
    AuditStored(u64),
    Accepted { key: u64, body: String },
    BackoffScheduled { key: u64, attempt: usize },
    RetryWoke { key: u64, attempt: usize },
    Rejected { key: u64, reason: String },
}

#[derive(Debug, Clone)]
enum AuditMsg {
    Record(u64),
}

#[derive(Debug)]
struct AuditSink {
    observed: Arc<Mutex<Vec<ServiceObservation>>>,
}

#[tina_runtime::isolate(message = AuditMsg, shard = ServiceShard)]
impl AuditSink {
    fn handle(&mut self, msg: AuditMsg, _ctx: &mut Context<'_, ServiceShard>) -> Effect<Self> {
        match msg {
            AuditMsg::Record(key) => {
                self.observed
                    .lock()
                    .expect("audit observed lock")
                    .push(ServiceObservation::AuditStored(key));
                noop()
            }
        }
    }
}

#[derive(Debug, Clone)]
enum AuditedWorkerMsg {
    Work(ServiceRequest),
    AuditResult(ServiceRequest, SendOutcome),
}

#[derive(Debug)]
struct AuditedWorker {
    audit: Address<AuditMsg>,
    observed: Arc<Mutex<Vec<ServiceObservation>>>,
}

#[tina_runtime::isolate(
    message = AuditedWorkerMsg,
    reply = WorkerReply,
    call = tina_runtime::RuntimeCall<AuditedWorkerMsg>,
    shard = ServiceShard
)]
impl AuditedWorker {
    fn handle(
        &mut self,
        msg: AuditedWorkerMsg,
        _ctx: &mut Context<'_, ServiceShard>,
    ) -> Effect<Self> {
        match msg {
            AuditedWorkerMsg::Work(request) => {
                send_observed(self.audit, AuditMsg::Record(request.key))
                    .reply(move |outcome| AuditedWorkerMsg::AuditResult(request, outcome))
            }
            AuditedWorkerMsg::AuditResult(request, SendOutcome::Accepted) => {
                self.observed
                    .lock()
                    .expect("audited worker observed lock")
                    .push(ServiceObservation::WorkerStored(request.key));
                reply(WorkerReply::Stored {
                    key: request.key,
                    body: request.body,
                })
            }
            AuditedWorkerMsg::AuditResult(_, SendOutcome::Full) => {
                reply(WorkerReply::DurableFailure(CallError::TargetFull))
            }
            AuditedWorkerMsg::AuditResult(_, SendOutcome::Closed) => {
                reply(WorkerReply::DurableFailure(CallError::TargetClosed))
            }
        }
    }
}

#[derive(Debug, Clone)]
enum AuditedClientMsg {
    Start(ServiceRequest),
    Returned(ServiceRequest, CallOutcome<WorkerReply>),
}

#[derive(Debug)]
struct AuditedClient {
    worker: Address<AuditedWorkerMsg, WorkerReply>,
    observed: Arc<Mutex<Vec<ServiceObservation>>>,
}

#[tina_runtime::isolate(
    message = AuditedClientMsg,
    call = tina_runtime::RuntimeCall<AuditedClientMsg>,
    shard = ServiceShard
)]
impl AuditedClient {
    fn handle(
        &mut self,
        msg: AuditedClientMsg,
        _ctx: &mut Context<'_, ServiceShard>,
    ) -> Effect<Self> {
        match msg {
            AuditedClientMsg::Start(request) => call(
                self.worker,
                AuditedWorkerMsg::Work(request.clone()),
                Duration::from_secs(1),
            )
            .reply(move |outcome| AuditedClientMsg::Returned(request, outcome)),
            AuditedClientMsg::Returned(request, outcome) => {
                let observation = match outcome {
                    CallOutcome::Replied(WorkerReply::Stored { body, .. }) => {
                        ServiceObservation::Accepted {
                            key: request.key,
                            body,
                        }
                    }
                    CallOutcome::Replied(WorkerReply::DurableFailure(error)) => {
                        ServiceObservation::Rejected {
                            key: request.key,
                            reason: format!("audit:{error:?}"),
                        }
                    }
                    CallOutcome::Full => ServiceObservation::Rejected {
                        key: request.key,
                        reason: "worker-full".to_string(),
                    },
                    CallOutcome::Closed => ServiceObservation::Rejected {
                        key: request.key,
                        reason: "worker-closed".to_string(),
                    },
                    CallOutcome::Timeout => ServiceObservation::Rejected {
                        key: request.key,
                        reason: "worker-timeout".to_string(),
                    },
                };
                self.observed
                    .lock()
                    .expect("audited client observed lock")
                    .push(observation);
                noop()
            }
        }
    }
}

fn wait_until<F>(label: &str, predicate: F)
where
    F: Fn() -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(5);
    while !predicate() {
        if Instant::now() > deadline {
            panic!("wait_until({label}) timed out");
        }
        std::thread::yield_now();
    }
}

fn temp_dir(prefix: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("tina-{prefix}-{}-{unique}", std::process::id()))
}

fn new_worker(
    dir: &std::path::Path,
    name: &str,
    observed: &Arc<Mutex<Vec<ServiceObservation>>>,
) -> DurableWorker {
    DurableWorker {
        journal_path: dir.join(format!("{name}.journal")),
        next_index: 1,
        observed: Arc::clone(observed),
    }
}

#[test]
fn portable_service_harness_routes_persists_and_reports_terminal_truth() {
    let dir = temp_dir("portable-service-happy");
    fs::create_dir_all(&dir).expect("create portable service dir");
    let observed = Arc::new(Mutex::new(Vec::new()));

    let app =
        LocalSystem::<ServiceShard, ServiceMailboxFactory>::multi_shard(ServiceMailboxFactory)
            .shard(ServiceShard(10))
            .shard(ServiceShard(11))
            .shard(ServiceShard(12))
            .ingress_capacity(16)
            .shard_pair_capacity(16)
            .storage_lane_capacity(4)
            .remote_inbound_drain_budget(2)
            .trace_retention(TraceRetention::Bounded(1024))
            .build();

    let even = app
        .register_root_on::<DurableWorker, Infallible>(
            ShardId::new(11),
            new_worker(&dir, "even", &observed),
            8,
        )
        .expect("register even worker");
    let odd = app
        .register_root_on::<DurableWorker, Infallible>(
            ShardId::new(12),
            new_worker(&dir, "odd", &observed),
            8,
        )
        .expect("register odd worker");
    let router = app
        .register_root_on::<Router, Infallible>(
            ShardId::new(10),
            Router {
                even_worker: even,
                odd_worker: odd,
                max_retries: 0,
                observed: Arc::clone(&observed),
            },
            8,
        )
        .expect("register router");

    app.try_send(
        router,
        RouterMsg::Submit(ServiceRequest {
            key: 2,
            body: "llama-feed".to_string(),
        }),
    )
    .expect("submit even request");
    app.try_send(
        router,
        RouterMsg::Submit(ServiceRequest {
            key: 3,
            body: "hay-stack".to_string(),
        }),
    )
    .expect("submit odd request");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let current = observed.lock().expect("observed lock").clone();
        if current.contains(&ServiceObservation::Accepted {
            key: 2,
            body: "llama-feed".to_string(),
        }) && current.contains(&ServiceObservation::Accepted {
            key: 3,
            body: "hay-stack".to_string(),
        }) {
            break;
        }
        assert!(
            Instant::now() <= deadline,
            "portable service accepted both requests; observed = {current:?}; trace = {:#?}",
            app.trace()
        );
        std::thread::yield_now();
    }

    let terminal = app
        .shutdown()
        .drain()
        .join()
        .expect("portable service shutdown");
    assert_eq!(terminal.state(), LocalSystemState::Closed);
    assert!(terminal.shutdown_report().clean());
    assert_eq!(terminal.summary().journal_appended, 2);
    assert_eq!(terminal.summary().call_failed, 0);
    assert_eq!(terminal.summary().send_rejected, 0);
    let topology = terminal.topology().expect("terminal topology exists");
    assert!(topology.shard(ShardId::new(10)).is_some());
    assert!(topology.shard(ShardId::new(11)).is_some());
    assert!(topology.shard(ShardId::new(12)).is_some());
    assert!(
        terminal.trace().iter().any(|event| {
            event.shard() == ShardId::new(10)
                && matches!(
                    event.kind(),
                    RuntimeEventKind::SendAccepted {
                        target_shard,
                        ..
                    } if target_shard == ShardId::new(11)
                )
        }),
        "router should visibly send/call to even worker shard"
    );
    assert!(
        terminal.trace().iter().any(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallCompleted {
                    call_kind: CallKind::JournalAppend,
                    ..
                }
            )
        }),
        "journal append should be visible in terminal trace"
    );

    let even_replay = tina_runtime::persistence::replay_journal(&dir.join("even.journal"))
        .expect("even journal replays");
    assert_eq!(even_replay.records.len(), 1);
    assert_eq!(even_replay.records[0].bytes, b"2:llama-feed");
    let odd_replay = tina_runtime::persistence::replay_journal(&dir.join("odd.journal"))
        .expect("odd journal replays");
    assert_eq!(odd_replay.records.len(), 1);
    assert_eq!(odd_replay.records[0].bytes, b"3:hay-stack");
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn portable_service_wrong_shard_placement_rejects_before_work_runs() {
    let dir = temp_dir("portable-service-wrong-shard");
    fs::create_dir_all(&dir).expect("create portable service dir");
    let observed = Arc::new(Mutex::new(Vec::new()));

    let app =
        LocalSystem::<ServiceShard, ServiceMailboxFactory>::multi_shard(ServiceMailboxFactory)
            .shard(ServiceShard(20))
            .shard(ServiceShard(21))
            .shard(ServiceShard(22))
            .ingress_capacity(8)
            .shard_pair_capacity(8)
            .storage_lane_capacity(2)
            .trace_retention(TraceRetention::Bounded(256))
            .build();
    let even = app
        .register_root_on::<DurableWorker, Infallible>(
            ShardId::new(21),
            new_worker(&dir, "even", &observed),
            8,
        )
        .expect("register even worker");
    let odd = app
        .register_root_on::<DurableWorker, Infallible>(
            ShardId::new(22),
            new_worker(&dir, "odd", &observed),
            8,
        )
        .expect("register odd worker");
    let router = app
        .register_root_on::<Router, Infallible>(
            ShardId::new(20),
            Router {
                even_worker: even,
                odd_worker: odd,
                max_retries: 0,
                observed: Arc::clone(&observed),
            },
            8,
        )
        .expect("register router");

    app.try_send(
        router,
        RouterMsg::RouteToWrongShard(
            ServiceRequest {
                key: 2,
                body: "should-not-store".to_string(),
            },
            odd,
        ),
    )
    .expect("send bad placement request");

    wait_until("wrong shard rejected", || {
        observed
            .lock()
            .expect("observed lock")
            .contains(&ServiceObservation::Rejected {
                key: 2,
                reason: "wrong-shard".to_string(),
            })
    });

    let terminal = app
        .shutdown()
        .drain()
        .join()
        .expect("wrong-shard app shutdown");
    assert_eq!(terminal.summary().journal_appended, 0);
    assert!(terminal.shutdown_report().clean());
    assert!(!dir.join("even.journal").exists());
    assert!(!dir.join("odd.journal").exists());
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn portable_service_busy_policy_rejects_after_explicit_timer_retry() {
    let dir = temp_dir("portable-service-busy-retry");
    fs::create_dir_all(&dir).expect("create portable service dir");
    let observed = Arc::new(Mutex::new(Vec::new()));

    let app =
        LocalSystem::<ServiceShard, ServiceMailboxFactory>::multi_shard(ServiceMailboxFactory)
            .shard(ServiceShard(30))
            .shard(ServiceShard(31))
            .shard(ServiceShard(32))
            .ingress_capacity(16)
            .shard_pair_capacity(16)
            .storage_lane_capacity(2)
            .trace_retention(TraceRetention::Bounded(512))
            .build();
    let even = app
        .register_root_on::<DurableWorker, Infallible>(
            ShardId::new(31),
            new_worker(&dir, "even", &observed),
            0,
        )
        .expect("register even worker");
    let odd = app
        .register_root_on::<DurableWorker, Infallible>(
            ShardId::new(32),
            new_worker(&dir, "odd", &observed),
            8,
        )
        .expect("register odd worker");
    let router = app
        .register_root_on::<Router, Infallible>(
            ShardId::new(30),
            Router {
                even_worker: even,
                odd_worker: odd,
                max_retries: 1,
                observed: Arc::clone(&observed),
            },
            8,
        )
        .expect("register router");

    app.try_send(
        router,
        RouterMsg::Submit(ServiceRequest {
            key: 2,
            body: "busy".to_string(),
        }),
    )
    .expect("send busy request");

    wait_until("busy request exhausted after retry", || {
        let observed = observed.lock().expect("observed lock");
        observed.contains(&ServiceObservation::BackoffScheduled { key: 2, attempt: 1 })
            && observed.contains(&ServiceObservation::RetryWoke { key: 2, attempt: 1 })
            && observed.contains(&ServiceObservation::Rejected {
                key: 2,
                reason: "busy".to_string(),
            })
    });

    let terminal = app
        .shutdown()
        .drain()
        .join()
        .expect("busy-retry app shutdown");
    assert_eq!(terminal.summary().journal_appended, 0);
    assert!(terminal.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallFailed {
                call_kind: CallKind::IsolateCall,
                reason: CallError::TargetFull,
                ..
            }
        )
    }));
    assert!(terminal.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallCompleted {
                call_kind: CallKind::Sleep,
                ..
            }
        )
    }));
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn portable_service_unknown_shard_registration_fails_before_start() {
    let app =
        LocalSystem::<ServiceShard, ServiceMailboxFactory>::multi_shard(ServiceMailboxFactory)
            .shard(ServiceShard(40))
            .ingress_capacity(8)
            .shard_pair_capacity(8)
            .trace_retention(TraceRetention::Bounded(64))
            .build();
    let observed = Arc::new(Mutex::new(Vec::new()));

    let result = app.register_root_on::<DurableWorker, Infallible>(
        ShardId::new(41),
        DurableWorker {
            journal_path: PathBuf::from("unused.journal"),
            next_index: 1,
            observed,
        },
        8,
    );
    assert!(matches!(
        result,
        Err(ThreadedRuntimeError::UnknownShard(shard)) if shard == ShardId::new(41)
    ));

    let terminal = app
        .shutdown()
        .drain()
        .join()
        .expect("unknown-shard app shutdown");
    assert!(terminal.shutdown_report().clean());
}

#[test]
fn portable_service_observed_send_accepted_preserves_original_call() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let app =
        LocalSystem::<ServiceShard, ServiceMailboxFactory>::multi_shard(ServiceMailboxFactory)
            .shard(ServiceShard(60))
            .shard(ServiceShard(61))
            .shard(ServiceShard(62))
            .ingress_capacity(8)
            .shard_pair_capacity(8)
            .trace_retention(TraceRetention::Bounded(512))
            .build();

    let audit = app
        .register_root_on::<AuditSink, Infallible>(
            ShardId::new(61),
            AuditSink {
                observed: Arc::clone(&observed),
            },
            8,
        )
        .expect("register audit sink");
    let worker = app
        .register_root_on::<AuditedWorker, Infallible>(
            ShardId::new(61),
            AuditedWorker {
                audit,
                observed: Arc::clone(&observed),
            },
            8,
        )
        .expect("register audited worker");
    let client = app
        .register_root_on::<AuditedClient, Infallible>(
            ShardId::new(60),
            AuditedClient {
                worker,
                observed: Arc::clone(&observed),
            },
            8,
        )
        .expect("register audited client");

    app.try_send(
        client,
        AuditedClientMsg::Start(ServiceRequest {
            key: 9,
            body: "audit-first".to_string(),
        }),
    )
    .expect("start audited request");
    wait_until(
        "audited observed-send accepted continuation replies",
        || {
            observed
                .lock()
                .expect("observed lock")
                .contains(&ServiceObservation::Accepted {
                    key: 9,
                    body: "audit-first".to_string(),
                })
        },
    );
    wait_until("audit target eventually handles accepted send", || {
        observed
            .lock()
            .expect("observed lock")
            .contains(&ServiceObservation::AuditStored(9))
    });
    let seen = observed.lock().expect("observed lock").clone();
    assert!(
        seen.contains(&ServiceObservation::AuditStored(9)),
        "accepted audit send should eventually run the target side effect: {seen:?}"
    );
    assert!(
        seen.contains(&ServiceObservation::WorkerStored(9)),
        "worker should record stored after observed-send completion: {seen:?}"
    );

    let terminal = app
        .shutdown()
        .drain()
        .join()
        .expect("audited service shutdown");
    assert!(terminal.shutdown_report().clean());
    assert!(
        terminal.trace().iter().any(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallCompleted {
                    call_kind: CallKind::ObservedSend,
                    ..
                }
            )
        }),
        "observed send completion should be visible in terminal trace"
    );
    assert_eq!(terminal.summary().call_failed, 0);
    assert_eq!(terminal.summary().call_reply_rejected, 0);
}

#[test]
fn portable_service_observed_send_full_still_replies_to_original_call() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let app =
        LocalSystem::<ServiceShard, ServiceMailboxFactory>::multi_shard(ServiceMailboxFactory)
            .shard(ServiceShard(70))
            .shard(ServiceShard(71))
            .shard(ServiceShard(72))
            .ingress_capacity(8)
            .shard_pair_capacity(8)
            .trace_retention(TraceRetention::Bounded(512))
            .build();

    let audit = app
        .register_root_on::<AuditSink, Infallible>(
            ShardId::new(71),
            AuditSink {
                observed: Arc::clone(&observed),
            },
            0,
        )
        .expect("register full audit sink");
    let worker = app
        .register_root_on::<AuditedWorker, Infallible>(
            ShardId::new(71),
            AuditedWorker {
                audit,
                observed: Arc::clone(&observed),
            },
            8,
        )
        .expect("register audited worker");
    let client = app
        .register_root_on::<AuditedClient, Infallible>(
            ShardId::new(70),
            AuditedClient {
                worker,
                observed: Arc::clone(&observed),
            },
            8,
        )
        .expect("register full-audit client");

    app.try_send(
        client,
        AuditedClientMsg::Start(ServiceRequest {
            key: 10,
            body: "audit-full".to_string(),
        }),
    )
    .expect("start full-audit request");
    wait_until("full observed-send continuation replies", || {
        observed
            .lock()
            .expect("observed lock")
            .contains(&ServiceObservation::Rejected {
                key: 10,
                reason: "audit:TargetFull".to_string(),
            })
    });
    assert!(
        !observed
            .lock()
            .expect("observed lock")
            .contains(&ServiceObservation::AuditStored(10)),
        "full audit mailbox must not run the side effect"
    );

    let terminal = app
        .shutdown()
        .drain()
        .join()
        .expect("full-audit service shutdown");
    assert!(terminal.shutdown_report().clean());
    assert!(terminal.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::SendRejected {
                reason: tina_runtime::SendRejectedReason::Full,
                ..
            }
        )
    }));
    assert_eq!(terminal.summary().call_reply_rejected, 0);
}
