#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

//! Small current-thread runtime core for `tina-rs`.
//!
//! This crate starts Mariner with the narrowest useful runtime surface:
//!
//! - deterministic runtime event IDs
//! - causal links between runtime events
//! - a legacy one-step single-isolate runner
//! - a tiny single-shard runtime that can host more than one isolate
//!
//! The multi-isolate runtime still stays narrow on purpose. It can register
//! isolates, step them in deterministic order, and execute local same-shard
//! [`Effect::Send`] requests that use [`tina::SendMessage`]. Other effects are
//! still traced before they are executed in later slices.
//!
//! `Effect::Stop` stays immediate, but `CurrentRuntime` now also drains and
//! traces any already-buffered messages that become abandoned when an isolate
//! stops.
//!
//! `CurrentRuntime` also captures unwinding panics from handler calls and turns
//! them into deterministic runtime events. Binaries built with `panic = "abort"`
//! remain out of scope for this crate.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::marker::PhantomData;
use std::panic::{AssertUnwindSafe, catch_unwind};

use tina::{
    Address, Context, Effect, Isolate, IsolateId, Mailbox, SendMessage, Shard, ShardId,
    TrySendError,
};

/// Stable identifier for one runtime event in a deterministic trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EventId(u64);

impl EventId {
    /// Creates an event identifier from a raw integer.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw event identifier.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Identifier used to point at the event that caused another event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CauseId(EventId);

impl CauseId {
    /// Creates a cause identifier from an event identifier.
    pub const fn new(event: EventId) -> Self {
        Self(event)
    }

    /// Returns the event identifier this cause points to.
    pub const fn event(self) -> EventId {
        self.0
    }
}

impl From<EventId> for CauseId {
    fn from(value: EventId) -> Self {
        Self(value)
    }
}

/// Trace-level view of a handler effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EffectKind {
    /// The handler returned [`Effect::Noop`].
    Noop,

    /// The handler returned [`Effect::Reply`].
    Reply,

    /// The handler returned [`Effect::Send`].
    Send,

    /// The handler returned [`Effect::Spawn`].
    Spawn,

    /// The handler returned [`Effect::Stop`].
    Stop,

    /// The handler returned [`Effect::RestartChildren`].
    RestartChildren,
}

/// Why a local send could not be enqueued into the target mailbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SendRejectedReason {
    /// The target mailbox was full.
    Full,

    /// The target mailbox was closed.
    Closed,
}

/// Kind of one runtime event emitted by the runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuntimeEventKind {
    /// The runner accepted one message from a mailbox for delivery.
    MailboxAccepted,

    /// The runner began one handler invocation.
    HandlerStarted,

    /// The handler unwound with a panic instead of returning an effect.
    HandlerPanicked,

    /// The handler returned, including the effect kind it produced.
    HandlerFinished {
        /// The effect kind returned by the handler.
        effect: EffectKind,
    },

    /// The runner observed an effect without executing it.
    EffectObserved {
        /// The effect kind that was observed.
        effect: EffectKind,
    },

    /// The runtime tried to route a local send to another isolate on the same
    /// shard.
    SendDispatchAttempted {
        /// The destination shard.
        target_shard: ShardId,

        /// The destination isolate on that shard.
        target_isolate: IsolateId,
    },

    /// The runtime accepted a local send into the target mailbox.
    SendAccepted {
        /// The destination shard.
        target_shard: ShardId,

        /// The destination isolate on that shard.
        target_isolate: IsolateId,
    },

    /// The runtime rejected a local send.
    SendRejected {
        /// The destination shard.
        target_shard: ShardId,

        /// The destination isolate on that shard.
        target_isolate: IsolateId,

        /// Why the target mailbox rejected the send.
        reason: SendRejectedReason,
    },

    /// The runtime created one local child isolate from a spawn effect.
    Spawned {
        /// The isolate identifier assigned to the new child.
        child_isolate: IsolateId,
    },

    /// The runner applied the stopped state after observing [`Effect::Stop`].
    IsolateStopped,

    /// The runtime drained one already-buffered message from a stopped
    /// isolate's mailbox without delivering it to the handler.
    MessageAbandoned,
}

/// One deterministic runtime event with a causal link to an earlier event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RuntimeEvent {
    id: EventId,
    cause: Option<CauseId>,
    shard: ShardId,
    isolate: IsolateId,
    kind: RuntimeEventKind,
}

impl RuntimeEvent {
    /// Creates a new runtime event.
    pub const fn new(
        id: EventId,
        cause: Option<CauseId>,
        shard: ShardId,
        isolate: IsolateId,
        kind: RuntimeEventKind,
    ) -> Self {
        Self {
            id,
            cause,
            shard,
            isolate,
            kind,
        }
    }

    /// Returns the event identifier.
    pub const fn id(self) -> EventId {
        self.id
    }

    /// Returns the optional cause identifier.
    pub const fn cause(self) -> Option<CauseId> {
        self.cause
    }

    /// Returns the shard that emitted the event.
    pub const fn shard(self) -> ShardId {
        self.shard
    }

    /// Returns the isolate that emitted the event.
    pub const fn isolate(self) -> IsolateId {
        self.isolate
    }

    /// Returns the event kind.
    pub const fn kind(self) -> RuntimeEventKind {
        self.kind
    }
}

/// Result of one `step_once` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcome {
    /// No message was delivered. The mailbox was empty or the isolate was
    /// already stopped.
    Idle,

    /// One message was delivered and traced.
    Delivered,
}

/// Runtime-owned mailbox factory for spawned children.
///
/// The factory lives in `tina-runtime-current`, not in `tina`, because child
/// mailbox allocation is a runtime concern rather than a trait-crate concern.
pub trait MailboxFactory {
    /// Creates one typed mailbox with the requested capacity.
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>>;
}

/// Tiny single-isolate runtime runner for the first Mariner slice.
///
/// The runner owns one isolate, one mailbox, and one shard value. Each
/// [`step_once`](Self::step_once) call delivers at most one message, runs one
/// handler, and appends the matching runtime events to the trace.
pub struct SingleIsolateRunner<I, M, S>
where
    I: Isolate<Shard = S>,
    M: Mailbox<I::Message>,
    S: Shard,
{
    isolate: I,
    mailbox: M,
    shard: S,
    isolate_id: IsolateId,
    stopped: bool,
    next_event_id: u64,
    trace: Vec<RuntimeEvent>,
}

impl<I, M, S> SingleIsolateRunner<I, M, S>
where
    I: Isolate<Shard = S>,
    M: Mailbox<I::Message>,
    S: Shard,
{
    /// Creates a new single-isolate runner.
    pub fn new(isolate: I, mailbox: M, shard: S, isolate_id: IsolateId) -> Self {
        Self {
            isolate,
            mailbox,
            shard,
            isolate_id,
            stopped: false,
            next_event_id: 1,
            trace: Vec::new(),
        }
    }

    /// Returns a shared reference to the isolate.
    pub const fn isolate(&self) -> &I {
        &self.isolate
    }

    /// Returns a shared reference to the mailbox.
    pub const fn mailbox(&self) -> &M {
        &self.mailbox
    }

    /// Returns whether the isolate has already stopped.
    pub const fn is_stopped(&self) -> bool {
        self.stopped
    }

    /// Returns the runtime trace accumulated so far.
    pub fn trace(&self) -> &[RuntimeEvent] {
        &self.trace
    }

    /// Delivers at most one message and records the resulting runtime events.
    pub fn step_once(&mut self) -> StepOutcome {
        if self.stopped {
            return StepOutcome::Idle;
        }

        let Some(message) = self.mailbox.recv() else {
            return StepOutcome::Idle;
        };

        let mailbox_accepted = push_event(
            &mut self.next_event_id,
            &mut self.trace,
            self.shard.id(),
            self.isolate_id,
            None,
            RuntimeEventKind::MailboxAccepted,
        );
        let handler_started = push_event(
            &mut self.next_event_id,
            &mut self.trace,
            self.shard.id(),
            self.isolate_id,
            Some(mailbox_accepted.into()),
            RuntimeEventKind::HandlerStarted,
        );

        let effect = {
            let mut ctx = Context::new(&mut self.shard, self.isolate_id);
            self.isolate.handle(message, &mut ctx)
        };

        let effect_kind = effect_kind(&effect);
        let handler_finished = push_event(
            &mut self.next_event_id,
            &mut self.trace,
            self.shard.id(),
            self.isolate_id,
            Some(handler_started.into()),
            RuntimeEventKind::HandlerFinished {
                effect: effect_kind,
            },
        );

        match effect {
            Effect::Stop => {
                self.stopped = true;
                push_event(
                    &mut self.next_event_id,
                    &mut self.trace,
                    self.shard.id(),
                    self.isolate_id,
                    Some(handler_finished.into()),
                    RuntimeEventKind::IsolateStopped,
                );
            }
            Effect::Noop
            | Effect::Reply(_)
            | Effect::Send(_)
            | Effect::Spawn(_)
            | Effect::RestartChildren => {
                push_event(
                    &mut self.next_event_id,
                    &mut self.trace,
                    self.shard.id(),
                    self.isolate_id,
                    Some(handler_finished.into()),
                    RuntimeEventKind::EffectObserved {
                        effect: effect_kind,
                    },
                );
            }
        }

        StepOutcome::Delivered
    }
}

/// Small deterministic single-shard runtime for the second Mariner slice.
///
/// The runtime owns one shard value plus a private registry of isolates and
/// mailboxes. [`step`](Self::step) walks registered isolates in registration
/// order and gives each isolate at most one delivery chance per round.
pub struct CurrentRuntime<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    shard: S,
    mailbox_factory: F,
    entries: Vec<RegisteredEntry<S, F>>,
    next_isolate_id: u64,
    next_event_id: u64,
    trace: Vec<RuntimeEvent>,
}

impl<S, F> CurrentRuntime<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    /// Creates a new runtime for one shard plus one runtime-owned mailbox
    /// factory for future spawned children.
    pub fn new(shard: S, mailbox_factory: F) -> Self {
        Self {
            shard,
            mailbox_factory,
            entries: Vec::new(),
            next_isolate_id: 1,
            next_event_id: 1,
            trace: Vec::new(),
        }
    }

    /// Returns a shared reference to the shard.
    pub const fn shard(&self) -> &S {
        &self.shard
    }

    /// Returns the accumulated runtime trace.
    pub fn trace(&self) -> &[RuntimeEvent] {
        &self.trace
    }

    /// Registers one isolate and returns its typed address.
    ///
    /// Isolate identifiers are assigned in registration order, starting at `1`.
    #[allow(private_bounds)]
    pub fn register<I, M, Outbound>(&mut self, isolate: I, mailbox: M) -> Address<I::Message>
    where
        I: Isolate<Shard = S, Send = SendMessage<Outbound>> + 'static,
        I::Message: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        Outbound: 'static,
        M: Mailbox<I::Message> + 'static,
    {
        let isolate_id = self.register_entry::<I, Outbound>(
            isolate,
            Box::new(MailboxAdapter::<M, I::Message> {
                mailbox,
                marker: PhantomData,
            }),
        );

        self.shard.address(isolate_id)
    }

    /// Attempts to enqueue a typed message into one registered isolate.
    ///
    /// This is the runtime-side ingress surface for tests and later drivers.
    /// It preserves the mailbox's typed `Full` and `Closed` outcomes, while
    /// still treating unknown isolate IDs as programmer error.
    pub fn try_send<M: 'static>(
        &self,
        address: Address<M>,
        message: M,
    ) -> Result<(), TrySendError<M>> {
        if address.shard() != self.shard.id() {
            panic!(
                "cross-shard runtime ingress is out of scope in this slice: target shard {} != runtime shard {}",
                address.shard().get(),
                self.shard.id().get(),
            );
        }

        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.id == address.isolate())
        else {
            panic!(
                "runtime ingress targeted unknown isolate {} on shard {}",
                address.isolate().get(),
                address.shard().get(),
            );
        };

        match entry.mailbox.try_send_boxed(Box::new(message)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(message)) => Err(TrySendError::Full(
                *message.downcast::<M>().unwrap_or_else(|_| {
                    panic!("runtime ingress attempted to deliver a message to a mailbox with the wrong type")
                }),
            )),
            Err(TrySendError::Closed(message)) => Err(TrySendError::Closed(
                *message.downcast::<M>().unwrap_or_else(|_| {
                    panic!("runtime ingress attempted to deliver a message to a mailbox with the wrong type")
                }),
            )),
        }
    }

    /// Runs one deterministic round over all registered isolates.
    ///
    /// The return value is the number of handlers that ran in this round.
    pub fn step(&mut self) -> usize {
        let round_messages: Vec<Option<Box<dyn Any>>> = self
            .entries
            .iter()
            .map(|entry| {
                if entry.stopped.get() {
                    None
                } else {
                    entry.mailbox.recv_boxed()
                }
            })
            .collect();

        let mut delivered = 0;

        for (index, maybe_message) in round_messages.into_iter().enumerate() {
            let Some(message) = maybe_message else {
                continue;
            };

            delivered += 1;

            let isolate_id = self.entries[index].id;
            let mailbox_accepted =
                self.push_event(isolate_id, None, RuntimeEventKind::MailboxAccepted);
            let handler_started = self.push_event(
                isolate_id,
                Some(mailbox_accepted.into()),
                RuntimeEventKind::HandlerStarted,
            );

            let effect = {
                let mut handler = self.entries[index].handler.borrow_mut();
                catch_unwind(AssertUnwindSafe(|| {
                    handler.handle_boxed(message, &mut self.shard, isolate_id)
                }))
            };

            let effect = match effect {
                Ok(effect) => effect,
                Err(_) => {
                    let handler_panicked = self.push_event(
                        isolate_id,
                        Some(handler_started.into()),
                        RuntimeEventKind::HandlerPanicked,
                    );
                    self.stop_entry(index, isolate_id, handler_panicked.into());
                    continue;
                }
            };

            let effect_kind = effect.kind();
            let handler_finished = self.push_event(
                isolate_id,
                Some(handler_started.into()),
                RuntimeEventKind::HandlerFinished {
                    effect: effect_kind,
                },
            );

            match effect {
                ErasedEffect::Stop => {
                    self.stop_entry(index, isolate_id, handler_finished.into());
                }
                ErasedEffect::Send(send) => {
                    let target_shard = send.target_shard;
                    let target_isolate = send.target_isolate;
                    let attempted = self.push_event(
                        isolate_id,
                        Some(handler_finished.into()),
                        RuntimeEventKind::SendDispatchAttempted {
                            target_shard,
                            target_isolate,
                        },
                    );

                    match self.dispatch_local_send(send) {
                        Ok(()) => {
                            self.push_event(
                                isolate_id,
                                Some(attempted.into()),
                                RuntimeEventKind::SendAccepted {
                                    target_shard,
                                    target_isolate,
                                },
                            );
                        }
                        Err((target_shard, target_isolate, reason)) => {
                            self.push_event(
                                isolate_id,
                                Some(attempted.into()),
                                RuntimeEventKind::SendRejected {
                                    target_shard,
                                    target_isolate,
                                    reason,
                                },
                            );
                        }
                    }
                }
                ErasedEffect::Spawn(spawn) => {
                    let child_isolate = spawn.spawn(self);
                    self.push_event(
                        isolate_id,
                        Some(handler_finished.into()),
                        RuntimeEventKind::Spawned { child_isolate },
                    );
                }
                ErasedEffect::Noop | ErasedEffect::Reply | ErasedEffect::RestartChildren => {
                    self.push_event(
                        isolate_id,
                        Some(handler_finished.into()),
                        RuntimeEventKind::EffectObserved {
                            effect: effect_kind,
                        },
                    );
                }
            }
        }

        delivered
    }

    fn stop_entry(&mut self, index: usize, isolate_id: IsolateId, cause: CauseId) -> EventId {
        self.entries[index].stopped.set(true);
        self.entries[index].mailbox.close();
        let stopped = self.push_event(isolate_id, Some(cause), RuntimeEventKind::IsolateStopped);
        while self.entries[index].mailbox.recv_boxed().is_some() {
            self.push_event(
                isolate_id,
                Some(stopped.into()),
                RuntimeEventKind::MessageAbandoned,
            );
        }
        stopped
    }

    fn push_event(
        &mut self,
        isolate: IsolateId,
        cause: Option<CauseId>,
        kind: RuntimeEventKind,
    ) -> EventId {
        push_event(
            &mut self.next_event_id,
            &mut self.trace,
            self.shard.id(),
            isolate,
            cause,
            kind,
        )
    }

    fn dispatch_local_send(
        &self,
        send: ErasedSend,
    ) -> Result<(), (ShardId, IsolateId, SendRejectedReason)> {
        if send.target_shard != self.shard.id() {
            panic!(
                "cross-shard send is out of scope in this slice: target shard {} != runtime shard {}",
                send.target_shard.get(),
                self.shard.id().get(),
            );
        }

        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.id == send.target_isolate)
        else {
            panic!(
                "send targeted unknown isolate {} on shard {}",
                send.target_isolate.get(),
                send.target_shard.get(),
            );
        };

        entry
            .mailbox
            .try_send_boxed(send.message)
            .map_err(|reason| match reason {
                TrySendError::Full(_) => (
                    send.target_shard,
                    send.target_isolate,
                    SendRejectedReason::Full,
                ),
                TrySendError::Closed(_) => (
                    send.target_shard,
                    send.target_isolate,
                    SendRejectedReason::Closed,
                ),
            })
    }

    fn register_entry<I, Outbound>(
        &mut self,
        isolate: I,
        mailbox: Box<dyn ErasedMailbox>,
    ) -> IsolateId
    where
        I: Isolate<Shard = S, Send = SendMessage<Outbound>> + 'static,
        I::Message: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        Outbound: 'static,
    {
        let isolate_id = IsolateId::new(self.next_isolate_id);
        self.next_isolate_id += 1;

        self.entries.push(RegisteredEntry {
            id: isolate_id,
            stopped: Cell::new(false),
            mailbox,
            handler: RefCell::new(Box::new(HandlerAdapter::<I, Outbound> {
                isolate,
                marker: PhantomData,
            })),
        });

        isolate_id
    }

    fn spawn_isolate<I, Outbound>(&mut self, isolate: I, mailbox_capacity: usize) -> IsolateId
    where
        I: Isolate<Shard = S, Send = SendMessage<Outbound>> + 'static,
        I::Message: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        Outbound: 'static,
    {
        if mailbox_capacity == 0 {
            panic!("spawn requested mailbox capacity 0, which is out of scope for this slice");
        }

        self.register_entry::<I, Outbound>(
            isolate,
            Box::new(DynMailboxAdapter::<I::Message> {
                mailbox: self.mailbox_factory.create::<I::Message>(mailbox_capacity),
                marker: PhantomData,
            }),
        )
    }
}

fn push_event(
    next_event_id: &mut u64,
    trace: &mut Vec<RuntimeEvent>,
    shard: ShardId,
    isolate: IsolateId,
    cause: Option<CauseId>,
    kind: RuntimeEventKind,
) -> EventId {
    let id = EventId::new(*next_event_id);
    *next_event_id += 1;
    trace.push(RuntimeEvent::new(id, cause, shard, isolate, kind));
    id
}

fn effect_kind<I>(effect: &Effect<I>) -> EffectKind
where
    I: Isolate,
{
    match effect {
        Effect::Noop => EffectKind::Noop,
        Effect::Reply(_) => EffectKind::Reply,
        Effect::Send(_) => EffectKind::Send,
        Effect::Spawn(_) => EffectKind::Spawn,
        Effect::Stop => EffectKind::Stop,
        Effect::RestartChildren => EffectKind::RestartChildren,
    }
}

trait ErasedMailbox {
    fn recv_boxed(&self) -> Option<Box<dyn Any>>;
    fn try_send_boxed(&self, message: Box<dyn Any>) -> Result<(), TrySendError<Box<dyn Any>>>;
    fn close(&self);
}

struct MailboxAdapter<M, Msg>
where
    M: Mailbox<Msg>,
{
    mailbox: M,
    marker: PhantomData<fn(Msg) -> Msg>,
}

impl<M, Msg> ErasedMailbox for MailboxAdapter<M, Msg>
where
    M: Mailbox<Msg>,
    Msg: 'static,
{
    fn recv_boxed(&self) -> Option<Box<dyn Any>> {
        self.mailbox
            .recv()
            .map(|message| Box::new(message) as Box<dyn Any>)
    }

    fn try_send_boxed(&self, message: Box<dyn Any>) -> Result<(), TrySendError<Box<dyn Any>>> {
        let message = message.downcast::<Msg>().unwrap_or_else(|_| {
            panic!("runtime attempted to deliver a message to a mailbox with the wrong type")
        });

        match self.mailbox.try_send(*message) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(message)) => {
                Err(TrySendError::Full(Box::new(message) as Box<dyn Any>))
            }
            Err(TrySendError::Closed(message)) => {
                Err(TrySendError::Closed(Box::new(message) as Box<dyn Any>))
            }
        }
    }

    fn close(&self) {
        self.mailbox.close();
    }
}

struct DynMailboxAdapter<Msg> {
    mailbox: Box<dyn Mailbox<Msg>>,
    marker: PhantomData<fn(Msg) -> Msg>,
}

impl<Msg> ErasedMailbox for DynMailboxAdapter<Msg>
where
    Msg: 'static,
{
    fn recv_boxed(&self) -> Option<Box<dyn Any>> {
        self.mailbox
            .recv()
            .map(|message| Box::new(message) as Box<dyn Any>)
    }

    fn try_send_boxed(&self, message: Box<dyn Any>) -> Result<(), TrySendError<Box<dyn Any>>> {
        let message = message.downcast::<Msg>().unwrap_or_else(|_| {
            panic!("runtime attempted to deliver a message to a mailbox with the wrong type")
        });

        match self.mailbox.try_send(*message) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(message)) => {
                Err(TrySendError::Full(Box::new(message) as Box<dyn Any>))
            }
            Err(TrySendError::Closed(message)) => {
                Err(TrySendError::Closed(Box::new(message) as Box<dyn Any>))
            }
        }
    }

    fn close(&self) {
        self.mailbox.close();
    }
}

trait ErasedHandler<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    fn handle_boxed(
        &mut self,
        message: Box<dyn Any>,
        shard: &mut S,
        isolate_id: IsolateId,
    ) -> ErasedEffect<S, F>;
}

trait ErasedSpawn<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    fn spawn(self: Box<Self>, runtime: &mut CurrentRuntime<S, F>) -> IsolateId;
}

trait IntoErasedSpawn<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    fn into_erased_spawn(self) -> Box<dyn ErasedSpawn<S, F>>;
}

struct HandlerAdapter<I, Outbound>
where
    I: Isolate,
{
    isolate: I,
    marker: PhantomData<fn(Outbound) -> Outbound>,
}

impl<I, S, F, Outbound> ErasedHandler<S, F> for HandlerAdapter<I, Outbound>
where
    I: Isolate<Shard = S, Send = SendMessage<Outbound>>,
    I::Message: 'static,
    I::Spawn: IntoErasedSpawn<S, F> + 'static,
    Outbound: 'static,
    S: Shard,
    F: MailboxFactory,
{
    fn handle_boxed(
        &mut self,
        message: Box<dyn Any>,
        shard: &mut S,
        isolate_id: IsolateId,
    ) -> ErasedEffect<S, F> {
        let message = message.downcast::<I::Message>().unwrap_or_else(|_| {
            panic!("runtime attempted to deliver a handler message with the wrong type")
        });

        let effect = {
            let mut ctx = Context::new(shard, isolate_id);
            self.isolate.handle(*message, &mut ctx)
        };

        match effect {
            Effect::Noop => ErasedEffect::Noop,
            Effect::Reply(_) => ErasedEffect::Reply,
            Effect::Send(send) => {
                let (destination, message) = send.into_parts();
                ErasedEffect::Send(ErasedSend {
                    target_shard: destination.shard(),
                    target_isolate: destination.isolate(),
                    message: Box::new(message),
                })
            }
            Effect::Spawn(spawn) => ErasedEffect::Spawn(spawn.into_erased_spawn()),
            Effect::Stop => ErasedEffect::Stop,
            Effect::RestartChildren => ErasedEffect::RestartChildren,
        }
    }
}

struct RegisteredEntry<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    id: IsolateId,
    stopped: Cell<bool>,
    mailbox: Box<dyn ErasedMailbox>,
    handler: RefCell<Box<dyn ErasedHandler<S, F>>>,
}

enum ErasedEffect<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    Noop,
    Reply,
    Send(ErasedSend),
    Spawn(Box<dyn ErasedSpawn<S, F>>),
    Stop,
    RestartChildren,
}

impl<S, F> ErasedEffect<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    fn kind(&self) -> EffectKind {
        match self {
            Self::Noop => EffectKind::Noop,
            Self::Reply => EffectKind::Reply,
            Self::Send(_) => EffectKind::Send,
            Self::Spawn(_) => EffectKind::Spawn,
            Self::Stop => EffectKind::Stop,
            Self::RestartChildren => EffectKind::RestartChildren,
        }
    }
}

struct ErasedSend {
    target_shard: ShardId,
    target_isolate: IsolateId,
    message: Box<dyn Any>,
}

impl<S, F> IntoErasedSpawn<S, F> for std::convert::Infallible
where
    S: Shard,
    F: MailboxFactory,
{
    fn into_erased_spawn(self) -> Box<dyn ErasedSpawn<S, F>> {
        match self {}
    }
}

struct SpawnAdapter<I, Outbound>
where
    I: Isolate,
{
    isolate: I,
    mailbox_capacity: usize,
    marker: PhantomData<fn(Outbound) -> Outbound>,
}

impl<I, S, F, Outbound> ErasedSpawn<S, F> for SpawnAdapter<I, Outbound>
where
    I: Isolate<Shard = S, Send = SendMessage<Outbound>> + 'static,
    I::Message: 'static,
    I::Spawn: IntoErasedSpawn<S, F> + 'static,
    Outbound: 'static,
    S: Shard,
    F: MailboxFactory,
{
    fn spawn(self: Box<Self>, runtime: &mut CurrentRuntime<S, F>) -> IsolateId {
        runtime.spawn_isolate::<I, Outbound>(self.isolate, self.mailbox_capacity)
    }
}

impl<I, S, F, Outbound> IntoErasedSpawn<S, F> for tina::SpawnSpec<I>
where
    I: Isolate<Shard = S, Send = SendMessage<Outbound>> + 'static,
    I::Message: 'static,
    I::Spawn: IntoErasedSpawn<S, F> + 'static,
    Outbound: 'static,
    S: Shard,
    F: MailboxFactory,
{
    fn into_erased_spawn(self) -> Box<dyn ErasedSpawn<S, F>> {
        let (isolate, mailbox_capacity) = self.into_parts();
        Box::new(SpawnAdapter::<I, Outbound> {
            isolate,
            mailbox_capacity,
            marker: PhantomData,
        })
    }
}
