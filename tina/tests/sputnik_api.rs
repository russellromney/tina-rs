use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;

use tina::{
    Address, Context, Effect, Isolate, IsolateId, Mailbox, RestartableSpawnSpec, SendMessage,
    Shard, ShardId, SpawnSpec, TrySendError,
};

#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionMsg {
    Note(String),
    Read,
    SpawnWorker,
    Stop,
    RestartWorkers,
    Ignore,
    StartIo(SessionCallRequest),
    IoCompleted(SessionCallResult),
}

/// A handler-defined description of an external operation the runtime should
/// perform. Substrate-neutral by design: a downstream isolate states what it
/// wants in its own vocabulary without knowing how the runtime executes it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionCallRequest {
    Greet(String),
}

/// What the runtime hands back when the call finishes. Mirrors what real
/// runtime crates would do: typed result that the call payload's translator
/// turns into one ordinary `Message` for the isolate.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionCallResult {
    Greeted(String),
}

/// Backend-shape Call type a downstream consumer might define inside a runtime
/// crate. The trait crate only requires `Isolate::Call` to exist; the shape
/// here exercises the "translator inside the call payload" design.
struct SessionCall {
    request: SessionCallRequest,
    translator: Box<dyn FnOnce(SessionCallResult) -> SessionMsg>,
}

impl std::fmt::Debug for SessionCall {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionCall")
            .field("request", &self.request)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AuditMsg {
    Record(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WorkerMsg {
    Start,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Session {
    notes: Vec<String>,
    audit: Address<AuditMsg>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Worker {
    tenant_id: u64,
}

impl Worker {
    fn new(tenant_id: u64) -> Self {
        Self { tenant_id }
    }
}

#[derive(Debug, Default)]
struct InlineShard;

impl Shard for InlineShard {
    fn id(&self) -> ShardId {
        ShardId::new(11)
    }
}

impl Isolate for Worker {
    type Message = WorkerMsg;
    type Reply = ();
    type Send = SendMessage<WorkerMsg>;
    type Spawn = SpawnSpec<Self>;
    type Call = Infallible;
    type Shard = InlineShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            WorkerMsg::Start => Effect::Noop,
        }
    }
}

impl Isolate for Session {
    type Message = SessionMsg;
    type Reply = Vec<String>;
    type Send = SendMessage<AuditMsg>;
    type Spawn = SpawnSpec<Worker>;
    type Call = SessionCall;
    type Shard = InlineShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            SessionMsg::Note(note) => {
                self.notes.push(note.clone());
                Effect::Send(SendMessage::new(self.audit, AuditMsg::Record(note)))
            }
            SessionMsg::Read => Effect::Reply(self.notes.clone()),
            SessionMsg::SpawnWorker => Effect::Spawn(SpawnSpec::new(Worker::new(0), 8)),
            SessionMsg::Stop => Effect::Stop,
            SessionMsg::RestartWorkers => Effect::RestartChildren,
            SessionMsg::Ignore => Effect::Noop,
            SessionMsg::StartIo(request) => Effect::Call(SessionCall {
                request,
                translator: Box::new(SessionMsg::IoCompleted),
            }),
            SessionMsg::IoCompleted(SessionCallResult::Greeted(text)) => {
                self.notes.push(text.clone());
                Effect::Send(SendMessage::new(self.audit, AuditMsg::Record(text)))
            }
        }
    }
}

struct BoundedMailbox<T> {
    capacity: usize,
    queue: RefCell<VecDeque<T>>,
    closed: Cell<bool>,
}

impl<T> BoundedMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: RefCell::new(VecDeque::new()),
            closed: Cell::new(false),
        }
    }
}

impl<T> Mailbox<T> for BoundedMailbox<T> {
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

#[test]
fn downstream_consumer_can_define_isolates_and_observe_all_effect_kinds() {
    let audit = Address::new(ShardId::new(3), IsolateId::new(41));
    let mut shard = InlineShard;
    let mut ctx = Context::new(&mut shard, IsolateId::new(7));
    let mut session = Session {
        notes: Vec::new(),
        audit,
    };

    match session.handle(SessionMsg::Note("hello".to_owned()), &mut ctx) {
        Effect::Send(outbound) => {
            assert_eq!(outbound.destination(), audit);
            assert_eq!(outbound.message(), &AuditMsg::Record("hello".to_owned()));
        }
        other => panic!("expected send effect, got {other:?}"),
    }

    assert!(matches!(
        session.handle(SessionMsg::Read, &mut ctx),
        Effect::Reply(ref notes) if notes == &vec!["hello".to_owned()]
    ));

    match session.handle(SessionMsg::SpawnWorker, &mut ctx) {
        Effect::Spawn(spec) => {
            assert_eq!(spec.mailbox_capacity(), 8);
            let (_worker, mailbox_capacity, bootstrap) = spec.into_parts();
            assert_eq!(mailbox_capacity, 8);
            assert!(bootstrap.is_none());
        }
        other => panic!("expected spawn effect, got {other:?}"),
    }

    assert!(matches!(
        session.handle(SessionMsg::Stop, &mut ctx),
        Effect::Stop
    ));
    assert!(matches!(
        session.handle(SessionMsg::RestartWorkers, &mut ctx),
        Effect::RestartChildren
    ));
    assert!(matches!(
        session.handle(SessionMsg::Ignore, &mut ctx),
        Effect::Noop
    ));

    // Call effect: handler returns a runtime-owned call description that
    // bundles a translator. Invoking the translator with a runtime-shaped
    // result must yield exactly one ordinary `SessionMsg` value the handler
    // would receive on a later turn — proving the "completion as ordinary
    // message" design without making the handler async.
    let call_request = SessionCallRequest::Greet("hello".to_owned());
    match session.handle(SessionMsg::StartIo(call_request.clone()), &mut ctx) {
        Effect::Call(call) => {
            assert_eq!(call.request, call_request);
            let later = (call.translator)(SessionCallResult::Greeted("hello back".to_owned()));
            assert_eq!(
                later,
                SessionMsg::IoCompleted(SessionCallResult::Greeted("hello back".to_owned()))
            );
        }
        other => panic!("expected call effect, got {other:?}"),
    }
}

#[test]
fn context_builds_typed_addresses_for_current_and_remote_isolates() {
    let mut shard = InlineShard;
    let ctx = Context::new(&mut shard, IsolateId::new(12));

    let current = ctx.current_address::<SessionMsg>();
    let local = ctx.local_address::<AuditMsg>(IsolateId::new(19));
    let remote = ctx.address::<WorkerMsg>(ShardId::new(9), IsolateId::new(23));

    assert_eq!(ctx.shard_id(), ShardId::new(11));
    assert_eq!(ctx.isolate_id(), IsolateId::new(12));
    assert_eq!(current, Address::new(ShardId::new(11), IsolateId::new(12)));
    assert_eq!(local, Address::new(ShardId::new(11), IsolateId::new(19)));
    assert_eq!(remote, Address::new(ShardId::new(9), IsolateId::new(23)));
}

#[test]
fn downstream_worker_isolate_can_compile_and_run_through_the_same_surface() {
    let mut shard = InlineShard;
    let mut ctx = Context::new(&mut shard, IsolateId::new(88));
    let mut worker = Worker::new(0);

    assert!(matches!(
        worker.handle(WorkerMsg::Start, &mut ctx),
        Effect::Noop
    ));
}

#[test]
fn restartable_spawn_spec_is_available_as_a_distinct_public_payload() {
    let tenant_id = 9_u64;
    let spec = RestartableSpawnSpec::new(move || Worker::new(tenant_id), 13);

    assert_eq!(spec.mailbox_capacity(), 13);

    let (factory, mailbox_capacity, bootstrap) = spec.into_parts();
    assert_eq!(mailbox_capacity, 13);
    assert_eq!(factory(), Worker::new(tenant_id));
    assert!(bootstrap.is_none());
}

#[test]
fn mailbox_trait_can_express_bounded_delivery_and_error_recovery() {
    let mailbox = BoundedMailbox::new(1);

    assert_eq!(mailbox.capacity(), 1);
    assert_eq!(mailbox.try_send("first"), Ok(()));
    assert_eq!(
        mailbox.try_send("second"),
        Err(TrySendError::Full("second"))
    );
    assert_eq!(mailbox.recv(), Some("first"));

    mailbox.close();

    assert_eq!(
        mailbox.try_send("third"),
        Err(TrySendError::Closed("third"))
    );
    assert_eq!(mailbox.recv(), None);
}
