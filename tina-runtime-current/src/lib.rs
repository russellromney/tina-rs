#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

//! Small single-isolate runtime core for `tina-rs`.
//!
//! This crate starts Mariner with the narrowest useful surface:
//!
//! - deterministic runtime event IDs
//! - causal links between runtime events
//! - a one-step single-isolate runner
//!
//! The first runner executes one handler at a time from an injected mailbox and
//! records a trace of what happened. In this slice it only applies one effect
//! state transition itself: `Effect::Stop` marks the isolate as stopped. Other
//! effects are observed and traced, but not executed.

use tina::{Context, Effect, Isolate, IsolateId, Mailbox, Shard, ShardId};

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

/// Kind of one runtime event emitted by the runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuntimeEventKind {
    /// The runner accepted one message from the mailbox for delivery.
    MailboxAccepted,

    /// The runner began one handler invocation.
    HandlerStarted,

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

    /// The runner applied the stopped state after observing [`Effect::Stop`].
    IsolateStopped,
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

        let mailbox_accepted = self.push_event(None, RuntimeEventKind::MailboxAccepted);
        let handler_started = self.push_event(
            Some(mailbox_accepted.into()),
            RuntimeEventKind::HandlerStarted,
        );

        let effect = {
            let mut ctx = Context::new(&mut self.shard, self.isolate_id);
            self.isolate.handle(message, &mut ctx)
        };

        let effect_kind = effect_kind(&effect);
        let handler_finished = self.push_event(
            Some(handler_started.into()),
            RuntimeEventKind::HandlerFinished {
                effect: effect_kind,
            },
        );

        match effect {
            Effect::Stop => {
                self.stopped = true;
                self.push_event(
                    Some(handler_finished.into()),
                    RuntimeEventKind::IsolateStopped,
                );
            }
            Effect::Noop
            | Effect::Reply(_)
            | Effect::Send(_)
            | Effect::Spawn(_)
            | Effect::RestartChildren => {
                self.push_event(
                    Some(handler_finished.into()),
                    RuntimeEventKind::EffectObserved {
                        effect: effect_kind,
                    },
                );
            }
        }

        StepOutcome::Delivered
    }

    fn push_event(&mut self, cause: Option<CauseId>, kind: RuntimeEventKind) -> EventId {
        let id = EventId::new(self.next_event_id);
        self.next_event_id += 1;
        self.trace.push(RuntimeEvent::new(
            id,
            cause,
            self.shard.id(),
            self.isolate_id,
            kind,
        ));
        id
    }
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
