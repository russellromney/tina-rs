//! Internal state and handler-erasure machinery for the simulator.
//!
//! Extracted from lib.rs (phase 055). Houses the private state types
//! (`RegisteredAddress`, `InFlightCall`, the TCP/UDP/TLS/file/process state
//! structs, `LocalInbox`, …), the `ErasedHandler` / `ErasedSpawn` /
//! `ErasedRestartRecipe` / `IntoErasedSpawn` trait family and their
//! adapter types, the simulator-only erased-effect/erased-call/remote-envelope
//! data, and the deterministic id source. The `Simulator` impl in lib.rs
//! consumes these items directly; nothing here is part of the public API.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use tina::{
    AddressGeneration, CallContext, CallRejectedReason, Context, DeferredReplyHandle, Effect,
    Isolate, IsolateId, MessageCaller, Outbound as TinaOutbound, RestartBudgetState, Shard,
    ShardId, SpawnObservedError, TrySendError,
};
use tina_runtime::{
    CallId, CallInput, CallKind, CallOutcome, CallOutput, EffectKind, ListenerId,
    PersistenceTraceInfo, RuntimeCall, RuntimeCallParts, SendOutcome, TlsListenerId, TlsStreamId,
    UdpSocketId,
};
use tina_supervisor::SupervisorConfig;

use crate::Simulator;
use crate::config::{ScriptedTlsReadResult, ScriptedTlsWriteResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RegisteredAddress {
    pub(crate) shard: ShardId,
    pub(crate) isolate: IsolateId,
    pub(crate) generation: AddressGeneration,
}

#[derive(Debug)]
pub(crate) struct InFlightCall {
    pub(crate) call_id: CallId,
    pub(crate) call_kind: CallKind,
    pub(crate) requester: RegisteredAddress,
    pub(crate) cause: tina_runtime::CauseId,
    pub(crate) persistence: Option<PersistenceTraceInfo>,
    pub(crate) continuation_context: Option<MessageCallContext>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CallDispatchContext {
    pub(crate) call_id: CallId,
    pub(crate) requester: RegisteredAddress,
    pub(crate) cause: tina_runtime::CauseId,
    pub(crate) continuation_context: Option<MessageCallContext>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct IsolateCallDeliveryContext {
    pub(crate) call_id: CallId,
    pub(crate) requester: RegisteredAddress,
    pub(crate) cause: tina_runtime::CauseId,
    pub(crate) continuation_context: Option<MessageCallContext>,
    pub(crate) visible_at_step: u64,
}

pub(crate) type ErasedTranslator = Box<dyn FnOnce(CallOutput) -> Box<dyn Any>>;
pub(crate) type ErasedIsolateCallTranslator =
    Box<dyn FnOnce(CallOutcome<Box<dyn Any>>) -> Box<dyn Any>>;
pub(crate) type ErasedCancelCallTranslator = Box<dyn FnOnce(tina::CancelOutcome) -> Box<dyn Any>>;

pub(crate) const INITIAL_ENTRY_CAPACITY: usize = 8;
pub(crate) const INITIAL_CHILD_RECORD_CAPACITY: usize = 8;
pub(crate) const INITIAL_SUPERVISOR_CAPACITY: usize = 4;
pub(crate) const INITIAL_TRACE_CAPACITY: usize = 256;
pub(crate) const INITIAL_CALL_CAPACITY: usize = 8;
pub(crate) const INITIAL_TCP_RESOURCE_CAPACITY: usize = 4;

pub(crate) fn reserve_round_message_scratch(
    round_messages: &mut Vec<Option<DeliveredMessage>>,
    entry_count: usize,
) {
    debug_assert!(round_messages.is_empty());
    if round_messages.capacity() < entry_count {
        round_messages.reserve(entry_count);
    }
}

pub(crate) struct StoredTranslator {
    pub(crate) call_id: CallId,
    pub(crate) translator: Option<ErasedTranslator>,
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
pub(crate) struct TimerEntry {
    pub(crate) call_id: CallId,
    pub(crate) deadline: Duration,
    pub(crate) insertion_order: u64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum MessageCallContext {
    Local {
        call_id: CallId,
    },
    Remote {
        call_id: CallId,
        requester: RegisteredAddress,
        cause: tina_runtime::CauseId,
        expected_reply_type_id: std::any::TypeId,
    },
}

pub(crate) struct DeliveredMessage {
    pub(crate) message: Box<dyn Any>,
    pub(crate) call_context: Option<MessageCallContext>,
}

pub(crate) struct PendingIsolateCall {
    pub(crate) call_id: CallId,
    pub(crate) requester: RegisteredAddress,
    pub(crate) cause: tina_runtime::CauseId,
    pub(crate) deadline: Duration,
    pub(crate) insertion_order: u64,
    pub(crate) continuation_context: Option<MessageCallContext>,
    pub(crate) translator: Option<ErasedIsolateCallTranslator>,
    pub(crate) expected_reply_type_id: std::any::TypeId,
    pub(crate) handle_shared: Option<std::sync::Arc<tina::CallHandleShared>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum TcpResourceKey {
    TcpConnect(CallId),
    ListenerAccept(ListenerId),
    StreamRead(tina_runtime::StreamId),
    StreamWrite(tina_runtime::StreamId),
    UdpSend(CallId),
    UdpRecv(UdpSocketId),
    Dns(CallId),
    TlsConnect(CallId),
    TlsAccept(TlsListenerId),
    TlsStream(TlsStreamId),
    Signal(CallId),
    Process(CallId),
    FileRead(tina_runtime::FileId),
    FileWrite(tina_runtime::FileId),
    FileFsync(tina_runtime::FileId),
    FileSize(tina_runtime::FileId),
    Mkdir(CallId),
    PathOp(CallId),
}

impl TcpResourceKey {
    pub(crate) fn is_udp(self) -> bool {
        matches!(self, Self::UdpSend(_) | Self::UdpRecv(_))
    }

    pub(crate) fn is_dns(self) -> bool {
        matches!(self, Self::Dns(_))
    }

    pub(crate) fn is_tls(self) -> bool {
        matches!(
            self,
            Self::TlsConnect(_) | Self::TlsAccept(_) | Self::TlsStream(_)
        )
    }

    pub(crate) fn is_process(self) -> bool {
        matches!(self, Self::Process(_))
    }

    pub(crate) fn is_signal(self) -> bool {
        matches!(self, Self::Signal(_))
    }
}

#[derive(Debug)]
pub(crate) struct PendingTcpCompletion {
    pub(crate) call_id: CallId,
    pub(crate) ready_at_step: u64,
    pub(crate) insertion_order: u64,
    pub(crate) resource: TcpResourceKey,
    pub(crate) result: CallOutput,
}

#[derive(Debug)]
pub(crate) struct PendingAccept {
    pub(crate) call_id: CallId,
    pub(crate) listener: ListenerId,
    pub(crate) insertion_order: u64,
}

#[derive(Debug)]
pub(crate) struct PendingConnect {
    pub(crate) call_id: CallId,
    pub(crate) addr: SocketAddr,
    pub(crate) insertion_order: u64,
}

#[derive(Debug)]
pub(crate) struct ScriptedPeerState {
    pub(crate) accept_after_step: u64,
    pub(crate) peer_addr: SocketAddr,
    pub(crate) inbound_chunks: VecDeque<Vec<u8>>,
    pub(crate) read_chunk_cap: Option<usize>,
    pub(crate) write_cap: usize,
    pub(crate) output_capacity: usize,
    pub(crate) output: Vec<u8>,
}

#[derive(Debug)]
pub(crate) struct ListenerState {
    pub(crate) id: ListenerId,
    pub(crate) bind_addr: SocketAddr,
    pub(crate) local_addr: SocketAddr,
    pub(crate) backlog_capacity: usize,
    pub(crate) future_peers: VecDeque<ScriptedPeerState>,
    pub(crate) ready_peers: VecDeque<ScriptedPeerState>,
    pub(crate) closed: bool,
}

#[derive(Debug)]
pub(crate) struct StreamState {
    pub(crate) id: tina_runtime::StreamId,
    pub(crate) peer_addr: SocketAddr,
    pub(crate) inbound_chunks: VecDeque<Vec<u8>>,
    pub(crate) read_chunk_cap: Option<usize>,
    pub(crate) write_cap: usize,
    pub(crate) output_capacity: usize,
    pub(crate) output: Vec<u8>,
    pub(crate) closed: bool,
}

#[derive(Debug)]
pub(crate) struct ScriptedUdpDatagramState {
    pub(crate) deliver_after_step: u64,
    pub(crate) peer_addr: SocketAddr,
    pub(crate) bytes: Vec<u8>,
}

#[derive(Debug)]
pub(crate) struct UdpSocketState {
    pub(crate) id: UdpSocketId,
    pub(crate) bind_addr: SocketAddr,
    pub(crate) local_addr: SocketAddr,
    pub(crate) recv_capacity: usize,
    pub(crate) future_datagrams: VecDeque<ScriptedUdpDatagramState>,
    pub(crate) ready_datagrams: VecDeque<ScriptedUdpDatagramState>,
    pub(crate) closed: bool,
}

#[derive(Debug)]
pub(crate) struct TlsStreamState {
    pub(crate) id: TlsStreamId,
    pub(crate) reads: VecDeque<ScriptedTlsReadResult>,
    pub(crate) writes: VecDeque<ScriptedTlsWriteResult>,
    pub(crate) closed: bool,
    pub(crate) pending_connect_call: Option<CallId>,
}

#[derive(Debug)]
pub(crate) struct TlsListenerState {
    pub(crate) id: TlsListenerId,
    pub(crate) local_addr: SocketAddr,
    pub(crate) closed: bool,
}

#[derive(Debug)]
pub(crate) struct PendingUdpRecv {
    pub(crate) call_id: CallId,
    pub(crate) socket: UdpSocketId,
    pub(crate) max_len: usize,
    pub(crate) insertion_order: u64,
}

#[derive(Debug)]
pub(crate) struct FileState {
    pub(crate) id: tina_runtime::FileId,
    pub(crate) path: PathBuf,
    pub(crate) options: tina_runtime::FileOpenOptions,
    pub(crate) closed: bool,
}

pub(crate) struct QueuedMessage {
    pub(crate) message: Box<dyn Any>,
    pub(crate) visible_at_step: u64,
    pub(crate) call_context: Option<MessageCallContext>,
}

pub(crate) struct LocalInbox {
    pub(crate) capacity: usize,
    pub(crate) queue: RefCell<VecDeque<QueuedMessage>>,
    pub(crate) closed: Cell<bool>,
}

impl std::fmt::Debug for LocalInbox {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LocalInbox")
            .field("capacity", &self.capacity)
            .field("closed", &self.closed.get())
            .field("len", &self.queue.borrow().len())
            .finish()
    }
}

impl LocalInbox {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: RefCell::new(VecDeque::new()),
            closed: Cell::new(false),
        }
    }

    pub(crate) fn push(
        &self,
        message: Box<dyn Any>,
        visible_at_step: u64,
        call_context: Option<MessageCallContext>,
    ) -> Result<(), TrySendError<Box<dyn Any>>> {
        if self.closed.get() {
            return Err(TrySendError::Closed(message));
        }
        let mut queue = self.queue.borrow_mut();
        if queue.len() >= self.capacity {
            return Err(TrySendError::Full(message));
        }
        queue.push_back(QueuedMessage {
            message,
            visible_at_step,
            call_context,
        });
        Ok(())
    }

    pub(crate) fn pop_visible(&self, current_step: u64) -> Option<DeliveredMessage> {
        let mut queue = self.queue.borrow_mut();
        let visible = queue
            .front()
            .map(|entry| entry.visible_at_step <= current_step)
            .unwrap_or(false);
        if visible {
            queue.pop_front().map(|entry| DeliveredMessage {
                message: entry.message,
                call_context: entry.call_context,
            })
        } else {
            None
        }
    }

    pub(crate) fn close(&self) {
        self.closed.set(true);
    }

    pub(crate) fn has_pending(&self) -> bool {
        !self.queue.borrow().is_empty()
    }
}

pub(crate) trait ErasedHandler<S>
where
    S: Shard,
{
    fn handle_boxed(
        &mut self,
        message: Box<dyn Any>,
        shard: &mut S,
        isolate_id: IsolateId,
        caller: Option<MessageCaller>,
        now: std::time::Instant,
    ) -> ErasedEffect<S>;

    fn handle_call_boxed(
        &mut self,
        message: Box<dyn Any>,
        shard: &mut S,
        isolate_id: IsolateId,
        caller: MessageCaller,
        now: std::time::Instant,
    ) -> ErasedEffect<S>;
}

pub(crate) trait ErasedSpawn<S>
where
    S: Shard,
{
    fn spawn(self: Box<Self>, sim: &mut Simulator<S>, parent: IsolateId) -> SpawnOutcome<S>;

    fn try_spawn_observed(
        self: Box<Self>,
        sim: &mut Simulator<S>,
        parent: IsolateId,
    ) -> Result<SpawnOutcome<S>, SpawnObservedError> {
        Ok(self.spawn(sim, parent))
    }
}

pub(crate) trait ErasedRestartRecipe<S>
where
    S: Shard,
{
    fn create(&self, sim: &mut Simulator<S>, parent: IsolateId) -> SpawnOutcome<S>;
}

pub(crate) trait IntoErasedSpawn<S>
where
    S: Shard,
{
    fn into_erased_spawn(self) -> Box<dyn ErasedSpawn<S>>;
}

pub(crate) trait ErasedSpawnObserved<S>
where
    S: Shard,
{
    fn spawn_observed(
        self: Box<Self>,
        sim: &mut Simulator<S>,
        parent: IsolateId,
    ) -> SpawnObservedOutcome<S>;
}

pub(crate) trait IntoErasedSpawnObserved<S, ParentMessage>
where
    S: Shard,
{
    fn into_erased_spawn_observed(self) -> Box<dyn ErasedSpawnObserved<S>>;
}

pub(crate) struct HandlerAdapter<I, Outbound>
where
    I: Isolate,
{
    pub(crate) isolate: I,
    pub(crate) marker: PhantomData<fn(Outbound) -> Outbound>,
}

impl<I, S, Msg, Outbound> ErasedHandler<S> for HandlerAdapter<I, Outbound>
where
    I: Isolate<Message = Msg, Shard = S, Send = TinaOutbound<Outbound>, Call = RuntimeCall<Msg>>
        + 'static,
    I::Spawn: IntoErasedSpawn<S> + 'static,
    I::SpawnObserved: IntoErasedSpawnObserved<S, I::Message> + 'static,
    I::Reply: 'static,
    Msg: 'static,
    Outbound: 'static,
    S: Shard,
{
    fn handle_boxed(
        &mut self,
        message: Box<dyn Any>,
        shard: &mut S,
        isolate_id: IsolateId,
        caller: Option<MessageCaller>,
        now: std::time::Instant,
    ) -> ErasedEffect<S> {
        let message = message.downcast::<Msg>().unwrap_or_else(|_| {
            panic!("simulator attempted to deliver a handler message with the wrong type")
        });

        let effect = {
            let mut ctx = Context::<_, I::Reply>::new_typed(shard, isolate_id).with_now(now);
            if let Some(caller) = caller {
                ctx = ctx.with_caller(caller);
            }
            self.isolate.handle(*message, &mut ctx)
        };

        erase_effect::<I, S, Msg, Outbound>(effect)
    }

    fn handle_call_boxed(
        &mut self,
        message: Box<dyn Any>,
        shard: &mut S,
        isolate_id: IsolateId,
        caller: MessageCaller,
        now: std::time::Instant,
    ) -> ErasedEffect<S> {
        let message = message.downcast::<Msg>().unwrap_or_else(|_| {
            panic!("simulator attempted to deliver a call handler message with the wrong type")
        });

        let effect = {
            let ctx = Context::<_, I::Reply>::new_typed(shard, isolate_id)
                .with_now(now)
                .with_caller(caller);
            self.isolate.handle_call(*message, CallContext::new(ctx))
        };

        erase_effect::<I, S, Msg, Outbound>(effect)
    }
}

pub(crate) fn erase_effect<I, S, Msg, Outbound>(effect: Effect<I>) -> ErasedEffect<S>
where
    I: Isolate<Message = Msg, Shard = S, Send = TinaOutbound<Outbound>, Call = RuntimeCall<Msg>>
        + 'static,
    I::Spawn: IntoErasedSpawn<S> + 'static,
    I::SpawnObserved: IntoErasedSpawnObserved<S, I::Message> + 'static,
    I::Reply: 'static,
    Msg: 'static,
    Outbound: 'static,
    S: Shard,
{
    match effect {
        Effect::Noop => ErasedEffect::Noop,
        Effect::Reply(reply) => ErasedEffect::Reply(Box::new(reply)),
        Effect::Reject(reason) => ErasedEffect::Reject(reason),
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
        Effect::SpawnObserved(spawn) => {
            ErasedEffect::SpawnObserved(spawn.into_erased_spawn_observed())
        }
        Effect::Stop => ErasedEffect::Stop,
        // Sim has no host result waiter surface; StopWith stops the isolate
        // and drops the value. Trace still distinguishes via EffectKind.
        Effect::StopWith(_) => ErasedEffect::StopWith,
        Effect::RestartChildren => ErasedEffect::RestartChildren,
        Effect::Call(call) => {
            let call = match call.into_runtime_parts() {
                RuntimeCallParts::Backend {
                    request,
                    translator,
                } => ErasedCall {
                    kind: ErasedCallKind::Backend {
                        request,
                        translator: Box::new(move |result| {
                            Box::new(translator(result)) as Box<dyn Any>
                        }),
                    },
                },
                RuntimeCallParts::ObservedSend {
                    target_shard,
                    target_isolate,
                    target_generation,
                    message,
                    translator,
                } => ErasedCall {
                    kind: ErasedCallKind::ObservedSend {
                        send: ErasedSend {
                            target_shard,
                            target_isolate,
                            target_generation,
                            message,
                        },
                        translator: Box::new(move |outcome| {
                            Box::new(translator(outcome)) as Box<dyn Any>
                        }),
                    },
                },
                RuntimeCallParts::IsolateCall {
                    target_shard,
                    target_isolate,
                    target_generation,
                    message,
                    timeout,
                    translator,
                    expected_reply_type_id,
                    handle_shared,
                } => ErasedCall {
                    kind: ErasedCallKind::IsolateCall {
                        send: ErasedSend {
                            target_shard,
                            target_isolate,
                            target_generation,
                            message,
                        },
                        timeout,
                        translator: Box::new(move |outcome| {
                            Box::new(translator(outcome)) as Box<dyn Any>
                        }),
                        expected_reply_type_id,
                        handle_shared,
                    },
                },
                RuntimeCallParts::CancelCall {
                    handle_shared,
                    translator,
                } => ErasedCall {
                    kind: ErasedCallKind::CancelCall {
                        handle_shared,
                        translator: Box::new(move |outcome| {
                            Box::new(translator(outcome)) as Box<dyn Any>
                        }),
                    },
                },
            };
            ErasedEffect::Call(call)
        }
        Effect::Batch(effects) => ErasedEffect::Batch(
            effects
                .into_iter()
                .map(erase_effect::<I, S, Msg, Outbound>)
                .collect(),
        ),
        Effect::ReplyTo(slot, reply) => ErasedEffect::ReplyTo {
            handle: tina::runtime_internal::deferred_into_handle(slot),
            message: Box::new(reply),
        },
    }
}

pub(crate) struct RegisteredEntry<S>
where
    S: Shard,
{
    pub(crate) id: IsolateId,
    pub(crate) generation: AddressGeneration,
    pub(crate) parent: Option<IsolateId>,
    pub(crate) stopped: Cell<bool>,
    pub(crate) stopped_event: Cell<Option<tina_runtime::EventId>>,
    pub(crate) inbox: LocalInbox,
    pub(crate) handler: RefCell<Box<dyn ErasedHandler<S>>>,
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
            .field("parent", &self.parent)
            .field("stopped", &self.stopped.get())
            .field("stopped_event", &self.stopped_event.get())
            .finish_non_exhaustive()
    }
}

pub(crate) enum ErasedEffect<S>
where
    S: Shard,
{
    Noop,
    Reply(Box<dyn Any>),
    Reject(CallRejectedReason),
    Send(ErasedSend),
    Spawn(Box<dyn ErasedSpawn<S>>),
    SpawnObserved(Box<dyn ErasedSpawnObserved<S>>),
    Stop,
    StopWith,
    RestartChildren,
    Call(ErasedCall),
    Batch(Vec<ErasedEffect<S>>),
    ReplyTo {
        handle: DeferredReplyHandle,
        message: Box<dyn Any>,
    },
}

impl<S> ErasedEffect<S>
where
    S: Shard,
{
    pub(crate) fn kind(&self) -> EffectKind {
        match self {
            Self::Noop => EffectKind::Noop,
            Self::Reply(_) => EffectKind::Reply,
            Self::Reject(_) => EffectKind::Reject,
            Self::Send(_) => EffectKind::Send,
            Self::Spawn(_) => EffectKind::Spawn,
            Self::SpawnObserved(_) => EffectKind::SpawnObserved,
            Self::Stop => EffectKind::Stop,
            Self::StopWith => EffectKind::StopWith,
            Self::RestartChildren => EffectKind::RestartChildren,
            Self::Call(_) => EffectKind::Call,
            Self::Batch(_) => EffectKind::Batch,
            Self::ReplyTo { .. } => EffectKind::ReplyTo,
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

    fn stops_before_consuming_call_context(&self) -> bool {
        match self {
            Self::Stop | Self::StopWith => true,
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
    pub(crate) message: Box<dyn Any>,
}

pub(crate) struct QueuedRemoteSend {
    pub(crate) send: ErasedSend,
    pub(crate) call_context: Option<MessageCallContext>,
    pub(crate) cause: tina_runtime::CauseId,
}

pub(crate) enum QueuedRemoteEnvelope {
    Send(QueuedRemoteSend),
    CallReply(RemoteCallReply),
}

impl QueuedRemoteEnvelope {
    pub(crate) fn target_shard(&self) -> ShardId {
        match self {
            Self::Send(queued) => queued.send.target_shard,
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
        reply: outcome,
        cause,
    }))
}

pub(crate) struct RemoteCallReply {
    pub(crate) call_id: CallId,
    pub(crate) requester: RegisteredAddress,
    pub(crate) reply: RemoteCallOutcome,
    pub(crate) cause: tina_runtime::CauseId,
}

pub(crate) enum RemoteCallOutcome {
    Replied(Box<dyn Any>),
    Full,
    Closed,
    Rejected(CallRejectedReason),
}

pub(crate) struct ErasedCall {
    pub(crate) kind: ErasedCallKind,
}

pub(crate) enum ErasedCallKind {
    Backend {
        request: CallInput,
        translator: ErasedTranslator,
    },
    ObservedSend {
        send: ErasedSend,
        translator: Box<dyn FnOnce(SendOutcome) -> Box<dyn Any>>,
    },
    IsolateCall {
        send: ErasedSend,
        timeout: Duration,
        translator: ErasedIsolateCallTranslator,
        expected_reply_type_id: std::any::TypeId,
        handle_shared: Option<std::sync::Arc<tina::CallHandleShared>>,
    },
    CancelCall {
        handle_shared: std::sync::Arc<tina::CallHandleShared>,
        translator: ErasedCancelCallTranslator,
    },
}

pub(crate) struct SpawnOutcome<S>
where
    S: Shard,
{
    pub(crate) child: RegisteredAddress,
    pub(crate) mailbox_capacity: usize,
    pub(crate) restart_recipe: Option<Rc<dyn ErasedRestartRecipe<S>>>,
    pub(crate) bootstrap_message: Option<Box<dyn Any>>,
}

pub(crate) struct SpawnObservedOutcome<S>
where
    S: Shard,
{
    pub(crate) spawn: Option<SpawnOutcome<S>>,
    pub(crate) continuation: Option<Box<dyn Any>>,
}

pub(crate) struct ChildRecord<S>
where
    S: Shard,
{
    pub(crate) parent: IsolateId,
    pub(crate) child: RegisteredAddress,
    pub(crate) child_ordinal: usize,
    pub(crate) mailbox_capacity: usize,
    pub(crate) restart_recipe: Option<Rc<dyn ErasedRestartRecipe<S>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SupervisorRecord {
    pub(crate) parent: RegisteredAddress,
    pub(crate) config: SupervisorConfig,
    pub(crate) budget_state: RestartBudgetState,
}

#[derive(Debug, Clone)]
pub(crate) struct IdSource {
    pub(crate) next_event_id: Rc<Cell<u64>>,
    pub(crate) next_call_id: Rc<Cell<u64>>,
}

impl IdSource {
    pub(crate) fn new() -> Self {
        Self {
            next_event_id: Rc::new(Cell::new(1)),
            next_call_id: Rc::new(Cell::new(1)),
        }
    }

    pub(crate) fn next_event_id(&self) -> tina_runtime::EventId {
        let raw = self.next_event_id.get();
        self.next_event_id.set(raw + 1);
        tina_runtime::EventId::new(raw)
    }

    pub(crate) fn next_call_id(&self) -> CallId {
        let raw = self.next_call_id.get();
        self.next_call_id.set(raw + 1);
        CallId::new(raw)
    }
}
