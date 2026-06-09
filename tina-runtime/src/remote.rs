//! Cross-shard send and call-reply transport for [`Runtime`].
//!
//! This module owns the vocabulary the local runtime uses to send
//! envelopes across shards (`QueuedRemoteEnvelope`,
//! `SendableQueuedRemoteEnvelope`, and the call-reply transports) and
//! the harvest-side methods on [`Runtime`] that consume those
//! envelopes when they reach the destination shard.
//!
//! Same-shard delivery still uses [`Runtime::dispatch_local_send`] /
//! [`Runtime::dispatch_local_send_with_context`]; their definitions
//! live here so the local and remote paths read together.

use std::any::Any;

use tina::{AddressGeneration, CallRejectedReason, Shard, ShardId, TrySendError};

use crate::call::{CallId, CallOutcome};
use crate::mailbox::MailboxFactory;
use crate::trace::{CallReplyRejectedReason, CauseId, RuntimeEventKind, SendRejectedReason};
use crate::{
    ErasedMessage, ErasedSend, MessageCallContext, PendingRemoteSpawn, RegisteredAddress, Runtime,
    SendErasedSpawn, call_reply_reason_for_cause,
};

pub(crate) enum QueuedRemoteEnvelope {
    Send(QueuedRemoteSend),
    CallReply(RemoteCallReply),
    SpawnRequest(RemoteSpawnRequest),
    SpawnReply(RemoteSpawnReply),
    SpawnCancel(RemoteSpawnCancel),
    ChildStop(RemoteChildStop),
    ChildStopped(RemoteChildStopped),
    ChildRestart(RemoteChildRestart),
    ChildRestarted(RemoteChildRestarted),
}

impl QueuedRemoteEnvelope {
    pub(crate) fn target_shard(&self) -> ShardId {
        match self {
            Self::Send(send) => send.send.target_shard,
            Self::CallReply(reply) => reply.requester.shard,
            Self::SpawnRequest(request) => request.target_shard,
            Self::SpawnReply(reply) => reply.requester.shard,
            Self::SpawnCancel(cancel) => cancel.target_shard,
            Self::ChildStop(stop) => stop.child.shard,
            Self::ChildStopped(stopped) => stopped.owner.shard,
            Self::ChildRestart(restart) => restart.child.shard,
            Self::ChildRestarted(restarted) => restarted.owner.shard,
        }
    }

    pub(crate) fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::CallReply(_)
                | Self::SpawnReply(_)
                | Self::ChildStopped(_)
                | Self::ChildRestarted(_)
        )
    }

    pub(crate) fn reserves_terminal_response(&self) -> bool {
        match self {
            Self::Send(send) => {
                matches!(send.call_context, Some(MessageCallContext::Remote { .. }))
            }
            Self::SpawnRequest(_)
            | Self::SpawnCancel(_)
            | Self::ChildStop(_)
            | Self::ChildRestart(_) => true,
            Self::CallReply(_)
            | Self::SpawnReply(_)
            | Self::ChildStopped(_)
            | Self::ChildRestarted(_) => false,
        }
    }
}

/// A cross-shard `spawn_observed(...).on_shard(B)` request. The `payload` is a
/// type-erased `Box<dyn SendErasedSpawn<S, F>>` (boxed as `Any` so the
/// monomorphic envelope can carry it); the destination shard — same `S, F` —
/// downcasts it back, registers the child, and replies.
pub(crate) struct RemoteSpawnRequest {
    pub(crate) request_id: CallId,
    pub(crate) target_shard: ShardId,
    pub(crate) owner: RegisteredAddress,
    pub(crate) child_ordinal: usize,
    pub(crate) payload: Box<dyn Any + Send>,
    pub(crate) cause: CauseId,
}

/// The reply to a [`RemoteSpawnRequest`]: the new child's address, or the
/// spawn error, routed back to the owner shard.
pub(crate) struct RemoteSpawnReply {
    pub(crate) request_id: CallId,
    pub(crate) requester: RegisteredAddress,
    pub(crate) cause: CauseId,
    pub(crate) outcome: Result<RegisteredAddress, tina::SpawnObservedError>,
}

pub(crate) struct RemoteSpawnCancel {
    pub(crate) request_id: CallId,
    pub(crate) target_shard: ShardId,
    pub(crate) owner: RegisteredAddress,
    pub(crate) cause: CauseId,
}

pub(crate) struct RemoteChildStop {
    pub(crate) owner: RegisteredAddress,
    pub(crate) child: RegisteredAddress,
    pub(crate) child_ordinal: usize,
    pub(crate) cause: CauseId,
}

pub(crate) struct RemoteChildStopped {
    pub(crate) owner: RegisteredAddress,
    pub(crate) child: RegisteredAddress,
    pub(crate) child_ordinal: usize,
    pub(crate) cause: CauseId,
}

pub(crate) struct RemoteChildRestart {
    pub(crate) owner: RegisteredAddress,
    pub(crate) child: RegisteredAddress,
    pub(crate) child_ordinal: usize,
    pub(crate) cause: CauseId,
}

pub(crate) struct RemoteChildRestarted {
    pub(crate) owner: RegisteredAddress,
    pub(crate) child_ordinal: usize,
    pub(crate) old_child: RegisteredAddress,
    pub(crate) outcome: Result<RegisteredAddress, crate::RestartSkippedReason>,
    pub(crate) cause: CauseId,
}

pub(crate) fn remote_call_outcome_envelope(
    context: Option<MessageCallContext>,
    outcome: RemoteCallOutcome,
) -> Option<QueuedRemoteEnvelope> {
    let Some(MessageCallContext::Remote {
        call_id,
        requester,
        cause,
        ..
    }) = context
    else {
        return None;
    };
    Some(QueuedRemoteEnvelope::CallReply(RemoteCallReply {
        call_id,
        requester,
        cause,
        outcome,
    }))
}

pub(crate) struct QueuedRemoteSend {
    pub(crate) send: ErasedSend,
    pub(crate) call_context: Option<MessageCallContext>,
    pub(crate) cause: CauseId,
}

pub(crate) struct SendableQueuedRemoteSend {
    pub(crate) target_shard: ShardId,
    pub(crate) target_isolate: tina::IsolateId,
    pub(crate) target_generation: AddressGeneration,
    pub(crate) message: Box<dyn Any + Send>,
    pub(crate) call_context: Option<MessageCallContext>,
    pub(crate) cause: CauseId,
}

impl SendableQueuedRemoteSend {
    pub(crate) fn new(
        send: ErasedSend,
        call_context: Option<MessageCallContext>,
        cause: CauseId,
    ) -> Self {
        Self {
            target_shard: send.target_shard,
            target_isolate: send.target_isolate,
            target_generation: send.target_generation,
            message: send.message.into_sendable(),
            call_context,
            cause,
        }
    }

    pub(crate) fn into_queued_remote_send(self) -> QueuedRemoteSend {
        QueuedRemoteSend {
            send: ErasedSend {
                target_shard: self.target_shard,
                target_isolate: self.target_isolate,
                target_generation: self.target_generation,
                message: ErasedMessage::Sendable(self.message),
            },
            call_context: self.call_context,
            cause: self.cause,
        }
    }
}

pub(crate) enum SendableQueuedRemoteEnvelope {
    Send(SendableQueuedRemoteSend),
    CallReply(SendableRemoteCallReply),
    // Spawn request/reply payloads are already `Send`, so they cross threads
    // unchanged — no separate sendable mirror is needed.
    SpawnRequest(RemoteSpawnRequest),
    SpawnReply(RemoteSpawnReply),
    SpawnCancel(RemoteSpawnCancel),
    ChildStop(RemoteChildStop),
    ChildStopped(RemoteChildStopped),
    ChildRestart(RemoteChildRestart),
    ChildRestarted(RemoteChildRestarted),
}

impl SendableQueuedRemoteEnvelope {
    pub(crate) fn new(envelope: QueuedRemoteEnvelope) -> Self {
        match envelope {
            QueuedRemoteEnvelope::Send(send) => Self::Send(SendableQueuedRemoteSend::new(
                send.send,
                send.call_context,
                send.cause,
            )),
            QueuedRemoteEnvelope::CallReply(reply) => {
                Self::CallReply(SendableRemoteCallReply::new(reply))
            }
            QueuedRemoteEnvelope::SpawnRequest(request) => Self::SpawnRequest(request),
            QueuedRemoteEnvelope::SpawnReply(reply) => Self::SpawnReply(reply),
            QueuedRemoteEnvelope::SpawnCancel(cancel) => Self::SpawnCancel(cancel),
            QueuedRemoteEnvelope::ChildStop(stop) => Self::ChildStop(stop),
            QueuedRemoteEnvelope::ChildStopped(stopped) => Self::ChildStopped(stopped),
            QueuedRemoteEnvelope::ChildRestart(restart) => Self::ChildRestart(restart),
            QueuedRemoteEnvelope::ChildRestarted(restarted) => Self::ChildRestarted(restarted),
        }
    }

    pub(crate) fn into_queued_remote_envelope(self) -> QueuedRemoteEnvelope {
        match self {
            Self::Send(send) => QueuedRemoteEnvelope::Send(send.into_queued_remote_send()),
            Self::CallReply(reply) => {
                QueuedRemoteEnvelope::CallReply(reply.into_remote_call_reply())
            }
            Self::SpawnRequest(request) => QueuedRemoteEnvelope::SpawnRequest(request),
            Self::SpawnReply(reply) => QueuedRemoteEnvelope::SpawnReply(reply),
            Self::SpawnCancel(cancel) => QueuedRemoteEnvelope::SpawnCancel(cancel),
            Self::ChildStop(stop) => QueuedRemoteEnvelope::ChildStop(stop),
            Self::ChildStopped(stopped) => QueuedRemoteEnvelope::ChildStopped(stopped),
            Self::ChildRestart(restart) => QueuedRemoteEnvelope::ChildRestart(restart),
            Self::ChildRestarted(restarted) => QueuedRemoteEnvelope::ChildRestarted(restarted),
        }
    }
}

pub(crate) struct RemoteCallReply {
    pub(crate) call_id: CallId,
    pub(crate) requester: RegisteredAddress,
    pub(crate) cause: CauseId,
    pub(crate) outcome: RemoteCallOutcome,
}

pub(crate) enum RemoteCallOutcome {
    Replied(ErasedMessage),
    Full,
    Closed,
    Rejected(CallRejectedReason),
}

pub(crate) struct SendableRemoteCallReply {
    pub(crate) call_id: CallId,
    pub(crate) requester: RegisteredAddress,
    pub(crate) cause: CauseId,
    pub(crate) outcome: SendableRemoteCallOutcome,
}

impl SendableRemoteCallReply {
    pub(crate) fn new(reply: RemoteCallReply) -> Self {
        match reply.outcome {
            RemoteCallOutcome::Replied(message) => Self {
                call_id: reply.call_id,
                requester: reply.requester,
                cause: reply.cause,
                outcome: SendableRemoteCallOutcome::Replied(message.into_sendable()),
            },
            RemoteCallOutcome::Full => Self {
                call_id: reply.call_id,
                requester: reply.requester,
                cause: reply.cause,
                outcome: SendableRemoteCallOutcome::Full,
            },
            RemoteCallOutcome::Closed => Self {
                call_id: reply.call_id,
                requester: reply.requester,
                cause: reply.cause,
                outcome: SendableRemoteCallOutcome::Closed,
            },
            RemoteCallOutcome::Rejected(reason) => Self {
                call_id: reply.call_id,
                requester: reply.requester,
                cause: reply.cause,
                outcome: SendableRemoteCallOutcome::Rejected(reason),
            },
        }
    }

    pub(crate) fn into_remote_call_reply(self) -> RemoteCallReply {
        let outcome = match self.outcome {
            SendableRemoteCallOutcome::Replied(reply) => {
                RemoteCallOutcome::Replied(ErasedMessage::Sendable(reply))
            }
            SendableRemoteCallOutcome::Full => RemoteCallOutcome::Full,
            SendableRemoteCallOutcome::Closed => RemoteCallOutcome::Closed,
            SendableRemoteCallOutcome::Rejected(reason) => RemoteCallOutcome::Rejected(reason),
        };
        RemoteCallReply {
            call_id: self.call_id,
            requester: self.requester,
            cause: self.cause,
            outcome,
        }
    }
}

pub(crate) enum SendableRemoteCallOutcome {
    Replied(Box<dyn Any + Send>),
    Full,
    Closed,
    Rejected(CallRejectedReason),
}

impl<S, F> Runtime<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    pub(crate) fn dispatch_local_send(&self, send: ErasedSend) -> Result<(), SendRejectedReason> {
        self.dispatch_local_send_with_context(send, None)
    }

    pub(crate) fn dispatch_local_send_with_context(
        &self,
        send: ErasedSend,
        call_context: Option<MessageCallContext>,
    ) -> Result<(), SendRejectedReason> {
        if send.target_shard != self.shard.id() {
            panic!(
                "cross-shard send is out of scope in this slice: target shard {} != runtime shard {}",
                send.target_shard.get(),
                self.shard.id().get(),
            );
        }

        let Some(entry_index) = self
            .entries
            .iter()
            .position(|entry| entry.id == send.target_isolate)
        else {
            return Err(SendRejectedReason::Closed);
        };
        let entry = &self.entries[entry_index];

        if entry.generation != send.target_generation {
            return Err(SendRejectedReason::Closed);
        }

        self.enqueue_entry_message(entry_index, send.message.into_any(), call_context)
            .map_err(|reason| match reason {
                TrySendError::Full(_) => SendRejectedReason::Full,
                TrySendError::Closed(_) => SendRejectedReason::Closed,
            })
    }

    pub(crate) fn harvest_remote_envelope(
        &mut self,
        queued: QueuedRemoteEnvelope,
    ) -> Option<QueuedRemoteEnvelope>
    where
        S: 'static,
        F: 'static,
    {
        match queued {
            QueuedRemoteEnvelope::Send(send) => self.harvest_remote_send(send),
            QueuedRemoteEnvelope::CallReply(reply) => {
                self.harvest_remote_call_reply(reply);
                None
            }
            QueuedRemoteEnvelope::SpawnRequest(request) => {
                self.harvest_remote_spawn_request(request)
            }
            QueuedRemoteEnvelope::SpawnReply(reply) => self.harvest_remote_spawn_reply(reply),
            QueuedRemoteEnvelope::SpawnCancel(cancel) => self.harvest_remote_spawn_cancel(cancel),
            QueuedRemoteEnvelope::ChildStop(stop) => self.harvest_remote_child_stop(stop),
            QueuedRemoteEnvelope::ChildStopped(stopped) => {
                self.harvest_remote_child_stopped(stopped);
                None
            }
            QueuedRemoteEnvelope::ChildRestart(restart) => {
                self.harvest_remote_child_restart(restart)
            }
            QueuedRemoteEnvelope::ChildRestarted(restarted) => {
                self.harvest_remote_child_restarted(restarted);
                None
            }
        }
    }

    /// Destination-shard harvest of a cross-shard spawn request: recover the
    /// `Send`-erased spawn (boxed as `Any`; same `S, F` here so the downcast
    /// succeeds), register the child, and reply with its address.
    pub(crate) fn harvest_remote_spawn_request(
        &mut self,
        request: RemoteSpawnRequest,
    ) -> Option<QueuedRemoteEnvelope>
    where
        S: 'static,
        F: 'static,
    {
        let RemoteSpawnRequest {
            request_id,
            target_shard: _,
            owner,
            child_ordinal,
            payload,
            cause,
        } = request;
        if self
            .remote_spawn_cancel_tombstones
            .iter()
            .any(|t| *t == request_id)
        {
            return Some(QueuedRemoteEnvelope::SpawnReply(RemoteSpawnReply {
                request_id,
                requester: owner,
                cause,
                outcome: Err(tina::SpawnObservedError::DestinationUnavailable),
            }));
        }
        let outcome = match payload.downcast::<Box<dyn SendErasedSpawn<S, F>>>() {
            Ok(spawn) => (*spawn).spawn_remote(self, owner, child_ordinal, Some(request_id), cause),
            // Unreachable in practice: only this runtime boxes that payload and
            // both shards share `S, F` (identical `TypeId`). A failure is an
            // internal invariant break, so trip in debug/test rather than
            // silently masquerading as a transport failure; release degrades to
            // a typed DestinationUnavailable.
            Err(_) => {
                debug_assert!(false, "cross-shard spawn payload downcast failed");
                Err(tina::SpawnObservedError::DestinationUnavailable)
            }
        };
        Some(QueuedRemoteEnvelope::SpawnReply(RemoteSpawnReply {
            request_id,
            requester: owner,
            cause,
            outcome,
        }))
    }

    /// Owner-shard harvest of a cross-shard spawn reply: record the
    /// `ChildStarted` truth and run the held continuation into the owner's
    /// mailbox so the parent learns the child's address.
    pub(crate) fn harvest_remote_spawn_reply(
        &mut self,
        reply: RemoteSpawnReply,
    ) -> Option<QueuedRemoteEnvelope> {
        let RemoteSpawnReply {
            request_id,
            requester: _,
            cause,
            outcome,
        } = reply;
        let index = self
            .pending_remote_spawns
            .iter()
            .position(|pending| pending.request_id == request_id)?;
        let pending: PendingRemoteSpawn = self.pending_remote_spawns.remove(index);
        // The owner address comes from the pending record we kept, not the
        // reply, so a stray reply cannot redirect the continuation.
        let requester = pending.requester;
        if let Ok(child) = &outcome {
            self.record_remote_child_on_owner(
                requester.isolate,
                *child,
                pending.child_ordinal,
                pending.mailbox_capacity,
                pending.remote_restartable,
            );
            self.push_event(
                requester.isolate,
                Some(cause),
                RuntimeEventKind::ChildStarted {
                    child_shard: child.shard,
                    child_isolate: child.isolate,
                    child_generation: child.generation,
                },
            );
            let owner_stopped = self
                .entry_by_isolate(requester.isolate)
                .is_none_or(|entry| {
                    entry.generation != requester.generation || entry.stopped.get()
                });
            if owner_stopped {
                return Some(QueuedRemoteEnvelope::ChildStop(RemoteChildStop {
                    owner: requester,
                    child: *child,
                    child_ordinal: pending.child_ordinal,
                    cause,
                }));
            }
        }
        let message = (pending.continuation)(outcome);
        // Deliver through the traced local-send path so a full/closed/stale
        // owner mailbox records SendRejected truth rather than dropping the
        // continuation silently.
        self.deliver_observed_continuation(requester, message, cause);
        None
    }

    pub(crate) fn harvest_remote_spawn_cancel(
        &mut self,
        cancel: RemoteSpawnCancel,
    ) -> Option<QueuedRemoteEnvelope> {
        self.remember_remote_spawn_cancel(cancel.request_id, cancel.owner.isolate, cancel.cause);
        if let Some(record_index) = self
            .child_records
            .iter()
            .position(|record| record.remote_request_id == Some(cancel.request_id))
        {
            let child = self.child_records[record_index].child;
            let child_ordinal = self.child_records[record_index].child_ordinal;
            let cause = cancel.cause;
            self.stop_remote_owned_child(cancel.owner, child, child_ordinal, cause)
        } else {
            None
        }
    }

    pub(crate) fn harvest_remote_child_stop(
        &mut self,
        stop: RemoteChildStop,
    ) -> Option<QueuedRemoteEnvelope> {
        self.stop_remote_owned_child(stop.owner, stop.child, stop.child_ordinal, stop.cause)
    }

    pub(crate) fn harvest_remote_child_stopped(&mut self, stopped: RemoteChildStopped) {
        self.push_event(
            stopped.owner.isolate,
            Some(stopped.cause),
            RuntimeEventKind::RemoteChildStopped {
                child_shard: stopped.child.shard,
                child_ordinal: stopped.child_ordinal,
                child_isolate: stopped.child.isolate,
                child_generation: stopped.child.generation,
            },
        );
    }

    pub(crate) fn harvest_remote_child_restart(
        &mut self,
        restart: RemoteChildRestart,
    ) -> Option<QueuedRemoteEnvelope> {
        self.restart_remote_owned_child(
            restart.owner,
            restart.child,
            restart.child_ordinal,
            restart.cause,
        )
    }

    pub(crate) fn harvest_remote_child_restarted(&mut self, restarted: RemoteChildRestarted) {
        let Some(record_index) = self.child_records.iter().position(|record| {
            record.parent == restarted.owner.isolate
                && record.remote_owner.is_none()
                && record.child_ordinal == restarted.child_ordinal
                && record.child == restarted.old_child
        }) else {
            return;
        };
        match restarted.outcome {
            Ok(new_child) => {
                self.child_records[record_index].child = new_child;
                self.push_event(
                    restarted.owner.isolate,
                    Some(restarted.cause),
                    RuntimeEventKind::RestartChildCompleted {
                        child_ordinal: restarted.child_ordinal,
                        old_isolate: restarted.old_child.isolate,
                        old_generation: restarted.old_child.generation,
                        new_isolate: new_child.isolate,
                        new_generation: new_child.generation,
                    },
                );
                self.observation.notify_child_restarted(
                    restarted.owner.isolate,
                    crate::observation::ChildRestarted {
                        child_ordinal: restarted.child_ordinal,
                        new_shard: new_child.shard,
                        new_isolate: new_child.isolate,
                        new_generation: new_child.generation,
                    },
                );
            }
            Err(reason) => {
                self.push_event(
                    restarted.owner.isolate,
                    Some(restarted.cause),
                    RuntimeEventKind::RestartChildSkipped {
                        child_ordinal: restarted.child_ordinal,
                        old_isolate: restarted.old_child.isolate,
                        old_generation: restarted.old_child.generation,
                        reason,
                    },
                );
            }
        }
    }

    pub(crate) fn harvest_remote_send(
        &mut self,
        queued: QueuedRemoteSend,
    ) -> Option<QueuedRemoteEnvelope> {
        // Cross-shard transport admission already happened on the source shard.
        // What we record here is destination-local harvest outcome, not a
        // retroactive change to the source-side send result.
        let send = queued.send;
        let Some(entry_index) = self
            .entries
            .iter()
            .position(|entry| entry.id == send.target_isolate)
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
            return remote_call_outcome_envelope(queued.call_context, RemoteCallOutcome::Closed);
        };
        let entry = &self.entries[entry_index];

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
            return remote_call_outcome_envelope(queued.call_context, RemoteCallOutcome::Closed);
        }

        match self.enqueue_entry_message(entry_index, send.message.into_any(), queued.call_context)
        {
            Ok(()) => {
                self.push_event(
                    send.target_isolate,
                    Some(queued.cause),
                    RuntimeEventKind::MailboxAccepted,
                );
                None
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
                remote_call_outcome_envelope(queued.call_context, RemoteCallOutcome::Full)
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
                remote_call_outcome_envelope(queued.call_context, RemoteCallOutcome::Closed)
            }
        }
    }

    pub(crate) fn harvest_remote_call_reply(&mut self, reply: RemoteCallReply) {
        match reply.outcome {
            RemoteCallOutcome::Replied(message) => {
                if !self.complete_isolate_call(
                    reply.call_id,
                    reply.cause,
                    CallOutcome::Replied(message.into_any()),
                ) {
                    // Cross-shard late reply path: same cause-aware
                    // classification as the local-reply path. Without
                    // this, a cancelled or timed-out cross-shard call
                    // loses the new `CallerCancelled` / `CallerTimedOut`
                    // / `OwnerStopped` / `RuntimeStopped` truth and
                    // surfaces as the generic `NoPendingCall`.
                    let reason = match self.recently_cancelled_cause(reply.call_id) {
                        Some(c) => call_reply_reason_for_cause(c),
                        None => CallReplyRejectedReason::NoPendingCall,
                    };
                    self.push_event(
                        reply.requester.isolate,
                        Some(reply.cause),
                        RuntimeEventKind::CallReplyRejected {
                            call_id: reply.call_id,
                            reason,
                        },
                    );
                }
            }
            RemoteCallOutcome::Full => {
                self.complete_remote_isolate_call(reply, CallOutcome::Full);
            }
            RemoteCallOutcome::Closed => {
                self.complete_remote_isolate_call(reply, CallOutcome::Closed);
            }
            RemoteCallOutcome::Rejected(reason) => {
                self.complete_remote_isolate_call(reply, CallOutcome::Rejected(reason));
            }
        }
    }

    pub(crate) fn complete_remote_isolate_call(
        &mut self,
        reply: RemoteCallReply,
        outcome: CallOutcome<Box<dyn Any>>,
    ) {
        if !self.complete_isolate_call(reply.call_id, reply.cause, outcome) {
            self.push_event(
                reply.requester.isolate,
                Some(reply.cause),
                RuntimeEventKind::CallReplyRejected {
                    call_id: reply.call_id,
                    reason: CallReplyRejectedReason::NoPendingCall,
                },
            );
        }
    }
}
