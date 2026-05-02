#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]
// Phase Mariner 012 substrate is Betelgeuse, which exposes its
// `IOLoopHandle<A: Allocator>` over the unstable `allocator_api`. We
// commit to nightly Rust for `tina-runtime` per the reopened
// 012 plan; the feature gate is scoped to this crate.
#![feature(allocator_api)]

//! Small deterministic single-shard runtime core for `tina-rs`.
//!
//! This crate starts Mariner with the narrowest useful runtime surface:
//!
//! - deterministic runtime event IDs
//! - causal links between runtime events
//! - a tiny single-shard runtime that can host more than one isolate
//!
//! The multi-isolate runtime still stays narrow on purpose. It can register
//! isolates, step them in deterministic order, execute local same-shard
//! [`Effect::Send`] requests that use [`tina::Outbound`], spawn local
//! children, and restart direct restartable children. Reply effects are still
//! traced without execution until a later slice gives them runtime semantics.
//!
//! `Effect::Stop` stays immediate, but `Runtime` also drains and
//! traces any already-buffered messages that become abandoned when an isolate
//! stops.
//!
//! `Runtime` also captures unwinding panics from handler calls and turns
//! them into deterministic runtime events. Binaries built with `panic = "abort"`
//! remain out of scope for this crate.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
use std::marker::PhantomData;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tina::{
    Address, AddressGeneration, ChildRelation, Context, Effect, Isolate, IsolateId, Mailbox,
    Outbound as TinaOutbound, RestartBudgetState, Shard, ShardId, TrySendError,
};
use tina_supervisor::SupervisorConfig;

mod call;
mod io_backend;
mod trace;

pub use call::{
    CallError, CallId, CallInput, CallOutput, ErasedCall, IntoErasedCall, ListenerId, RuntimeCall,
    StreamId, TypedCall, sleep, tcp_accept, tcp_bind, tcp_close_listener, tcp_close_stream,
    tcp_read, tcp_write,
};
pub use trace::{
    CallCompletionRejectedReason, CallKind, CauseId, EffectKind, EventId, RestartSkippedReason,
    RuntimeEvent, RuntimeEventKind, SendRejectedReason, SupervisionRejectedReason,
};

use io_backend::IoBackend;

/// Runtime-owned mailbox factory for spawned children.
///
/// The factory lives in `tina-runtime`, not in `tina`, because child
/// mailbox allocation is a runtime concern rather than a trait-crate concern.
pub trait MailboxFactory {
    /// Creates one typed mailbox with the requested capacity.
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>>;
}

/// Runtime-owned clock abstraction.
///
/// Production uses a monotonic wall-clock. Tests may inject a manual clock
/// so timer behavior can be driven deterministically without real sleeps.
trait Clock: std::fmt::Debug {
    fn now(&self) -> Instant;
}

#[derive(Debug)]
struct MonotonicClock;

impl Clock for MonotonicClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

#[cfg(test)]
#[derive(Debug)]
struct ManualClock {
    base: Instant,
    offset: Cell<Duration>,
}

#[cfg(test)]
impl ManualClock {
    fn new() -> Self {
        Self {
            base: Instant::now(),
            offset: Cell::new(Duration::ZERO),
        }
    }

    fn advance(&self, by: Duration) {
        self.offset.set(self.offset.get() + by);
    }
}

#[cfg(test)]
impl Clock for ManualClock {
    fn now(&self) -> Instant {
        self.base + self.offset.get()
    }
}

#[cfg(test)]
impl Clock for Rc<ManualClock> {
    fn now(&self) -> Instant {
        self.base + self.offset.get()
    }
}

/// One pending timer tracked by the runtime.
#[derive(Debug)]
struct TimerEntry {
    call_id: CallId,
    deadline: Instant,
    insertion_order: u64,
}

#[derive(Debug, Clone)]
struct IdSource {
    next_event_id: Rc<Cell<u64>>,
    next_call_id: Rc<Cell<u64>>,
}

impl IdSource {
    fn new() -> Self {
        Self {
            next_event_id: Rc::new(Cell::new(1)),
            next_call_id: Rc::new(Cell::new(1)),
        }
    }

    fn next_event_id(&self) -> EventId {
        let raw = self.next_event_id.get();
        self.next_event_id.set(raw + 1);
        EventId::new(raw)
    }

    fn next_call_id(&self) -> CallId {
        let raw = self.next_call_id.get();
        self.next_call_id.set(raw + 1);
        CallId::new(raw)
    }
}

/// Small deterministic single-shard runtime for the second Mariner slice.
///
/// The runtime owns one shard value plus a private registry of isolates and
/// mailboxes. [`step`](Self::step) walks registered isolates in registration
/// order and gives each isolate at most one delivery chance per round.
pub struct Runtime<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    shard: S,
    mailbox_factory: F,
    entries: Vec<RegisteredEntry<S, F>>,
    child_records: Vec<ChildRecord<S, F>>,
    supervisors: Vec<SupervisorRecord>,
    next_isolate_id: u64,
    ids: IdSource,
    trace: Vec<RuntimeEvent>,
    io_backend: IoBackend,
    in_flight_calls: Vec<InFlightCall>,
    translators: Vec<StoredTranslator>,
    clock: Box<dyn Clock>,
    timers: Vec<TimerEntry>,
    next_timer_ordinal: u64,
}

#[derive(Debug)]
struct InFlightCall {
    call_id: CallId,
    call_kind: CallKind,
    requester: RegisteredAddress,
    cause: CauseId,
}

type ErasedTranslator = Box<dyn FnOnce(CallOutput) -> Box<dyn Any>>;

struct StoredTranslator {
    call_id: CallId,
    translator: Option<ErasedTranslator>,
}

impl std::fmt::Debug for StoredTranslator {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StoredTranslator")
            .field("call_id", &self.call_id)
            .finish_non_exhaustive()
    }
}

impl<S, F> Runtime<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    /// Creates a new runtime for one shard plus one runtime-owned mailbox
    /// factory for future spawned children.
    pub fn new(shard: S, mailbox_factory: F) -> Self {
        Self::with_clock_and_ids(
            shard,
            mailbox_factory,
            Box::new(MonotonicClock),
            IdSource::new(),
        )
    }

    #[cfg(test)]
    fn with_clock(shard: S, mailbox_factory: F, clock: Box<dyn Clock>) -> Self {
        Self::with_clock_and_ids(shard, mailbox_factory, clock, IdSource::new())
    }

    fn with_clock_and_ids(
        shard: S,
        mailbox_factory: F,
        clock: Box<dyn Clock>,
        ids: IdSource,
    ) -> Self {
        Self {
            shard,
            mailbox_factory,
            entries: Vec::new(),
            child_records: Vec::new(),
            supervisors: Vec::new(),
            next_isolate_id: 1,
            ids,
            trace: Vec::new(),
            io_backend: IoBackend::new(),
            in_flight_calls: Vec::new(),
            translators: Vec::new(),
            clock,
            timers: Vec::new(),
            next_timer_ordinal: 0,
        }
    }

    /// Returns whether the runtime has any in-flight calls that have not
    /// yet been delivered. Tests use this to know when stepping further
    /// can produce more I/O completions.
    pub fn has_in_flight_calls(&self) -> bool {
        !self.in_flight_calls.is_empty() || self.io_backend.has_pending() || !self.timers.is_empty()
    }

    #[cfg(test)]
    fn io_pending_count(&self) -> usize {
        self.io_backend.pending_count()
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
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
        I::Message: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
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

    /// Registers one isolate and lets the runtime allocate the mailbox.
    #[allow(private_bounds)]
    pub fn register_with_capacity<I, Outbound>(
        &mut self,
        isolate: I,
        mailbox_capacity: usize,
    ) -> Address<I::Message>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
        I::Message: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        Outbound: 'static,
    {
        let address = self.register_entry::<I, Outbound>(
            isolate,
            None,
            Box::new(MailboxAdapter::<Box<dyn Mailbox<I::Message>>, I::Message> {
                mailbox: self.mailbox_factory.create::<I::Message>(mailbox_capacity),
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

    /// Configures a registered isolate as supervisor for its direct children.
    ///
    /// This is a setup-time runtime API. Unknown, stale, or cross-shard parent
    /// addresses are programmer errors and panic. Reconfiguring the same parent
    /// replaces the config and resets the runtime-lifetime budget tracker.
    pub fn supervise<M: 'static>(&mut self, parent: Address<M>, config: SupervisorConfig) {
        let parent = self.checked_registered_address(parent, "supervise");
        let budget_state = config.budget().tracker();

        if let Some(record) = self
            .supervisors
            .iter_mut()
            .find(|record| record.parent == parent)
        {
            record.config = config;
            record.budget_state = budget_state;
            return;
        }

        self.supervisors.push(SupervisorRecord {
            parent,
            config,
            budget_state,
        });
    }

    /// Runs one deterministic round over all registered isolates.
    ///
    /// The runtime first advances its I/O backend so any pending
    /// runtime-owned calls that finished since the previous step can be
    /// delivered as ordinary later-turn messages. Then each registered
    /// isolate gets at most one delivery chance, in registration order.
    ///
    /// The return value is the number of handlers that ran in this round.
    pub fn step(&mut self) -> usize {
        let shard_id = self.shard.id();
        self.step_with_remote(&mut |_source_shard, send, _cause| {
            panic!(
                "cross-shard send is out of scope in this slice: target shard {} != runtime shard {}",
                send.target_shard.get(),
                shard_id.get(),
            );
        })
    }

    fn step_with_remote<FR>(&mut self, route_remote: &mut FR) -> usize
    where
        FR: FnMut(ShardId, ErasedSend, CauseId) -> Result<(), SendRejectedReason>,
    {
        let now = self.clock.now();
        self.advance_io_backend();
        self.harvest_timers(now);

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
                    self.supervise_panic(
                        RegisteredAddress {
                            shard: self.shard.id(),
                            isolate: isolate_id,
                            generation: self.entries[index].generation,
                        },
                        handler_panicked.into(),
                        &mut round_messages,
                    );
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

            self.execute_effect(
                index,
                isolate_id,
                handler_finished.into(),
                effect,
                &mut round_messages,
                route_remote,
            );
        }

        delivered
    }

    fn execute_effect(
        &mut self,
        index: usize,
        isolate_id: IsolateId,
        cause: CauseId,
        effect: ErasedEffect<S, F>,
        round_messages: &mut [Option<Box<dyn Any>>],
        route_remote: &mut impl FnMut(ShardId, ErasedSend, CauseId) -> Result<(), SendRejectedReason>,
    ) -> bool {
        match effect {
            ErasedEffect::Stop => {
                self.stop_entry(index, isolate_id, cause);
                true
            }
            ErasedEffect::Send(send) => {
                let target_shard = send.target_shard;
                let target_isolate = send.target_isolate;
                let target_generation = send.target_generation;
                let attempted = self.push_event(
                    isolate_id,
                    Some(cause),
                    RuntimeEventKind::SendDispatchAttempted {
                        target_shard,
                        target_isolate,
                        target_generation,
                    },
                );

                let delivery = if target_shard == self.shard.id() {
                    self.dispatch_local_send(send)
                } else {
                    route_remote(self.shard.id(), send, attempted.into())
                };

                match delivery {
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
                    Err(reason) => {
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
                false
            }
            ErasedEffect::Spawn(spawn) => {
                let mut outcome = spawn.spawn(self, isolate_id);
                let child_isolate = outcome.child.isolate;
                let child = outcome.child;
                let bootstrap_message = outcome.bootstrap_message.take();
                self.record_child(isolate_id, outcome);
                let spawned = self.push_event(
                    isolate_id,
                    Some(cause),
                    RuntimeEventKind::Spawned { child_isolate },
                );
                if let Some(message) = bootstrap_message {
                    self.enqueue_bootstrap_message(child, message, spawned.into());
                }
                false
            }
            ErasedEffect::RestartChildren => {
                self.restart_children(isolate_id, cause, round_messages);
                false
            }
            ErasedEffect::Call(call) => {
                let requester = RegisteredAddress {
                    shard: self.shard.id(),
                    isolate: isolate_id,
                    generation: self.entries[index].generation,
                };
                self.dispatch_call(call, requester, cause);
                false
            }
            ErasedEffect::Noop => {
                self.push_event(
                    isolate_id,
                    Some(cause),
                    RuntimeEventKind::EffectObserved {
                        effect: EffectKind::Noop,
                    },
                );
                false
            }
            ErasedEffect::Reply => {
                self.push_event(
                    isolate_id,
                    Some(cause),
                    RuntimeEventKind::EffectObserved {
                        effect: EffectKind::Reply,
                    },
                );
                false
            }
            ErasedEffect::Batch(effects) => {
                for subeffect in effects {
                    if self.execute_effect(
                        index,
                        isolate_id,
                        cause,
                        subeffect,
                        round_messages,
                        route_remote,
                    ) {
                        return true;
                    }
                }
                false
            }
        }
    }

    fn dispatch_call(&mut self, call: ErasedCall, requester: RegisteredAddress, cause: CauseId) {
        let call_id = self.ids.next_call_id();
        let call_kind = call.request.kind();

        self.push_event(
            requester.isolate,
            Some(cause),
            RuntimeEventKind::CallDispatchAttempted { call_id, call_kind },
        );

        let ErasedCall {
            request,
            translator,
        } = call;

        // Register the translator and in-flight tracking before submission
        // so a synchronous completion (bind / close on Betelgeuse) can be
        // delivered through the same path as async completions.
        self.in_flight_calls.push(InFlightCall {
            call_id,
            call_kind,
            requester,
            cause,
        });
        self.translators.push(StoredTranslator {
            call_id,
            translator: Some(translator),
        });

        match request {
            CallInput::Sleep { after } => {
                let deadline = self.clock.now() + after;
                let insertion_order = self.next_timer_ordinal;
                self.next_timer_ordinal += 1;
                self.timers.push(TimerEntry {
                    call_id,
                    deadline,
                    insertion_order,
                });
            }
            other => {
                if let Some(immediate) = self.io_backend.submit(call_id, other) {
                    self.deliver_completion(immediate.call_id, immediate.result);
                }
            }
        }
    }

    fn advance_io_backend(&mut self) {
        let completed = self.io_backend.advance();
        for op in completed {
            self.deliver_completion(op.call_id, op.result);
        }
    }

    fn harvest_timers(&mut self, now: Instant) {
        let mut due = Vec::new();
        let mut still_pending = Vec::new();
        for entry in std::mem::take(&mut self.timers) {
            if entry.deadline <= now {
                due.push(entry);
            } else {
                still_pending.push(entry);
            }
        }
        self.timers = still_pending;
        due.sort_by(|a, b| {
            a.deadline
                .cmp(&b.deadline)
                .then_with(|| a.insertion_order.cmp(&b.insertion_order))
        });
        for entry in due {
            self.deliver_completion(entry.call_id, CallOutput::TimerFired);
        }
    }

    fn deliver_completion(&mut self, call_id: CallId, result: CallOutput) {
        let in_flight_index = self
            .in_flight_calls
            .iter()
            .position(|entry| entry.call_id == call_id)
            .unwrap_or_else(|| {
                panic!("io backend produced completion for unknown call {call_id:?}")
            });
        let in_flight = self.in_flight_calls.remove(in_flight_index);

        let translator_index = self
            .translators
            .iter()
            .position(|entry| entry.call_id == call_id)
            .unwrap_or_else(|| panic!("missing translator for call {call_id:?}"));
        let mut stored = self.translators.remove(translator_index);
        let translator = stored
            .translator
            .take()
            .unwrap_or_else(|| panic!("translator for call {call_id:?} already consumed"));

        // Trace semantics: `CallFailed` records that the runtime
        // observed a failure result for this call. `CallCompleted`
        // records that a *successful* result's translated message
        // reached the requester's mailbox. `CallCompletionRejected`
        // records that the translator's message could not reach the
        // mailbox (regardless of whether the underlying result was a
        // success or a failure). A failed call therefore emits at most
        // `CallFailed` plus, if delivery also fails, one
        // `CallCompletionRejected` — never `CallCompleted`.
        let failure_reason = match &result {
            CallOutput::Failed(reason) => Some(*reason),
            _ => None,
        };
        if let Some(reason) = failure_reason {
            self.push_event(
                in_flight.requester.isolate,
                Some(in_flight.cause),
                RuntimeEventKind::CallFailed {
                    call_id,
                    call_kind: in_flight.call_kind,
                    reason,
                },
            );
        }

        let message = translator(result);

        let entry_index = self.entries.iter().position(|entry| {
            entry.id == in_flight.requester.isolate
                && entry.generation == in_flight.requester.generation
        });
        let Some(entry_index) = entry_index else {
            self.push_event(
                in_flight.requester.isolate,
                Some(in_flight.cause),
                RuntimeEventKind::CallCompletionRejected {
                    call_id,
                    call_kind: in_flight.call_kind,
                    reason: CallCompletionRejectedReason::RequesterClosed,
                },
            );
            return;
        };

        if self.entries[entry_index].stopped.get() {
            self.push_event(
                in_flight.requester.isolate,
                Some(in_flight.cause),
                RuntimeEventKind::CallCompletionRejected {
                    call_id,
                    call_kind: in_flight.call_kind,
                    reason: CallCompletionRejectedReason::RequesterClosed,
                },
            );
            return;
        }

        match self.entries[entry_index].mailbox.try_send_boxed(message) {
            Ok(()) => {
                if failure_reason.is_none() {
                    self.push_event(
                        in_flight.requester.isolate,
                        Some(in_flight.cause),
                        RuntimeEventKind::CallCompleted {
                            call_id,
                            call_kind: in_flight.call_kind,
                        },
                    );
                }
                // For failed results we already emitted `CallFailed`
                // above; the translator's message reaching the mailbox
                // is the expected behavior and does not need a second
                // event.
            }
            Err(TrySendError::Full(_)) => {
                self.push_event(
                    in_flight.requester.isolate,
                    Some(in_flight.cause),
                    RuntimeEventKind::CallCompletionRejected {
                        call_id,
                        call_kind: in_flight.call_kind,
                        reason: CallCompletionRejectedReason::MailboxFull,
                    },
                );
            }
            Err(TrySendError::Closed(_)) => {
                self.push_event(
                    in_flight.requester.isolate,
                    Some(in_flight.cause),
                    RuntimeEventKind::CallCompletionRejected {
                        call_id,
                        call_kind: in_flight.call_kind,
                        reason: CallCompletionRejectedReason::RequesterClosed,
                    },
                );
            }
        }
    }

    fn enqueue_bootstrap_message(
        &mut self,
        child: RegisteredAddress,
        message: Box<dyn Any>,
        cause: CauseId,
    ) {
        let entry = self
            .entries
            .iter()
            .find(|entry| entry.id == child.isolate && entry.generation == child.generation)
            .unwrap_or_else(|| panic!("bootstrap referenced unknown child {:?}", child.isolate));
        entry.mailbox.try_send_boxed(message).unwrap_or_else(|_| {
            panic!(
                "runtime failed to enqueue bootstrap message for child {:?}",
                child.isolate
            )
        });
        self.push_event(
            child.isolate,
            Some(cause),
            RuntimeEventKind::MailboxAccepted,
        );
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

    fn supervise_panic(
        &mut self,
        failed_child: RegisteredAddress,
        cause: CauseId,
        round_messages: &mut [Option<Box<dyn Any>>],
    ) {
        let Some(failed_record_index) = self.child_record_index_by_child(failed_child) else {
            return;
        };

        let parent = self.child_records[failed_record_index].parent;
        let failed_ordinal = self.child_records[failed_record_index].child_ordinal;
        let Some(supervisor_index) = self.supervisor_index(parent) else {
            return;
        };

        if self
            .entry_by_isolate(parent)
            .is_some_and(|entry| entry.stopped.get())
        {
            self.push_event(
                parent,
                Some(cause),
                RuntimeEventKind::SupervisorRestartRejected {
                    failed_child: failed_child.isolate,
                    failed_ordinal,
                    reason: SupervisionRejectedReason::SupervisorStopped,
                },
            );
            return;
        }

        let config = self.supervisors[supervisor_index].config;
        let policy = config.policy();
        let budget_state = self.supervisors[supervisor_index].budget_state;
        let budget_state = match budget_state.record_restart() {
            Ok(next) => next,
            Err(error) => {
                self.push_event(
                    parent,
                    Some(cause),
                    RuntimeEventKind::SupervisorRestartRejected {
                        failed_child: failed_child.isolate,
                        failed_ordinal,
                        reason: SupervisionRejectedReason::BudgetExceeded {
                            attempted_restart: error.attempted_restart(),
                            max_restarts: error.max_restarts(),
                        },
                    },
                );
                return;
            }
        };
        self.supervisors[supervisor_index].budget_state = budget_state;

        let triggered = self.push_event(
            parent,
            Some(cause),
            RuntimeEventKind::SupervisorRestartTriggered {
                policy,
                failed_child: failed_child.isolate,
                failed_ordinal,
            },
        );

        let selected: Vec<usize> = self
            .child_records
            .iter()
            .enumerate()
            .filter_map(|(index, record)| {
                if record.parent != parent {
                    return None;
                }

                let relation = ChildRelation::from_ordinals(record.child_ordinal, failed_ordinal);
                policy.restarts(relation).then_some(index)
            })
            .collect();

        for child_record_index in selected {
            self.restart_child_record(parent, child_record_index, triggered.into(), round_messages);
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
        let bootstrap_message = outcome.bootstrap_message;
        self.child_records[child_record_index].child = new_child;
        self.child_records[child_record_index].mailbox_capacity = outcome.mailbox_capacity;
        // Rebind the same restart recipe so this child slot remains
        // restartable after the first replacement.
        self.child_records[child_record_index].restart_recipe = Some(recipe);

        let restarted = self.push_event(
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
        if let Some(message) = bootstrap_message {
            self.enqueue_bootstrap_message(new_child, message, restarted.into());
        }
    }

    fn push_event(
        &mut self,
        isolate: IsolateId,
        cause: Option<CauseId>,
        kind: RuntimeEventKind,
    ) -> EventId {
        let id = self.ids.next_event_id();
        self.trace
            .push(RuntimeEvent::new(id, cause, self.shard.id(), isolate, kind));
        id
    }

    fn dispatch_local_send(&self, send: ErasedSend) -> Result<(), SendRejectedReason> {
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
            return Err(SendRejectedReason::Closed);
        }

        entry
            .mailbox
            .try_send_boxed(send.message)
            .map_err(|reason| match reason {
                TrySendError::Full(_) => SendRejectedReason::Full,
                TrySendError::Closed(_) => SendRejectedReason::Closed,
            })
    }

    fn harvest_remote_send(&mut self, queued: QueuedRemoteSend) {
        // Cross-shard transport admission already happened on the source shard.
        // What we record here is destination-local harvest outcome, not a
        // retroactive change to the source-side send result.
        let send = queued.send;
        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.id == send.target_isolate)
        else {
            self.push_event(
                send.target_isolate,
                Some(queued.cause),
                RuntimeEventKind::SendRejected {
                    target_shard: send.target_shard,
                    target_isolate: send.target_isolate,
                    target_generation: send.target_generation,
                    reason: SendRejectedReason::Closed,
                },
            );
            return;
        };

        if entry.generation != send.target_generation {
            self.push_event(
                send.target_isolate,
                Some(queued.cause),
                RuntimeEventKind::SendRejected {
                    target_shard: send.target_shard,
                    target_isolate: send.target_isolate,
                    target_generation: send.target_generation,
                    reason: SendRejectedReason::Closed,
                },
            );
            return;
        }

        match entry.mailbox.try_send_boxed(send.message) {
            Ok(()) => {
                self.push_event(
                    send.target_isolate,
                    Some(queued.cause),
                    RuntimeEventKind::MailboxAccepted,
                );
            }
            Err(TrySendError::Full(_)) => {
                self.push_event(
                    send.target_isolate,
                    Some(queued.cause),
                    RuntimeEventKind::SendRejected {
                        target_shard: send.target_shard,
                        target_isolate: send.target_isolate,
                        target_generation: send.target_generation,
                        reason: SendRejectedReason::Full,
                    },
                );
            }
            Err(TrySendError::Closed(_)) => {
                self.push_event(
                    send.target_isolate,
                    Some(queued.cause),
                    RuntimeEventKind::SendRejected {
                        target_shard: send.target_shard,
                        target_isolate: send.target_isolate,
                        target_generation: send.target_generation,
                        reason: SendRejectedReason::Closed,
                    },
                );
            }
        }
    }

    fn entry_index(&self, address: RegisteredAddress) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| entry.id == address.isolate && entry.generation == address.generation)
    }

    fn entry_by_isolate(&self, isolate: IsolateId) -> Option<&RegisteredEntry<S, F>> {
        self.entries.iter().find(|entry| entry.id == isolate)
    }

    fn child_record_index_by_child(&self, child: RegisteredAddress) -> Option<usize> {
        self.child_records
            .iter()
            .position(|record| record.child == child)
    }

    fn supervisor_index(&self, parent: IsolateId) -> Option<usize> {
        self.supervisors
            .iter()
            .position(|record| record.parent.isolate == parent)
    }

    fn checked_registered_address<M: 'static>(
        &self,
        address: Address<M>,
        operation: &str,
    ) -> RegisteredAddress {
        if address.shard() != self.shard.id() {
            panic!(
                "{operation} targeted a parent on another shard: target shard {} != runtime shard {}",
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
                "{operation} targeted unknown parent isolate {} on shard {}",
                address.isolate().get(),
                address.shard().get(),
            );
        };

        if entry.generation != address.generation() {
            panic!(
                "{operation} targeted stale parent isolate {} generation {} on shard {}",
                address.isolate().get(),
                address.generation().get(),
                address.shard().get(),
            );
        }

        RegisteredAddress {
            shard: address.shard(),
            isolate: address.isolate(),
            generation: address.generation(),
        }
    }

    fn register_entry<I, Outbound>(
        &mut self,
        isolate: I,
        parent: Option<IsolateId>,
        mailbox: Box<dyn ErasedMailbox>,
    ) -> RegisteredAddress
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
        I::Message: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
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
        bootstrap_message: Option<I::Message>,
    ) -> SpawnOutcome<S, F>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
        I::Message: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
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
            bootstrap_message: bootstrap_message.map(|message| Box::new(message) as Box<dyn Any>),
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

    /// Returns the stored supervisor records in configuration order.
    #[cfg(test)]
    pub(crate) fn supervisor_snapshot(&self) -> Vec<SupervisorRecordSnapshot> {
        self.supervisors
            .iter()
            .map(|record| SupervisorRecordSnapshot {
                parent: record.parent,
                config: record.config,
                budget_state: record.budget_state,
            })
            .collect()
    }
}

/// Deterministic explicit-step coordinator over a fixed set of shard runtimes.
///
/// This additive shell preserves the existing single-shard [`Runtime`] API
/// while giving Galileo one honest place to define global ingress, global
/// stepping order, and explicit root placement by shard.
pub struct MultiShardRuntime<S, F>
where
    S: Shard + 'static,
    F: MailboxFactory + 'static,
{
    runtimes: Vec<Runtime<S, F>>,
    shard_indexes: BTreeMap<ShardId, usize>,
    config: MultiShardRuntimeConfig,
    remote_queues: BTreeMap<(ShardId, ShardId), VecDeque<QueuedRemoteSend>>,
}

/// Bounded coordinator config for additive multi-shard runtime shells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MultiShardRuntimeConfig {
    /// Capacity of each source-shard -> destination-shard queue.
    pub shard_pair_capacity: usize,
}

impl Default for MultiShardRuntimeConfig {
    fn default() -> Self {
        Self {
            shard_pair_capacity: 64,
        }
    }
}

impl<S, F> MultiShardRuntime<S, F>
where
    S: Shard + 'static,
    F: MailboxFactory + Clone + 'static,
{
    /// Creates one additive multi-shard coordinator over the provided shards.
    ///
    /// Shards are stepped in ascending [`ShardId`] order, regardless of input
    /// order. Empty shard sets and duplicate shard ids are programmer errors
    /// and panic.
    pub fn new<I>(shards: I, mailbox_factory: F) -> Self
    where
        I: IntoIterator<Item = S>,
    {
        Self::with_config(shards, mailbox_factory, MultiShardRuntimeConfig::default())
    }

    /// Creates one additive multi-shard coordinator with explicit shard-pair
    /// queue boundedness.
    pub fn with_config<I>(shards: I, mailbox_factory: F, config: MultiShardRuntimeConfig) -> Self
    where
        I: IntoIterator<Item = S>,
    {
        let mut shards: Vec<S> = shards.into_iter().collect();
        if shards.is_empty() {
            panic!("multi-shard runtime requires at least one shard");
        }
        if config.shard_pair_capacity == 0 {
            panic!("multi-shard runtime requires shard-pair capacity > 0");
        }

        shards.sort_by_key(Shard::id);
        for pair in shards.windows(2) {
            if pair[0].id() == pair[1].id() {
                panic!(
                    "multi-shard runtime received duplicate shard id {}",
                    pair[0].id().get()
                );
            }
        }

        let ids = IdSource::new();
        let mut runtimes = Vec::with_capacity(shards.len());
        let mut shard_indexes = BTreeMap::new();
        for shard in shards {
            let shard_id = shard.id();
            shard_indexes.insert(shard_id, runtimes.len());
            runtimes.push(Runtime::with_clock_and_ids(
                shard,
                mailbox_factory.clone(),
                Box::new(MonotonicClock),
                ids.clone(),
            ));
        }

        Self {
            runtimes,
            shard_indexes,
            config,
            remote_queues: BTreeMap::new(),
        }
    }

    /// Returns the shard ids owned by this coordinator in global step order.
    pub fn shard_ids(&self) -> Vec<ShardId> {
        self.runtimes
            .iter()
            .map(|runtime| runtime.shard().id())
            .collect()
    }

    /// Returns the merged deterministic event record in global event-id order.
    pub fn trace(&self) -> Vec<RuntimeEvent> {
        let mut events: Vec<_> = self
            .runtimes
            .iter()
            .flat_map(|runtime| runtime.trace().iter().copied())
            .collect();
        events.sort_by_key(|event| event.id());
        events
    }

    /// Returns whether any owned shard still has in-flight runtime-owned work.
    pub fn has_in_flight_calls(&self) -> bool {
        self.runtimes.iter().any(Runtime::has_in_flight_calls)
    }

    /// Registers one root isolate on the requested owning shard.
    #[allow(private_bounds)]
    pub fn register_on<I, M, Outbound>(
        &mut self,
        shard: ShardId,
        isolate: I,
        mailbox: M,
    ) -> Address<I::Message>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
        I::Message: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        Outbound: 'static,
        M: Mailbox<I::Message> + 'static,
    {
        self.runtime_mut(shard)
            .register::<I, M, Outbound>(isolate, mailbox)
    }

    /// Registers one root isolate on the requested shard and lets that shard
    /// runtime allocate the mailbox.
    #[allow(private_bounds)]
    pub fn register_with_capacity_on<I, Outbound>(
        &mut self,
        shard: ShardId,
        isolate: I,
        mailbox_capacity: usize,
    ) -> Address<I::Message>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
        I::Message: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        Outbound: 'static,
    {
        self.runtime_mut(shard)
            .register_with_capacity::<I, Outbound>(isolate, mailbox_capacity)
    }

    /// Configures a registered isolate as supervisor on its owning shard.
    pub fn supervise<M: 'static>(&mut self, parent: Address<M>, config: SupervisorConfig) {
        self.runtime_mut(parent.shard()).supervise(parent, config);
    }

    /// Attempts one typed global ingress send routed strictly by target shard.
    pub fn try_send<M: 'static>(
        &self,
        address: Address<M>,
        message: M,
    ) -> Result<(), TrySendError<M>> {
        self.runtime(address.shard()).try_send(address, message)
    }

    /// Runs one global deterministic round in ascending shard-id order.
    pub fn step(&mut self) -> usize {
        let mut ready = std::mem::take(&mut self.remote_queues);
        let shard_ids = self.shard_ids();
        let mut delivered = 0;

        for destination in &shard_ids {
            self.harvest_for_destination(*destination, &shard_ids, &mut ready);
            let index = self.checked_shard_index(*destination);
            let config = self.config;
            let shard_indexes = self.shard_indexes.clone();
            let remote_queues = &mut self.remote_queues;
            delivered += self.runtimes[index].step_with_remote(&mut |source_shard, send, cause| {
                if !shard_indexes.contains_key(&send.target_shard) {
                    panic!(
                        "multi-shard runtime targeted unknown destination shard {}",
                        send.target_shard.get()
                    );
                }

                let key = (source_shard, send.target_shard);
                let queue = remote_queues.entry(key).or_default();
                if queue.len() >= config.shard_pair_capacity {
                    return Err(SendRejectedReason::Full);
                }
                queue.push_back(QueuedRemoteSend { send, cause });
                Ok(())
            });
        }

        delivered
    }

    fn runtime(&self, shard: ShardId) -> &Runtime<S, F> {
        &self.runtimes[self.checked_shard_index(shard)]
    }

    fn runtime_mut(&mut self, shard: ShardId) -> &mut Runtime<S, F> {
        let index = self.checked_shard_index(shard);
        &mut self.runtimes[index]
    }

    fn checked_shard_index(&self, shard: ShardId) -> usize {
        self.shard_indexes
            .get(&shard)
            .copied()
            .unwrap_or_else(|| panic!("multi-shard runtime targeted unknown shard {}", shard.get()))
    }

    fn harvest_for_destination(
        &mut self,
        destination: ShardId,
        shard_ids: &[ShardId],
        ready: &mut BTreeMap<(ShardId, ShardId), VecDeque<QueuedRemoteSend>>,
    ) {
        let index = self.checked_shard_index(destination);
        for source in shard_ids {
            if *source == destination {
                continue;
            }
            let key = (*source, destination);
            let Some(queue) = ready.get_mut(&key) else {
                continue;
            };
            while let Some(queued) = queue.pop_front() {
                self.runtimes[index].harvest_remote_send(queued);
            }
        }
    }
}

/// Configuration for [`ThreadedRuntime`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreadedRuntimeConfig {
    /// Capacity of the bounded control/ingress queue feeding the shard worker.
    pub command_capacity: usize,

    /// How long an idle worker may park before checking runtime-owned work
    /// again.
    pub idle_wait: Duration,
}

impl Default for ThreadedRuntimeConfig {
    fn default() -> Self {
        Self {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
        }
    }
}

/// Error returned by setup/control operations on [`ThreadedRuntime`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadedControlError {
    /// The worker thread stopped before it could accept or answer the command.
    WorkerStopped,
}

/// Error returned by [`ThreadedRuntime::try_send`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadedTrySendError {
    /// The bounded worker ingress queue is full.
    IngressFull,

    /// The worker thread stopped before it could accept the ingress command.
    WorkerStopped,
}

/// Error returned by [`ThreadedRuntime::send_and_observe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadedSendObservedError {
    /// The bounded worker ingress queue is full.
    IngressFull,

    /// The target isolate mailbox is full.
    MailboxFull,

    /// The target isolate is closed or stale.
    MailboxClosed,

    /// The worker thread stopped before the send could be observed.
    WorkerStopped,
}

type ThreadedCommandFn<S, F> = Box<dyn FnOnce(&mut Runtime<S, F>) + Send>;

enum ThreadedCommand<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    Run(ThreadedCommandFn<S, F>),
    Shutdown,
}

/// One live shard-owned runtime worker.
///
/// The worker constructs and owns a single [`Runtime`] on its OS thread. The
/// handle only communicates through a bounded command queue, so ingress
/// pressure remains visible instead of falling into an unbounded executor
/// backlog. This is the smallest live substrate shape for Huygens; the
/// explicit-step [`Runtime`] and [`MultiShardRuntime`] remain the semantic
/// oracle.
pub struct ThreadedRuntime<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    commands: std::sync::mpsc::SyncSender<ThreadedCommand<S, F>>,
    handle: Option<JoinHandle<Vec<RuntimeEvent>>>,
}

impl<S, F> ThreadedRuntime<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    /// Starts one worker thread for one shard runtime.
    pub fn new(shard: S, mailbox_factory: F) -> Self {
        Self::with_config(shard, mailbox_factory, ThreadedRuntimeConfig::default())
    }

    /// Starts one worker thread with explicit bounded-command configuration.
    pub fn with_config(shard: S, mailbox_factory: F, config: ThreadedRuntimeConfig) -> Self {
        if config.command_capacity == 0 {
            panic!("threaded runtime requires command capacity > 0");
        }

        let (commands, receiver) = std::sync::mpsc::sync_channel(config.command_capacity);
        let handle =
            thread::spawn(move || threaded_worker_loop(shard, mailbox_factory, receiver, config));

        Self {
            commands,
            handle: Some(handle),
        }
    }

    /// Registers one root isolate and lets the worker allocate its mailbox.
    #[allow(private_bounds)]
    pub fn register_with_capacity<I, Outbound>(
        &self,
        isolate: I,
        mailbox_capacity: usize,
    ) -> Result<Address<I::Message>, ThreadedControlError>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + Send + 'static,
        I::Message: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        Outbound: 'static,
    {
        self.call(move |runtime| {
            runtime.register_with_capacity::<I, Outbound>(isolate, mailbox_capacity)
        })
    }

    /// Configures a registered isolate as supervisor on the worker shard.
    pub fn supervise<M: 'static>(
        &self,
        parent: Address<M>,
        config: SupervisorConfig,
    ) -> Result<(), ThreadedControlError> {
        self.call(move |runtime| runtime.supervise(parent, config))
    }

    /// Attempts one typed ingress handoff through the bounded worker queue.
    ///
    /// Success means the worker accepted ownership of the message command. It
    /// does not mean the target mailbox has accepted the message yet. Mailbox
    /// `Full` / `Closed` outcomes are observed on the worker side through trace
    /// or through [`send_and_observe`](Self::send_and_observe).
    pub fn try_send<M: Send + 'static>(
        &self,
        address: Address<M>,
        message: M,
    ) -> Result<(), ThreadedTrySendError> {
        let command = ThreadedCommand::Run(Box::new(move |runtime| {
            let _ = runtime.try_send(address, message);
        }));

        match self.commands.try_send(command) {
            Ok(()) => Ok(()),
            Err(std::sync::mpsc::TrySendError::Full(_)) => Err(ThreadedTrySendError::IngressFull),
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                Err(ThreadedTrySendError::WorkerStopped)
            }
        }
    }

    /// Attempts one typed ingress send and waits for the worker to observe the
    /// target mailbox outcome.
    ///
    /// This is a synchronous control path for tests and setup code that need to
    /// distinguish mailbox `Full` from `Closed`. Ordinary ingress should prefer
    /// [`try_send`](Self::try_send), which only proves bounded handoff.
    pub fn send_and_observe<M: Send + 'static>(
        &self,
        address: Address<M>,
        message: M,
    ) -> Result<(), ThreadedSendObservedError> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        let command = ThreadedCommand::Run(Box::new(move |runtime| {
            let result = runtime
                .try_send(address, message)
                .map_err(|error| match error {
                    TrySendError::Full(_) => ThreadedSendObservedError::MailboxFull,
                    TrySendError::Closed(_) => ThreadedSendObservedError::MailboxClosed,
                });
            let _ = reply_tx.send(result);
        }));

        match self.commands.try_send(command) {
            Ok(()) => reply_rx
                .recv()
                .unwrap_or(Err(ThreadedSendObservedError::WorkerStopped)),
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                Err(ThreadedSendObservedError::IngressFull)
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                Err(ThreadedSendObservedError::WorkerStopped)
            }
        }
    }

    /// Returns a snapshot of the worker trace.
    pub fn trace(&self) -> Result<Vec<RuntimeEvent>, ThreadedControlError> {
        self.call(|runtime| runtime.trace().to_vec())
    }

    /// Returns whether the worker still has runtime-owned work pending.
    pub fn has_in_flight_calls(&self) -> Result<bool, ThreadedControlError> {
        self.call(|runtime| runtime.has_in_flight_calls())
    }

    /// Requests shutdown and joins the worker, returning its final trace.
    pub fn shutdown(mut self) -> Result<Vec<RuntimeEvent>, ThreadedControlError> {
        self.shutdown_inner()
    }

    fn call<R, C>(&self, command: C) -> Result<R, ThreadedControlError>
    where
        R: Send + 'static,
        C: FnOnce(&mut Runtime<S, F>) -> R + Send + 'static,
    {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.commands
            .send(ThreadedCommand::Run(Box::new(move |runtime| {
                let _ = reply_tx.send(command(runtime));
            })))
            .map_err(|_| ThreadedControlError::WorkerStopped)?;
        reply_rx
            .recv()
            .map_err(|_| ThreadedControlError::WorkerStopped)
    }

    fn shutdown_inner(&mut self) -> Result<Vec<RuntimeEvent>, ThreadedControlError> {
        let Some(handle) = self.handle.take() else {
            return Ok(Vec::new());
        };
        let _ = self.commands.send(ThreadedCommand::Shutdown);
        handle
            .join()
            .map_err(|_| ThreadedControlError::WorkerStopped)
    }
}

impl<S, F> Drop for ThreadedRuntime<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    fn drop(&mut self) {
        let _ = self.shutdown_inner();
    }
}

fn threaded_worker_loop<S, F>(
    shard: S,
    mailbox_factory: F,
    receiver: std::sync::mpsc::Receiver<ThreadedCommand<S, F>>,
    config: ThreadedRuntimeConfig,
) -> Vec<RuntimeEvent>
where
    S: Shard,
    F: MailboxFactory,
{
    let mut runtime = Runtime::new(shard, mailbox_factory);

    loop {
        match receiver.try_recv() {
            Ok(ThreadedCommand::Run(command)) => {
                command(&mut runtime);
                continue;
            }
            Ok(ThreadedCommand::Shutdown) => break,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }

        let delivered = runtime.step();
        if delivered == 0 && !runtime.has_in_flight_calls() {
            match receiver.recv_timeout(config.idle_wait) {
                Ok(ThreadedCommand::Run(command)) => command(&mut runtime),
                Ok(ThreadedCommand::Shutdown) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            }
        } else {
            thread::yield_now();
        }
    }

    runtime.trace().to_vec()
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
    bootstrap_message: Option<Box<dyn Any>>,
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

struct SupervisorRecord {
    parent: RegisteredAddress,
    config: SupervisorConfig,
    budget_state: RestartBudgetState,
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

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SupervisorRecordSnapshot {
    parent: RegisteredAddress,
    config: SupervisorConfig,
    budget_state: RestartBudgetState,
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
    fn spawn(self: Box<Self>, runtime: &mut Runtime<S, F>, parent: IsolateId)
    -> SpawnOutcome<S, F>;
}

trait ErasedRestartRecipe<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    fn create(&self, runtime: &mut Runtime<S, F>, parent: IsolateId) -> SpawnOutcome<S, F>;
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
    I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
    I::Message: 'static,
    I::Spawn: IntoErasedSpawn<S, F> + 'static,
    I::Call: IntoErasedCall<I::Message> + 'static,
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

        erase_effect::<I, S, F, Outbound>(effect)
    }
}

fn erase_effect<I, S, F, Outbound>(effect: Effect<I>) -> ErasedEffect<S, F>
where
    I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
    I::Message: 'static,
    I::Spawn: IntoErasedSpawn<S, F> + 'static,
    I::Call: IntoErasedCall<I::Message> + 'static,
    Outbound: 'static,
    S: Shard,
    F: MailboxFactory,
{
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
        Effect::Call(call) => ErasedEffect::Call(call.into_erased_call()),
        Effect::Batch(effects) => ErasedEffect::Batch(
            effects
                .into_iter()
                .map(erase_effect::<I, S, F, Outbound>)
                .collect(),
        ),
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
    Call(ErasedCall),
    Batch(Vec<ErasedEffect<S, F>>),
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
            Self::Call(_) => EffectKind::Call,
            Self::Batch(_) => EffectKind::Batch,
        }
    }
}

struct ErasedSend {
    target_shard: ShardId,
    target_isolate: IsolateId,
    target_generation: AddressGeneration,
    message: Box<dyn Any>,
}

struct QueuedRemoteSend {
    send: ErasedSend,
    cause: CauseId,
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
    bootstrap_message: Option<I::Message>,
    marker: PhantomData<fn(Outbound) -> Outbound>,
}

impl<I, S, F, Outbound> ErasedSpawn<S, F> for SpawnAdapter<I, Outbound>
where
    I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
    I::Message: 'static,
    I::Spawn: IntoErasedSpawn<S, F> + 'static,
    I::Call: IntoErasedCall<I::Message> + 'static,
    Outbound: 'static,
    S: Shard,
    F: MailboxFactory,
{
    fn spawn(
        self: Box<Self>,
        runtime: &mut Runtime<S, F>,
        parent: IsolateId,
    ) -> SpawnOutcome<S, F> {
        runtime.spawn_isolate::<I, Outbound>(
            parent,
            self.isolate,
            self.mailbox_capacity,
            self.bootstrap_message,
        )
    }
}

impl<I, S, F, OutboundMsg> IntoErasedSpawn<S, F> for tina::ChildDefinition<I>
where
    I: Isolate<Shard = S, Send = TinaOutbound<OutboundMsg>> + 'static,
    I::Message: 'static,
    I::Spawn: IntoErasedSpawn<S, F> + 'static,
    I::Call: IntoErasedCall<I::Message> + 'static,
    OutboundMsg: 'static,
    S: Shard,
    F: MailboxFactory,
{
    fn into_erased_spawn(self) -> Box<dyn ErasedSpawn<S, F>> {
        let (isolate, mailbox_capacity, bootstrap_message) = self.into_parts();
        Box::new(SpawnAdapter::<I, OutboundMsg> {
            isolate,
            mailbox_capacity,
            bootstrap_message,
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
    bootstrap_factory: Option<Box<dyn Fn() -> I::Message>>,
    marker: PhantomData<fn(Outbound) -> Outbound>,
}

impl<I, S, F, Outbound> ErasedSpawn<S, F> for RestartableSpawnAdapter<I, Outbound>
where
    I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
    I::Message: 'static,
    I::Spawn: IntoErasedSpawn<S, F> + 'static,
    I::Call: IntoErasedCall<I::Message> + 'static,
    Outbound: 'static,
    S: Shard,
    F: MailboxFactory,
{
    fn spawn(
        self: Box<Self>,
        runtime: &mut Runtime<S, F>,
        parent: IsolateId,
    ) -> SpawnOutcome<S, F> {
        let adapter = Rc::new(*self);
        let isolate = (adapter.factory)();
        let mailbox_capacity = adapter.mailbox_capacity;
        let bootstrap_message = adapter.bootstrap_factory.as_ref().map(|f| f());
        let mut outcome = runtime.spawn_isolate::<I, Outbound>(
            parent,
            isolate,
            mailbox_capacity,
            bootstrap_message,
        );
        outcome.restart_recipe = Some(adapter);
        outcome
    }
}

impl<I, S, F, Outbound> ErasedRestartRecipe<S, F> for RestartableSpawnAdapter<I, Outbound>
where
    I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
    I::Message: 'static,
    I::Spawn: IntoErasedSpawn<S, F> + 'static,
    I::Call: IntoErasedCall<I::Message> + 'static,
    Outbound: 'static,
    S: Shard,
    F: MailboxFactory,
{
    fn create(&self, runtime: &mut Runtime<S, F>, parent: IsolateId) -> SpawnOutcome<S, F> {
        let isolate = (self.factory)();
        let bootstrap_message = self.bootstrap_factory.as_ref().map(|f| f());
        runtime.spawn_isolate::<I, Outbound>(
            parent,
            isolate,
            self.mailbox_capacity,
            bootstrap_message,
        )
    }
}

impl<I, S, F, OutboundMsg> IntoErasedSpawn<S, F> for tina::RestartableChildDefinition<I>
where
    I: Isolate<Shard = S, Send = TinaOutbound<OutboundMsg>> + 'static,
    I::Message: 'static,
    I::Spawn: IntoErasedSpawn<S, F> + 'static,
    I::Call: IntoErasedCall<I::Message> + 'static,
    OutboundMsg: 'static,
    S: Shard,
    F: MailboxFactory,
{
    fn into_erased_spawn(self) -> Box<dyn ErasedSpawn<S, F>> {
        let (factory, mailbox_capacity, bootstrap_factory) = self.into_parts();
        Box::new(RestartableSpawnAdapter::<I, OutboundMsg> {
            factory,
            mailbox_capacity,
            bootstrap_factory,
            marker: PhantomData,
        })
    }
}

#[cfg(test)]
mod tests;
