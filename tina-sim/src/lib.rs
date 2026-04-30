#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

//! Single-shard virtual-time simulator for `tina-rs`.
//!
//! Phase 016 starts Voyager with the narrowest honest simulator slice:
//!
//! - one shard
//! - virtual monotonic time
//! - the shipped `Sleep { after }` / `TimerFired` contract
//! - deterministic replay artifacts
//!
//! `tina-sim` intentionally reuses the live runtime's event vocabulary from
//! `tina-runtime-current` instead of inventing a second user-visible meaning
//! model. The simulator is narrower than the live runtime: it supports local
//! sends, stop, reply/noop observation, ordered `Batch`, and runtime-owned
//! `Sleep` calls. Spawn, restart, and TCP simulation are deferred to later
//! Voyager slices.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::marker::PhantomData;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Duration;

use tina::{
    Address, AddressGeneration, Context, Effect, Isolate, IsolateId, SendMessage, Shard, ShardId,
    TrySendError,
};
use tina_runtime_current::{
    CallCompletionRejectedReason, CallFailureReason, CallId, CallKind, CallRequest, CallResult,
    CurrentCall, EffectKind, RuntimeEvent, RuntimeEventKind, SendRejectedReason,
};

/// Deterministic simulator configuration for one run.
///
/// The first Voyager slice keeps the config intentionally small. `seed` is
/// reserved in the replay artifact shape now so later Voyager slices can add
/// seeded scheduling or fault behavior without renegotiating the config
/// format. In 016 itself, execution is deterministic without consuming the
/// seed yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct SimulatorConfig {
    /// Reserved replay seed for future Voyager slices.
    ///
    /// 016 records this value in replay artifacts but does not yet use it to
    /// perturb scheduling, timer ordering, or failures.
    pub seed: u64,
}

/// Captured replay data for one deterministic simulator run.
///
/// In 016, replay means rerunning the same workload under the same simulator
/// config and reproducing the same event record and final virtual time. Later
/// Voyager slices may broaden that to seeded perturbation while preserving the
/// same artifact shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayArtifact {
    config: SimulatorConfig,
    final_time: Duration,
    event_record: Vec<RuntimeEvent>,
}

impl ReplayArtifact {
    /// Returns the simulator config used for the original run.
    pub const fn config(&self) -> SimulatorConfig {
        self.config
    }

    /// Returns the final virtual time reached by the run.
    pub const fn final_time(&self) -> Duration {
        self.final_time
    }

    /// Returns the deterministic event record captured for the run.
    pub fn event_record(&self) -> &[RuntimeEvent] {
        &self.event_record
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RegisteredAddress {
    shard: ShardId,
    isolate: IsolateId,
    generation: AddressGeneration,
}

#[derive(Debug)]
struct InFlightCall {
    call_id: CallId,
    call_kind: CallKind,
    requester: RegisteredAddress,
    cause: tina_runtime_current::CauseId,
}

type ErasedTranslator = Box<dyn FnOnce(CallResult) -> Box<dyn Any>>;

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

#[derive(Debug)]
struct TimerEntry {
    call_id: CallId,
    deadline: Duration,
    insertion_order: u64,
}

struct LocalInbox {
    queue: RefCell<VecDeque<Box<dyn Any>>>,
    closed: Cell<bool>,
}

impl std::fmt::Debug for LocalInbox {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LocalInbox")
            .field("closed", &self.closed.get())
            .field("len", &self.queue.borrow().len())
            .finish()
    }
}

impl LocalInbox {
    fn new() -> Self {
        Self {
            queue: RefCell::new(VecDeque::new()),
            closed: Cell::new(false),
        }
    }

    fn push(&self, message: Box<dyn Any>) -> Result<(), TrySendError<Box<dyn Any>>> {
        if self.closed.get() {
            return Err(TrySendError::Closed(message));
        }
        self.queue.borrow_mut().push_back(message);
        Ok(())
    }

    fn pop(&self) -> Option<Box<dyn Any>> {
        self.queue.borrow_mut().pop_front()
    }

    fn close(&self) {
        self.closed.set(true);
    }
}

trait ErasedHandler<S>
where
    S: Shard,
{
    fn handle_boxed(
        &mut self,
        message: Box<dyn Any>,
        shard: &mut S,
        isolate_id: IsolateId,
    ) -> ErasedEffect;
}

struct HandlerAdapter<I, Outbound>
where
    I: Isolate,
{
    isolate: I,
    marker: PhantomData<fn(Outbound) -> Outbound>,
}

impl<I, S, Msg, Outbound> ErasedHandler<S> for HandlerAdapter<I, Outbound>
where
    I: Isolate<
            Message = Msg,
            Shard = S,
            Send = SendMessage<Outbound>,
            Spawn = Infallible,
            Call = CurrentCall<Msg>,
        > + 'static,
    Msg: 'static,
    Outbound: 'static,
    S: Shard,
{
    fn handle_boxed(
        &mut self,
        message: Box<dyn Any>,
        shard: &mut S,
        isolate_id: IsolateId,
    ) -> ErasedEffect {
        let message = message.downcast::<Msg>().unwrap_or_else(|_| {
            panic!("simulator attempted to deliver a handler message with the wrong type")
        });

        let effect = {
            let mut ctx = Context::new(shard, isolate_id);
            self.isolate.handle(*message, &mut ctx)
        };

        erase_effect::<I, S, Msg, Outbound>(effect)
    }
}

fn erase_effect<I, S, Msg, Outbound>(effect: Effect<I>) -> ErasedEffect
where
    I: Isolate<
            Message = Msg,
            Shard = S,
            Send = SendMessage<Outbound>,
            Spawn = Infallible,
            Call = CurrentCall<Msg>,
        > + 'static,
    Msg: 'static,
    Outbound: 'static,
    S: Shard,
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
        Effect::Spawn(spawn) => match spawn {},
        Effect::Stop => ErasedEffect::Stop,
        Effect::RestartChildren => ErasedEffect::RestartChildren,
        Effect::Call(call) => {
            let (request, translator) = call.into_parts();
            ErasedEffect::Call(ErasedCall {
                request,
                translator: Box::new(move |result| Box::new(translator(result)) as Box<dyn Any>),
            })
        }
        Effect::Batch(effects) => ErasedEffect::Batch(
            effects
                .into_iter()
                .map(erase_effect::<I, S, Msg, Outbound>)
                .collect(),
        ),
    }
}

struct RegisteredEntry<S>
where
    S: Shard,
{
    id: IsolateId,
    generation: AddressGeneration,
    stopped: Cell<bool>,
    stopped_event: Cell<Option<tina_runtime_current::EventId>>,
    inbox: LocalInbox,
    handler: RefCell<Box<dyn ErasedHandler<S>>>,
}

impl<S> std::fmt::Debug for RegisteredEntry<S>
where
    S: Shard,
{
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RegisteredEntry")
            .field("id", &self.id)
            .field("generation", &self.generation)
            .field("stopped", &self.stopped.get())
            .field("stopped_event", &self.stopped_event.get())
            .finish_non_exhaustive()
    }
}

enum ErasedEffect {
    Noop,
    Reply,
    Send(ErasedSend),
    Stop,
    RestartChildren,
    Call(ErasedCall),
    Batch(Vec<ErasedEffect>),
}

impl ErasedEffect {
    fn kind(&self) -> EffectKind {
        match self {
            Self::Noop => EffectKind::Noop,
            Self::Reply => EffectKind::Reply,
            Self::Send(_) => EffectKind::Send,
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

struct ErasedCall {
    request: CallRequest,
    translator: ErasedTranslator,
}

/// Narrow single-shard simulator for timer-driven `tina-rs` workloads.
pub struct Simulator<S>
where
    S: Shard,
{
    shard: S,
    config: SimulatorConfig,
    entries: Vec<RegisteredEntry<S>>,
    next_isolate_id: u64,
    next_event_id: u64,
    next_call_id: u64,
    trace: Vec<RuntimeEvent>,
    virtual_now: Duration,
    timers: Vec<TimerEntry>,
    next_timer_ordinal: u64,
    in_flight_calls: Vec<InFlightCall>,
    translators: Vec<StoredTranslator>,
}

impl<S> Simulator<S>
where
    S: Shard,
{
    /// Creates a new simulator with virtual time starting at zero.
    pub fn new(shard: S, config: SimulatorConfig) -> Self {
        Self {
            shard,
            config,
            entries: Vec::new(),
            next_isolate_id: 1,
            next_event_id: 1,
            next_call_id: 1,
            trace: Vec::new(),
            virtual_now: Duration::ZERO,
            timers: Vec::new(),
            next_timer_ordinal: 0,
            in_flight_calls: Vec::new(),
            translators: Vec::new(),
        }
    }

    /// Returns the immutable simulator config for this run.
    pub const fn config(&self) -> SimulatorConfig {
        self.config
    }

    /// Returns the current virtual monotonic time.
    pub const fn now(&self) -> Duration {
        self.virtual_now
    }

    /// Returns the accumulated deterministic event record.
    pub fn trace(&self) -> &[RuntimeEvent] {
        &self.trace
    }

    /// Returns whether the simulator still has pending timers or undelivered
    /// call completions.
    pub fn has_in_flight_calls(&self) -> bool {
        !self.in_flight_calls.is_empty() || !self.timers.is_empty()
    }

    /// Registers one isolate and returns its typed address.
    ///
    /// 016 intentionally requires spawnless isolates that use the current
    /// runtime's timer-aware call vocabulary and local `SendMessage` sends.
    pub fn register<I, Msg, Outbound>(&mut self, isolate: I) -> Address<Msg>
    where
        I: Isolate<
                Message = Msg,
                Shard = S,
                Send = SendMessage<Outbound>,
                Spawn = Infallible,
                Call = CurrentCall<Msg>,
            > + 'static,
        Msg: 'static,
        Outbound: 'static,
    {
        let isolate_id = IsolateId::new(self.next_isolate_id);
        self.next_isolate_id += 1;
        let generation = AddressGeneration::new(0);
        self.entries.push(RegisteredEntry {
            id: isolate_id,
            generation,
            stopped: Cell::new(false),
            stopped_event: Cell::new(None),
            inbox: LocalInbox::new(),
            handler: RefCell::new(Box::new(HandlerAdapter::<I, Outbound> {
                isolate,
                marker: PhantomData,
            })),
        });

        Address::new_with_generation(self.shard.id(), isolate_id, generation)
    }

    /// Attempts to inject one typed message into a registered isolate.
    pub fn try_send<M: 'static>(
        &self,
        address: Address<M>,
        message: M,
    ) -> Result<(), TrySendError<M>> {
        if address.shard() != self.shard.id() {
            panic!(
                "cross-shard simulator ingress is out of scope in this slice: target shard {} != simulator shard {}",
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
                "simulator ingress targeted unknown isolate {} on shard {}",
                address.isolate().get(),
                address.shard().get(),
            );
        };

        if entry.generation != address.generation() || entry.stopped.get() {
            return Err(TrySendError::Closed(message));
        }

        match entry.inbox.push(Box::new(message)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(message)) => {
                Err(TrySendError::Full(*message.downcast::<M>().unwrap_or_else(
                    |_| panic!("simulator ingress hit wrong mailbox type"),
                )))
            }
            Err(TrySendError::Closed(message)) => Err(TrySendError::Closed(
                *message
                    .downcast::<M>()
                    .unwrap_or_else(|_| panic!("simulator ingress hit wrong mailbox type")),
            )),
        }
    }

    /// Advances virtual monotonic time by `by`.
    pub fn advance_time(&mut self, by: Duration) {
        self.virtual_now += by;
    }

    /// Advances virtual time directly to the next due timer, if any.
    pub fn advance_to_next_timer(&mut self) -> bool {
        let Some(next_deadline) = self.timers.iter().map(|entry| entry.deadline).min() else {
            return false;
        };
        if next_deadline > self.virtual_now {
            self.virtual_now = next_deadline;
        }
        true
    }

    /// Runs one deterministic simulator round.
    ///
    /// The simulator samples virtual time once at the start of the round,
    /// harvests due timers, and then gives each registered isolate at most one
    /// delivery chance in registration order.
    pub fn step(&mut self) -> usize {
        let now = self.virtual_now;
        self.harvest_timers(now);

        let mut round_messages: Vec<Option<Box<dyn Any>>> = self
            .entries
            .iter()
            .map(|entry| {
                if entry.stopped.get() {
                    None
                } else {
                    entry.inbox.pop()
                }
            })
            .collect();

        let mut delivered = 0;

        for (index, slot) in round_messages.iter_mut().enumerate() {
            let Some(message) = slot.take() else {
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
                    let panicked = self.push_event(
                        isolate_id,
                        Some(handler_started.into()),
                        RuntimeEventKind::HandlerPanicked,
                    );
                    self.stop_entry(index, isolate_id, panicked.into());
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

            self.execute_effect(index, isolate_id, handler_finished.into(), effect);
        }

        delivered
    }

    /// Continues running until the simulator becomes quiescent.
    ///
    /// When no messages are ready but timers remain pending, the simulator
    /// advances virtual time to the earliest pending deadline and keeps
    /// stepping. This is the simplest honest driver for the first replayable
    /// Voyager slice.
    pub fn run_until_quiescent(&mut self) -> usize {
        let mut total = 0;
        loop {
            let delivered = self.step();
            total += delivered;
            if delivered > 0 {
                continue;
            }
            if self.advance_to_next_timer() {
                continue;
            }
            break total;
        }
    }

    /// Captures a deterministic replay artifact for the current run.
    pub fn replay_artifact(&self) -> ReplayArtifact {
        ReplayArtifact {
            config: self.config,
            final_time: self.virtual_now,
            event_record: self.trace.clone(),
        }
    }

    fn execute_effect(
        &mut self,
        index: usize,
        isolate_id: IsolateId,
        cause: tina_runtime_current::CauseId,
        effect: ErasedEffect,
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
            ErasedEffect::RestartChildren => {
                panic!("RestartChildren is out of scope for tina-sim phase 016");
            }
            ErasedEffect::Batch(effects) => {
                for subeffect in effects {
                    if self.execute_effect(index, isolate_id, cause, subeffect) {
                        return true;
                    }
                }
                false
            }
        }
    }

    fn dispatch_call(
        &mut self,
        call: ErasedCall,
        requester: RegisteredAddress,
        cause: tina_runtime_current::CauseId,
    ) {
        let call_id = CallId::new(self.next_call_id);
        self.next_call_id += 1;
        let call_kind = call_kind(&call.request);
        self.push_event(
            requester.isolate,
            Some(cause),
            RuntimeEventKind::CallDispatchAttempted { call_id, call_kind },
        );

        let ErasedCall {
            request,
            translator,
        } = call;
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
            CallRequest::Sleep { after } => {
                let deadline = self.virtual_now + after;
                let insertion_order = self.next_timer_ordinal;
                self.next_timer_ordinal += 1;
                self.timers.push(TimerEntry {
                    call_id,
                    deadline,
                    insertion_order,
                });
            }
            _ => {
                self.deliver_completion(call_id, CallResult::Failed(CallFailureReason::Unsupported))
            }
        }
    }

    fn harvest_timers(&mut self, now: Duration) {
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
            self.deliver_completion(entry.call_id, CallResult::TimerFired);
        }
    }

    fn deliver_completion(&mut self, call_id: CallId, result: CallResult) {
        let in_flight_index = self
            .in_flight_calls
            .iter()
            .position(|entry| entry.call_id == call_id)
            .unwrap_or_else(|| {
                panic!("simulator produced completion for unknown call {call_id:?}")
            });
        let in_flight = self.in_flight_calls.remove(in_flight_index);

        let translator_index = self
            .translators
            .iter()
            .position(|entry| entry.call_id == call_id)
            .unwrap_or_else(|| panic!("missing simulator translator for call {call_id:?}"));
        let mut stored = self.translators.remove(translator_index);
        let translator = stored
            .translator
            .take()
            .unwrap_or_else(|| panic!("translator for call {call_id:?} already consumed"));

        let failure_reason = match &result {
            CallResult::Failed(reason) => Some(*reason),
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

        match self.entries[entry_index].inbox.push(message) {
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

    fn dispatch_local_send(
        &self,
        send: ErasedSend,
    ) -> Result<(), (ShardId, IsolateId, AddressGeneration, SendRejectedReason)> {
        if send.target_shard != self.shard.id() {
            panic!(
                "cross-shard simulation is out of scope in this slice: target shard {} != simulator shard {}",
                send.target_shard.get(),
                self.shard.id().get(),
            );
        }

        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.id == send.target_isolate)
        else {
            return Err((
                send.target_shard,
                send.target_isolate,
                send.target_generation,
                SendRejectedReason::Closed,
            ));
        };

        if entry.generation != send.target_generation || entry.stopped.get() {
            return Err((
                send.target_shard,
                send.target_isolate,
                send.target_generation,
                SendRejectedReason::Closed,
            ));
        }

        entry
            .inbox
            .push(send.message)
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

    fn stop_entry(
        &mut self,
        index: usize,
        isolate_id: IsolateId,
        cause: tina_runtime_current::CauseId,
    ) -> tina_runtime_current::EventId {
        if self.entries[index].stopped.get() {
            return self.entries[index]
                .stopped_event
                .get()
                .unwrap_or_else(|| panic!("stopped isolate has no stopped event"));
        }

        self.entries[index].stopped.set(true);
        self.entries[index].inbox.close();
        let stopped = self.push_event(isolate_id, Some(cause), RuntimeEventKind::IsolateStopped);
        self.entries[index].stopped_event.set(Some(stopped));
        while self.entries[index].inbox.pop().is_some() {
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
        cause: Option<tina_runtime_current::CauseId>,
        kind: RuntimeEventKind,
    ) -> tina_runtime_current::EventId {
        let id = tina_runtime_current::EventId::new(self.next_event_id);
        self.next_event_id += 1;
        self.trace
            .push(RuntimeEvent::new(id, cause, self.shard.id(), isolate, kind));
        id
    }
}

fn call_kind(request: &CallRequest) -> CallKind {
    match request {
        CallRequest::TcpBind { .. } => CallKind::TcpBind,
        CallRequest::TcpAccept { .. } => CallKind::TcpAccept,
        CallRequest::TcpRead { .. } => CallKind::TcpRead,
        CallRequest::TcpWrite { .. } => CallKind::TcpWrite,
        CallRequest::TcpListenerClose { .. } => CallKind::TcpListenerClose,
        CallRequest::TcpStreamClose { .. } => CallKind::TcpStreamClose,
        CallRequest::Sleep { .. } => CallKind::Sleep,
    }
}
