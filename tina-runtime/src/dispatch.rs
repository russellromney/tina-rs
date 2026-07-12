//! Effect execution, call dispatch, and isolate-lifecycle machinery on
//! [`Runtime`].
//!
//! This is the largest bin in the runtime split: it owns every method
//! that walks a handler turn, fires a runtime call, settles a call
//! outcome, executes an [`Effect`], stops or restarts an isolate, or
//! pushes a [`RuntimeEvent`] into the trace. The `Erased*` adapter
//! family that hides the handler / spawn / mailbox generics also lives
//! here so the dispatch path and the type erasure read together.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::marker::PhantomData;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tina::{
    Address, AddressGeneration, CallContext, CallRejectedReason, CallRouting, ChildRef,
    ChildRelation, Context, DeferredReplyHandle, DeferredSlotState, Effect, Isolate, IsolateId,
    Mailbox, MessageCaller, Outbound as TinaOutbound, RestartBudgetState, Shard, ShardId,
    SpawnObservedError, StopResult, TrySendError,
};

use crate::call::{
    CallError, CallId, CallInput, CallOutcome, CallOutput, ErasedCall, ErasedRuntimeCallCompletion,
    IntoErasedCall, SendOutcome,
};
use crate::driver::DriverShutdownError;
use crate::fact::{IntoRuntimeFact, RuntimeFact};
use crate::mailbox::MailboxFactory;
use crate::registration::ContinuationDelivery;
use crate::remote::{
    QueuedRemoteEnvelope, QueuedRemoteSend, RemoteCallOutcome, RemoteCallReply, RemoteChildRestart,
    RemoteChildRestarted, RemoteChildStop, RemoteSpawnCancel, RemoteSpawnRequest,
};
use crate::trace::{
    CallCompletionRejectedReason, CallKind, CallReplyRejectedReason, CauseId,
    DeferredReplyRejectedReason, DeferredSlotId, EffectKind, EventId, RestartSkippedReason,
    RuntimeEvent, RuntimeEventKind, SendRejectedReason, SupervisionRejectedReason,
    TerminalCompletionAction,
};
use crate::{
    CANCELLED_CALL_RING_CAPACITY, CallDispatchContext, DeliveredMessage, DriverCall,
    DriverCallHead, ErasedIsolateCallTranslator, MessageCallContext, PendingIsolateCall, Runtime,
    TraceRetention, call, call_reply_reason_for_cause, deferred, deferred_reply_reason_for_cause,
    observation, reserve_round_message_scratch, trace,
};
use tina_supervisor::SupervisorConfig;

impl<S, F> Runtime<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    /// Cancels every in-flight runtime-owned call ahead of shutdown.
    ///
    /// The terminal-outcome priority for any call that could resolve
    /// multiple ways is fixed:
    /// 1. **requester stopped/full**: a stopped or full requester wins
    ///    its local completion path (the in-flight-call entry was
    ///    already removed when the requester stopped, so any later
    ///    completion routes through `RequesterClosed` tombstoning).
    /// 2. **shard failed**: a failed source/destination shard wins over
    ///    a later success because [`LiveShardState::Failed`] gates
    ///    ingress and the worker thread has stopped delivering.
    /// 3. **timeout**: a deadline that fired before the failure was
    ///    observed wins the call's result via `CallError::Timeout`.
    /// 4. **full transport/mailbox**: full reasons only stick when no
    ///    higher-priority terminal state already exists.
    ///
    /// The "exactly one terminal outcome" property is enforced
    /// structurally by the `in_flight_calls` map: the first terminal
    /// event removes the entry; subsequent attempts hit a missing
    /// call_id and tombstone (or get dropped at the lane's
    /// `finish_completion` when the user-cancelled flag is set).
    pub(crate) fn cancel_in_flight_calls_for_shutdown(
        &mut self,
        deadline: Instant,
    ) -> Result<(), DriverShutdownError> {
        let driver_result = self.driver.cancel_pending(deadline);
        // Undelivered carried completions reference in-flight calls that are
        // about to be rejected as RequesterClosed; drop them so a later advance
        // cannot try to deliver a completion for a call (and translator) that no
        // longer exists.
        self.pending_completions.clear();

        let driver_calls: Vec<_> = self.call_table.drain_driver().collect();
        for call in driver_calls {
            self.push_event(
                call.head.requester.isolate,
                Some(call.head.cause),
                RuntimeEventKind::CallCompletionRejected {
                    call_id: call.head.call_id,
                    call_kind: call.head.call_kind,
                    reason: CallCompletionRejectedReason::RequesterClosed,
                },
            );
        }

        let pending_isolate_calls: Vec<_> = self.call_table.drain_isolate().collect();
        for call in pending_isolate_calls {
            // Mark any caller-held CallHandle as Cancelled so a late
            // poll of `handle.state()` reflects the truth instead of
            // staying `Pending` forever.
            if let Some(shared) = &call.handle_shared {
                shared.set_state(tina::CallHandleState::Cancelled);
            }
            // Record the cause so any late callee reply that races
            // shutdown surfaces as `RuntimeStopped` rather than the
            // generic `NoPendingCall` / `CallerClosed`.
            self.record_cancelled_call(call.call_id, tina::CancelCause::RuntimeStopped);
            self.push_event(
                call.requester.isolate,
                Some(call.cause),
                RuntimeEventKind::CallCancelled {
                    call_id: call.call_id,
                    cause: tina::CancelCause::RuntimeStopped,
                },
            );
        }
        driver_result
    }

    pub(crate) fn notify_signal(&mut self, name: &str) {
        let mut completed = std::mem::take(&mut self.driver_completions);
        completed.clear();
        self.driver.notify_signal(name, &mut completed);
        for op in completed.drain(..) {
            self.deliver_completion(op.call_id, op.result);
        }
        self.driver_completions = completed;
    }

    pub(crate) fn cancel_driver_calls_for_requester(&mut self, requester: RegisteredAddress) {
        // Ascending call-id order (== insertion order); the simulator mirrors it.
        for call_id in self.call_table.driver_call_ids_for_requester(requester) {
            let call = self
                .call_table
                .remove_driver(call_id)
                .expect("indexed in-flight call exists");
            self.driver.cancel(call.head.call_id);
            // A completion already harvested for this call but carried past the
            // per-step drain budget would otherwise be delivered after the entry
            // is gone. That is NOT a driver bug — the requester just stopped — so
            // drop the carried completion here instead of letting it fall
            // through to `deliver_completion`'s quarantine path. `RequesterClosed`
            // below is the completion's observable settlement.
            self.pending_completions.retain(|op| op.call_id != call_id);
            self.push_event(
                call.head.requester.isolate,
                Some(call.head.cause),
                RuntimeEventKind::CallCompletionRejected {
                    call_id: call.head.call_id,
                    call_kind: call.head.call_kind,
                    reason: CallCompletionRejectedReason::RequesterClosed,
                },
            );
        }
    }

    /// Runs one deterministic round over all registered isolates.
    ///
    /// The runtime first advances its driver so any pending
    /// runtime-owned calls that finished since the previous step can be
    /// delivered as ordinary later-turn messages. Then each registered
    /// isolate gets at most one delivery chance, in registration order.
    ///
    /// The return value is the number of handlers that ran in this round.
    pub fn step(&mut self) -> usize
    where
        S: 'static,
        F: 'static,
    {
        let shard_id = self.shard.id();
        self.step_with_remote(&mut |_source_shard, envelope| {
            let target_shard = envelope.target_shard();
            match envelope {
                QueuedRemoteEnvelope::Send(queued) => {
                panic!(
                    "cross-shard send is out of scope in this slice: target shard {} != runtime shard {}",
                    queued.send.target_shard.get(),
                    shard_id.get(),
                );
                }
                QueuedRemoteEnvelope::CallReply(_) => {
                panic!(
                    "cross-shard call reply is out of scope in this slice: requester shard {} != runtime shard {}",
                    target_shard.get(),
                    shard_id.get(),
                );
                }
                QueuedRemoteEnvelope::SpawnRequest(_)
                | QueuedRemoteEnvelope::SpawnReply(_)
                | QueuedRemoteEnvelope::SpawnCancel(_)
                | QueuedRemoteEnvelope::ChildStop(_)
                | QueuedRemoteEnvelope::ChildStopped(_)
                | QueuedRemoteEnvelope::ChildRestart(_)
                | QueuedRemoteEnvelope::ChildRestarted(_) => {
                    panic!(
                        "cross-shard child control requires a multi-shard runtime: target shard {} != runtime shard {}",
                        target_shard.get(),
                        shard_id.get(),
                    );
                }
            }
        })
    }

    pub(crate) fn step_with_remote<FR>(&mut self, route_remote: &mut FR) -> usize
    where
        FR: FnMut(ShardId, QueuedRemoteEnvelope) -> Result<(), SendRejectedReason>,
        S: 'static,
        F: 'static,
    {
        let now = self.clock.now();
        self.advance_driver(now);
        self.harvest_isolate_call_timeouts(now);

        let mut round_messages = std::mem::take(&mut self.round_messages);
        round_messages.clear();
        reserve_round_message_scratch(&mut round_messages, self.entries.len());
        for index in 0..self.entries.len() {
            // Skip-empty scan: a quiet entry answers cheaply, so we avoid the
            // expensive `recv` (virtual call + lock + context pop) on idle
            // isolates. The check covers both ingress paths — bounded mailbox
            // and continuation overflow — so a parked continuation is never
            // skipped.
            let message =
                if self.entries[index].stopped.get() || !self.entry_has_pending_message(index) {
                    None
                } else {
                    self.recv_entry_message(index)
                };
            round_messages.push(message);
        }

        let mut delivered = 0;

        for index in 0..round_messages.len() {
            let Some(message) = round_messages[index].take() else {
                continue;
            };

            if self.entries[index].stopped.get() {
                if let Some(stopped) = self.entries[index].stopped_event.get()
                    && !self.close_drained_local_call_context(stopped.into(), message)
                {
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

            let incoming_call_context = message.call_context;
            let caller = self.build_message_caller(incoming_call_context, isolate_id);
            let now = self.clock.now();

            let effect = {
                let mut handler = self.entries[index].handler.borrow_mut();
                catch_unwind(AssertUnwindSafe(|| match caller {
                    Some(caller) => handler.handle_call_boxed(
                        message.message,
                        &mut self.shard,
                        isolate_id,
                        self.entries[index].generation,
                        caller,
                        now,
                    ),
                    None => handler.handle_boxed(
                        message.message,
                        &mut self.shard,
                        isolate_id,
                        self.entries[index].generation,
                        None,
                        now,
                    ),
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
                    let captured_any = self.drop_pending_deferred_captures(handler_panicked.into());
                    if !captured_any {
                        if let Some(context) = incoming_call_context {
                            self.reject_call_context(
                                isolate_id,
                                handler_panicked.into(),
                                context,
                                CallRejectedReason::HandlerPanicked,
                                route_remote,
                            );
                        }
                    }
                    self.cleanup_remote_children_for_owner(
                        isolate_id,
                        handler_panicked.into(),
                        route_remote,
                    );
                    self.stop_entry(index, isolate_id, handler_panicked.into());
                    self.supervise_failed_child(
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

            // If the handler captured the caller, promote the new slot
            // record(s) and zero out the message's call_context so a
            // stray Effect::Reply does not also fire on the captured
            // call.
            let captured_any = self.promote_captures(isolate_id, handler_finished.into());

            let consumed_by_effect = effect.consumes_call_context();
            let abandoned_context = if !captured_any && !consumed_by_effect {
                message.call_context
            } else {
                None
            };
            if let Some(context) = abandoned_context {
                self.reject_call_context(
                    isolate_id,
                    handler_finished.into(),
                    context,
                    CallRejectedReason::ReplyAbandoned,
                    route_remote,
                );
            }

            let effective_context = if captured_any || abandoned_context.is_some() {
                None
            } else {
                message.call_context
            };

            self.execute_effect(
                index,
                handler_finished.into(),
                effect,
                effective_context,
                &mut round_messages,
                route_remote,
            );
        }

        round_messages.clear();
        self.round_messages = round_messages;

        self.sweep_dropped_deferred_slots();
        self.gc_stopped_entries();

        delivered
    }

    pub(crate) fn build_message_caller(
        &self,
        call_context: Option<MessageCallContext>,
        isolate_id: IsolateId,
    ) -> Option<MessageCaller> {
        let ctx = call_context?;
        let (call_id, routing, remote_expected_reply_type_id) = match ctx {
            MessageCallContext::Local { call_id } => (call_id, CallRouting::Local, None),
            MessageCallContext::Remote {
                call_id,
                requester,
                cause,
                expected_reply_type_id,
            } => (
                call_id,
                CallRouting::Remote {
                    requester_shard: requester.shard,
                    requester_isolate: requester.isolate,
                    requester_generation: requester.generation,
                    cause: cause.event().get(),
                },
                Some(expected_reply_type_id),
            ),
        };
        let (expected_reply_type_id, caller_open) = match routing {
            CallRouting::Local => match self.call_table.isolate_expected_reply_type_id(call_id) {
                Some(expected) => (expected, true),
                // The request was already queued when its caller timed out or
                // cancelled. Preserve late-reply handler/trace behavior, but
                // any RequestContext captured from it must start closed.
                None => (std::any::TypeId::of::<()>(), false),
            },
            CallRouting::Remote { .. } => remote_expected_reply_type_id
                .map(|expected| (expected, true))
                .expect("remote call context carries the expected reply type"),
        };
        let constructor = if caller_open {
            MessageCaller::new
        } else {
            MessageCaller::new_closed
        };
        Some(constructor(
            Rc::clone(&self.deferred_registry),
            call_id.get(),
            isolate_id,
            routing,
            expected_reply_type_id,
        ))
    }

    pub(crate) fn promote_captures(&mut self, isolate_id: IsolateId, cause: CauseId) -> bool {
        let captures = self.deferred_registry.drain_pending();
        if captures.is_empty() {
            return false;
        }
        for capture in captures {
            let slot_id = DeferredSlotId::new(capture.slot_id);
            let call_id = CallId::new(capture.call_id);
            let routing = match capture.routing {
                CallRouting::Local => deferred::DeferredRouting::Local,
                CallRouting::Remote {
                    requester_shard,
                    requester_isolate,
                    requester_generation,
                    cause: remote_cause,
                } => deferred::DeferredRouting::Remote {
                    requester: RegisteredAddress {
                        shard: requester_shard,
                        isolate: requester_isolate,
                        generation: requester_generation,
                    },
                    cause: CauseId::new(EventId::new(remote_cause)),
                },
            };
            self.push_event(
                isolate_id,
                Some(cause),
                RuntimeEventKind::DeferredReplyCaptured { slot_id, call_id },
            );
            self.promoted_slots.push(deferred::DeferredSlotRecord {
                slot_id,
                call_id,
                capturing_isolate: capture.capturing_isolate,
                shared: capture.shared,
                routing,
            });
        }
        true
    }

    pub(crate) fn sweep_dropped_deferred_slots(&mut self) {
        // Nothing promoted: skip the scan. Steady-state shards with no
        // outstanding deferred replies pay nothing here.
        if self.promoted_slots.is_empty() {
            return;
        }
        // Single pass: independent Rcs cannot cascade.
        let dropped = self.promoted_slots.sweep_dropped();
        for record in dropped {
            self.drop_promoted_deferred_slot(record, None);
        }
    }

    pub(crate) fn drop_pending_deferred_captures(&mut self, cause: CauseId) -> bool {
        let captures = self.deferred_registry.drain_pending();
        let captured_any = !captures.is_empty();
        for capture in captures {
            capture.shared.set_state(DeferredSlotState::Closed);
            let slot_id = DeferredSlotId::new(capture.slot_id);
            let call_id = CallId::new(capture.call_id);
            let captured = self.push_event(
                capture.capturing_isolate,
                Some(cause),
                RuntimeEventKind::DeferredReplyCaptured { slot_id, call_id },
            );
            let dropped = self.push_event(
                capture.capturing_isolate,
                Some(captured.into()),
                RuntimeEventKind::DeferredReplyDropped { slot_id, call_id },
            );
            self.complete_isolate_call(call_id, dropped.into(), CallOutcome::Closed);
        }
        captured_any
    }

    pub(crate) fn drop_promoted_deferred_slot(
        &mut self,
        record: deferred::DeferredSlotRecord,
        cause: Option<CauseId>,
    ) {
        if record.shared.state() != DeferredSlotState::Open {
            return;
        }
        record.shared.set_state(DeferredSlotState::Closed);
        let dropped = self.push_event(
            record.capturing_isolate,
            cause,
            RuntimeEventKind::DeferredReplyDropped {
                slot_id: record.slot_id,
                call_id: record.call_id,
            },
        );
        self.complete_isolate_call(record.call_id, dropped.into(), CallOutcome::Closed);
    }

    pub(crate) fn execute_effect(
        &mut self,
        index: usize,
        cause: CauseId,
        effect: ErasedEffect<S, F>,
        call_context: Option<MessageCallContext>,
        round_messages: &mut [Option<DeliveredMessage>],
        route_remote: &mut impl FnMut(ShardId, QueuedRemoteEnvelope) -> Result<(), SendRejectedReason>,
    ) -> bool
    where
        S: 'static,
        F: 'static,
    {
        let isolate_id = self.entries[index].id;
        match effect {
            ErasedEffect::Stop => {
                self.cleanup_remote_children_for_owner(isolate_id, cause, route_remote);
                self.stop_entry(index, isolate_id, cause);
                true
            }
            ErasedEffect::Fail => {
                // Typed, non-panic failure: record it distinctly, stop the
                // isolate (any in-flight caller already settled visibly via
                // the abandoned-context path), then feed supervision exactly
                // like a panic.
                let generation = self.entries[index].generation;
                let failed = self.push_event(
                    isolate_id,
                    Some(cause),
                    RuntimeEventKind::HandlerReportedFailure,
                );
                self.cleanup_remote_children_for_owner(isolate_id, failed.into(), route_remote);
                self.stop_entry(index, isolate_id, failed.into());
                self.supervise_failed_child(
                    RegisteredAddress {
                        shard: self.shard.id(),
                        isolate: isolate_id,
                        generation,
                    },
                    failed.into(),
                    round_messages,
                );
                true
            }
            ErasedEffect::StopWith(result) => {
                self.cleanup_remote_children_for_owner(isolate_id, cause, route_remote);
                self.stop_entry_with_result(index, isolate_id, cause, result);
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
                    route_remote(
                        self.shard.id(),
                        QueuedRemoteEnvelope::Send(QueuedRemoteSend {
                            send,
                            call_context: None,
                            cause: attempted.into(),
                        }),
                    )
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
            ErasedEffect::SpawnObserved(spawn) => {
                let mut outcome = spawn.spawn_observed(self, isolate_id);
                let continuation = outcome.continuation.take();
                let continuation_cause = if let Some(mut spawn_outcome) = outcome.spawn.take() {
                    let child_isolate = spawn_outcome.child.isolate;
                    let child = spawn_outcome.child;
                    let bootstrap_message = spawn_outcome.bootstrap_message.take();
                    self.record_child(isolate_id, spawn_outcome);
                    let spawned = self.push_event(
                        isolate_id,
                        Some(cause),
                        RuntimeEventKind::Spawned { child_isolate },
                    );
                    if let Some(message) = bootstrap_message {
                        self.enqueue_bootstrap_message(child, message, spawned.into());
                    }
                    spawned.into()
                } else {
                    cause
                };
                if let Some(message) = continuation {
                    let send = ErasedSend {
                        target_shard: self.shard.id(),
                        target_isolate: isolate_id,
                        target_generation: self.entries[index].generation,
                        message,
                    };
                    let attempted = self.push_event(
                        isolate_id,
                        Some(continuation_cause),
                        RuntimeEventKind::SendDispatchAttempted {
                            target_shard: send.target_shard,
                            target_isolate: send.target_isolate,
                            target_generation: send.target_generation,
                        },
                    );
                    match self.dispatch_local_send(send) {
                        Ok(()) => {
                            self.push_event(
                                isolate_id,
                                Some(attempted.into()),
                                RuntimeEventKind::SendAccepted {
                                    target_shard: self.shard.id(),
                                    target_isolate: isolate_id,
                                    target_generation: self.entries[index].generation,
                                },
                            );
                        }
                        Err(reason) => {
                            self.push_event(
                                isolate_id,
                                Some(attempted.into()),
                                RuntimeEventKind::SendRejected {
                                    target_shard: self.shard.id(),
                                    target_isolate: isolate_id,
                                    target_generation: self.entries[index].generation,
                                    reason,
                                },
                            );
                        }
                    }
                }
                false
            }
            ErasedEffect::SpawnObservedOn(parts) => {
                let owner = RegisteredAddress {
                    shard: self.shard.id(),
                    isolate: isolate_id,
                    generation: self.entries[index].generation,
                };
                if parts.target_shard == self.shard.id() {
                    // on_shard() pointed at the owner's own shard: this is an
                    // ordinary local owned observed spawn (register_remote_child
                    // records the ChildRecord and parent-attributed Spawned), so
                    // no round trip and no cross-shard ChildStarted fact.
                    let child_ordinal = self.next_child_ordinal(owner.isolate);
                    let outcome = parts
                        .spawn
                        .spawn_remote(self, owner, child_ordinal, None, cause);
                    let message = (parts.continuation)(outcome);
                    self.deliver_observed_continuation(owner, message, cause);
                } else {
                    let request_id = self.ids.next_call_id();
                    let child_ordinal = self.next_child_ordinal(owner.isolate);
                    let mailbox_capacity = parts.mailbox_capacity;
                    let remote_restartable = parts.remote_restartable;
                    let payload: Box<dyn Any + Send> = Box::new(parts.spawn);
                    self.pending_remote_spawns.push(PendingRemoteSpawn {
                        request_id,
                        requester: owner,
                        target_shard: parts.target_shard,
                        child_ordinal,
                        mailbox_capacity,
                        remote_restartable,
                        continuation: parts.continuation,
                    });
                    let routed = route_remote(
                        self.shard.id(),
                        QueuedRemoteEnvelope::SpawnRequest(RemoteSpawnRequest {
                            request_id,
                            target_shard: parts.target_shard,
                            owner,
                            child_ordinal,
                            payload,
                            cause,
                        }),
                    );
                    if routed.is_err() {
                        // The target shard could not accept the request; settle
                        // the owner's continuation with a typed error now.
                        if let Some(pos) = self
                            .pending_remote_spawns
                            .iter()
                            .position(|pending| pending.request_id == request_id)
                        {
                            let pending = self.pending_remote_spawns.remove(pos);
                            let message = (pending.continuation)(Err(
                                SpawnObservedError::DestinationUnavailable,
                            ));
                            self.deliver_observed_continuation(owner, message, cause);
                        }
                    }
                }
                false
            }
            ErasedEffect::RestartChildren => {
                self.restart_children(isolate_id, cause, round_messages, route_remote);
                false
            }
            ErasedEffect::StopChildren => {
                self.stop_children(isolate_id, cause, round_messages, route_remote);
                false
            }
            ErasedEffect::Io(call) => {
                let requester = RegisteredAddress {
                    shard: self.shard.id(),
                    isolate: isolate_id,
                    generation: self.entries[index].generation,
                };
                self.dispatch_call(call, requester, cause, call_context, route_remote);
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
            ErasedEffect::Reply(reply) => {
                if let Some(context) = call_context {
                    match context {
                        MessageCallContext::Local { call_id } => {
                            if !self.complete_isolate_call(
                                call_id,
                                cause,
                                CallOutcome::Replied(reply.into_any()),
                            ) {
                                let reason = match self.recently_cancelled_cause(call_id) {
                                    Some(c) => call_reply_reason_for_cause(c),
                                    None => CallReplyRejectedReason::NoPendingCall,
                                };
                                self.push_event(
                                    isolate_id,
                                    Some(cause),
                                    RuntimeEventKind::CallReplyRejected { call_id, reason },
                                );
                            }
                        }
                        MessageCallContext::Remote {
                            call_id,
                            requester,
                            cause: request_cause,
                            ..
                        } => {
                            let reply = RemoteCallReply {
                                call_id,
                                requester,
                                cause: request_cause,
                                outcome: RemoteCallOutcome::Replied(reply),
                            };
                            if let Err(reason) = route_remote(
                                self.shard.id(),
                                QueuedRemoteEnvelope::CallReply(reply),
                            ) {
                                let reason = match reason {
                                    SendRejectedReason::Full => {
                                        CallReplyRejectedReason::ReplyPathFull
                                    }
                                    SendRejectedReason::Closed => {
                                        CallReplyRejectedReason::RequesterShardClosed
                                    }
                                };
                                self.push_event(
                                    isolate_id,
                                    Some(cause),
                                    RuntimeEventKind::CallReplyRejected { call_id, reason },
                                );
                            }
                        }
                    }
                } else {
                    self.push_event(
                        isolate_id,
                        Some(cause),
                        RuntimeEventKind::EffectObserved {
                            effect: EffectKind::Reply,
                        },
                    );
                }
                false
            }
            ErasedEffect::Reject(reason) => {
                if let Some(context) = call_context {
                    self.reject_call_context(isolate_id, cause, context, reason, route_remote);
                } else {
                    self.push_event(
                        isolate_id,
                        Some(cause),
                        RuntimeEventKind::EffectObserved {
                            effect: EffectKind::Reject,
                        },
                    );
                }
                false
            }
            ErasedEffect::Batch(effects) => {
                let mut batch_context = call_context;
                for subeffect in effects {
                    let consumes_context = subeffect.consumes_call_context();
                    if self.execute_effect(
                        index,
                        cause,
                        subeffect,
                        batch_context,
                        round_messages,
                        route_remote,
                    ) {
                        return true;
                    }
                    if consumes_context {
                        batch_context = None;
                    }
                }
                false
            }
            ErasedEffect::ReplyTo { handle, message } => {
                self.execute_reply_to(isolate_id, cause, handle, message, route_remote);
                false
            }
            ErasedEffect::Fact(fact) => {
                self.push_event(
                    isolate_id,
                    Some(cause),
                    RuntimeEventKind::FactObserved { fact },
                );
                false
            }
        }
    }

    pub(crate) fn reject_call_context(
        &mut self,
        isolate_id: IsolateId,
        cause: CauseId,
        context: MessageCallContext,
        reason: CallRejectedReason,
        route_remote: &mut impl FnMut(ShardId, QueuedRemoteEnvelope) -> Result<(), SendRejectedReason>,
    ) {
        match context {
            MessageCallContext::Local { call_id } => {
                self.push_call_rejected_event(isolate_id, cause, call_id, reason);
                if !self.complete_isolate_call(call_id, cause, CallOutcome::Rejected(reason)) {
                    let reason = match self.recently_cancelled_cause(call_id) {
                        Some(c) => call_reply_reason_for_cause(c),
                        None => CallReplyRejectedReason::NoPendingCall,
                    };
                    self.push_event(
                        isolate_id,
                        Some(cause),
                        RuntimeEventKind::CallReplyRejected { call_id, reason },
                    );
                }
            }
            MessageCallContext::Remote {
                call_id,
                requester,
                cause: request_cause,
                ..
            } => {
                self.push_call_rejected_event(isolate_id, cause, call_id, reason);
                let reply = RemoteCallReply {
                    call_id,
                    requester,
                    cause: request_cause,
                    outcome: RemoteCallOutcome::Rejected(reason),
                };
                if let Err(rejected) =
                    route_remote(self.shard.id(), QueuedRemoteEnvelope::CallReply(reply))
                {
                    let reason = match rejected {
                        SendRejectedReason::Full => CallReplyRejectedReason::ReplyPathFull,
                        SendRejectedReason::Closed => CallReplyRejectedReason::RequesterShardClosed,
                    };
                    self.push_event(
                        isolate_id,
                        Some(cause),
                        RuntimeEventKind::CallReplyRejected { call_id, reason },
                    );
                }
            }
        }
    }

    pub(crate) fn push_call_rejected_event(
        &mut self,
        isolate_id: IsolateId,
        cause: CauseId,
        call_id: CallId,
        reason: CallRejectedReason,
    ) {
        let kind = match reason {
            CallRejectedReason::ReplyAbandoned => RuntimeEventKind::CallReplyAbandoned { call_id },
            CallRejectedReason::HandlerPanicked | CallRejectedReason::UnsupportedMessage => {
                RuntimeEventKind::CallRejected { call_id, reason }
            }
        };
        if matches!(reason, CallRejectedReason::UnsupportedMessage) {
            self.note_unsupported_message_rejection(isolate_id, call_id);
        }
        self.push_event(isolate_id, Some(cause), kind);
    }

    /// Debug tripwire for the "answers `call()` but only implements `handle`"
    /// bug class. `UnsupportedMessage` is the default `handle_call`'s reject
    /// reason, so a call resolving that way is a candidate for a target that
    /// never defined `handle_call`. Bumps a debug-only counter tests can assert
    /// on (`unsupported_message_rejections()`) so that class of bug surfaces.
    ///
    /// PRECISION CAVEAT: this counts EVERY `UnsupportedMessage` reject — the
    /// default handler's auto-reject AND a handler that deliberately calls
    /// `call.reject(UnsupportedMessage)`. The runtime cannot distinguish them
    /// (both are just `handle_call` returning the same effect), so a nonzero
    /// count means "investigate", not "definitely a missing `handle_call`". Use
    /// it in a controlled per-test runtime with known traffic; do NOT wire a
    /// global "count == 0" gate — it would false-fire on every isolate that
    /// legitimately rejects unsupported messages.
    ///
    /// Deliberately allocation-free and trace-free: it is a bare scalar bump on
    /// an already-cold reject path, so it perturbs neither the allocation pins
    /// nor golden trace hashes. Compiled out entirely in release for zero cost.
    #[cfg(debug_assertions)]
    #[inline]
    fn note_unsupported_message_rejection(&mut self, _isolate_id: IsolateId, _call_id: CallId) {
        self.unsupported_message_rejections += 1;
    }

    #[cfg(not(debug_assertions))]
    #[inline(always)]
    fn note_unsupported_message_rejection(&mut self, _isolate_id: IsolateId, _call_id: CallId) {}

    pub(crate) fn execute_reply_to(
        &mut self,
        isolate_id: IsolateId,
        cause: CauseId,
        handle: DeferredReplyHandle,
        message: ErasedMessage,
        route_remote: &mut impl FnMut(ShardId, QueuedRemoteEnvelope) -> Result<(), SendRejectedReason>,
    ) {
        // Locate by handle identity. If the slot is not in the registry,
        // the caller already closed (and we already emitted the
        // terminal Rejected event). User reply is silently consumed.
        let shared = tina::runtime_internal::handle_shared(&handle).clone();
        let Some(record) = self.promoted_slots.take_by_handle(&shared) else {
            // Already terminal: caller closed and we already emitted
            // Rejected{CallerClosed}. The payload is consumed without
            // re-emitting a terminal fact.
            return;
        };

        // Typecheck the payload before invoking the original caller's
        // translator. The translator's downcast would panic on a wrong
        // type; this surfaces as a typed Rejected event instead.
        let payload_type_id = message.payload_type_id();
        if payload_type_id != record.shared.expected_reply_type_id() {
            record.shared.set_state(DeferredSlotState::Closed);
            self.push_event(
                isolate_id,
                Some(cause),
                RuntimeEventKind::DeferredReplyRejected {
                    slot_id: record.slot_id,
                    call_id: record.call_id,
                    reason: DeferredReplyRejectedReason::TypeMismatch,
                },
            );
            return;
        }

        // Slot is live. Reply through it.
        match record.routing {
            deferred::DeferredRouting::Local => {
                if self.complete_isolate_call(
                    record.call_id,
                    cause,
                    CallOutcome::Replied(message.into_any()),
                ) {
                    record.shared.set_state(DeferredSlotState::Replied);
                    self.push_event(
                        isolate_id,
                        Some(cause),
                        RuntimeEventKind::DeferredReplySent {
                            slot_id: record.slot_id,
                            call_id: record.call_id,
                        },
                    );
                } else {
                    // Pending call gone — name the cause from the
                    // recently-cancelled ring when we have it; fall
                    // through to the generic `CallerClosed` only if
                    // the entry has been evicted.
                    let reason = match self.recently_cancelled_cause(record.call_id) {
                        Some(c) => deferred_reply_reason_for_cause(c),
                        None => DeferredReplyRejectedReason::CallerClosed,
                    };
                    record.shared.set_state(DeferredSlotState::Closed);
                    self.push_event(
                        isolate_id,
                        Some(cause),
                        RuntimeEventKind::DeferredReplyRejected {
                            slot_id: record.slot_id,
                            call_id: record.call_id,
                            reason,
                        },
                    );
                }
            }
            deferred::DeferredRouting::Remote {
                requester,
                cause: request_cause,
            } => {
                let envelope = QueuedRemoteEnvelope::CallReply(RemoteCallReply {
                    call_id: record.call_id,
                    requester,
                    cause: request_cause,
                    outcome: RemoteCallOutcome::Replied(message),
                });
                match route_remote(self.shard.id(), envelope) {
                    Ok(()) => {
                        record.shared.set_state(DeferredSlotState::Replied);
                        self.push_event(
                            isolate_id,
                            Some(cause),
                            RuntimeEventKind::DeferredReplySent {
                                slot_id: record.slot_id,
                                call_id: record.call_id,
                            },
                        );
                    }
                    Err(reason) => {
                        let reject_reason = match reason {
                            SendRejectedReason::Full => DeferredReplyRejectedReason::ReplyPathFull,
                            SendRejectedReason::Closed => {
                                DeferredReplyRejectedReason::RequesterShardClosed
                            }
                        };
                        record.shared.set_state(DeferredSlotState::Closed);
                        self.push_event(
                            isolate_id,
                            Some(cause),
                            RuntimeEventKind::DeferredReplyRejected {
                                slot_id: record.slot_id,
                                call_id: record.call_id,
                                reason: reject_reason,
                            },
                        );
                    }
                }
            }
        }
    }

    pub(crate) fn dispatch_call(
        &mut self,
        call: ErasedCall,
        requester: RegisteredAddress,
        cause: CauseId,
        continuation_context: Option<MessageCallContext>,
        route_remote: &mut impl FnMut(ShardId, QueuedRemoteEnvelope) -> Result<(), SendRejectedReason>,
    ) {
        let call_id = self.ids.next_call_id();
        let call_kind = match &call.kind {
            call::ErasedCallKind::Backend { request, .. } => request.kind(),
            call::ErasedCallKind::ObservedSend { .. } => CallKind::ObservedSend,
            call::ErasedCallKind::IsolateCall { .. } => CallKind::IsolateCall,
            call::ErasedCallKind::CancelCall { .. } => CallKind::CancelCall,
        };

        let attempted = self.push_event(
            requester.isolate,
            Some(cause),
            RuntimeEventKind::CallDispatchAttempted { call_id, call_kind },
        );
        let dispatch_context = CallDispatchContext {
            call_id,
            requester,
            cause: attempted.into(),
            continuation_context,
        };

        match call.kind {
            call::ErasedCallKind::Backend {
                request,
                translator,
            } => {
                self.dispatch_driver_call(dispatch_context, call_kind, request, translator);
            }
            call::ErasedCallKind::ObservedSend { send, translator } => {
                self.dispatch_observed_send(dispatch_context, send, translator, route_remote);
            }
            call::ErasedCallKind::IsolateCall {
                send,
                timeout,
                translator,
                expected_reply_type_id,
                handle_shared,
            } => {
                self.dispatch_isolate_call(
                    dispatch_context,
                    send,
                    timeout,
                    translator,
                    expected_reply_type_id,
                    handle_shared,
                    route_remote,
                );
            }
            call::ErasedCallKind::CancelCall {
                handle_shared,
                translator,
            } => {
                self.dispatch_cancel_call(dispatch_context, handle_shared, translator);
            }
        }
    }

    pub(crate) fn dispatch_driver_call(
        &mut self,
        context: CallDispatchContext,
        call_kind: CallKind,
        request: CallInput,
        translator: Box<dyn FnOnce(CallOutput) -> ErasedRuntimeCallCompletion>,
    ) {
        let persistence = request.persistence_trace_info();
        if persistence == Some(call::PersistenceTraceInfo::Recovery) {
            self.push_event(
                context.requester.isolate,
                Some(context.cause),
                RuntimeEventKind::RecoveryStarted,
            );
        }
        // Register the translator and in-flight tracking before submission
        // so a synchronous completion (bind / close on Betelgeuse) can be
        // delivered through the same path as async completions.
        self.call_table.insert_driver(DriverCall {
            head: DriverCallHead {
                call_id: context.call_id,
                call_kind,
                requester: context.requester,
                cause: context.cause,
                persistence,
                continuation_context: context.continuation_context,
            },
            translator,
        });

        if let Some(immediate) = self
            .driver
            .submit(context.call_id, request, self.clock.now())
        {
            self.deliver_completion(immediate.call_id, immediate.result);
        }

        // Driver cancelled some pending calls because their resource
        // closed. Drop matching runtime state, or `has_in_flight_calls`
        // stays true forever. Mirror `advance_driver`'s belt-and-braces
        // purge at its twin site: drop any carried completion for a call
        // this close just cancelled, so a later advance cannot deliver a
        // completion for a call whose entry is gone (which would trip
        // `deliver_completion`'s unknown-call quarantine). Safe without it
        // under the driver contract — a lane resolves each call once, so a
        // cancelled-by-close call is never also carried — but kept symmetric
        // with `advance_driver` so the invariant holds even if a future lane
        // breaks that rule.
        for cancelled in self.driver.take_cancelled_by_close() {
            self.pending_completions
                .retain(|op| op.call_id != cancelled);
            self.cancel_in_flight_call_for_resource_close(cancelled);
        }
    }

    /// Drops runtime state for a call cancelled by resource close.
    /// Translator is not run; caller's continuation does not fire.
    /// Trace records `ResourceClosed`.
    pub(crate) fn cancel_in_flight_call_for_resource_close(&mut self, call_id: CallId) {
        // Removing the entry drops its translator; the continuation never fires.
        let Some(in_flight) = self.call_table.remove_driver(call_id) else {
            return;
        };

        self.push_event(
            in_flight.head.requester.isolate,
            Some(in_flight.head.cause),
            RuntimeEventKind::CallCompletionRejected {
                call_id,
                call_kind: in_flight.head.call_kind,
                reason: CallCompletionRejectedReason::ResourceClosed,
            },
        );
    }

    pub(crate) fn dispatch_observed_send(
        &mut self,
        context: CallDispatchContext,
        send: ErasedSend,
        translator: Box<dyn FnOnce(SendOutcome) -> Box<dyn Any>>,
        route_remote: &mut impl FnMut(ShardId, QueuedRemoteEnvelope) -> Result<(), SendRejectedReason>,
    ) {
        let target_shard = send.target_shard;
        let target_isolate = send.target_isolate;
        let target_generation = send.target_generation;
        let send_attempted = self.push_event(
            context.requester.isolate,
            Some(context.cause),
            RuntimeEventKind::SendDispatchAttempted {
                target_shard,
                target_isolate,
                target_generation,
            },
        );

        let delivery = if target_shard == self.shard.id() {
            self.dispatch_local_send(send)
        } else {
            route_remote(
                self.shard.id(),
                QueuedRemoteEnvelope::Send(QueuedRemoteSend {
                    send,
                    call_context: None,
                    cause: send_attempted.into(),
                }),
            )
        };

        let outcome = match delivery {
            Ok(()) => {
                self.push_event(
                    context.requester.isolate,
                    Some(send_attempted.into()),
                    RuntimeEventKind::SendAccepted {
                        target_shard,
                        target_isolate,
                        target_generation,
                    },
                );
                SendOutcome::Accepted
            }
            Err(reason) => {
                self.push_event(
                    context.requester.isolate,
                    Some(send_attempted.into()),
                    RuntimeEventKind::SendRejected {
                        target_shard,
                        target_isolate,
                        target_generation,
                        reason,
                    },
                );
                SendOutcome::from_rejected(reason)
            }
        };

        self.deliver_observed_send_outcome(
            context.call_id,
            context.requester,
            context.cause,
            outcome,
            translator,
            context.continuation_context,
        );
    }

    pub(crate) fn deliver_observed_send_outcome(
        &mut self,
        call_id: CallId,
        requester: RegisteredAddress,
        cause: CauseId,
        outcome: SendOutcome,
        translator: Box<dyn FnOnce(SendOutcome) -> Box<dyn Any>>,
        continuation_context: Option<MessageCallContext>,
    ) {
        let call_kind = CallKind::ObservedSend;
        let message = translator(outcome);

        let entry_index = self.entry_index(requester);
        let Some(entry_index) = entry_index else {
            self.push_event(
                requester.isolate,
                Some(cause),
                RuntimeEventKind::CallCompletionRejected {
                    call_id,
                    call_kind,
                    reason: CallCompletionRejectedReason::RequesterClosed,
                },
            );
            return;
        };

        if self.entries[entry_index].stopped.get() {
            self.push_event(
                requester.isolate,
                Some(cause),
                RuntimeEventKind::CallCompletionRejected {
                    call_id,
                    call_kind,
                    reason: CallCompletionRejectedReason::RequesterClosed,
                },
            );
            return;
        }

        match self.enqueue_entry_message(entry_index, message, continuation_context) {
            Ok(()) => {
                self.push_event(
                    requester.isolate,
                    Some(cause),
                    RuntimeEventKind::CallCompleted { call_id, call_kind },
                );
            }
            Err(TrySendError::Full(_)) => {
                self.push_event(
                    requester.isolate,
                    Some(cause),
                    RuntimeEventKind::CallCompletionRejected {
                        call_id,
                        call_kind,
                        reason: CallCompletionRejectedReason::MailboxFull,
                    },
                );
            }
            Err(TrySendError::Closed(_)) => {
                self.push_event(
                    requester.isolate,
                    Some(cause),
                    RuntimeEventKind::CallCompletionRejected {
                        call_id,
                        call_kind,
                        reason: CallCompletionRejectedReason::RequesterClosed,
                    },
                );
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn dispatch_isolate_call(
        &mut self,
        context: CallDispatchContext,
        send: ErasedSend,
        timeout: Duration,
        translator: ErasedIsolateCallTranslator,
        expected_reply_type_id: std::any::TypeId,
        handle_shared: Option<std::sync::Arc<tina::CallHandleShared>>,
        route_remote: &mut impl FnMut(ShardId, QueuedRemoteEnvelope) -> Result<(), SendRejectedReason>,
    ) {
        let target_shard = send.target_shard;
        let target_isolate = send.target_isolate;
        let target_generation = send.target_generation;
        let send_attempted = self.push_event(
            context.requester.isolate,
            Some(context.cause),
            RuntimeEventKind::SendDispatchAttempted {
                target_shard,
                target_isolate,
                target_generation,
            },
        );

        let call_context = if target_shard == self.shard.id() {
            MessageCallContext::Local {
                call_id: context.call_id,
            }
        } else {
            MessageCallContext::Remote {
                call_id: context.call_id,
                requester: context.requester,
                cause: context.cause,
                expected_reply_type_id,
            }
        };

        let delivery = if target_shard == self.shard.id() {
            self.dispatch_local_send_with_context(send, Some(call_context))
        } else {
            route_remote(
                self.shard.id(),
                QueuedRemoteEnvelope::Send(QueuedRemoteSend {
                    send,
                    call_context: Some(call_context),
                    cause: send_attempted.into(),
                }),
            )
        };

        match delivery {
            Ok(()) => {
                self.push_event(
                    context.requester.isolate,
                    Some(send_attempted.into()),
                    RuntimeEventKind::SendAccepted {
                        target_shard,
                        target_isolate,
                        target_generation,
                    },
                );
                let insertion_order = self.next_isolate_call_ordinal;
                self.next_isolate_call_ordinal += 1;
                if let Some(shared) = &handle_shared {
                    shared.set_call_id(context.call_id.get());
                    shared.set_shard_id(self.shard.id().get());
                }
                self.call_table.insert_isolate(PendingIsolateCall {
                    call_id: context.call_id,
                    requester: context.requester,
                    cause: context.cause,
                    deadline: tina::Deadline::from_instant(self.clock.now(), timeout).instant(),
                    insertion_order,
                    continuation_context: context.continuation_context,
                    translator,
                    expected_reply_type_id,
                    handle_shared,
                });
            }
            Err(reason) => {
                self.push_event(
                    context.requester.isolate,
                    Some(send_attempted.into()),
                    RuntimeEventKind::SendRejected {
                        target_shard,
                        target_isolate,
                        target_generation,
                        reason,
                    },
                );
                let outcome = match reason {
                    SendRejectedReason::Full => CallOutcome::Full,
                    SendRejectedReason::Closed => CallOutcome::Closed,
                };
                if let Some(shared) = &handle_shared {
                    shared.set_call_id(context.call_id.get());
                    shared.set_shard_id(self.shard.id().get());
                    shared.set_state(tina::CallHandleState::Settled);
                }
                self.deliver_isolate_call_outcome(
                    context.call_id,
                    context.requester,
                    context.cause,
                    outcome,
                    translator,
                    context.continuation_context,
                );
            }
        }
    }

    pub(crate) fn dispatch_cancel_call(
        &mut self,
        context: CallDispatchContext,
        handle_shared: std::sync::Arc<tina::CallHandleShared>,
        translator: Box<dyn FnOnce(tina::CancelOutcome) -> Box<dyn Any>>,
    ) {
        // Cancel is single-writer on this shard. We still verify the
        // handle was minted on this shard; cross-shard handles would
        // miss the pending-table lookup and silently fall through to
        // `AlreadyCompleted` without us.
        let outcome = match handle_shared.state() {
            tina::CallHandleState::Settled => tina::CancelOutcome::AlreadyCompleted,
            tina::CallHandleState::Cancelled => tina::CancelOutcome::AlreadyCancelled,
            tina::CallHandleState::Pending => {
                match handle_shared.call_id() {
                    None => tina::CancelOutcome::NotAdmitted,
                    Some(raw_call_id) => {
                        let stamped_shard = handle_shared.shard_id().expect(
                            "shard_id is stamped together with call_id — call_id was Some but \
                             shard_id was None, which violates set_call_id / set_shard_id pairing.",
                        );
                        if stamped_shard != self.shard.id().get() {
                            // Cross-shard cancel: the pending-call entry lives
                            // on the originating shard. Reject with a typed
                            // outcome instead of silently no-op'ing into
                            // `AlreadyCompleted`.
                            tina::CancelOutcome::WrongShard
                        } else {
                            let call_id = CallId::new(raw_call_id);
                            match self.call_table.remove_isolate(call_id) {
                                Some(entry) => {
                                    // CallCancelled's trace cause chains back
                                    // to the original CallDispatchAttempted so
                                    // every CallDispatchAttempted has exactly
                                    // one settlement event downstream of it.
                                    // Dropping `entry` drops its translator; the
                                    // continuation never fires.
                                    let original_cause = entry.cause;
                                    handle_shared.set_state(tina::CallHandleState::Cancelled);
                                    self.record_cancelled_call(
                                        call_id,
                                        tina::CancelCause::CallerCancelled,
                                    );
                                    self.close_deferred_slot_for_call_with_reason(
                                        call_id,
                                        trace::DeferredReplyRejectedReason::CallerCancelled,
                                    );
                                    self.push_event(
                                        context.requester.isolate,
                                        Some(original_cause),
                                        RuntimeEventKind::CallCancelled {
                                            call_id,
                                            cause: tina::CancelCause::CallerCancelled,
                                        },
                                    );
                                    tina::CancelOutcome::Cancelled
                                }
                                None => tina::CancelOutcome::AlreadyCompleted,
                            }
                        }
                    }
                }
            }
        };

        let message_any = translator(outcome);
        let Some(entry_index) = self.entry_index(context.requester) else {
            self.push_event(
                context.requester.isolate,
                Some(context.cause),
                RuntimeEventKind::CallCompletionRejected {
                    call_id: context.call_id,
                    call_kind: trace::CallKind::CancelCall,
                    reason: trace::CallCompletionRejectedReason::RequesterClosed,
                },
            );
            return;
        };
        if self.entries[entry_index].stopped.get() {
            self.push_event(
                context.requester.isolate,
                Some(context.cause),
                RuntimeEventKind::CallCompletionRejected {
                    call_id: context.call_id,
                    call_kind: trace::CallKind::CancelCall,
                    reason: trace::CallCompletionRejectedReason::RequesterClosed,
                },
            );
            return;
        }
        match self.enqueue_call_continuation(entry_index, message_any, context.continuation_context)
        {
            Ok(delivery) => {
                if matches!(delivery, ContinuationDelivery::Overflow) {
                    self.push_event(
                        context.requester.isolate,
                        Some(context.cause),
                        RuntimeEventKind::CallContinuationOverflowed {
                            call_id: context.call_id,
                            call_kind: trace::CallKind::CancelCall,
                        },
                    );
                }
                self.push_event(
                    context.requester.isolate,
                    Some(context.cause),
                    RuntimeEventKind::CallCompleted {
                        call_id: context.call_id,
                        call_kind: trace::CallKind::CancelCall,
                    },
                );
            }
            Err(TrySendError::Closed(_)) => {
                self.push_event(
                    context.requester.isolate,
                    Some(context.cause),
                    RuntimeEventKind::CallCompletionRejected {
                        call_id: context.call_id,
                        call_kind: trace::CallKind::CancelCall,
                        reason: trace::CallCompletionRejectedReason::RequesterClosed,
                    },
                );
            }
            Err(TrySendError::Full(_)) => unreachable!(
                "enqueue_call_continuation converts full mailboxes into continuation overflow"
            ),
        }
    }

    pub(crate) fn harvest_isolate_call_timeouts(&mut self, now: Instant) {
        while let Some(call_id) = self.call_table.next_due_isolate(now) {
            let Some(entry) = self.call_table.remove_isolate(call_id) else {
                // Deadline index and entry map are updated together; a due id
                // with no entry cannot occur, but skip defensively.
                continue;
            };
            let translator = entry.translator;
            // Timeout shares cancel's cleanup path; cause stays
            // distinct in the trace via `CallFailed { Timeout }` vs
            // `CallCancelled { CallerCancelled }`. Late callee replies
            // for this call_id surface as `CallerTimedOut` rejection
            // reasons (recorded in the ring with that cause) instead
            // of the generic `NoPendingCall` / `CallerClosed`.
            if let Some(shared) = &entry.handle_shared {
                shared.set_state(tina::CallHandleState::Settled);
            }
            self.record_cancelled_call(entry.call_id, tina::CancelCause::CallerTimedOut);
            self.close_deferred_slot_for_call_with_reason(
                entry.call_id,
                trace::DeferredReplyRejectedReason::CallerTimedOut,
            );
            self.deliver_isolate_call_outcome(
                entry.call_id,
                entry.requester,
                entry.cause,
                CallOutcome::Timeout,
                translator,
                entry.continuation_context,
            );
        }
    }

    /// Cancels every pending isolate call whose caller is the stopping
    /// isolate. Emits `CallCancelled { OwnerStopped }`, marks each
    /// shared cell `Cancelled`, and closes any captured deferred slot
    /// with `CallerCancelled`. The callee may still finish; its later
    /// reply hits the same rejection path as explicit cancel.
    pub(crate) fn cancel_pending_isolate_calls_for_owner(
        &mut self,
        owner_isolate: IsolateId,
        owner_generation: AddressGeneration,
        _stopped_cause: CauseId,
    ) {
        // Remove every owned call in ascending call-id (== insertion) order,
        // then record all cancellations before emitting events, so ring-eviction
        // and trace order stay deterministic and match the simulator.
        let owned = self
            .call_table
            .take_isolate_calls_for_owner(owner_isolate, owner_generation);
        for entry in owned.iter() {
            self.record_cancelled_call(entry.call_id, tina::CancelCause::OwnerStopped);
        }
        for entry in owned {
            let original_cause = entry.cause;
            // Dropping `entry` drops its translator; the continuation never fires.
            if let Some(shared) = &entry.handle_shared {
                shared.set_state(tina::CallHandleState::Cancelled);
            }
            // Owner stopped: distinct from `CallerCancelled` in the
            // late-reply rejection reason as well as the settlement
            // event.
            self.close_deferred_slot_for_call_with_reason(
                entry.call_id,
                trace::DeferredReplyRejectedReason::OwnerStopped,
            );
            self.push_event(
                owner_isolate,
                Some(original_cause),
                RuntimeEventKind::CallCancelled {
                    call_id: entry.call_id,
                    cause: tina::CancelCause::OwnerStopped,
                },
            );
        }
    }

    /// Records `call_id` in the bounded recently-cancelled ring with
    /// the cause that closed it. Late replies for these surface as a
    /// rejection reason that mirrors the cause, instead of the
    /// generic `NoPendingCall` / `CallerClosed`.
    ///
    /// The ring is bounded at `CANCELLED_CALL_RING_CAPACITY` by design: late
    /// cause attribution is best-effort. Beyond capacity, the oldest entry is
    /// evicted and a later reply for it degrades to the generic reason. Each
    /// eviction increments `cancelled_call_cause_evictions`, exposed through
    /// [`crate::Runtime::cancelled_call_cause_evictions`], so the degradation is
    /// visible without adding an unbounded late-reply table.
    pub(crate) fn record_cancelled_call(&mut self, call_id: CallId, cause: tina::CancelCause) {
        if self.cancelled_calls.len() == CANCELLED_CALL_RING_CAPACITY {
            self.cancelled_calls.pop_front();
            self.cancelled_call_cause_evictions =
                self.cancelled_call_cause_evictions.saturating_add(1);
        }
        self.cancelled_calls.push_back((call_id, cause));
    }

    pub(crate) fn recently_cancelled_cause(&self, call_id: CallId) -> Option<tina::CancelCause> {
        self.cancelled_calls
            .iter()
            .find_map(|(id, cause)| (*id == call_id).then_some(*cause))
    }

    pub(crate) fn close_deferred_slot_for_call_with_reason(
        &mut self,
        call_id: CallId,
        reason: DeferredReplyRejectedReason,
    ) {
        // Local-only: caller-liveness sweep here is driven by this
        // shard's pending isolate calls. First form refuses cross-shard
        // captures so every promoted slot is local — see
        // `PromotedSlots::take_by_local_call_id`.
        if let Some(record) = self.promoted_slots.take_by_local_call_id(call_id) {
            record.shared.set_state(DeferredSlotState::Closed);
            self.push_event(
                record.capturing_isolate,
                None,
                RuntimeEventKind::DeferredReplyRejected {
                    slot_id: record.slot_id,
                    call_id: record.call_id,
                    reason,
                },
            );
        }
    }

    pub(crate) fn complete_isolate_call(
        &mut self,
        call_id: CallId,
        cause: CauseId,
        outcome: CallOutcome<Box<dyn Any>>,
    ) -> bool {
        let Some(pending) = self.call_table.remove_isolate(call_id) else {
            return false;
        };
        if let Some(shared) = &pending.handle_shared {
            shared.set_state(tina::CallHandleState::Settled);
        }
        self.deliver_isolate_call_outcome(
            call_id,
            pending.requester,
            cause,
            outcome,
            pending.translator,
            pending.continuation_context,
        );
        true
    }

    pub(crate) fn deliver_isolate_call_outcome(
        &mut self,
        call_id: CallId,
        requester: RegisteredAddress,
        cause: CauseId,
        outcome: CallOutcome<Box<dyn Any>>,
        translator: ErasedIsolateCallTranslator,
        continuation_context: Option<MessageCallContext>,
    ) {
        let failure_reason = match &outcome {
            CallOutcome::Replied(_) => None,
            CallOutcome::Full => Some(CallError::TargetFull),
            CallOutcome::Closed => Some(CallError::TargetClosed),
            CallOutcome::Timeout => Some(CallError::Timeout),
            CallOutcome::Rejected(reason) => Some(CallError::Rejected(*reason)),
        };

        if let Some(reason) = failure_reason {
            self.push_event(
                requester.isolate,
                Some(cause),
                RuntimeEventKind::CallFailed {
                    call_id,
                    call_kind: CallKind::IsolateCall,
                    reason,
                },
            );
        }

        let message = translator(outcome);
        let Some(entry_index) = self.entry_index(requester) else {
            self.push_event(
                requester.isolate,
                Some(cause),
                RuntimeEventKind::CallCompletionRejected {
                    call_id,
                    call_kind: CallKind::IsolateCall,
                    reason: CallCompletionRejectedReason::RequesterClosed,
                },
            );
            return;
        };

        if self.entries[entry_index].stopped.get() {
            self.push_event(
                requester.isolate,
                Some(cause),
                RuntimeEventKind::CallCompletionRejected {
                    call_id,
                    call_kind: CallKind::IsolateCall,
                    reason: CallCompletionRejectedReason::RequesterClosed,
                },
            );
            return;
        }

        match self.enqueue_entry_message(entry_index, message, continuation_context) {
            Ok(()) => {
                if failure_reason.is_none() {
                    self.push_event(
                        requester.isolate,
                        Some(cause),
                        RuntimeEventKind::CallCompleted {
                            call_id,
                            call_kind: CallKind::IsolateCall,
                        },
                    );
                }
            }
            Err(TrySendError::Full(_)) => {
                self.push_event(
                    requester.isolate,
                    Some(cause),
                    RuntimeEventKind::CallCompletionRejected {
                        call_id,
                        call_kind: CallKind::IsolateCall,
                        reason: CallCompletionRejectedReason::MailboxFull,
                    },
                );
            }
            Err(TrySendError::Closed(_)) => {
                self.push_event(
                    requester.isolate,
                    Some(cause),
                    RuntimeEventKind::CallCompletionRejected {
                        call_id,
                        call_kind: CallKind::IsolateCall,
                        reason: CallCompletionRejectedReason::RequesterClosed,
                    },
                );
            }
        }
    }

    /// True when the runtime owns work that can make progress on its own —
    /// pending lane I/O, a runtime timer/signal, or an isolate-call deadline —
    /// none of which signals a parked worker. The worker uses this to pick a
    /// short re-poll park when such work is pending versus a longer idle park
    /// when it is not. Does not include host mailbox messages, which a host
    /// `send`/`call` wakes the worker for through the command queue.
    pub(crate) fn has_pending_runtime_work(&self) -> bool {
        self.driver.has_pending()
            || self.call_table.has_isolate_deadlines()
            || !self.pending_completions.is_empty()
    }

    pub(crate) fn advance_driver(&mut self, now: Instant) {
        // Harvest every completion the driver has ready this advance, but
        // deliver at most `driver_completion_drain_budget` into mailboxes per
        // step. Carried completions (`pending_completions`) are delivered
        // first, in FIFO order, so completion order is deterministic across the
        // budget boundary and nothing is dropped — the remainder simply waits
        // for the next advance. `has_pending_runtime_work` reports a non-empty
        // carry-over, so the worker keeps stepping (does not park) until it
        // drains.
        let mut completed = std::mem::take(&mut self.driver_completions);
        completed.clear();
        self.driver.advance(now, &mut completed);
        self.pending_completions.extend(completed.drain(..));
        self.driver_completions = completed;

        let budget = self.driver_completion_drain_budget.max(1);
        let mut delivered = 0;
        while delivered < budget {
            let Some(op) = self.pending_completions.pop_front() else {
                break;
            };
            self.deliver_completion(op.call_id, op.result);
            delivered += 1;
        }

        // Some close-like operations complete during driver advancement
        // (for example terminal write-close) and cancel sibling pending
        // resource calls as they close. Drain those cancellations here just
        // like `dispatch_runtime_call` does for close calls that complete
        // inline during submit. A lane resolves each call exactly once
        // (close wins over a pending op), so a carried completion's call is
        // never also cancelled; the `retain` purge is a cheap belt-and-braces
        // guard that keeps `deliver_completion`'s unknown-call panic a true
        // invariant even if a future lane breaks that rule.
        for cancelled in self.driver.take_cancelled_by_close() {
            self.pending_completions
                .retain(|op| op.call_id != cancelled);
            self.cancel_in_flight_call_for_resource_close(cancelled);
        }
    }

    pub(crate) fn deliver_completion(&mut self, call_id: CallId, result: CallOutput) {
        // Driver-sourced inconsistency: a completion for a call the table no
        // longer tracks (already settled, cancelled, or never admitted — a
        // driver accounting bug). Quarantine it: trace the event, drop the
        // result, keep the shard alive. A buggy driver must not kill unrelated
        // isolates. Attributed to the shard sentinel isolate with no cause.
        let Some(DriverCall { head, translator }) = self.call_table.remove_driver(call_id) else {
            self.push_event(
                IsolateId::new(0),
                None,
                RuntimeEventKind::DriverCompletionQuarantined { call_id },
            );
            return;
        };

        // Trace semantics: `CallFailed` records that the runtime
        // observed a failure result for this call. `CallCompleted`
        // records that a *successful* result's translated message
        // reached the requester's mailbox. `CallCompletionRejected`
        // records that the translator's message could not reach the
        // mailbox (regardless of whether the underlying result was a
        // success or a failure). A failed call therefore emits at most
        // `CallFailed` plus, if delivery also fails, one
        // `CallCompletionRejected` — never `CallCompleted`.
        let failure_reason = call_output_failure_reason(&result);
        if let Some(reason) = failure_reason {
            self.push_event(
                head.requester.isolate,
                Some(head.cause),
                RuntimeEventKind::CallFailed {
                    call_id,
                    call_kind: head.call_kind,
                    reason,
                },
            );
        }
        self.push_persistence_completion_events(head, &result, failure_reason);

        if matches!(head.call_kind, CallKind::TcpBind) {
            match (&result, failure_reason) {
                (CallOutput::TcpBound { local_addr, .. }, _) => {
                    self.observation
                        .notify_bound(observation::BoundAddressOutcome::Bound(*local_addr));
                }
                (_, Some(reason)) => {
                    self.observation
                        .notify_bound(observation::BoundAddressOutcome::Failed(reason));
                }
                _ => {}
            }
        }
        if matches!(head.call_kind, CallKind::TlsBind) {
            match (&result, failure_reason) {
                (CallOutput::TlsBound { local_addr, .. }, _) => {
                    self.observation
                        .notify_tls_bound(observation::BoundAddressOutcome::Bound(*local_addr));
                }
                (_, Some(reason)) => {
                    self.observation
                        .notify_tls_bound(observation::BoundAddressOutcome::Failed(reason));
                }
                _ => {}
            }
        }

        match failure_reason {
            None => self.observation.notify_operation_completed(
                head.requester.isolate,
                head.call_kind,
                call_id,
            ),
            Some(error) => self.observation.notify_operation_failed(
                head.requester.isolate,
                head.call_kind,
                call_id,
                error,
            ),
        }

        let completion = translator(result);
        if failure_reason.is_some()
            && !matches!(completion, ErasedRuntimeCallCompletion::Message(_))
        {
            self.push_event(
                head.requester.isolate,
                Some(head.cause),
                RuntimeEventKind::CallCompletionRejected {
                    call_id,
                    call_kind: head.call_kind,
                    reason: CallCompletionRejectedReason::TerminalActionOnFailure,
                },
            );
            return;
        }

        let entry_index = self.entry_index(head.requester);
        let Some(entry_index) = entry_index else {
            self.push_event(
                head.requester.isolate,
                Some(head.cause),
                RuntimeEventKind::CallCompletionRejected {
                    call_id,
                    call_kind: head.call_kind,
                    reason: CallCompletionRejectedReason::RequesterClosed,
                },
            );
            return;
        };

        if self.entries[entry_index].stopped.get() {
            self.push_event(
                head.requester.isolate,
                Some(head.cause),
                RuntimeEventKind::CallCompletionRejected {
                    call_id,
                    call_kind: head.call_kind,
                    reason: CallCompletionRejectedReason::RequesterClosed,
                },
            );
            return;
        }

        self.deliver_backend_completion_action(
            entry_index,
            head,
            call_id,
            completion,
            failure_reason,
        );
    }

    fn deliver_backend_completion_action(
        &mut self,
        entry_index: usize,
        head: DriverCallHead,
        call_id: CallId,
        completion: ErasedRuntimeCallCompletion,
        failure_reason: Option<CallError>,
    ) {
        match completion {
            ErasedRuntimeCallCompletion::Message(message) => {
                // A runtime-call continuation keeps a held resource alive (a
                // bridge poll loop, a read/write loop). It must never be
                // dropped on a full mailbox, or the slot leaks forever. Deliver
                // it through the non-droppable continuation path: mailbox first,
                // priority overflow on Full.
                match self.enqueue_call_continuation(
                    entry_index,
                    message,
                    head.continuation_context,
                ) {
                    Ok(delivery) => {
                        if matches!(delivery, ContinuationDelivery::Overflow) {
                            self.push_event(
                                head.requester.isolate,
                                Some(head.cause),
                                RuntimeEventKind::CallContinuationOverflowed {
                                    call_id,
                                    call_kind: head.call_kind,
                                },
                            );
                        }
                        if failure_reason.is_none() {
                            self.push_event(
                                head.requester.isolate,
                                Some(head.cause),
                                RuntimeEventKind::CallCompleted {
                                    call_id,
                                    call_kind: head.call_kind,
                                },
                            );
                        }
                        // For failed results we already emitted `CallFailed`
                        // above; the translator's message reaching the isolate
                        // is the expected behavior and does not need a second
                        // event.
                    }
                    Err(_closed) => {
                        // Only a gone requester reaches here; overflow absorbs
                        // a full mailbox.
                        self.push_event(
                            head.requester.isolate,
                            Some(head.cause),
                            RuntimeEventKind::CallCompletionRejected {
                                call_id,
                                call_kind: head.call_kind,
                                reason: CallCompletionRejectedReason::RequesterClosed,
                            },
                        );
                    }
                }
            }
            ErasedRuntimeCallCompletion::Noop => {
                self.push_event(
                    head.requester.isolate,
                    Some(head.cause),
                    RuntimeEventKind::CallCompleted {
                        call_id,
                        call_kind: head.call_kind,
                    },
                );
                self.push_event(
                    head.requester.isolate,
                    Some(head.cause),
                    RuntimeEventKind::CallCompletionAction {
                        call_id,
                        call_kind: head.call_kind,
                        action: TerminalCompletionAction::Noop,
                    },
                );
            }
            ErasedRuntimeCallCompletion::StopRequester => {
                self.push_event(
                    head.requester.isolate,
                    Some(head.cause),
                    RuntimeEventKind::CallCompleted {
                        call_id,
                        call_kind: head.call_kind,
                    },
                );
                let action = self.push_event(
                    head.requester.isolate,
                    Some(head.cause),
                    RuntimeEventKind::CallCompletionAction {
                        call_id,
                        call_kind: head.call_kind,
                        action: TerminalCompletionAction::StopRequester,
                    },
                );
                self.stop_entry(entry_index, head.requester.isolate, action.into());
            }
        }
    }

    pub(crate) fn push_persistence_completion_events(
        &mut self,
        head: DriverCallHead,
        result: &CallOutput,
        failure_reason: Option<CallError>,
    ) {
        let Some(persistence) = head.persistence else {
            return;
        };
        match (persistence, failure_reason, result) {
            (call::PersistenceTraceInfo::SnapshotCommit, None, _) => {
                self.push_event(
                    head.requester.isolate,
                    Some(head.cause),
                    RuntimeEventKind::SnapshotCommitted,
                );
            }
            (call::PersistenceTraceInfo::SnapshotCommit, Some(reason), _) => {
                self.push_event(
                    head.requester.isolate,
                    Some(head.cause),
                    RuntimeEventKind::SnapshotCommitFailed { reason },
                );
            }
            (call::PersistenceTraceInfo::JournalAppend { record_index }, None, _) => {
                self.push_event(
                    head.requester.isolate,
                    Some(head.cause),
                    RuntimeEventKind::JournalAppended { record_index },
                );
            }
            (call::PersistenceTraceInfo::JournalAppend { record_index }, Some(reason), _) => {
                self.push_event(
                    head.requester.isolate,
                    Some(head.cause),
                    RuntimeEventKind::JournalAppendFailed {
                        record_index,
                        reason,
                    },
                );
            }
            (call::PersistenceTraceInfo::Recovery, None, _) => {
                self.push_event(
                    head.requester.isolate,
                    Some(head.cause),
                    RuntimeEventKind::RecoveryFinished,
                );
            }
            (call::PersistenceTraceInfo::Recovery, Some(reason), _) => {
                self.push_event(
                    head.requester.isolate,
                    Some(head.cause),
                    RuntimeEventKind::RecoveryFailed { reason },
                );
            }
        }
    }

    pub(crate) fn stop_entry(
        &mut self,
        index: usize,
        isolate_id: IsolateId,
        cause: CauseId,
    ) -> EventId {
        self.stop_entry_full(index, isolate_id, cause, None, None)
    }

    pub(crate) fn stop_entry_with_precollected(
        &mut self,
        index: usize,
        isolate_id: IsolateId,
        cause: CauseId,
        precollected: Option<DeliveredMessage>,
    ) -> EventId {
        self.stop_entry_full(index, isolate_id, cause, precollected, None)
    }

    pub(crate) fn stop_entry_with_result(
        &mut self,
        index: usize,
        isolate_id: IsolateId,
        cause: CauseId,
        result: StopResult,
    ) -> EventId {
        self.stop_entry_full(index, isolate_id, cause, None, Some(result))
    }

    pub(crate) fn stop_entry_full(
        &mut self,
        index: usize,
        isolate_id: IsolateId,
        cause: CauseId,
        precollected: Option<DeliveredMessage>,
        result: Option<StopResult>,
    ) -> EventId {
        if self.entries[index].stopped.get() {
            let stopped = self.entries[index]
                .stopped_event
                .get()
                .unwrap_or_else(|| panic!("stopped isolate has no stopped event"));
            if let Some(message) = precollected
                && !self.close_drained_local_call_context(stopped.into(), message)
            {
                self.push_event(
                    isolate_id,
                    Some(stopped.into()),
                    RuntimeEventKind::MessageAbandoned,
                );
            }
            // Late StopWith: isolate already stopped, drop the value.
            drop(result);
            return stopped;
        }

        self.entries[index].stopped.set(true);
        self.has_stopped_entries = true;
        self.entries[index].mailbox.close();
        let stopped = self.push_event(isolate_id, Some(cause), RuntimeEventKind::IsolateStopped);
        self.entries[index].stopped_event.set(Some(stopped));
        let generation = self.entries[index].generation;
        self.observation
            .notify_isolate_stopped(isolate_id, generation);
        let address = RegisteredAddress {
            shard: self.shard.id(),
            isolate: isolate_id,
            generation,
        };
        self.prune_terminal_child_records(address);

        // Drain any deferred reply slots this isolate captured. The
        // isolate's state (and its DeferredReply Rcs) is not freed
        // until the entry record is dropped, so sweep would not
        // notice. Walk the registry directly.
        for record in self.promoted_slots.take_by_isolate(isolate_id) {
            self.drop_promoted_deferred_slot(record, Some(stopped.into()));
        }
        self.cancel_driver_calls_for_requester(RegisteredAddress {
            shard: self.shard.id(),
            isolate: isolate_id,
            generation: self.entries[index].generation,
        });
        self.cancel_pending_isolate_calls_for_owner(
            isolate_id,
            self.entries[index].generation,
            stopped.into(),
        );
        if let Some(message) = precollected {
            if !self.close_drained_local_call_context(stopped.into(), message) {
                self.push_event(
                    isolate_id,
                    Some(stopped.into()),
                    RuntimeEventKind::MessageAbandoned,
                );
            }
        }
        while let Some(message) = self.recv_entry_message(index) {
            if !self.close_drained_local_call_context(stopped.into(), message) {
                self.push_event(
                    isolate_id,
                    Some(stopped.into()),
                    RuntimeEventKind::MessageAbandoned,
                );
            }
        }
        // Result delivery happens last so the host only wakes after every
        // lifecycle/trace fact is recorded. With no value, drain any
        // pending result waiter as `StoppedWithoutResult`.
        match result {
            Some(value) => self
                .observation
                .notify_isolate_result(isolate_id, generation, value),
            None => self
                .observation
                .notify_isolate_stopped_without_result(isolate_id, generation),
        }
        stopped
    }

    fn prune_terminal_child_records(&mut self, stopped: RegisteredAddress) {
        let supervised_parents: Vec<_> = self
            .supervisors
            .iter()
            .map(|record| record.parent.isolate)
            .collect();
        for record in self
            .child_records
            .iter_mut()
            .filter(|record| record.child == stopped)
        {
            record.terminal = true;
            if record.restart_recipe.is_none()
                && !record.remote_restartable
                && !supervised_parents.contains(&record.parent)
            {
                record.remote_request_id = None;
            }
        }
        self.child_records.retain(|record| {
            record.child != stopped
                || record.restart_recipe.is_some()
                || record.remote_restartable
                || supervised_parents.contains(&record.parent)
        });
    }

    fn close_drained_local_call_context(
        &mut self,
        cause: CauseId,
        message: DeliveredMessage,
    ) -> bool {
        let Some(MessageCallContext::Local { call_id }) = message.call_context else {
            return false;
        };
        self.complete_isolate_call(call_id, cause, CallOutcome::Closed)
    }

    pub(crate) fn restart_children(
        &mut self,
        parent: IsolateId,
        cause: CauseId,
        round_messages: &mut [Option<DeliveredMessage>],
        route_remote: &mut impl FnMut(ShardId, QueuedRemoteEnvelope) -> Result<(), SendRejectedReason>,
    ) {
        for child_record_index in 0..self.child_records.len() {
            if self.child_records[child_record_index].parent == parent
                && self.child_records[child_record_index]
                    .remote_owner
                    .is_none()
            {
                if self.child_records[child_record_index].child.shard != self.shard.id() {
                    let child = self.child_records[child_record_index].child;
                    let child_ordinal = self.child_records[child_record_index].child_ordinal;
                    if self.child_records[child_record_index].remote_restartable {
                        self.request_remote_child_restart(
                            parent,
                            child,
                            child_ordinal,
                            cause,
                            route_remote,
                        );
                        continue;
                    }
                    self.push_event(
                        parent,
                        Some(cause),
                        RuntimeEventKind::RestartChildSkipped {
                            child_ordinal,
                            old_isolate: child.isolate,
                            old_generation: child.generation,
                            reason: RestartSkippedReason::RemoteNotRestartable,
                        },
                    );
                    let _ = route_remote;
                    continue;
                }
                self.restart_child_record(parent, child_record_index, cause, round_messages);
            }
        }
    }

    pub(crate) fn request_remote_child_restart(
        &mut self,
        parent: IsolateId,
        child: RegisteredAddress,
        child_ordinal: usize,
        cause: CauseId,
        route_remote: &mut impl FnMut(ShardId, QueuedRemoteEnvelope) -> Result<(), SendRejectedReason>,
    ) {
        let Some(parent_generation) = self.entry_by_isolate(parent).map(|entry| entry.generation)
        else {
            return;
        };
        let owner = RegisteredAddress {
            shard: self.shard.id(),
            isolate: parent,
            generation: parent_generation,
        };
        let attempted = self.push_event(
            parent,
            Some(cause),
            RuntimeEventKind::RestartChildAttempted {
                child_ordinal,
                old_isolate: child.isolate,
                old_generation: child.generation,
            },
        );
        if let Err(reason) = route_remote(
            self.shard.id(),
            QueuedRemoteEnvelope::ChildRestart(RemoteChildRestart {
                owner,
                child,
                child_ordinal,
                cause: attempted.into(),
            }),
        ) {
            self.push_event(
                parent,
                Some(attempted.into()),
                RuntimeEventKind::RemoteChildControlRejected {
                    target_shard: child.shard,
                    reason,
                },
            );
        }
    }

    /// Stops every live child owned by `parent` (explicit supervised
    /// shutdown). Each child stops through the normal path, so its callers
    /// settle and its pending work is cancelled; a `ChildStopped` fact names
    /// it under the parent. The parent is not touched.
    pub(crate) fn stop_children(
        &mut self,
        parent: IsolateId,
        cause: CauseId,
        round_messages: &mut [Option<DeliveredMessage>],
        route_remote: &mut impl FnMut(ShardId, QueuedRemoteEnvelope) -> Result<(), SendRejectedReason>,
    ) {
        // Snapshot first: stopping a child mutates `entries` and the GC, which
        // would invalidate a borrow held across the loop.
        let children: Vec<(usize, RegisteredAddress)> = self
            .child_records
            .iter()
            .filter(|record| record.parent == parent && record.remote_owner.is_none())
            .map(|record| (record.child_ordinal, record.child))
            .collect();
        for (child_ordinal, child) in children {
            if child.shard != self.shard.id() {
                self.request_remote_child_stop(parent, child, child_ordinal, cause, route_remote);
                continue;
            }
            let Some(entry_index) = self.entry_index(child) else {
                continue;
            };
            if self.entries[entry_index].stopped.get() {
                continue;
            }
            let stopped = self.push_event(
                parent,
                Some(cause),
                RuntimeEventKind::ChildStopped {
                    child_ordinal,
                    child_isolate: child.isolate,
                    child_generation: child.generation,
                },
            );
            let precollected = round_messages.get_mut(entry_index).and_then(Option::take);
            self.stop_entry_with_precollected(
                entry_index,
                child.isolate,
                stopped.into(),
                precollected,
            );
        }
    }

    pub(crate) fn next_child_ordinal(&self, parent: IsolateId) -> usize {
        let records = self
            .child_records
            .iter()
            .filter(|record| record.parent == parent && record.remote_owner.is_none())
            .count();
        let pending = self
            .pending_remote_spawns
            .iter()
            .filter(|pending| pending.requester.isolate == parent)
            .count();
        records + pending
    }

    pub(crate) fn cleanup_remote_children_for_owner(
        &mut self,
        parent: IsolateId,
        cause: CauseId,
        route_remote: &mut impl FnMut(ShardId, QueuedRemoteEnvelope) -> Result<(), SendRejectedReason>,
    ) {
        let owner = RegisteredAddress {
            shard: self.shard.id(),
            isolate: parent,
            generation: self
                .entry_by_isolate(parent)
                .map(|entry| entry.generation)
                .unwrap_or_else(|| AddressGeneration::new(0)),
        };
        let pending: Vec<_> = self
            .pending_remote_spawns
            .iter()
            .filter(|pending| pending.requester.isolate == parent)
            .map(|pending| (pending.request_id, pending.target_shard))
            .collect();
        let mut cancelled = Vec::new();
        for (request_id, target_shard) in pending {
            let cancel = QueuedRemoteEnvelope::SpawnCancel(RemoteSpawnCancel {
                request_id,
                target_shard,
                owner,
                cause,
            });
            match route_remote(self.shard.id(), cancel) {
                Ok(()) => cancelled.push(request_id),
                Err(reason) => {
                    self.push_event(
                        parent,
                        Some(cause),
                        RuntimeEventKind::RemoteChildControlRejected {
                            target_shard,
                            reason,
                        },
                    );
                }
            }
        }
        self.pending_remote_spawns.retain(|pending| {
            pending.requester.isolate != parent || !cancelled.contains(&pending.request_id)
        });

        let children: Vec<_> = self
            .child_records
            .iter()
            .filter(|record| {
                record.parent == parent
                    && record.remote_owner.is_none()
                    && record.child.shard != self.shard.id()
            })
            .map(|record| (record.child_ordinal, record.child))
            .collect();
        for (child_ordinal, child) in children {
            self.request_remote_child_stop(parent, child, child_ordinal, cause, route_remote);
        }
    }

    pub(crate) fn request_remote_child_stop(
        &mut self,
        parent: IsolateId,
        child: RegisteredAddress,
        child_ordinal: usize,
        cause: CauseId,
        route_remote: &mut impl FnMut(ShardId, QueuedRemoteEnvelope) -> Result<(), SendRejectedReason>,
    ) {
        let owner = RegisteredAddress {
            shard: self.shard.id(),
            isolate: parent,
            generation: self
                .entry_by_isolate(parent)
                .map(|entry| entry.generation)
                .unwrap_or_else(|| AddressGeneration::new(0)),
        };
        let requested = self.push_event(
            parent,
            Some(cause),
            RuntimeEventKind::RemoteChildStopRequested {
                child_shard: child.shard,
                child_ordinal,
                child_isolate: child.isolate,
                child_generation: child.generation,
            },
        );
        if let Err(reason) = route_remote(
            self.shard.id(),
            QueuedRemoteEnvelope::ChildStop(RemoteChildStop {
                owner,
                child,
                child_ordinal,
                cause: requested.into(),
            }),
        ) {
            self.push_event(
                parent,
                Some(requested.into()),
                RuntimeEventKind::RemoteChildControlRejected {
                    target_shard: child.shard,
                    reason,
                },
            );
        }
    }

    pub(crate) fn remember_remote_spawn_cancel(
        &mut self,
        request_id: CallId,
        isolate: IsolateId,
        cause: CauseId,
    ) {
        if self
            .remote_spawn_cancel_tombstones
            .iter()
            .any(|existing| *existing == request_id)
        {
            return;
        }
        if self.remote_spawn_cancel_tombstones.len() == self.remote_child_control_capacity {
            self.remote_spawn_cancel_tombstones.pop_front();
            self.remote_child_control_full = self.remote_child_control_full.saturating_add(1);
            self.push_event(
                isolate,
                Some(cause),
                RuntimeEventKind::RemoteChildControlPressure {
                    capacity: self.remote_child_control_capacity,
                },
            );
        }
        self.remote_spawn_cancel_tombstones.push_back(request_id);
    }

    pub(crate) fn stop_remote_owned_child(
        &mut self,
        owner: RegisteredAddress,
        child: RegisteredAddress,
        child_ordinal: usize,
        cause: CauseId,
    ) -> Option<QueuedRemoteEnvelope> {
        let current_child = self
            .child_records
            .iter()
            .find(|record| {
                record.remote_owner == Some(owner) && record.child_ordinal == child_ordinal
            })
            .map(|record| record.child)
            .unwrap_or(child);
        let Some(entry_index) = self.entry_index(current_child) else {
            return Some(QueuedRemoteEnvelope::ChildStopped(
                crate::remote::RemoteChildStopped {
                    owner,
                    child: current_child,
                    child_ordinal,
                    cause,
                },
            ));
        };
        if !self.entries[entry_index].stopped.get() {
            self.push_event(
                current_child.isolate,
                Some(cause),
                RuntimeEventKind::RemoteChildStopRequested {
                    child_shard: current_child.shard,
                    child_ordinal,
                    child_isolate: current_child.isolate,
                    child_generation: current_child.generation,
                },
            );
            self.stop_entry(entry_index, current_child.isolate, cause);
        }
        Some(QueuedRemoteEnvelope::ChildStopped(
            crate::remote::RemoteChildStopped {
                owner,
                child: current_child,
                child_ordinal,
                cause,
            },
        ))
    }

    pub(crate) fn restart_remote_owned_child(
        &mut self,
        owner: RegisteredAddress,
        child: RegisteredAddress,
        child_ordinal: usize,
        cause: CauseId,
    ) -> Option<QueuedRemoteEnvelope> {
        let Some(record_index) = self.child_records.iter().position(|record| {
            record.remote_owner == Some(owner)
                && record.child_ordinal == child_ordinal
                && record.child == child
        }) else {
            return Some(QueuedRemoteEnvelope::ChildRestarted(RemoteChildRestarted {
                owner,
                child_ordinal,
                old_child: child,
                outcome: Err(RestartSkippedReason::RemoteNotRestartable),
                cause,
            }));
        };
        let Some(recipe) = self.child_records[record_index].restart_recipe.clone() else {
            return Some(QueuedRemoteEnvelope::ChildRestarted(RemoteChildRestarted {
                owner,
                child_ordinal,
                old_child: child,
                outcome: Err(RestartSkippedReason::RemoteNotRestartable),
                cause,
            }));
        };

        if let Some(old_entry_index) = self.entry_index(child) {
            if !self.entries[old_entry_index].stopped.get() {
                self.stop_entry(old_entry_index, child.isolate, cause);
            }
        }

        let outcome = match catch_unwind(AssertUnwindSafe(|| {
            recipe.create_remote(self, owner, child_ordinal, cause)
        })) {
            Ok(Some(outcome)) => outcome,
            Ok(None) => {
                self.child_records[record_index].restart_recipe = Some(recipe);
                return Some(QueuedRemoteEnvelope::ChildRestarted(RemoteChildRestarted {
                    owner,
                    child_ordinal,
                    old_child: child,
                    outcome: Err(RestartSkippedReason::RemoteNotRestartable),
                    cause,
                }));
            }
            Err(_) => {
                self.child_records[record_index].restart_recipe = Some(recipe);
                return Some(QueuedRemoteEnvelope::ChildRestarted(RemoteChildRestarted {
                    owner,
                    child_ordinal,
                    old_child: child,
                    outcome: Err(RestartSkippedReason::FactoryPanicked),
                    cause,
                }));
            }
        };

        let new_child = outcome.child;
        let bootstrap_message = outcome.bootstrap_message;
        self.child_records[record_index].child = new_child;
        self.child_records[record_index].mailbox_capacity = outcome.mailbox_capacity;
        self.child_records[record_index].restart_recipe = Some(recipe);
        self.child_records[record_index].remote_request_id = None;
        self.child_records[record_index].remote_owner = Some(owner);
        self.child_records[record_index].terminal = false;

        if let Some(message) = bootstrap_message {
            self.enqueue_bootstrap_message(new_child, message, cause);
        }

        Some(QueuedRemoteEnvelope::ChildRestarted(RemoteChildRestarted {
            owner,
            child_ordinal,
            old_child: child,
            outcome: Ok(new_child),
            cause,
        }))
    }

    /// Delivers an observed-spawn continuation message to its owner (on this
    /// shard) through the traced local-send path, so a full or closed owner
    /// mailbox produces the usual `SendDispatchAttempted` / `SendAccepted` /
    /// `SendRejected` truth instead of a silent drop — matching ordinary
    /// `spawn_observed`.
    pub(crate) fn deliver_observed_continuation(
        &mut self,
        owner: RegisteredAddress,
        message: ErasedMessage,
        cause: CauseId,
    ) {
        let attempted = self.push_event(
            owner.isolate,
            Some(cause),
            RuntimeEventKind::SendDispatchAttempted {
                target_shard: owner.shard,
                target_isolate: owner.isolate,
                target_generation: owner.generation,
            },
        );
        let send = ErasedSend {
            target_shard: owner.shard,
            target_isolate: owner.isolate,
            target_generation: owner.generation,
            message,
        };
        match self.dispatch_local_send(send) {
            Ok(()) => {
                self.push_event(
                    owner.isolate,
                    Some(attempted.into()),
                    RuntimeEventKind::SendAccepted {
                        target_shard: owner.shard,
                        target_isolate: owner.isolate,
                        target_generation: owner.generation,
                    },
                );
            }
            Err(reason) => {
                self.push_event(
                    owner.isolate,
                    Some(attempted.into()),
                    RuntimeEventKind::SendRejected {
                        target_shard: owner.shard,
                        target_isolate: owner.isolate,
                        target_generation: owner.generation,
                        reason,
                    },
                );
            }
        }
    }

    pub(crate) fn supervise_failed_child(
        &mut self,
        failed_child: RegisteredAddress,
        cause: CauseId,
        round_messages: &mut [Option<DeliveredMessage>],
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
        let budget_state = match budget_state.record_restart_at(self.clock.now()) {
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

        for child_record_index in 0..self.child_records.len() {
            if self.child_records[child_record_index].parent != parent {
                continue;
            }

            let relation = ChildRelation::from_ordinals(
                self.child_records[child_record_index].child_ordinal,
                failed_ordinal,
            );
            if policy.restarts(relation) {
                self.restart_child_record(
                    parent,
                    child_record_index,
                    triggered.into(),
                    round_messages,
                );
            }
        }
    }

    pub(crate) fn restart_child_record(
        &mut self,
        parent: IsolateId,
        child_record_index: usize,
        cause: CauseId,
        round_messages: &mut [Option<DeliveredMessage>],
    ) {
        let Some(parent_generation) = self.entry_by_isolate(parent).map(|entry| entry.generation)
        else {
            return;
        };
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

        let outcome = match catch_unwind(AssertUnwindSafe(|| recipe.create(self, parent))) {
            Ok(outcome) => outcome,
            Err(_) => {
                self.child_records[child_record_index].restart_recipe = Some(recipe);
                self.push_event(
                    parent,
                    Some(attempted.into()),
                    RuntimeEventKind::RestartChildSkipped {
                        child_ordinal,
                        old_isolate: old_child.isolate,
                        old_generation: old_child.generation,
                        reason: RestartSkippedReason::FactoryPanicked,
                    },
                );
                return;
            }
        };
        let new_child = outcome.child;
        let bootstrap_message = outcome.bootstrap_message;
        self.child_records[child_record_index].child = new_child;
        self.child_records[child_record_index].mailbox_capacity = outcome.mailbox_capacity;
        self.child_records[child_record_index].terminal = false;
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
        // Notify *after* the bootstrap message has been enqueued so a host
        // that wakes from `wait()` cannot race a `try_send` ahead of the
        // bootstrap delivery.
        self.observation.notify_child_restarted(
            self.shard.id(),
            parent,
            parent_generation,
            observation::ChildRestarted {
                child_ordinal,
                new_shard: new_child.shard,
                new_isolate: new_child.isolate,
                new_generation: new_child.generation,
            },
        );
    }

    pub(crate) fn push_event(
        &mut self,
        isolate: IsolateId,
        cause: Option<CauseId>,
        kind: RuntimeEventKind,
    ) -> EventId {
        let id = self.ids.next_event_id();
        let event = RuntimeEvent::new(id, cause, self.shard.id(), isolate, kind);
        // Observer first. Retention::Off does not silence it.
        // A panic here kills the recording thread by design.
        if let Some(obs) = &self.trace_observer {
            obs.on_event(&event);
        }
        match self.trace_retention {
            TraceRetention::Full => {
                self.compact_trace_prefix();
                self.trace.push(event);
            }
            TraceRetention::Bounded(capacity) if capacity > 0 => {
                if self.active_trace_len() == capacity {
                    self.trace_start += 1;
                    self.trace_dropped += 1;
                    if self.trace_start >= capacity {
                        self.compact_trace_prefix();
                    }
                }
                self.trace.push(event);
            }
            TraceRetention::Bounded(_) | TraceRetention::Off => {
                self.trace_dropped += 1;
            }
        }
        id
    }

    pub(crate) fn enforce_trace_retention(&mut self) {
        match self.trace_retention {
            TraceRetention::Full => {
                self.compact_trace_prefix();
            }
            TraceRetention::Bounded(capacity) => {
                let active = self.active_trace_len();
                if active > capacity {
                    let excess = active - capacity;
                    self.trace_start += excess;
                    self.trace_dropped += excess as u64;
                }
                self.compact_trace_prefix_if_empty_or_large(capacity.max(1));
            }
            TraceRetention::Off => {
                self.trace_dropped += self.active_trace_len() as u64;
                self.trace.clear();
                self.trace_start = 0;
            }
        }
    }

    pub(crate) fn active_trace_len(&self) -> usize {
        self.trace.len().saturating_sub(self.trace_start)
    }

    pub(crate) fn compact_trace_prefix(&mut self) {
        if self.trace_start == 0 {
            return;
        }
        self.trace.drain(0..self.trace_start);
        self.trace_start = 0;
    }

    pub(crate) fn compact_trace_prefix_if_empty_or_large(&mut self, threshold: usize) {
        if self.trace_start == 0 {
            return;
        }
        if self.trace_start >= self.trace.len() || self.trace_start >= threshold {
            self.compact_trace_prefix();
        }
    }

    pub(crate) fn gc_stopped_entries(&mut self) {
        // Skip the whole scan while no isolate is stopped. The flag is set
        // when an entry stops and re-derived below, so steady-state live
        // shards pay nothing here.
        if !self.has_stopped_entries {
            return;
        }

        // One pass: swap_remove collectable entries (O(1) each) and track
        // whether any stopped-but-blocked entry remains. A burst compacts
        // in O(N), not O(N^2); rebuild the id->index map once at the end.
        let mut index = 0;
        let mut removed_any = false;
        let mut stopped_remaining = false;
        while index < self.entries.len() {
            if self.can_gc_stopped_entry(index) {
                self.entries.swap_remove(index);
                removed_any = true;
                // swap_remove moved the tail entry into `index`; re-check it.
            } else {
                if self.entries[index].stopped.get() {
                    stopped_remaining = true;
                }
                index += 1;
            }
        }
        self.has_stopped_entries = stopped_remaining;
        if removed_any {
            self.rebuild_entry_indexes();
        }
    }

    pub(crate) fn can_gc_stopped_entry(&self, index: usize) -> bool {
        let entry = &self.entries[index];
        if !entry.stopped.get() {
            return false;
        }
        let address = RegisteredAddress {
            shard: self.shard.id(),
            isolate: entry.id,
            generation: entry.generation,
        };
        if self.child_records.iter().any(|record| {
            (record.parent == entry.id && record.remote_owner.is_none()) || record.child == address
        }) {
            return false;
        }
        if self
            .supervisors
            .iter()
            .any(|record| record.parent == address)
        {
            return false;
        }
        if self.call_table.has_driver_call_for_requester(address) {
            return false;
        }
        if self.call_table.has_isolate_call_for_requester(address) {
            return false;
        }
        true
    }
}

pub(crate) trait ErasedMailbox {
    fn recv_boxed(&self) -> Option<Box<dyn Any>>;
    fn try_send_boxed(&self, message: Box<dyn Any>) -> Result<(), TrySendError<Box<dyn Any>>>;
    /// Cheap readiness probe; lets the scheduler skip `recv_boxed` on quiet
    /// isolates. Reflects real mailbox state for every ingress path.
    fn is_empty(&self) -> bool;
    fn close(&self);
}

pub(crate) struct MailboxAdapter<M, Msg>
where
    M: Mailbox<Msg>,
{
    pub(crate) mailbox: M,
    pub(crate) marker: PhantomData<fn(Msg) -> Msg>,
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

    fn is_empty(&self) -> bool {
        self.mailbox.is_empty()
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

pub(crate) struct AnyMailboxAdapter {
    pub(crate) mailbox: Box<dyn Mailbox<Box<dyn Any>>>,
}

impl ErasedMailbox for AnyMailboxAdapter {
    fn recv_boxed(&self) -> Option<Box<dyn Any>> {
        self.mailbox.recv()
    }

    fn is_empty(&self) -> bool {
        self.mailbox.is_empty()
    }

    fn try_send_boxed(&self, message: Box<dyn Any>) -> Result<(), TrySendError<Box<dyn Any>>> {
        self.mailbox.try_send(message)
    }

    fn close(&self) {
        self.mailbox.close();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RegisteredAddress {
    pub(crate) shard: ShardId,
    pub(crate) isolate: IsolateId,
    pub(crate) generation: AddressGeneration,
}

pub(crate) struct SpawnOutcome<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    pub(crate) child: RegisteredAddress,
    pub(crate) mailbox_capacity: usize,
    pub(crate) restart_recipe: Option<Rc<dyn ErasedRestartRecipe<S, F>>>,
    pub(crate) bootstrap_message: Option<Box<dyn Any>>,
}

pub(crate) struct SpawnObservedOutcome<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    pub(crate) spawn: Option<SpawnOutcome<S, F>>,
    pub(crate) continuation: Option<ErasedMessage>,
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct ChildRecord<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    pub(crate) parent: IsolateId,
    pub(crate) child: RegisteredAddress,
    pub(crate) child_ordinal: usize,
    pub(crate) mailbox_capacity: usize,
    pub(crate) restart_recipe: Option<Rc<dyn ErasedRestartRecipe<S, F>>>,
    pub(crate) remote_request_id: Option<CallId>,
    pub(crate) remote_owner: Option<RegisteredAddress>,
    pub(crate) remote_restartable: bool,
    pub(crate) terminal: bool,
}

pub(crate) struct SupervisorRecord {
    pub(crate) parent: RegisteredAddress,
    pub(crate) config: SupervisorConfig,
    pub(crate) budget_state: RestartBudgetState,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChildRecordSnapshot {
    pub(crate) parent: IsolateId,
    pub(crate) child_shard: ShardId,
    pub(crate) child_isolate: IsolateId,
    pub(crate) child_generation: AddressGeneration,
    pub(crate) child_ordinal: usize,
    pub(crate) mailbox_capacity: usize,
    pub(crate) restartable: bool,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SupervisorRecordSnapshot {
    pub(crate) parent: RegisteredAddress,
    pub(crate) config: SupervisorConfig,
    pub(crate) budget_state: RestartBudgetState,
}

pub(crate) trait ErasedHandler<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    fn handle_boxed(
        &mut self,
        message: Box<dyn Any>,
        shard: &mut S,
        isolate_id: IsolateId,
        generation: AddressGeneration,
        caller: Option<MessageCaller>,
        now: std::time::Instant,
    ) -> ErasedEffect<S, F>;

    fn handle_call_boxed(
        &mut self,
        message: Box<dyn Any>,
        shard: &mut S,
        isolate_id: IsolateId,
        generation: AddressGeneration,
        caller: MessageCaller,
        now: std::time::Instant,
    ) -> ErasedEffect<S, F>;
}

pub(crate) trait ErasedSpawn<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    fn spawn(self: Box<Self>, runtime: &mut Runtime<S, F>, parent: IsolateId)
    -> SpawnOutcome<S, F>;

    fn try_spawn_observed(
        self: Box<Self>,
        runtime: &mut Runtime<S, F>,
        parent: IsolateId,
    ) -> Result<SpawnOutcome<S, F>, SpawnObservedError> {
        Ok(self.spawn(runtime, parent))
    }
}

pub(crate) trait ErasedRestartRecipe<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    fn create(&self, runtime: &mut Runtime<S, F>, parent: IsolateId) -> SpawnOutcome<S, F>;

    fn create_remote(
        &self,
        _runtime: &mut Runtime<S, F>,
        _owner: RegisteredAddress,
        _child_ordinal: usize,
        _cause: CauseId,
    ) -> Option<SpawnOutcome<S, F>> {
        None
    }
}

pub(crate) trait IntoErasedSpawn<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    fn into_erased_spawn(self) -> Box<dyn ErasedSpawn<S, F>>;
}

pub(crate) trait ErasedSpawnObserved<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    fn spawn_observed(
        self: Box<Self>,
        runtime: &mut Runtime<S, F>,
        parent: IsolateId,
    ) -> SpawnObservedOutcome<S, F>;
}

pub(crate) trait IntoErasedSpawnObserved<S, F, ParentMessage>
where
    S: Shard,
    F: MailboxFactory,
{
    fn into_erased_spawn_observed(self) -> Box<dyn ErasedSpawnObserved<S, F>>;
}

pub(crate) struct HandlerAdapter<I, Outbound>
where
    I: Isolate,
{
    pub(crate) isolate: I,
    pub(crate) marker: PhantomData<fn(Outbound) -> Outbound>,
}

impl<I, S, F, Outbound> ErasedHandler<S, F> for HandlerAdapter<I, Outbound>
where
    I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
    I::Message: 'static,
    I::Reply: 'static,
    I::Spawn: IntoErasedSpawn<S, F> + 'static,
    I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
    I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
    I::Io: IntoErasedCall<I::Message> + 'static,
    I::Fact: IntoRuntimeFact + 'static,
    Outbound: 'static,
    S: Shard,
    F: MailboxFactory,
{
    #[allow(unsafe_code)]
    fn handle_boxed(
        &mut self,
        message: Box<dyn Any>,
        shard: &mut S,
        isolate_id: IsolateId,
        generation: AddressGeneration,
        caller: Option<MessageCaller>,
        now: std::time::Instant,
    ) -> ErasedEffect<S, F> {
        let message = message.downcast::<I::Message>().unwrap_or_else(|_| {
            panic!("runtime attempted to deliver a handler message with the wrong type")
        });

        let effect = {
            let mut ctx = Context::<_, I::Reply>::new_typed(shard, isolate_id)
                .with_current_generation(generation)
                .with_now(now);
            if let Some(caller) = caller {
                // SAFETY: dispatch allocated this caller for this delivery.
                ctx = unsafe { ctx.with_caller(caller) };
            }
            self.isolate.handle(*message, &mut ctx)
        };

        erase_effect::<I, S, F, Outbound>(effect)
    }

    #[allow(unsafe_code)]
    fn handle_call_boxed(
        &mut self,
        message: Box<dyn Any>,
        shard: &mut S,
        isolate_id: IsolateId,
        generation: AddressGeneration,
        caller: MessageCaller,
        now: std::time::Instant,
    ) -> ErasedEffect<S, F> {
        let message = message.downcast::<I::Message>().unwrap_or_else(|_| {
            panic!("runtime attempted to deliver a call handler message with the wrong type")
        });

        let effect = {
            let call = unsafe {
                // SAFETY: dispatch allocated this caller for this delivery.
                CallContext::new(
                    Context::<_, I::Reply>::new_typed(shard, isolate_id)
                        .with_current_generation(generation)
                        .with_now(now)
                        .with_caller(caller),
                )
            };
            self.isolate.handle_call(*message, call)
        };

        erase_effect::<I, S, F, Outbound>(effect)
    }
}

pub(crate) struct SendableHandlerAdapter<I, Outbound>
where
    I: Isolate,
{
    pub(crate) isolate: I,
    pub(crate) marker: PhantomData<fn(Outbound) -> Outbound>,
}

impl<I, S, F, Outbound> ErasedHandler<S, F> for SendableHandlerAdapter<I, Outbound>
where
    I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
    I::Message: 'static,
    I::Reply: Send + 'static,
    I::Spawn: IntoErasedSpawn<S, F> + 'static,
    I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
    I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
    I::Io: IntoErasedCall<I::Message> + 'static,
    I::Fact: IntoRuntimeFact + 'static,
    Outbound: Send + 'static,
    S: Shard,
    F: MailboxFactory,
{
    #[allow(unsafe_code)]
    fn handle_boxed(
        &mut self,
        message: Box<dyn Any>,
        shard: &mut S,
        isolate_id: IsolateId,
        generation: AddressGeneration,
        caller: Option<MessageCaller>,
        now: std::time::Instant,
    ) -> ErasedEffect<S, F> {
        let message = message.downcast::<I::Message>().unwrap_or_else(|_| {
            panic!("runtime attempted to deliver a handler message with the wrong type")
        });

        let effect = {
            let mut ctx = Context::<_, I::Reply>::new_typed(shard, isolate_id)
                .with_current_generation(generation)
                .with_now(now);
            if let Some(caller) = caller {
                // SAFETY: dispatch allocated this caller for this delivery.
                ctx = unsafe { ctx.with_caller(caller) };
            }
            self.isolate.handle(*message, &mut ctx)
        };

        erase_effect_sendable::<I, S, F, Outbound>(effect)
    }

    #[allow(unsafe_code)]
    fn handle_call_boxed(
        &mut self,
        message: Box<dyn Any>,
        shard: &mut S,
        isolate_id: IsolateId,
        generation: AddressGeneration,
        caller: MessageCaller,
        now: std::time::Instant,
    ) -> ErasedEffect<S, F> {
        let message = message.downcast::<I::Message>().unwrap_or_else(|_| {
            panic!("runtime attempted to deliver a call handler message with the wrong type")
        });

        let effect = {
            let call = unsafe {
                // SAFETY: dispatch allocated this caller for this delivery.
                CallContext::new(
                    Context::<_, I::Reply>::new_typed(shard, isolate_id)
                        .with_current_generation(generation)
                        .with_now(now)
                        .with_caller(caller),
                )
            };
            self.isolate.handle_call(*message, call)
        };

        erase_effect_sendable::<I, S, F, Outbound>(effect)
    }
}

fn call_output_failure_reason(result: &CallOutput) -> Option<CallError> {
    match result {
        CallOutput::Failed(reason)
        | CallOutput::TcpReadBufFailed { error: reason, .. }
        | CallOutput::TcpWroteOwnedFailed { error: reason, .. }
        | CallOutput::TlsReadBufFailed { error: reason, .. }
        | CallOutput::TlsWroteOwnedFailed { error: reason, .. } => Some(*reason),
        _ => None,
    }
}

pub(crate) fn erase_effect<I, S, F, Outbound>(effect: Effect<I>) -> ErasedEffect<S, F>
where
    I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
    I::Message: 'static,
    I::Reply: 'static,
    I::Spawn: IntoErasedSpawn<S, F> + 'static,
    I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
    I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
    I::Io: IntoErasedCall<I::Message> + 'static,
    I::Fact: IntoRuntimeFact + 'static,
    Outbound: 'static,
    S: Shard,
    F: MailboxFactory,
{
    match effect {
        Effect::Noop => ErasedEffect::Noop,
        Effect::Reply(reply) => ErasedEffect::Reply(ErasedMessage::Local(Box::new(reply))),
        Effect::Reject(reason) => ErasedEffect::Reject(reason),
        Effect::Send(send) => {
            let (destination, message) = send.into_parts();
            ErasedEffect::Send(ErasedSend {
                target_shard: destination.shard(),
                target_isolate: destination.isolate(),
                target_generation: destination.generation(),
                message: ErasedMessage::Local(Box::new(message)),
            })
        }
        Effect::Spawn(spawn) => ErasedEffect::Spawn(spawn.into_erased_spawn()),
        Effect::SpawnObserved(spawn) => {
            ErasedEffect::SpawnObserved(spawn.into_erased_spawn_observed())
        }
        Effect::SpawnObservedOn(spawn) => {
            ErasedEffect::SpawnObservedOn(spawn.into_send_erased_spawn_observed())
        }
        Effect::Stop => ErasedEffect::Stop,
        Effect::Fail => ErasedEffect::Fail,
        Effect::StopWith(result) => ErasedEffect::StopWith(result),
        Effect::RestartChildren => ErasedEffect::RestartChildren,
        Effect::StopChildren => ErasedEffect::StopChildren,
        Effect::Io(call) => ErasedEffect::Io(call.into_erased_call()),
        Effect::Batch(effects) => ErasedEffect::Batch(
            effects
                .into_iter()
                .map(erase_effect::<I, S, F, Outbound>)
                .collect(),
        ),
        Effect::ReplyTo(slot, reply) => ErasedEffect::ReplyTo {
            handle: tina::runtime_internal::deferred_into_handle(slot),
            message: ErasedMessage::Local(Box::new(reply)),
        },
        Effect::Fact(fact) => ErasedEffect::Fact(fact.into_runtime_fact()),
    }
}

pub(crate) fn erase_effect_sendable<I, S, F, Outbound>(effect: Effect<I>) -> ErasedEffect<S, F>
where
    I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
    I::Message: 'static,
    I::Reply: Send + 'static,
    I::Spawn: IntoErasedSpawn<S, F> + 'static,
    I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
    I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
    I::Io: IntoErasedCall<I::Message> + 'static,
    I::Fact: IntoRuntimeFact + 'static,
    Outbound: Send + 'static,
    S: Shard,
    F: MailboxFactory,
{
    match effect {
        Effect::Noop => ErasedEffect::Noop,
        Effect::Reply(reply) => ErasedEffect::Reply(ErasedMessage::Sendable(Box::new(reply))),
        Effect::Reject(reason) => ErasedEffect::Reject(reason),
        Effect::Send(send) => {
            let (destination, message) = send.into_parts();
            ErasedEffect::Send(ErasedSend {
                target_shard: destination.shard(),
                target_isolate: destination.isolate(),
                target_generation: destination.generation(),
                message: ErasedMessage::Sendable(Box::new(message)),
            })
        }
        Effect::Spawn(spawn) => ErasedEffect::Spawn(spawn.into_erased_spawn()),
        Effect::SpawnObserved(spawn) => {
            ErasedEffect::SpawnObserved(spawn.into_erased_spawn_observed())
        }
        Effect::SpawnObservedOn(spawn) => {
            ErasedEffect::SpawnObservedOn(spawn.into_send_erased_spawn_observed())
        }
        Effect::Stop => ErasedEffect::Stop,
        Effect::Fail => ErasedEffect::Fail,
        Effect::StopWith(result) => ErasedEffect::StopWith(result),
        Effect::RestartChildren => ErasedEffect::RestartChildren,
        Effect::StopChildren => ErasedEffect::StopChildren,
        Effect::Io(call) => ErasedEffect::Io(call.into_erased_call()),
        Effect::Batch(effects) => ErasedEffect::Batch(
            effects
                .into_iter()
                .map(erase_effect_sendable::<I, S, F, Outbound>)
                .collect(),
        ),
        Effect::ReplyTo(slot, reply) => ErasedEffect::ReplyTo {
            handle: tina::runtime_internal::deferred_into_handle(slot),
            message: ErasedMessage::Sendable(Box::new(reply)),
        },
        Effect::Fact(fact) => ErasedEffect::Fact(fact.into_runtime_fact()),
    }
}

pub(crate) struct RegisteredEntry<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    pub(crate) id: IsolateId,
    pub(crate) generation: AddressGeneration,
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) parent: Option<IsolateId>,
    pub(crate) stopped: Cell<bool>,
    pub(crate) stopped_event: Cell<Option<EventId>>,
    pub(crate) mailbox: Box<dyn ErasedMailbox>,
    pub(crate) call_contexts: RefCell<VecDeque<Option<MessageCallContext>>>,
    /// Priority queue for runtime-call continuations that did not fit in the
    /// bounded mailbox. A held resource (a bridge's leased slot) stays alive
    /// only while its `sleep().then(Poll)` self-continuation keeps firing; if
    /// that continuation is dropped on a full mailbox the slot leaks forever.
    /// The overflow takes such continuations instead of dropping them and is
    /// drained ahead of the mailbox. This is intentionally a priority lane,
    /// not FIFO with ordinary ingress: the continuation holds runtime-owned
    /// liveness. It is bounded by the isolate's own outstanding runtime calls,
    /// so it cannot grow without bound.
    pub(crate) continuation_overflow: RefCell<VecDeque<DeliveredMessage>>,
    pub(crate) handler: RefCell<Box<dyn ErasedHandler<S, F>>>,
}

pub(crate) enum ErasedEffect<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    Noop,
    Reply(ErasedMessage),
    Reject(CallRejectedReason),
    Send(ErasedSend),
    Spawn(Box<dyn ErasedSpawn<S, F>>),
    SpawnObserved(Box<dyn ErasedSpawnObserved<S, F>>),
    SpawnObservedOn(SendSpawnObservedParts<S, F>),
    Stop,
    Fail,
    StopWith(StopResult),
    RestartChildren,
    StopChildren,
    Io(ErasedCall),
    Batch(Vec<ErasedEffect<S, F>>),
    ReplyTo {
        handle: DeferredReplyHandle,
        message: ErasedMessage,
    },
    Fact(RuntimeFact),
}

impl<S, F> ErasedEffect<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    pub(crate) fn kind(&self) -> EffectKind {
        match self {
            Self::Noop => EffectKind::Noop,
            Self::Reply(_) => EffectKind::Reply,
            Self::Reject(_) => EffectKind::Reject,
            Self::Send(_) => EffectKind::Send,
            Self::Spawn(_) => EffectKind::Spawn,
            Self::SpawnObserved(_) => EffectKind::SpawnObserved,
            Self::SpawnObservedOn(_) => EffectKind::SpawnObservedOn,
            Self::Stop => EffectKind::Stop,
            Self::Fail => EffectKind::Fail,
            Self::StopWith(_) => EffectKind::StopWith,
            Self::RestartChildren => EffectKind::RestartChildren,
            Self::StopChildren => EffectKind::StopChildren,
            Self::Io(_) => EffectKind::Io,
            Self::Batch(_) => EffectKind::Batch,
            Self::ReplyTo { .. } => EffectKind::ReplyTo,
            Self::Fact(_) => EffectKind::Fact,
        }
    }

    pub(crate) fn consumes_call_context(&self) -> bool {
        match self {
            Self::Reply(_) | Self::Reject(_) => true,
            Self::Batch(effects) => {
                for effect in effects {
                    if effect.consumes_call_context() {
                        return true;
                    }
                    if effect.stops_before_consuming_call_context() {
                        return false;
                    }
                }
                false
            }
            _ => false,
        }
    }

    pub(crate) fn stops_before_consuming_call_context(&self) -> bool {
        match self {
            Self::Stop | Self::Fail | Self::StopWith(_) => true,
            Self::Reply(_) | Self::Reject(_) => false,
            Self::Batch(effects) => {
                for effect in effects {
                    if effect.consumes_call_context() {
                        return false;
                    }
                    if effect.stops_before_consuming_call_context() {
                        return true;
                    }
                }
                false
            }
            _ => false,
        }
    }
}

pub(crate) struct ErasedSend {
    pub(crate) target_shard: ShardId,
    pub(crate) target_isolate: IsolateId,
    pub(crate) target_generation: AddressGeneration,
    pub(crate) message: ErasedMessage,
}

pub(crate) enum ErasedMessage {
    Local(Box<dyn Any>),
    Sendable(Box<dyn Any + Send>),
}

impl ErasedMessage {
    pub(crate) fn into_any(self) -> Box<dyn Any> {
        match self {
            Self::Local(message) => message,
            Self::Sendable(message) => message,
        }
    }

    pub(crate) fn payload_type_id(&self) -> std::any::TypeId {
        match self {
            Self::Local(message) => (**message).type_id(),
            Self::Sendable(message) => (**message).type_id(),
        }
    }

    pub(crate) fn into_sendable(self) -> Box<dyn Any + Send> {
        match self {
            Self::Local(_) => {
                panic!("live cross-shard send attempted to move a non-Send message")
            }
            Self::Sendable(message) => message,
        }
    }
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

impl<S, F, ParentMessage> IntoErasedSpawnObserved<S, F, ParentMessage> for std::convert::Infallible
where
    S: Shard,
    F: MailboxFactory,
{
    fn into_erased_spawn_observed(self) -> Box<dyn ErasedSpawnObserved<S, F>> {
        match self {}
    }
}

pub(crate) struct SpawnObservedAdapter<Spawn, ParentMessage, ChildMessage, ChildReply> {
    pub(crate) inner: tina::SpawnObserved<Spawn, ParentMessage, ChildMessage, ChildReply>,
}

impl<Spawn, ParentMessage, ChildMessage, ChildReply, S, F> ErasedSpawnObserved<S, F>
    for SpawnObservedAdapter<Spawn, ParentMessage, ChildMessage, ChildReply>
where
    Spawn: IntoErasedSpawn<S, F> + 'static,
    ParentMessage: 'static,
    ChildMessage: 'static,
    ChildReply: 'static,
    S: Shard,
    F: MailboxFactory,
{
    fn spawn_observed(
        self: Box<Self>,
        runtime: &mut Runtime<S, F>,
        parent: IsolateId,
    ) -> SpawnObservedOutcome<S, F> {
        let (spawn, continuation) = self.inner.into_parts();
        match spawn
            .into_erased_spawn()
            .try_spawn_observed(runtime, parent)
        {
            Ok(outcome) => {
                let child_address = Address::<ChildMessage, ChildReply>::new_with_generation(
                    outcome.child.shard,
                    outcome.child.isolate,
                    outcome.child.generation,
                );
                let child_ref = ChildRef::new(child_address);
                let message = continuation(Ok(child_ref));
                SpawnObservedOutcome {
                    spawn: Some(outcome),
                    continuation: Some(ErasedMessage::Local(Box::new(message))),
                }
            }
            Err(error) => {
                let message = continuation(Err(error));
                SpawnObservedOutcome {
                    spawn: None,
                    continuation: Some(ErasedMessage::Local(Box::new(message))),
                }
            }
        }
    }
}

impl<Spawn, ParentMessage, ChildMessage, ChildReply, S, F>
    IntoErasedSpawnObserved<S, F, ParentMessage>
    for tina::SpawnObserved<Spawn, ParentMessage, ChildMessage, ChildReply>
where
    Spawn: IntoErasedSpawn<S, F> + 'static,
    ParentMessage: 'static,
    ChildMessage: 'static,
    ChildReply: 'static,
    S: Shard,
    F: MailboxFactory,
{
    fn into_erased_spawn_observed(self) -> Box<dyn ErasedSpawnObserved<S, F>> {
        Box::new(SpawnObservedAdapter { inner: self })
    }
}

pub(crate) struct SpawnAdapter<I, Outbound>
where
    I: Isolate,
{
    pub(crate) isolate: I,
    pub(crate) mailbox_capacity: usize,
    pub(crate) bootstrap_message: Option<I::Message>,
    pub(crate) marker: PhantomData<fn(Outbound) -> Outbound>,
}

impl<I, S, F, Outbound> ErasedSpawn<S, F> for SpawnAdapter<I, Outbound>
where
    I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
    I::Message: 'static,
    I::Reply: 'static,
    I::Spawn: IntoErasedSpawn<S, F> + 'static,
    I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
    I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
    I::Io: IntoErasedCall<I::Message> + 'static,
    I::Fact: IntoRuntimeFact + 'static,
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

    fn try_spawn_observed(
        self: Box<Self>,
        runtime: &mut Runtime<S, F>,
        parent: IsolateId,
    ) -> Result<SpawnOutcome<S, F>, SpawnObservedError> {
        if self.mailbox_capacity == 0 {
            return Err(SpawnObservedError::ZeroMailboxCapacity);
        }
        Ok(self.spawn(runtime, parent))
    }
}

impl<I, S, F, OutboundMsg> IntoErasedSpawn<S, F> for tina::ChildDefinition<I>
where
    I: Isolate<Shard = S, Send = TinaOutbound<OutboundMsg>> + 'static,
    I::Message: 'static,
    I::Reply: 'static,
    I::Spawn: IntoErasedSpawn<S, F> + 'static,
    I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
    I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
    I::Io: IntoErasedCall<I::Message> + 'static,
    I::Fact: IntoRuntimeFact + 'static,
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

// --- Cross-shard (Send) spawn machinery -------------------------------------
//
// A cross-shard `spawn_observed(child).on_shard(B)` ships the child constructor
// to shard B, which registers it and replies with the new address. Only the
// *constructor* crosses the thread boundary (hence `Send`); the parent's
// continuation stays on the owner shard, held until the address comes back.

/// A `Send` spawn payload that registers a child on the destination shard and
/// returns its address. The owner's continuation is not carried here — it runs
/// on the owner shard when the reply arrives.
pub(crate) trait SendErasedSpawn<S, F>: Send
where
    S: Shard,
    F: MailboxFactory,
{
    fn spawn_remote(
        self: Box<Self>,
        runtime: &mut Runtime<S, F>,
        owner: RegisteredAddress,
        child_ordinal: usize,
        request_id: Option<CallId>,
        cause: CauseId,
    ) -> Result<RegisteredAddress, SpawnObservedError>;
}

pub(crate) trait IntoSendErasedSpawn<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    fn mailbox_capacity(&self) -> usize;

    fn remote_restartable(&self) -> bool {
        false
    }

    fn into_send_erased_spawn(self) -> Box<dyn SendErasedSpawn<S, F>>;
}

/// The owner-shard parts of a cross-shard observed spawn: the target shard, the
/// `Send` payload to ship there, and the continuation thunk that turns the
/// later `RegisteredAddress` (or error) into the parent's continuation message.
pub(crate) struct SendSpawnObservedParts<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    pub(crate) target_shard: ShardId,
    pub(crate) spawn: Box<dyn SendErasedSpawn<S, F>>,
    pub(crate) mailbox_capacity: usize,
    pub(crate) remote_restartable: bool,
    #[allow(clippy::type_complexity)]
    pub(crate) continuation:
        Box<dyn FnOnce(Result<RegisteredAddress, SpawnObservedError>) -> ErasedMessage>,
}

pub(crate) trait IntoSendErasedSpawnObserved<S, F, ParentMessage>
where
    S: Shard,
    F: MailboxFactory,
{
    fn into_send_erased_spawn_observed(self) -> SendSpawnObservedParts<S, F>;
}

/// A cross-shard observed spawn awaiting its address reply, held on the owner
/// shard. The continuation turns the later address (or error) into the parent's
/// continuation message.
pub(crate) struct PendingRemoteSpawn {
    pub(crate) request_id: CallId,
    pub(crate) requester: RegisteredAddress,
    pub(crate) target_shard: ShardId,
    pub(crate) child_ordinal: usize,
    pub(crate) mailbox_capacity: usize,
    pub(crate) remote_restartable: bool,
    #[allow(clippy::type_complexity)]
    pub(crate) continuation:
        Box<dyn FnOnce(Result<RegisteredAddress, SpawnObservedError>) -> ErasedMessage>,
}

impl<S, F> IntoSendErasedSpawn<S, F> for std::convert::Infallible
where
    S: Shard,
    F: MailboxFactory,
{
    fn mailbox_capacity(&self) -> usize {
        unreachable!("an Infallible spawn request cannot be borrowed")
    }

    fn into_send_erased_spawn(self) -> Box<dyn SendErasedSpawn<S, F>> {
        match self {}
    }
}

impl<S, F, ParentMessage> IntoSendErasedSpawnObserved<S, F, ParentMessage>
    for std::convert::Infallible
where
    S: Shard,
    F: MailboxFactory,
{
    fn into_send_erased_spawn_observed(self) -> SendSpawnObservedParts<S, F> {
        match self {}
    }
}

pub(crate) struct SendSpawnAdapter<I, Outbound>
where
    I: Isolate,
{
    pub(crate) isolate: I,
    pub(crate) mailbox_capacity: usize,
    pub(crate) bootstrap_message: Option<I::Message>,
    pub(crate) marker: PhantomData<fn(Outbound) -> Outbound>,
}

impl<I, S, F, Outbound> SendErasedSpawn<S, F> for SendSpawnAdapter<I, Outbound>
where
    I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + Send + 'static,
    I::Message: Send + 'static,
    I::Reply: 'static,
    I::Spawn: IntoErasedSpawn<S, F> + 'static,
    I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
    I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
    I::Io: IntoErasedCall<I::Message> + 'static,
    I::Fact: IntoRuntimeFact + 'static,
    Outbound: Send + 'static,
    S: Shard,
    F: MailboxFactory,
{
    fn spawn_remote(
        self: Box<Self>,
        runtime: &mut Runtime<S, F>,
        owner: RegisteredAddress,
        child_ordinal: usize,
        request_id: Option<CallId>,
        cause: CauseId,
    ) -> Result<RegisteredAddress, SpawnObservedError> {
        if self.mailbox_capacity == 0 {
            return Err(SpawnObservedError::ZeroMailboxCapacity);
        }
        Ok(runtime.register_remote_child::<I, Outbound>(
            self.isolate,
            self.mailbox_capacity,
            self.bootstrap_message,
            owner,
            child_ordinal,
            request_id,
            (owner.shard != runtime.shard.id()).then_some(owner),
            None,
            cause,
        ))
    }
}

impl<I, S, F, OutboundMsg> IntoSendErasedSpawn<S, F> for tina::ChildDefinition<I>
where
    I: Isolate<Shard = S, Send = TinaOutbound<OutboundMsg>> + Send + 'static,
    I::Message: Send + 'static,
    I::Reply: 'static,
    I::Spawn: IntoErasedSpawn<S, F> + 'static,
    I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
    I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
    I::Io: IntoErasedCall<I::Message> + 'static,
    I::Fact: IntoRuntimeFact + 'static,
    OutboundMsg: Send + 'static,
    S: Shard,
    F: MailboxFactory,
{
    fn mailbox_capacity(&self) -> usize {
        self.mailbox_capacity()
    }

    fn into_send_erased_spawn(self) -> Box<dyn SendErasedSpawn<S, F>> {
        let (isolate, mailbox_capacity, bootstrap_message) = self.into_parts();
        Box::new(SendSpawnAdapter::<I, OutboundMsg> {
            isolate,
            mailbox_capacity,
            bootstrap_message,
            marker: PhantomData,
        })
    }
}

impl<Spawn, ParentMessage, ChildMessage, ChildReply, S, F>
    IntoSendErasedSpawnObserved<S, F, ParentMessage>
    for tina::SpawnObservedRemote<Spawn, ParentMessage, ChildMessage, ChildReply>
where
    Spawn: IntoSendErasedSpawn<S, F> + Send + 'static,
    ParentMessage: 'static,
    ChildMessage: 'static,
    ChildReply: 'static,
    S: Shard,
    F: MailboxFactory,
{
    fn into_send_erased_spawn_observed(self) -> SendSpawnObservedParts<S, F> {
        let (spawn, target_shard, continuation) = self.into_parts();
        let mailbox_capacity = spawn.mailbox_capacity();
        let remote_restartable = spawn.remote_restartable();
        let continuation = Box::new(
            move |result: Result<RegisteredAddress, SpawnObservedError>| -> ErasedMessage {
                let typed = result.map(|address| {
                    ChildRef::new(Address::<ChildMessage, ChildReply>::new_with_generation(
                        address.shard,
                        address.isolate,
                        address.generation,
                    ))
                });
                ErasedMessage::Local(Box::new(continuation(typed)))
            },
        );
        SendSpawnObservedParts {
            target_shard,
            mailbox_capacity,
            remote_restartable,
            spawn: spawn.into_send_erased_spawn(),
            continuation,
        }
    }
}

pub(crate) struct RestartableSpawnAdapter<I, Outbound>
where
    I: Isolate,
{
    pub(crate) factory: Box<dyn Fn() -> I>,
    pub(crate) mailbox_capacity: usize,
    pub(crate) bootstrap_factory: Option<Box<dyn Fn() -> I::Message>>,
    pub(crate) marker: PhantomData<fn(Outbound) -> Outbound>,
}

impl<I, S, F, Outbound> ErasedSpawn<S, F> for RestartableSpawnAdapter<I, Outbound>
where
    I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
    I::Message: 'static,
    I::Reply: 'static,
    I::Spawn: IntoErasedSpawn<S, F> + 'static,
    I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
    I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
    I::Io: IntoErasedCall<I::Message> + 'static,
    I::Fact: IntoRuntimeFact + 'static,
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

    fn try_spawn_observed(
        self: Box<Self>,
        runtime: &mut Runtime<S, F>,
        parent: IsolateId,
    ) -> Result<SpawnOutcome<S, F>, SpawnObservedError> {
        if self.mailbox_capacity == 0 {
            return Err(SpawnObservedError::ZeroMailboxCapacity);
        }
        Ok(self.spawn(runtime, parent))
    }
}

impl<I, S, F, Outbound> ErasedRestartRecipe<S, F> for RestartableSpawnAdapter<I, Outbound>
where
    I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
    I::Message: 'static,
    I::Reply: 'static,
    I::Spawn: IntoErasedSpawn<S, F> + 'static,
    I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
    I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
    I::Io: IntoErasedCall<I::Message> + 'static,
    I::Fact: IntoRuntimeFact + 'static,
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
    I::Reply: 'static,
    I::Spawn: IntoErasedSpawn<S, F> + 'static,
    I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
    I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
    I::Io: IntoErasedCall<I::Message> + 'static,
    I::Fact: IntoRuntimeFact + 'static,
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

pub(crate) struct CrossShardRestartableSpawnAdapter<I, Outbound>
where
    I: Isolate,
{
    pub(crate) factory: Arc<dyn Fn() -> I + Send + Sync>,
    pub(crate) mailbox_capacity: usize,
    pub(crate) bootstrap_factory: Option<Arc<dyn Fn() -> I::Message + Send + Sync>>,
    pub(crate) marker: PhantomData<fn(Outbound) -> Outbound>,
}

impl<I, S, F, Outbound> SendErasedSpawn<S, F> for CrossShardRestartableSpawnAdapter<I, Outbound>
where
    I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + Send + 'static,
    I::Message: Send + 'static,
    I::Reply: 'static,
    I::Spawn: IntoErasedSpawn<S, F> + 'static,
    I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
    I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
    I::Io: IntoErasedCall<I::Message> + 'static,
    I::Fact: IntoRuntimeFact + 'static,
    Outbound: Send + 'static,
    S: Shard,
    F: MailboxFactory,
{
    fn spawn_remote(
        self: Box<Self>,
        runtime: &mut Runtime<S, F>,
        owner: RegisteredAddress,
        child_ordinal: usize,
        request_id: Option<CallId>,
        cause: CauseId,
    ) -> Result<RegisteredAddress, SpawnObservedError> {
        if self.mailbox_capacity == 0 {
            return Err(SpawnObservedError::ZeroMailboxCapacity);
        }
        let adapter = Rc::new(*self);
        let isolate = (adapter.factory)();
        let bootstrap = adapter.bootstrap_factory.as_ref().map(|f| f());
        Ok(runtime.register_remote_child::<I, Outbound>(
            isolate,
            adapter.mailbox_capacity,
            bootstrap,
            owner,
            child_ordinal,
            request_id,
            (owner.shard != runtime.shard.id()).then_some(owner),
            Some(adapter),
            cause,
        ))
    }
}

impl<I, S, F, Outbound> ErasedRestartRecipe<S, F> for CrossShardRestartableSpawnAdapter<I, Outbound>
where
    I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + Send + 'static,
    I::Message: Send + 'static,
    I::Reply: 'static,
    I::Spawn: IntoErasedSpawn<S, F> + 'static,
    I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
    I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
    I::Io: IntoErasedCall<I::Message> + 'static,
    I::Fact: IntoRuntimeFact + 'static,
    Outbound: Send + 'static,
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

    fn create_remote(
        &self,
        runtime: &mut Runtime<S, F>,
        owner: RegisteredAddress,
        _child_ordinal: usize,
        _cause: CauseId,
    ) -> Option<SpawnOutcome<S, F>> {
        let isolate = (self.factory)();
        let bootstrap_message = self.bootstrap_factory.as_ref().map(|f| f());
        let outcome = runtime.spawn_isolate::<I, Outbound>(
            owner.isolate,
            isolate,
            self.mailbox_capacity,
            bootstrap_message,
        );
        if owner.shard != runtime.shard.id() {
            if let Some(entry_index) = runtime.entry_index(outcome.child) {
                runtime.entries[entry_index].parent = None;
            }
        }
        Some(outcome)
    }
}

impl<I, S, F, OutboundMsg> IntoSendErasedSpawn<S, F>
    for tina::CrossShardRestartableChildDefinition<I>
where
    I: Isolate<Shard = S, Send = TinaOutbound<OutboundMsg>> + Send + 'static,
    I::Message: Send + 'static,
    I::Reply: 'static,
    I::Spawn: IntoErasedSpawn<S, F> + 'static,
    I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
    I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
    I::Io: IntoErasedCall<I::Message> + 'static,
    I::Fact: IntoRuntimeFact + 'static,
    OutboundMsg: Send + 'static,
    S: Shard,
    F: MailboxFactory,
{
    fn mailbox_capacity(&self) -> usize {
        self.mailbox_capacity()
    }

    fn remote_restartable(&self) -> bool {
        true
    }

    fn into_send_erased_spawn(self) -> Box<dyn SendErasedSpawn<S, F>> {
        let (factory, mailbox_capacity, bootstrap_factory) = self.into_parts();
        Box::new(CrossShardRestartableSpawnAdapter::<I, OutboundMsg> {
            factory: Arc::from(factory),
            mailbox_capacity,
            bootstrap_factory: bootstrap_factory.map(Arc::from),
            marker: PhantomData,
        })
    }
}
