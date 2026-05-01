use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;

use tina::{Mailbox, TrySendError, prelude::*};

#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionEvent {
    Note(String),
    Read,
    SpawnWorker,
    SpawnAndAudit,
    Stop,
    RestartWorkers,
    Ignore,
    StartIo(SessionCallInput),
    IoCompleted(SessionCallOutput),
}

/// A handler-defined description of an external operation the runtime should
/// perform. Substrate-neutral by design: a downstream isolate states what it
/// wants in its own vocabulary without knowing how the runtime executes it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionCallInput {
    Greet(String),
}

/// What the runtime hands back when the call finishes. Mirrors what real
/// runtime crates would do: typed result that the call payload's translator
/// turns into one ordinary `Message` for the isolate.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionCallOutput {
    Greeted(String),
}

/// Backend-shape Call type a downstream consumer might define inside a runtime
/// crate. The trait crate only requires `Isolate::Call` to exist; the shape
/// here exercises the "translator inside the call payload" design.
struct SessionCall {
    request: SessionCallInput,
    translator: Box<dyn FnOnce(SessionCallOutput) -> SessionEvent>,
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
enum AuditEvent {
    Record(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WorkerEvent {
    Start,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Session {
    notes: Vec<String>,
    audit: Address<AuditEvent>,
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
    tina::isolate_types! {
        message: WorkerEvent,
        reply: (),
        send: Outbound<WorkerEvent>,
        spawn: ChildDefinition<Self>,
        call: Infallible,
        shard: InlineShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            WorkerEvent::Start => noop(),
        }
    }
}

impl Isolate for Session {
    tina::isolate_types! {
        message: SessionEvent,
        reply: Vec<String>,
        send: Outbound<AuditEvent>,
        spawn: ChildDefinition<Worker>,
        call: SessionCall,
        shard: InlineShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            SessionEvent::Note(note) => {
                self.notes.push(note.clone());
                send(self.audit, AuditEvent::Record(note))
            }
            SessionEvent::Read => reply(self.notes.clone()),
            SessionEvent::SpawnWorker => spawn(ChildDefinition::new(Worker::new(0), 8)),
            SessionEvent::SpawnAndAudit => batch([
                spawn(ChildDefinition::new(Worker::new(7), 8)),
                send(self.audit, AuditEvent::Record("spawned".to_owned())),
            ]),
            SessionEvent::Stop => stop(),
            SessionEvent::RestartWorkers => restart_children(),
            SessionEvent::Ignore => noop(),
            SessionEvent::StartIo(request) => Effect::Call(SessionCall {
                request,
                translator: Box::new(SessionEvent::IoCompleted),
            }),
            SessionEvent::IoCompleted(SessionCallOutput::Greeted(text)) => {
                self.notes.push(text.clone());
                send(self.audit, AuditEvent::Record(text))
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

    match session.handle(SessionEvent::Note("hello".to_owned()), &mut ctx) {
        Effect::Send(outbound) => {
            assert_eq!(outbound.destination(), audit);
            assert_eq!(outbound.message(), &AuditEvent::Record("hello".to_owned()));
        }
        other => panic!("expected send effect, got {other:?}"),
    }

    assert!(matches!(
        session.handle(SessionEvent::Read, &mut ctx),
        Effect::Reply(ref notes) if notes == &vec!["hello".to_owned()]
    ));

    match session.handle(SessionEvent::SpawnWorker, &mut ctx) {
        Effect::Spawn(spec) => {
            assert_eq!(spec.mailbox_capacity(), 8);
            let (_worker, mailbox_capacity, bootstrap) = spec.into_parts();
            assert_eq!(mailbox_capacity, 8);
            assert!(bootstrap.is_none());
        }
        other => panic!("expected spawn effect, got {other:?}"),
    }

    match session.handle(SessionEvent::SpawnAndAudit, &mut ctx) {
        Effect::Batch(effects) => {
            assert_eq!(effects.len(), 2);
            let mut effects = effects.into_iter();
            match effects.next().expect("first batch effect") {
                Effect::Spawn(spec) => {
                    let (worker, mailbox_capacity, bootstrap) = spec.into_parts();
                    assert_eq!(worker.tenant_id, 7);
                    assert_eq!(mailbox_capacity, 8);
                    assert!(bootstrap.is_none());
                }
                other => panic!("expected first batched effect to be spawn, got {other:?}"),
            }
            match effects.next().expect("second batch effect") {
                Effect::Send(outbound) => {
                    assert_eq!(outbound.destination(), audit);
                    assert_eq!(
                        outbound.message(),
                        &AuditEvent::Record("spawned".to_owned())
                    );
                }
                other => panic!("expected second batched effect to be send, got {other:?}"),
            }
        }
        other => panic!("expected batch effect, got {other:?}"),
    }

    assert!(matches!(
        session.handle(SessionEvent::Stop, &mut ctx),
        Effect::Stop
    ));
    assert!(matches!(
        session.handle(SessionEvent::RestartWorkers, &mut ctx),
        Effect::RestartChildren
    ));
    assert!(matches!(
        session.handle(SessionEvent::Ignore, &mut ctx),
        Effect::Noop
    ));

    // Call effect: handler returns a runtime-owned call description that
    // bundles a translator. Invoking the translator with a runtime-shaped
    // result must yield exactly one ordinary `SessionEvent` value the handler
    // would receive on a later turn — proving the "completion as ordinary
    // message" design without making the handler async.
    let call_request = SessionCallInput::Greet("hello".to_owned());
    match session.handle(SessionEvent::StartIo(call_request.clone()), &mut ctx) {
        Effect::Call(call) => {
            assert_eq!(call.request, call_request);
            let later = (call.translator)(SessionCallOutput::Greeted("hello back".to_owned()));
            assert_eq!(
                later,
                SessionEvent::IoCompleted(SessionCallOutput::Greeted("hello back".to_owned()))
            );
        }
        other => panic!("expected call effect, got {other:?}"),
    }
}

#[test]
fn context_builds_typed_addresses_for_current_and_remote_isolates() {
    let mut shard = InlineShard;
    let ctx = Context::new(&mut shard, IsolateId::new(12));

    let current: Address<SessionEvent> = ctx.me();
    let local = ctx.local_address::<AuditEvent>(IsolateId::new(19));
    let remote = ctx.address::<WorkerEvent>(ShardId::new(9), IsolateId::new(23));

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
        worker.handle(WorkerEvent::Start, &mut ctx),
        Effect::Noop
    ));
}

#[test]
fn restartable_child_definition_is_available_as_a_distinct_public_payload() {
    let tenant_id = 9_u64;
    let spec = RestartableChildDefinition::new(move || Worker::new(tenant_id), 13);

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
