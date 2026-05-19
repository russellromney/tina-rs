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
    ErasedMessage, ErasedSend, MessageCallContext, RegisteredAddress, Runtime,
    call_reply_reason_for_cause,
};

pub(crate) enum QueuedRemoteEnvelope {
    Send(QueuedRemoteSend),
    CallReply(RemoteCallReply),
}

impl QueuedRemoteEnvelope {
    pub(crate) fn target_shard(&self) -> ShardId {
        match self {
            Self::Send(send) => send.send.target_shard,
            Self::CallReply(reply) => reply.requester.shard,
        }
    }
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
        }
    }

    pub(crate) fn into_queued_remote_envelope(self) -> QueuedRemoteEnvelope {
        match self {
            Self::Send(send) => QueuedRemoteEnvelope::Send(send.into_queued_remote_send()),
            Self::CallReply(reply) => {
                QueuedRemoteEnvelope::CallReply(reply.into_remote_call_reply())
            }
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
    ) -> Option<QueuedRemoteEnvelope> {
        match queued {
            QueuedRemoteEnvelope::Send(send) => self.harvest_remote_send(send),
            QueuedRemoteEnvelope::CallReply(reply) => {
                self.harvest_remote_call_reply(reply);
                None
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
