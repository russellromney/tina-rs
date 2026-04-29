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
//! isolates, step them in deterministic order, execute local same-shard
//! [`Effect::Send`] requests that use [`tina::SendMessage`], spawn local
//! children, and restart direct restartable children. Reply effects are still
//! traced without execution until a later slice gives them runtime semantics.
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
use std::rc::Rc;

use tina::{
    Address, AddressGeneration, Context, Effect, Isolate, IsolateId, Mailbox, SendMessage, Shard,
    ShardId, TrySendError,
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

/// Why a child was not restarted after a restart attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RestartSkippedReason {
    /// The child was spawned from a one-shot [`tina::SpawnSpec`] and has no
    /// restart recipe.
    NotRestartable,
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

        /// The destination generation for that isolate.
        target_generation: AddressGeneration,
    },

    /// The runtime accepted a local send into the target mailbox.
    SendAccepted {
        /// The destination shard.
        target_shard: ShardId,

        /// The destination isolate on that shard.
        target_isolate: IsolateId,

        /// The destination generation for that isolate.
        target_generation: AddressGeneration,
    },

    /// The runtime rejected a local send.
    SendRejected {
        /// The destination shard.
        target_shard: ShardId,

        /// The destination isolate on that shard.
        target_isolate: IsolateId,

        /// The destination generation for that isolate.
        target_generation: AddressGeneration,

        /// Why the target mailbox rejected the send.
        reason: SendRejectedReason,
    },

    /// The runtime created one local child isolate from a spawn effect.
    Spawned {
        /// The isolate identifier assigned to the new child.
        child_isolate: IsolateId,
    },

    /// The runtime began processing one child record for a restart request.
    RestartChildAttempted {
        /// Stable per-parent child ordinal.
        child_ordinal: usize,

        /// The child incarnation being replaced or skipped.
        old_isolate: IsolateId,

        /// The generation of the child incarnation being replaced or skipped.
        old_generation: AddressGeneration,
    },

    /// The runtime skipped one child during restart execution.
    RestartChildSkipped {
        /// Stable per-parent child ordinal.
        child_ordinal: usize,

        /// The child incarnation that was skipped.
        old_isolate: IsolateId,

        /// The generation of the child incarnation that was skipped.
        old_generation: AddressGeneration,

        /// Why the child was skipped.
        reason: RestartSkippedReason,
    },

    /// The runtime created a replacement child for one restartable child
    /// record.
    RestartChildCompleted {
        /// Stable per-parent child ordinal.
        child_ordinal: usize,

        /// The child incarnation that was replaced.
        old_isolate: IsolateId,

        /// The generation of the child incarnation that was replaced.
        old_generation: AddressGeneration,

        /// The fresh replacement isolate.
        new_isolate: IsolateId,

        /// The generation of the fresh replacement isolate.
        new_generation: AddressGeneration,
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
    child_records: Vec<ChildRecord<S, F>>,
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
            child_records: Vec::new(),
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
        let address = self.register_entry::<I, Outbound>(
            isolate,
            None,
            Box::new(MailboxAdapter::<M, I::Message> {
                mailbox,
                marker: PhantomData,
            }),
        );

        Address::new_with_generation(address.shard, address.isolate, address.generation)
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

        if entry.generation != address.generation() {
            return Err(TrySendError::Closed(message));
        }

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
        let mut round_messages: Vec<Option<Box<dyn Any>>> = self
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

        for index in 0..round_messages.len() {
            let Some(message) = round_messages[index].take() else {
                continue;
            };

            if self.entries[index].stopped.get() {
                if let Some(stopped) = self.entries[index].stopped_event.get() {
                    self.push_event(
                        self.entries[index].id,
                        Some(stopped.into()),
                        RuntimeEventKind::MessageAbandoned,
                    );
                }
                continue;
            }

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
                    let target_generation = send.target_generation;
                    let attempted = self.push_event(
                        isolate_id,
                        Some(handler_finished.into()),
                        RuntimeEventKind::SendDispatchAttempted {
                            target_shard,
                            target_isolate,
                            target_generation,
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
                                    target_generation,
                                },
                            );
                        }
                        Err((target_shard, target_isolate, target_generation, reason)) => {
                            self.push_event(
                                isolate_id,
                                Some(attempted.into()),
                                RuntimeEventKind::SendRejected {
                                    target_shard,
                                    target_isolate,
                                    target_generation,
                                    reason,
                                },
                            );
                        }
                    }
                }
                ErasedEffect::Spawn(spawn) => {
                    let outcome = spawn.spawn(self, isolate_id);
                    let child_isolate = outcome.child.isolate;
                    self.record_child(isolate_id, outcome);
                    self.push_event(
                        isolate_id,
                        Some(handler_finished.into()),
                        RuntimeEventKind::Spawned { child_isolate },
                    );
                }
                ErasedEffect::RestartChildren => {
                    self.restart_children(isolate_id, handler_finished.into(), &mut round_messages);
                }
                ErasedEffect::Noop | ErasedEffect::Reply => {
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
        self.stop_entry_with_precollected(index, isolate_id, cause, None)
    }

    fn stop_entry_with_precollected(
        &mut self,
        index: usize,
        isolate_id: IsolateId,
        cause: CauseId,
        precollected: Option<Box<dyn Any>>,
    ) -> EventId {
        if self.entries[index].stopped.get() {
            let stopped = self.entries[index]
                .stopped_event
                .get()
                .unwrap_or_else(|| panic!("stopped isolate has no stopped event"));
            if precollected.is_some() {
                self.push_event(
                    isolate_id,
                    Some(stopped.into()),
                    RuntimeEventKind::MessageAbandoned,
                );
            }
            return stopped;
        }

        self.entries[index].stopped.set(true);
        self.entries[index].mailbox.close();
        let stopped = self.push_event(isolate_id, Some(cause), RuntimeEventKind::IsolateStopped);
        self.entries[index].stopped_event.set(Some(stopped));
        if precollected.is_some() {
            self.push_event(
                isolate_id,
                Some(stopped.into()),
                RuntimeEventKind::MessageAbandoned,
            );
        }
        while self.entries[index].mailbox.recv_boxed().is_some() {
            self.push_event(
                isolate_id,
                Some(stopped.into()),
                RuntimeEventKind::MessageAbandoned,
            );
        }
        stopped
    }

    fn restart_children(
        &mut self,
        parent: IsolateId,
        cause: CauseId,
        round_messages: &mut [Option<Box<dyn Any>>],
    ) {
        let child_record_indices: Vec<usize> = self
            .child_records
            .iter()
            .enumerate()
            .filter_map(|(index, record)| (record.parent == parent).then_some(index))
            .collect();

        for child_record_index in child_record_indices {
            self.restart_child_record(parent, child_record_index, cause, round_messages);
        }
    }

    fn restart_child_record(
        &mut self,
        parent: IsolateId,
        child_record_index: usize,
        cause: CauseId,
        round_messages: &mut [Option<Box<dyn Any>>],
    ) {
        let child_ordinal = self.child_records[child_record_index].child_ordinal;
        let old_child = self.child_records[child_record_index].child;
        let attempted = self.push_event(
            parent,
            Some(cause),
            RuntimeEventKind::RestartChildAttempted {
                child_ordinal,
                old_isolate: old_child.isolate,
                old_generation: old_child.generation,
            },
        );

        // Preserve the recipe across restarts while calling back into the
        // runtime mutably to construct the replacement child.
        let Some(recipe) = self.child_records[child_record_index]
            .restart_recipe
            .clone()
        else {
            self.push_event(
                parent,
                Some(attempted.into()),
                RuntimeEventKind::RestartChildSkipped {
                    child_ordinal,
                    old_isolate: old_child.isolate,
                    old_generation: old_child.generation,
                    reason: RestartSkippedReason::NotRestartable,
                },
            );
            return;
        };

        if let Some(old_entry_index) = self.entry_index(old_child) {
            if !self.entries[old_entry_index].stopped.get() {
                let precollected = round_messages
                    .get_mut(old_entry_index)
                    .and_then(Option::take);
                self.stop_entry_with_precollected(
                    old_entry_index,
                    old_child.isolate,
                    attempted.into(),
                    precollected,
                );
            }
        }

        let outcome = recipe.create(self, parent);
        let new_child = outcome.child;
        self.child_records[child_record_index].child = new_child;
        self.child_records[child_record_index].mailbox_capacity = outcome.mailbox_capacity;
        // Rebind the same restart recipe so this child slot remains
        // restartable after the first replacement.
        self.child_records[child_record_index].restart_recipe = Some(recipe);

        self.push_event(
            parent,
            Some(attempted.into()),
            RuntimeEventKind::RestartChildCompleted {
                child_ordinal,
                old_isolate: old_child.isolate,
                old_generation: old_child.generation,
                new_isolate: new_child.isolate,
                new_generation: new_child.generation,
            },
        );
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
    ) -> Result<(), (ShardId, IsolateId, AddressGeneration, SendRejectedReason)> {
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

        if entry.generation != send.target_generation {
            return Err((
                send.target_shard,
                send.target_isolate,
                send.target_generation,
                SendRejectedReason::Closed,
            ));
        }

        entry
            .mailbox
            .try_send_boxed(send.message)
            .map_err(|reason| match reason {
                TrySendError::Full(_) => (
                    send.target_shard,
                    send.target_isolate,
                    send.target_generation,
                    SendRejectedReason::Full,
                ),
                TrySendError::Closed(_) => (
                    send.target_shard,
                    send.target_isolate,
                    send.target_generation,
                    SendRejectedReason::Closed,
                ),
            })
    }

    fn entry_index(&self, address: RegisteredAddress) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| entry.id == address.isolate && entry.generation == address.generation)
    }

    fn register_entry<I, Outbound>(
        &mut self,
        isolate: I,
        parent: Option<IsolateId>,
        mailbox: Box<dyn ErasedMailbox>,
    ) -> RegisteredAddress
    where
        I: Isolate<Shard = S, Send = SendMessage<Outbound>> + 'static,
        I::Message: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        Outbound: 'static,
    {
        let isolate_id = IsolateId::new(self.next_isolate_id);
        self.next_isolate_id += 1;
        let generation = AddressGeneration::new(0);

        self.entries.push(RegisteredEntry {
            id: isolate_id,
            generation,
            parent,
            stopped: Cell::new(false),
            stopped_event: Cell::new(None),
            mailbox,
            handler: RefCell::new(Box::new(HandlerAdapter::<I, Outbound> {
                isolate,
                marker: PhantomData,
            })),
        });

        RegisteredAddress {
            shard: self.shard.id(),
            isolate: isolate_id,
            generation,
        }
    }

    fn spawn_isolate<I, Outbound>(
        &mut self,
        parent: IsolateId,
        isolate: I,
        mailbox_capacity: usize,
    ) -> SpawnOutcome<S, F>
    where
        I: Isolate<Shard = S, Send = SendMessage<Outbound>> + 'static,
        I::Message: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        Outbound: 'static,
    {
        if mailbox_capacity == 0 {
            panic!("spawn requested mailbox capacity 0, which is out of scope for this slice");
        }

        let child = self.register_entry::<I, Outbound>(
            isolate,
            Some(parent),
            Box::new(DynMailboxAdapter::<I::Message> {
                mailbox: self.mailbox_factory.create::<I::Message>(mailbox_capacity),
                marker: PhantomData,
            }),
        );

        SpawnOutcome {
            child,
            mailbox_capacity,
            restart_recipe: None,
        }
    }

    fn record_child(&mut self, parent: IsolateId, outcome: SpawnOutcome<S, F>) {
        let child_ordinal = self
            .child_records
            .iter()
            .filter(|record| record.parent == parent)
            .count();

        self.child_records.push(ChildRecord {
            parent,
            child: outcome.child,
            child_ordinal,
            mailbox_capacity: outcome.mailbox_capacity,
            restart_recipe: outcome.restart_recipe,
        });
    }

    /// Returns the stored direct-parent lineage in registration order.
    #[cfg(test)]
    pub(crate) fn lineage_snapshot(&self) -> Vec<(IsolateId, Option<IsolateId>)> {
        self.entries
            .iter()
            .map(|entry| (entry.id, entry.parent))
            .collect()
    }

    /// Returns the stored child records in spawn order.
    #[cfg(test)]
    pub(crate) fn child_record_snapshot(&self) -> Vec<ChildRecordSnapshot> {
        self.child_records
            .iter()
            .map(|record| ChildRecordSnapshot {
                parent: record.parent,
                child_shard: record.child.shard,
                child_isolate: record.child.isolate,
                child_generation: record.child.generation,
                child_ordinal: record.child_ordinal,
                mailbox_capacity: record.mailbox_capacity,
                restartable: record.restart_recipe.is_some(),
            })
            .collect()
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RegisteredAddress {
    shard: ShardId,
    isolate: IsolateId,
    generation: AddressGeneration,
}

struct SpawnOutcome<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    child: RegisteredAddress,
    mailbox_capacity: usize,
    restart_recipe: Option<Rc<dyn ErasedRestartRecipe<S, F>>>,
}

#[cfg_attr(not(test), allow(dead_code))]
struct ChildRecord<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    parent: IsolateId,
    child: RegisteredAddress,
    child_ordinal: usize,
    mailbox_capacity: usize,
    restart_recipe: Option<Rc<dyn ErasedRestartRecipe<S, F>>>,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChildRecordSnapshot {
    parent: IsolateId,
    child_shard: ShardId,
    child_isolate: IsolateId,
    child_generation: AddressGeneration,
    child_ordinal: usize,
    mailbox_capacity: usize,
    restartable: bool,
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
    fn spawn(
        self: Box<Self>,
        runtime: &mut CurrentRuntime<S, F>,
        parent: IsolateId,
    ) -> SpawnOutcome<S, F>;
}

trait ErasedRestartRecipe<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    fn create(&self, runtime: &mut CurrentRuntime<S, F>, parent: IsolateId) -> SpawnOutcome<S, F>;
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
                    target_generation: destination.generation(),
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
    generation: AddressGeneration,
    #[cfg_attr(not(test), allow(dead_code))]
    parent: Option<IsolateId>,
    stopped: Cell<bool>,
    stopped_event: Cell<Option<EventId>>,
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
    target_generation: AddressGeneration,
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
    fn spawn(
        self: Box<Self>,
        runtime: &mut CurrentRuntime<S, F>,
        parent: IsolateId,
    ) -> SpawnOutcome<S, F> {
        runtime.spawn_isolate::<I, Outbound>(parent, self.isolate, self.mailbox_capacity)
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

struct RestartableSpawnAdapter<I, Outbound>
where
    I: Isolate,
{
    factory: Box<dyn Fn() -> I>,
    mailbox_capacity: usize,
    marker: PhantomData<fn(Outbound) -> Outbound>,
}

impl<I, S, F, Outbound> ErasedSpawn<S, F> for RestartableSpawnAdapter<I, Outbound>
where
    I: Isolate<Shard = S, Send = SendMessage<Outbound>> + 'static,
    I::Message: 'static,
    I::Spawn: IntoErasedSpawn<S, F> + 'static,
    Outbound: 'static,
    S: Shard,
    F: MailboxFactory,
{
    fn spawn(
        self: Box<Self>,
        runtime: &mut CurrentRuntime<S, F>,
        parent: IsolateId,
    ) -> SpawnOutcome<S, F> {
        let adapter = Rc::new(*self);
        let isolate = (adapter.factory)();
        let mailbox_capacity = adapter.mailbox_capacity;
        let mut outcome = runtime.spawn_isolate::<I, Outbound>(parent, isolate, mailbox_capacity);
        outcome.restart_recipe = Some(adapter);
        outcome
    }
}

impl<I, S, F, Outbound> ErasedRestartRecipe<S, F> for RestartableSpawnAdapter<I, Outbound>
where
    I: Isolate<Shard = S, Send = SendMessage<Outbound>> + 'static,
    I::Message: 'static,
    I::Spawn: IntoErasedSpawn<S, F> + 'static,
    Outbound: 'static,
    S: Shard,
    F: MailboxFactory,
{
    fn create(&self, runtime: &mut CurrentRuntime<S, F>, parent: IsolateId) -> SpawnOutcome<S, F> {
        let isolate = (self.factory)();
        runtime.spawn_isolate::<I, Outbound>(parent, isolate, self.mailbox_capacity)
    }
}

impl<I, S, F, Outbound> IntoErasedSpawn<S, F> for tina::RestartableSpawnSpec<I>
where
    I: Isolate<Shard = S, Send = SendMessage<Outbound>> + 'static,
    I::Message: 'static,
    I::Spawn: IntoErasedSpawn<S, F> + 'static,
    Outbound: 'static,
    S: Shard,
    F: MailboxFactory,
{
    fn into_erased_spawn(self) -> Box<dyn ErasedSpawn<S, F>> {
        let (factory, mailbox_capacity) = self.into_parts();
        Box::new(RestartableSpawnAdapter::<I, Outbound> {
            factory,
            mailbox_capacity,
            marker: PhantomData,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::rc::Rc;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum NeverOutbound {}

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum LineageMsg {
        SpawnChild,
        SpawnGrandchild,
        Stop,
        Panic,
        Restart,
    }

    type RunEvidence = (
        Vec<RuntimeEvent>,
        Vec<(IsolateId, Option<IsolateId>)>,
        Vec<ChildRecordSnapshot>,
    );

    #[derive(Debug, Default)]
    struct TestShard;

    impl Shard for TestShard {
        fn id(&self) -> ShardId {
            ShardId::new(3)
        }
    }

    struct TestMailbox<T> {
        capacity: usize,
        queue: Rc<RefCell<VecDeque<T>>>,
        closed: Rc<Cell<bool>>,
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

    #[derive(Debug)]
    struct RootIsolate {
        child_capacity: usize,
    }

    #[derive(Debug)]
    struct RestartableRootIsolate {
        child_capacity: usize,
        factory_calls: Rc<Cell<usize>>,
    }

    #[derive(Debug)]
    struct ChildIsolate {
        leaf_capacity: usize,
    }

    #[derive(Debug)]
    struct LeafIsolate;

    impl Isolate for RootIsolate {
        type Message = LineageMsg;
        type Reply = ();
        type Send = SendMessage<NeverOutbound>;
        type Spawn = tina::SpawnSpec<ChildIsolate>;
        type Shard = TestShard;

        fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard>,
        ) -> Effect<Self> {
            match msg {
                LineageMsg::SpawnChild => Effect::Spawn(tina::SpawnSpec::new(
                    ChildIsolate {
                        leaf_capacity: self.child_capacity,
                    },
                    self.child_capacity,
                )),
                LineageMsg::Stop => Effect::Stop,
                LineageMsg::Panic => panic!("panic inside root lineage isolate"),
                LineageMsg::Restart => Effect::RestartChildren,
                LineageMsg::SpawnGrandchild => Effect::Noop,
            }
        }
    }

    impl Isolate for RestartableRootIsolate {
        type Message = LineageMsg;
        type Reply = ();
        type Send = SendMessage<NeverOutbound>;
        type Spawn = tina::RestartableSpawnSpec<ChildIsolate>;
        type Shard = TestShard;

        fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard>,
        ) -> Effect<Self> {
            match msg {
                LineageMsg::SpawnChild => {
                    let child_capacity = self.child_capacity;
                    let factory_calls = Rc::clone(&self.factory_calls);
                    Effect::Spawn(tina::RestartableSpawnSpec::new(
                        move || {
                            factory_calls.set(factory_calls.get() + 1);
                            ChildIsolate {
                                leaf_capacity: child_capacity,
                            }
                        },
                        child_capacity,
                    ))
                }
                LineageMsg::Restart => Effect::RestartChildren,
                LineageMsg::Stop => Effect::Stop,
                LineageMsg::Panic => panic!("panic inside restartable root isolate"),
                LineageMsg::SpawnGrandchild => Effect::Noop,
            }
        }
    }

    impl Isolate for ChildIsolate {
        type Message = LineageMsg;
        type Reply = ();
        type Send = SendMessage<NeverOutbound>;
        type Spawn = tina::SpawnSpec<LeafIsolate>;
        type Shard = TestShard;

        fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard>,
        ) -> Effect<Self> {
            match msg {
                LineageMsg::SpawnGrandchild => {
                    Effect::Spawn(tina::SpawnSpec::new(LeafIsolate, self.leaf_capacity))
                }
                LineageMsg::Stop => Effect::Stop,
                LineageMsg::Panic => panic!("panic inside child lineage isolate"),
                LineageMsg::Restart => Effect::RestartChildren,
                LineageMsg::SpawnChild => Effect::Noop,
            }
        }
    }

    impl Isolate for LeafIsolate {
        type Message = LineageMsg;
        type Reply = ();
        type Send = SendMessage<NeverOutbound>;
        type Spawn = std::convert::Infallible;
        type Shard = TestShard;

        fn handle(
            &mut self,
            _msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard>,
        ) -> Effect<Self> {
            Effect::Noop
        }
    }

    fn new_root() -> RootIsolate {
        RootIsolate { child_capacity: 2 }
    }

    fn new_restartable_root(factory_calls: Rc<Cell<usize>>) -> RestartableRootIsolate {
        RestartableRootIsolate {
            child_capacity: 3,
            factory_calls,
        }
    }

    fn root_mailbox() -> TestMailbox<LineageMsg> {
        TestMailbox::new(8)
    }

    fn assert_root_and_child_lineage(
        runtime: &CurrentRuntime<TestShard, TestMailboxFactory>,
        root: Address<LineageMsg>,
        child: IsolateId,
    ) {
        assert_eq!(
            runtime.lineage_snapshot(),
            vec![(root.isolate(), None), (child, Some(root.isolate()))]
        );
    }

    fn assert_root_child_grandchild_lineage(
        runtime: &CurrentRuntime<TestShard, TestMailboxFactory>,
        root: Address<LineageMsg>,
        child: IsolateId,
        grandchild: IsolateId,
    ) {
        assert_eq!(
            runtime.lineage_snapshot(),
            vec![
                (root.isolate(), None),
                (child, Some(root.isolate())),
                (grandchild, Some(child)),
            ]
        );
    }

    fn lineage_address(isolate: IsolateId) -> Address<LineageMsg> {
        Address::new(ShardId::new(3), isolate)
    }

    fn last_spawned_child(trace: &[RuntimeEvent]) -> IsolateId {
        match trace.last().expect("expected spawn event").kind() {
            RuntimeEventKind::Spawned { child_isolate } => child_isolate,
            other => panic!("expected Spawned event, found {other:?}"),
        }
    }

    fn child_record(
        parent: IsolateId,
        child_isolate: IsolateId,
        child_ordinal: usize,
        mailbox_capacity: usize,
        restartable: bool,
    ) -> ChildRecordSnapshot {
        child_record_with_generation(
            parent,
            child_isolate,
            AddressGeneration::new(0),
            child_ordinal,
            mailbox_capacity,
            restartable,
        )
    }

    fn child_record_with_generation(
        parent: IsolateId,
        child_isolate: IsolateId,
        child_generation: AddressGeneration,
        child_ordinal: usize,
        mailbox_capacity: usize,
        restartable: bool,
    ) -> ChildRecordSnapshot {
        ChildRecordSnapshot {
            parent,
            child_shard: ShardId::new(3),
            child_isolate,
            child_generation,
            child_ordinal,
            mailbox_capacity,
            restartable,
        }
    }

    fn replacement_address(record: &ChildRecordSnapshot) -> Address<LineageMsg> {
        Address::new_with_generation(
            record.child_shard,
            record.child_isolate,
            record.child_generation,
        )
    }

    fn count_events(
        trace: &[RuntimeEvent],
        matches_event: impl Fn(RuntimeEventKind) -> bool,
    ) -> usize {
        trace
            .iter()
            .filter(|event| matches_event(event.kind()))
            .count()
    }

    fn restart_child_events(trace: &[RuntimeEvent]) -> Vec<RuntimeEventKind> {
        trace
            .iter()
            .filter_map(|event| match event.kind() {
                RuntimeEventKind::RestartChildAttempted { .. }
                | RuntimeEventKind::RestartChildSkipped { .. }
                | RuntimeEventKind::RestartChildCompleted { .. } => Some(event.kind()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn root_registered_isolates_have_no_parent() {
        let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);

        let first = runtime.register(new_root(), root_mailbox());
        let second = runtime.register(new_root(), root_mailbox());

        assert_eq!(
            runtime.lineage_snapshot(),
            vec![(first.isolate(), None), (second.isolate(), None)]
        );
        assert_eq!(runtime.child_record_snapshot(), Vec::new());
    }

    #[test]
    fn one_shot_spawn_records_non_restartable_child_metadata() {
        let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
        let root = runtime.register(new_root(), root_mailbox());

        assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
        assert_eq!(runtime.step(), 1);
        let child = last_spawned_child(runtime.trace());

        assert_eq!(
            runtime.child_record_snapshot(),
            vec![child_record(root.isolate(), child, 0, 2, false)]
        );
    }

    #[test]
    fn restartable_spawn_records_restartable_child_metadata_and_calls_factory_once() {
        let factory_calls = Rc::new(Cell::new(0));
        let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
        let root = runtime.register(
            new_restartable_root(Rc::clone(&factory_calls)),
            root_mailbox(),
        );

        assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
        assert_eq!(runtime.step(), 1);
        let child = last_spawned_child(runtime.trace());

        assert_eq!(factory_calls.get(), 1);
        assert_eq!(
            runtime.child_record_snapshot(),
            vec![child_record(root.isolate(), child, 0, 3, true)]
        );
    }

    #[test]
    fn per_parent_child_ordinals_increment_by_direct_spawn_order() {
        let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
        let root = runtime.register(new_root(), root_mailbox());

        assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
        assert_eq!(runtime.step(), 1);
        let first_child = last_spawned_child(runtime.trace());

        assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
        assert_eq!(runtime.step(), 1);
        let second_child = last_spawned_child(runtime.trace());

        assert_eq!(
            runtime.child_record_snapshot(),
            vec![
                child_record(root.isolate(), first_child, 0, 2, false),
                child_record(root.isolate(), second_child, 1, 2, false),
            ]
        );
    }

    #[test]
    fn nested_spawns_record_direct_parent_edges() {
        let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
        let root = runtime.register(new_root(), root_mailbox());

        assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
        assert_eq!(runtime.step(), 1);
        let child = last_spawned_child(runtime.trace());

        assert_root_and_child_lineage(&runtime, root, child);

        assert_eq!(
            runtime.try_send(lineage_address(child), LineageMsg::SpawnGrandchild),
            Ok(())
        );
        assert_eq!(runtime.step(), 1);
        let grandchild = last_spawned_child(runtime.trace());

        assert_root_child_grandchild_lineage(&runtime, root, child, grandchild);
        assert_eq!(
            runtime.child_record_snapshot(),
            vec![
                child_record(root.isolate(), child, 0, 2, false),
                child_record(child, grandchild, 0, 2, false),
            ]
        );
    }

    #[test]
    fn child_lineage_survives_when_parent_stops() {
        let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
        let root = runtime.register(new_root(), root_mailbox());

        assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
        assert_eq!(runtime.step(), 1);
        let child = last_spawned_child(runtime.trace());

        assert_eq!(runtime.try_send(root, LineageMsg::Stop), Ok(()));
        assert_eq!(runtime.step(), 1);

        assert_root_and_child_lineage(&runtime, root, child);
        assert_eq!(
            runtime.child_record_snapshot(),
            vec![child_record(root.isolate(), child, 0, 2, false)]
        );
    }

    #[test]
    fn child_lineage_survives_when_parent_panics() {
        let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
        let root = runtime.register(new_root(), root_mailbox());

        assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
        assert_eq!(runtime.step(), 1);
        let child = last_spawned_child(runtime.trace());

        assert_eq!(runtime.try_send(root, LineageMsg::Panic), Ok(()));
        assert_eq!(runtime.step(), 1);
        assert!(
            runtime
                .trace()
                .iter()
                .any(|event| matches!(event.kind(), RuntimeEventKind::HandlerPanicked))
        );

        assert_root_and_child_lineage(&runtime, root, child);
        assert_eq!(
            runtime.child_record_snapshot(),
            vec![child_record(root.isolate(), child, 0, 2, false)]
        );
    }

    #[test]
    fn child_record_survives_when_child_stops_or_panics() {
        let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
        let root = runtime.register(new_root(), root_mailbox());

        assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
        assert_eq!(runtime.step(), 1);
        let stopping_child = last_spawned_child(runtime.trace());

        assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
        assert_eq!(runtime.step(), 1);
        let panicking_child = last_spawned_child(runtime.trace());

        assert_eq!(
            runtime.try_send(lineage_address(stopping_child), LineageMsg::Stop),
            Ok(())
        );
        assert_eq!(
            runtime.try_send(lineage_address(panicking_child), LineageMsg::Panic),
            Ok(())
        );
        assert_eq!(runtime.step(), 2);

        assert_eq!(
            runtime.child_record_snapshot(),
            vec![
                child_record(root.isolate(), stopping_child, 0, 2, false),
                child_record(root.isolate(), panicking_child, 1, 2, false),
            ]
        );
    }

    #[test]
    fn restart_children_with_no_direct_children_emits_no_restart_subtree() {
        let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
        let root = runtime.register(new_root(), root_mailbox());

        assert_eq!(runtime.try_send(root, LineageMsg::Restart), Ok(()));
        assert_eq!(runtime.step(), 1);
        assert_eq!(runtime.lineage_snapshot(), vec![(root.isolate(), None)]);
        assert_eq!(restart_child_events(runtime.trace()), Vec::new());
        assert!(runtime.trace().iter().any(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::HandlerFinished {
                    effect: EffectKind::RestartChildren,
                }
            )
        }));
    }

    #[test]
    fn restart_children_replaces_restartable_child_and_preserves_ordinal() {
        let factory_calls = Rc::new(Cell::new(0));
        let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
        let root = runtime.register(
            new_restartable_root(Rc::clone(&factory_calls)),
            root_mailbox(),
        );

        assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
        assert_eq!(runtime.step(), 1);
        let old_child = last_spawned_child(runtime.trace());
        assert_eq!(factory_calls.get(), 1);

        assert_eq!(runtime.try_send(root, LineageMsg::Restart), Ok(()));
        assert_eq!(runtime.step(), 1);
        let records = runtime.child_record_snapshot();
        let replacement = records[0].child_isolate;

        assert_ne!(replacement, old_child);
        assert_eq!(factory_calls.get(), 2);
        assert_eq!(
            records,
            vec![child_record(root.isolate(), replacement, 0, 3, true)]
        );
        assert!(matches!(
            runtime.try_send(lineage_address(old_child), LineageMsg::SpawnChild),
            Err(TrySendError::Closed(LineageMsg::SpawnChild))
        ));
        assert_eq!(
            runtime.try_send(replacement_address(&records[0]), LineageMsg::SpawnChild),
            Ok(())
        );
        assert_eq!(runtime.step(), 1);

        let restart_events = restart_child_events(runtime.trace());
        assert_eq!(restart_events.len(), 2);
        assert!(matches!(
            restart_events[0],
            RuntimeEventKind::RestartChildAttempted {
                child_ordinal: 0,
                old_isolate,
                old_generation,
            } if old_isolate == old_child && old_generation == AddressGeneration::new(0)
        ));
        assert!(matches!(
            restart_events[1],
            RuntimeEventKind::RestartChildCompleted {
                child_ordinal: 0,
                old_isolate,
                old_generation,
                new_isolate,
                new_generation,
            } if old_isolate == old_child
                && old_generation == AddressGeneration::new(0)
                && new_isolate == replacement
                && new_generation == AddressGeneration::new(0)
        ));

        let attempt_id = runtime
            .trace()
            .iter()
            .find(|event| {
                matches!(
                    event.kind(),
                    RuntimeEventKind::RestartChildAttempted {
                        old_isolate,
                        ..
                    } if old_isolate == old_child
                )
            })
            .expect("expected restart attempt")
            .id();
        let direct_consequences: Vec<_> = runtime
            .trace()
            .iter()
            .filter(|event| event.cause() == Some(CauseId::new(attempt_id)))
            .map(|event| event.kind())
            .collect();
        assert!(
            direct_consequences
                .iter()
                .any(|kind| matches!(kind, RuntimeEventKind::IsolateStopped))
        );
        assert!(
            direct_consequences
                .iter()
                .any(|kind| matches!(kind, RuntimeEventKind::RestartChildCompleted { .. }))
        );
    }

    #[test]
    fn restart_children_abandons_precollected_old_child_message() {
        let factory_calls = Rc::new(Cell::new(0));
        let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
        let root = runtime.register(
            new_restartable_root(Rc::clone(&factory_calls)),
            root_mailbox(),
        );

        assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
        assert_eq!(runtime.step(), 1);
        let old_child = last_spawned_child(runtime.trace());

        assert_eq!(runtime.try_send(root, LineageMsg::Restart), Ok(()));
        assert_eq!(
            runtime.try_send(lineage_address(old_child), LineageMsg::SpawnGrandchild),
            Ok(())
        );
        assert_eq!(runtime.step(), 1);

        assert_eq!(
            count_events(runtime.trace(), |kind| matches!(
                kind,
                RuntimeEventKind::MessageAbandoned
            )),
            1
        );
        assert_eq!(
            count_events(runtime.trace(), |kind| matches!(
                kind,
                RuntimeEventKind::RestartChildCompleted {
                    old_isolate,
                    ..
                } if old_isolate == old_child
            )),
            1
        );
        assert_eq!(runtime.child_record_snapshot().len(), 1);
    }

    #[test]
    fn restart_children_restarts_already_stopped_or_panicked_children_without_duplicate_stop() {
        let factory_calls = Rc::new(Cell::new(0));
        let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
        let root = runtime.register(
            new_restartable_root(Rc::clone(&factory_calls)),
            root_mailbox(),
        );

        assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
        assert_eq!(runtime.step(), 1);
        let stopped_child = last_spawned_child(runtime.trace());
        assert_eq!(
            runtime.try_send(lineage_address(stopped_child), LineageMsg::Stop),
            Ok(())
        );
        assert_eq!(runtime.step(), 1);

        assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
        assert_eq!(runtime.step(), 1);
        let panicked_child = last_spawned_child(runtime.trace());
        assert_eq!(
            runtime.try_send(lineage_address(panicked_child), LineageMsg::Panic),
            Ok(())
        );
        assert_eq!(runtime.step(), 1);

        assert_eq!(runtime.try_send(root, LineageMsg::Restart), Ok(()));
        assert_eq!(runtime.step(), 1);

        assert_eq!(
            count_events(runtime.trace(), |kind| matches!(
                kind,
                RuntimeEventKind::IsolateStopped
            )),
            2
        );
        assert_eq!(factory_calls.get(), 4);
        let records = runtime.child_record_snapshot();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].child_ordinal, 0);
        assert_eq!(records[1].child_ordinal, 1);
        assert_ne!(records[0].child_isolate, stopped_child);
        assert_ne!(records[1].child_isolate, panicked_child);
    }

    #[test]
    fn restart_children_visits_multiple_children_in_child_ordinal_order() {
        let factory_calls = Rc::new(Cell::new(0));
        let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
        let root = runtime.register(
            new_restartable_root(Rc::clone(&factory_calls)),
            root_mailbox(),
        );

        assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
        assert_eq!(runtime.step(), 1);
        let first_old = last_spawned_child(runtime.trace());
        assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
        assert_eq!(runtime.step(), 1);
        let second_old = last_spawned_child(runtime.trace());

        assert_eq!(runtime.try_send(root, LineageMsg::Restart), Ok(()));
        assert_eq!(runtime.step(), 1);

        let attempted: Vec<_> = runtime
            .trace()
            .iter()
            .filter_map(|event| match event.kind() {
                RuntimeEventKind::RestartChildAttempted {
                    child_ordinal,
                    old_isolate,
                    ..
                } => Some((child_ordinal, old_isolate)),
                _ => None,
            })
            .collect();

        assert_eq!(attempted, vec![(0, first_old), (1, second_old)]);
        assert_eq!(factory_calls.get(), 4);
    }

    #[test]
    fn restart_children_skips_non_restartable_children_with_trace() {
        let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
        let root = runtime.register(new_root(), root_mailbox());

        assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
        assert_eq!(runtime.step(), 1);
        let child = last_spawned_child(runtime.trace());

        assert_eq!(runtime.try_send(root, LineageMsg::Restart), Ok(()));
        assert_eq!(runtime.step(), 1);

        assert_eq!(
            runtime.child_record_snapshot(),
            vec![child_record(root.isolate(), child, 0, 2, false)]
        );
        assert!(restart_child_events(runtime.trace()).iter().any(|kind| {
            matches!(
                kind,
                RuntimeEventKind::RestartChildSkipped {
                    child_ordinal: 0,
                    old_isolate,
                    old_generation,
                    reason: RestartSkippedReason::NotRestartable,
                } if *old_isolate == child && *old_generation == AddressGeneration::new(0)
            )
        }));
    }

    #[test]
    fn restart_children_does_not_restart_grandchildren() {
        let factory_calls = Rc::new(Cell::new(0));
        let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
        let root = runtime.register(
            new_restartable_root(Rc::clone(&factory_calls)),
            root_mailbox(),
        );

        assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
        assert_eq!(runtime.step(), 1);
        let child = last_spawned_child(runtime.trace());
        assert_eq!(
            runtime.try_send(lineage_address(child), LineageMsg::SpawnGrandchild),
            Ok(())
        );
        assert_eq!(runtime.step(), 1);
        let grandchild = last_spawned_child(runtime.trace());

        assert_eq!(runtime.try_send(root, LineageMsg::Restart), Ok(()));
        assert_eq!(runtime.step(), 1);

        let records = runtime.child_record_snapshot();
        assert_eq!(records.len(), 2);
        assert_ne!(records[0].child_isolate, child);
        assert_eq!(records[0].child_ordinal, 0);
        assert_eq!(records[1], child_record(child, grandchild, 0, 3, false));
        assert_eq!(
            runtime.try_send(lineage_address(grandchild), LineageMsg::SpawnChild),
            Ok(())
        );
    }

    #[test]
    fn restart_children_can_restart_child_before_its_first_turn() {
        let factory_calls = Rc::new(Cell::new(0));
        let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
        let root = runtime.register(
            new_restartable_root(Rc::clone(&factory_calls)),
            root_mailbox(),
        );

        assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
        assert_eq!(runtime.step(), 1);
        let old_child = last_spawned_child(runtime.trace());

        assert_eq!(runtime.try_send(root, LineageMsg::Restart), Ok(()));
        assert_eq!(runtime.step(), 1);

        assert_eq!(
            count_events(runtime.trace(), |kind| matches!(
                kind,
                RuntimeEventKind::HandlerStarted
            )),
            2
        );
        assert_ne!(runtime.child_record_snapshot()[0].child_isolate, old_child);
    }

    #[test]
    fn identical_runs_produce_identical_trace_and_lineage() {
        fn run_once() -> RunEvidence {
            let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
            let root = runtime.register(new_root(), root_mailbox());

            assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
            assert_eq!(runtime.step(), 1);
            let child = last_spawned_child(runtime.trace());

            assert_eq!(
                runtime.try_send(lineage_address(child), LineageMsg::SpawnGrandchild),
                Ok(())
            );
            assert_eq!(runtime.step(), 1);
            let grandchild = last_spawned_child(runtime.trace());

            assert_root_child_grandchild_lineage(&runtime, root, child, grandchild);

            (
                runtime.trace().to_vec(),
                runtime.lineage_snapshot(),
                runtime.child_record_snapshot(),
            )
        }

        assert_eq!(run_once(), run_once());
    }
}
