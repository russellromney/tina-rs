#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

//! Single-shard virtual-time simulator for `tina-rs`.
//!
//! Phase 016 started Voyager with the narrowest honest simulator slice:
//!
//! - one shard
//! - virtual monotonic time
//! - the shipped `Sleep { after }` / `TimerFired` contract
//! - deterministic replay artifacts
//!
//! Phase 017 widens that slice just enough to make replayed divergence useful:
//!
//! - seeded perturbation over local-send and timer-wake delivery
//! - a small checker surface that can halt a run with a structured reason
//! - replay artifacts that preserve checker failures alongside the event record
//!
//! Phase 018 adds single-shard spawn and supervision replay:
//!
//! - public `ChildDefinition` / `RestartableChildDefinition` execution
//! - runtime-owned direct parent-child lineage and restartable child records
//! - direct-child `RestartChildren` and supervised panic restart
//! - bootstrap re-delivery, stale identity rejection, and budget exhaustion
//!   through the live runtime event vocabulary
//!
//! Phase 019 adds scripted single-shard TCP simulation:
//!
//! - bind, accept, read, write, listener close, and stream close
//! - replayable peer-visible output
//! - checker-backed TCP completion perturbation replay
//!
//! `tina-sim` intentionally reuses the live runtime's event vocabulary from
//! `tina-runtime` instead of inventing a second user-visible meaning model.
//! The simulator is still narrower than the live runtime: it supports local
//! sends, stop, reply/noop observation, ordered `Batch`, runtime-owned
//! `Sleep`, single-shard spawn/supervision, and scripted single-shard TCP
//! simulation.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
use std::convert::Infallible;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use tina::{
    Address, AddressGeneration, CallRejectedReason, CallRouting, ChildDefinition, ChildRef,
    ChildRelation, DeferredReplyHandle, DeferredSlotRegistry, Isolate, IsolateId, MessageCaller,
    Outbound as TinaOutbound, RestartableChildDefinition, Shard, ShardId, SpawnObservedError,
    TrySendError,
};
#[cfg(test)]
use tina::{Context, Effect};
use tina_runtime::{
    CallCompletionRejectedReason, CallError, CallId, CallInput, CallKind, CallOutcome, CallOutput,
    CallReplyRejectedReason, EffectKind, ListenerId, PathKind, PathMetadata, PersistenceTraceInfo,
    ProcessStatus, RestartSkippedReason, RuntimeCall, RuntimeCallable, RuntimeEvent,
    RuntimeEventKind, SendOutcome, SendRejectedReason, SupervisionRejectedReason, TlsListenerId,
    TlsStreamId, UdpSocketId,
};
use tina_supervisor::SupervisorConfig;

mod config;
mod deferred;
mod internals;
mod multi_shard;

use internals::*;

pub use config::{
    Checker, CheckerDecision, CheckerFailure, DurableImage, FaultConfig, FaultMode,
    LocalSendFaultMode, MultiShardReplayArtifact, ObservedPeerOutput, ReplayArtifact,
    ScriptedDnsConfig, ScriptedDnsLookupConfig, ScriptedDnsResult, ScriptedListenerConfig,
    ScriptedPeerConfig, ScriptedProcessConfig, ScriptedProcessResult, ScriptedProcessRunConfig,
    ScriptedSignalConfig, ScriptedSignalEventConfig, ScriptedSignalResult,
    ScriptedStorageFaultConfig, ScriptedTcpConfig, ScriptedTlsConfig, ScriptedTlsConnectConfig,
    ScriptedTlsConnectResult, ScriptedTlsReadResult, ScriptedTlsWriteResult, ScriptedUdpConfig,
    ScriptedUdpDatagramConfig, ScriptedUdpSocketConfig, SimulatorConfig, TcpCompletionFaultMode,
};
pub use multi_shard::{MultiShardSimulator, MultiShardSimulatorConfig};

pub mod dst;

/// Narrow single-shard simulator for timer-driven `tina-rs` workloads.
pub struct Simulator<S>
where
    S: Shard,
{
    shard: S,
    config: SimulatorConfig,
    entries: Vec<RegisteredEntry<S>>,
    child_records: Vec<ChildRecord<S>>,
    supervisors: Vec<SupervisorRecord>,
    next_isolate_id: u64,
    next_listener_id: u64,
    next_stream_id: u64,
    next_udp_socket_id: u64,
    next_tls_listener_id: u64,
    next_tls_stream_id: u64,
    next_file_id: u64,
    ids: IdSource,
    trace: Vec<RuntimeEvent>,
    virtual_now: Duration,
    /// `Instant` anchor that pairs with `virtual_now` to give the
    /// simulator a same-shape "now" as the live runtime's `Clock`.
    /// Stamped once at construction (from `Instant::now()`) and never
    /// mutated; handlers see `virtual_anchor + virtual_now`.
    ///
    /// **Determinism scope.** Within a single simulator run the anchor
    /// is stable, so every `Context::now()` and every `Deadline` built
    /// from it is deterministic. *Across* runs the anchor differs
    /// (`Instant::now()` is wall-clock time at construction). Trace
    /// events themselves never embed `Instant`s — they record
    /// `Duration` deltas against `virtual_now`, which IS replay-stable
    /// — so the DST trace-hash claim still holds. The cross-run
    /// difference only matters if user code embeds raw `Instant`s in
    /// `stop_with` payloads or other observed values; user code that
    /// wants byte-identical replay should compare `virtual_now`
    /// `Duration`s, not `Instant`s.
    virtual_anchor: std::time::Instant,
    step_ordinal: u64,
    timers: Vec<TimerEntry>,
    next_timer_ordinal: u64,
    next_send_ordinal: u64,
    next_tcp_completion_ordinal: u64,
    next_storage_ordinal: u64,
    listeners: Vec<ListenerState>,
    streams: Vec<StreamState>,
    udp_sockets: Vec<UdpSocketState>,
    tls_listeners: Vec<TlsListenerState>,
    tls_streams: Vec<TlsStreamState>,
    files: Vec<FileState>,
    file_storage: BTreeMap<PathBuf, Vec<u8>>,
    directories: Vec<PathBuf>,
    pending_accepts: Vec<PendingAccept>,
    pending_connects: Vec<PendingConnect>,
    pending_udp_recvs: Vec<PendingUdpRecv>,
    pending_tcp_completions: Vec<PendingTcpCompletion>,
    in_flight_calls: Vec<InFlightCall>,
    translators: Vec<StoredTranslator>,
    pending_isolate_calls: Vec<PendingIsolateCall>,
    round_messages: Vec<Option<DeliveredMessage>>,
    next_isolate_call_ordinal: u64,
    last_checker_failure: Option<CheckerFailure>,
    deferred_registry: std::rc::Rc<DeferredSlotRegistry>,
    promoted_slots: deferred::PromotedSlots,
    /// Bounded ring of recently-cancelled calls plus the cause that
    /// closed each one. Mirrors the runtime so late callee replies
    /// surface as the matching cause-specific rejection reason
    /// (`CallerCancelled` / `CallerTimedOut` / `OwnerStopped` /
    /// `RuntimeStopped`) instead of the generic
    /// `NoPendingCall` / `CallerClosed`.
    cancelled_calls: std::collections::VecDeque<(CallId, tina::CancelCause)>,
    /// Live trace observer. Fires before in-memory push so DST replay
    /// sees the same stream a live operator does.
    trace_observer: Option<Arc<dyn tina_runtime::TraceObserver>>,
}

impl<S> Simulator<S>
where
    S: Shard,
{
    /// Creates a new simulator with virtual time starting at zero.
    pub fn new(shard: S, config: SimulatorConfig) -> Self {
        Self::with_ids(shard, config, IdSource::new())
    }

    fn with_ids(shard: S, config: SimulatorConfig, ids: IdSource) -> Self {
        Self {
            shard,
            config,
            entries: Vec::with_capacity(INITIAL_ENTRY_CAPACITY),
            child_records: Vec::with_capacity(INITIAL_CHILD_RECORD_CAPACITY),
            supervisors: Vec::with_capacity(INITIAL_SUPERVISOR_CAPACITY),
            next_isolate_id: 1,
            next_listener_id: 1,
            next_stream_id: 1,
            next_udp_socket_id: 1,
            next_tls_listener_id: 1,
            next_tls_stream_id: 1,
            next_file_id: 1,
            ids,
            trace: Vec::with_capacity(INITIAL_TRACE_CAPACITY),
            virtual_now: Duration::ZERO,
            virtual_anchor: std::time::Instant::now(),
            step_ordinal: 0,
            timers: Vec::with_capacity(INITIAL_CALL_CAPACITY),
            next_timer_ordinal: 0,
            next_send_ordinal: 0,
            next_tcp_completion_ordinal: 0,
            next_storage_ordinal: 0,
            listeners: Vec::with_capacity(INITIAL_TCP_RESOURCE_CAPACITY),
            streams: Vec::with_capacity(INITIAL_TCP_RESOURCE_CAPACITY),
            udp_sockets: Vec::with_capacity(INITIAL_TCP_RESOURCE_CAPACITY),
            tls_listeners: Vec::with_capacity(INITIAL_TCP_RESOURCE_CAPACITY),
            tls_streams: Vec::with_capacity(INITIAL_TCP_RESOURCE_CAPACITY),
            files: Vec::with_capacity(INITIAL_TCP_RESOURCE_CAPACITY),
            file_storage: BTreeMap::new(),
            directories: Vec::with_capacity(INITIAL_TCP_RESOURCE_CAPACITY),
            pending_accepts: Vec::with_capacity(INITIAL_CALL_CAPACITY),
            pending_connects: Vec::with_capacity(INITIAL_CALL_CAPACITY),
            pending_udp_recvs: Vec::with_capacity(INITIAL_CALL_CAPACITY),
            pending_tcp_completions: Vec::with_capacity(INITIAL_CALL_CAPACITY),
            in_flight_calls: Vec::with_capacity(INITIAL_CALL_CAPACITY),
            translators: Vec::with_capacity(INITIAL_CALL_CAPACITY),
            pending_isolate_calls: Vec::with_capacity(INITIAL_CALL_CAPACITY),
            round_messages: Vec::with_capacity(INITIAL_ENTRY_CAPACITY),
            next_isolate_call_ordinal: 0,
            last_checker_failure: None,
            deferred_registry: std::rc::Rc::new(DeferredSlotRegistry::new()),
            promoted_slots: deferred::PromotedSlots::default(),
            cancelled_calls: std::collections::VecDeque::with_capacity(
                tina_runtime::CANCELLED_CALL_RING_CAPACITY,
            ),
            trace_observer: None,
        }
    }

    /// Returns the immutable simulator config for this run.
    pub fn config(&self) -> &SimulatorConfig {
        &self.config
    }

    /// Sets the live trace observer. `None` detaches. Set before the
    /// first `step()`. Same contract as the runtime's; see
    /// [`tina_runtime::TraceObserver`].
    pub fn set_trace_observer(&mut self, observer: Option<Arc<dyn tina_runtime::TraceObserver>>) {
        self.trace_observer = observer;
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
        !self.in_flight_calls.is_empty()
            || !self.timers.is_empty()
            || !self.pending_tcp_completions.is_empty()
            || !self.pending_accepts.is_empty()
            || !self.pending_connects.is_empty()
            || !self.pending_udp_recvs.is_empty()
            || !self.pending_isolate_calls.is_empty()
    }

    /// Registers one isolate and returns its typed address.
    ///
    /// 016 intentionally requires spawnless isolates that use the current
    /// runtime's timer-aware call vocabulary and local `Outbound` sends.
    #[allow(private_bounds)]
    pub fn register<I, Msg, Outbound>(&mut self, isolate: I) -> Address<Msg, I::Reply>
    where
        I: Isolate<
                Message = Msg,
                Shard = S,
                Send = TinaOutbound<Outbound>,
                Call = RuntimeCall<Msg>,
            > + 'static,
        I::Call: RuntimeCallable,
        I::Spawn: IntoErasedSpawn<S> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, I::Message> + 'static,
        I::Reply: 'static,
        Msg: 'static,
        Outbound: 'static,
    {
        let address = self.register_entry::<I, Msg, Outbound>(isolate, None, usize::MAX);
        Address::new_with_generation(address.shard, address.isolate, address.generation)
    }

    /// Registers one isolate with an explicit simulator mailbox capacity.
    ///
    /// The redundant `I::Call: RuntimeCallable` bound
    /// surfaces a targeted compile diagnostic when an isolate authored
    /// with `#[tina::isolate(...)]` (which defaults `Call = Infallible`)
    /// is registered with the simulator: instead of an opaque
    /// `Call = RuntimeCall<...>` mismatch, Rust also reports that
    /// `Infallible: RuntimeCallable` is not satisfied, and the trait's
    /// `#[diagnostic::on_unimplemented]` note points at the
    /// `#[tina_runtime::isolate(...)]` fix.
    #[allow(private_bounds)]
    pub fn register_with_mailbox_capacity<I, Msg, Outbound>(
        &mut self,
        isolate: I,
        mailbox_capacity: usize,
    ) -> Address<Msg, I::Reply>
    where
        I: Isolate<
                Message = Msg,
                Shard = S,
                Send = TinaOutbound<Outbound>,
                Call = RuntimeCall<Msg>,
            > + 'static,
        I::Call: RuntimeCallable,
        I::Spawn: IntoErasedSpawn<S> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, I::Message> + 'static,
        I::Reply: 'static,
        Msg: 'static,
        Outbound: 'static,
    {
        let address = self.register_entry::<I, Msg, Outbound>(isolate, None, mailbox_capacity);
        Address::new_with_generation(address.shard, address.isolate, address.generation)
    }

    /// Configures a registered isolate as supervisor for its direct children.
    ///
    /// The simulator mirrors `tina-runtime`: supervision applies to
    /// direct children only, and reconfiguring a parent resets the
    /// runtime-lifetime restart budget tracker.
    pub fn supervise<M: 'static, R>(&mut self, parent: Address<M, R>, config: SupervisorConfig) {
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

    /// Attempts to inject one typed message into a registered isolate.
    pub fn try_send<M: 'static, R>(
        &self,
        address: Address<M, R>,
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

        match entry.inbox.push(Box::new(message), self.step_ordinal, None) {
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
        let Some(next_deadline) = self
            .timers
            .iter()
            .map(|entry| entry.deadline)
            .chain(
                self.pending_isolate_calls
                    .iter()
                    .map(|entry| entry.deadline),
            )
            .min()
        else {
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
        let shard_id = self.shard.id();
        self.step_with_remote(&mut |_source_shard, envelope| {
            let target_shard = envelope.target_shard();
            match envelope {
                QueuedRemoteEnvelope::Send(queued) => {
                    panic!(
                        "cross-shard send is out of scope in this slice: target shard {} != simulator shard {}",
                        queued.send.target_shard.get(),
                        shard_id.get(),
                    );
                }
                QueuedRemoteEnvelope::CallReply(_) => {
                    panic!(
                        "cross-shard call reply is out of scope in this slice: requester shard {} != simulator shard {}",
                        target_shard.get(),
                        shard_id.get(),
                    );
                }
            }
        })
    }

    fn step_with_remote<FR>(&mut self, route_remote: &mut FR) -> usize
    where
        FR: FnMut(ShardId, QueuedRemoteEnvelope) -> Result<(), SendRejectedReason>,
    {
        self.step_ordinal += 1;
        let now = self.virtual_now;
        self.harvest_timers(now);
        self.harvest_isolate_call_timeouts(now);
        self.harvest_tcp();

        let mut round_messages = std::mem::take(&mut self.round_messages);
        round_messages.clear();
        reserve_round_message_scratch(&mut round_messages, self.entries.len());
        for entry in &self.entries {
            let message = if entry.stopped.get() {
                None
            } else {
                entry.inbox.pop_visible(self.step_ordinal)
            };
            round_messages.push(message);
        }

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

            let incoming_call_context = message.call_context;
            let caller = self.build_message_caller(incoming_call_context, isolate_id);
            let now = self.virtual_anchor + self.virtual_now;

            let effect = {
                let mut handler = self.entries[index].handler.borrow_mut();
                catch_unwind(AssertUnwindSafe(|| match caller {
                    Some(caller) => handler.handle_call_boxed(
                        message.message,
                        &mut self.shard,
                        isolate_id,
                        caller,
                        now,
                    ),
                    None => handler.handle_boxed(
                        message.message,
                        &mut self.shard,
                        isolate_id,
                        None,
                        now,
                    ),
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
                    let captured_any = self.drop_pending_deferred_captures(panicked.into());
                    if !captured_any {
                        if let Some(context) = incoming_call_context {
                            self.reject_call_context(
                                isolate_id,
                                panicked.into(),
                                context,
                                CallRejectedReason::HandlerPanicked,
                                route_remote,
                            );
                        }
                    }
                    self.stop_entry(index, isolate_id, panicked.into());
                    self.supervise_panic(
                        RegisteredAddress {
                            shard: self.shard.id(),
                            isolate: isolate_id,
                            generation: self.entries[index].generation,
                        },
                        panicked.into(),
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
            if self.has_in_flight_calls() {
                continue;
            }
            if self.has_pending_messages() {
                continue;
            }
            break total;
        }
    }

    /// Runs until quiescent or until a checker halts the run.
    pub fn run_until_quiescent_checked<C: Checker>(
        &mut self,
        checker: &mut C,
    ) -> Option<CheckerFailure> {
        self.last_checker_failure = None;
        let mut observed_len = 0;
        loop {
            let delivered = self.step();
            if let Some(failure) = self.observe_new_events(checker, &mut observed_len) {
                self.last_checker_failure = Some(failure.clone());
                return Some(failure);
            }
            if delivered > 0 {
                continue;
            }
            if self.advance_to_next_timer() {
                continue;
            }
            if self.has_in_flight_calls() {
                continue;
            }
            if self.has_pending_messages() {
                continue;
            }
            return None;
        }
    }

    /// Captures a deterministic replay artifact for the current run.
    pub fn replay_artifact(&self) -> ReplayArtifact {
        ReplayArtifact {
            config: self.config.clone(),
            final_time: self.virtual_now,
            event_record: self.trace.clone(),
            checker_failure: self.last_checker_failure.clone(),
            observed_peer_output: self.observed_peer_output(),
            durable_image: self.durable_image(),
        }
    }

    /// Replaces the simulator's durable file image.
    pub fn load_durable_image(&mut self, image: DurableImage) {
        self.file_storage = image.files;
    }

    /// Captures the simulator's deterministic durable file image.
    pub fn durable_image(&self) -> DurableImage {
        DurableImage {
            files: self.file_storage.clone(),
        }
    }

    fn observe_new_events<C: Checker>(
        &self,
        checker: &mut C,
        observed_len: &mut usize,
    ) -> Option<CheckerFailure> {
        while *observed_len < self.trace.len() {
            let event = self.trace[*observed_len];
            *observed_len += 1;
            match checker.on_event(&event) {
                CheckerDecision::Continue => {}
                CheckerDecision::Fail(reason) => {
                    return Some(CheckerFailure {
                        checker_name: checker.name(),
                        event_id: event.id(),
                        reason,
                    });
                }
            }
        }
        None
    }

    fn execute_effect(
        &mut self,
        index: usize,
        cause: tina_runtime::CauseId,
        effect: ErasedEffect<S>,
        call_context: Option<MessageCallContext>,
        round_messages: &mut [Option<DeliveredMessage>],
        route_remote: &mut impl FnMut(ShardId, QueuedRemoteEnvelope) -> Result<(), SendRejectedReason>,
    ) -> bool {
        let isolate_id = self.entries[index].id;
        match effect {
            ErasedEffect::Stop | ErasedEffect::StopWith => {
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
            ErasedEffect::Call(call) => {
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
                                CallOutcome::Replied(reply),
                            ) {
                                let reason = match self.recently_cancelled_cause(call_id) {
                                    Some(c) => tina_runtime::call_reply_reason_for_cause(c),
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
                            cause: call_cause,
                            ..
                        } => {
                            let remote_reply = RemoteCallReply {
                                call_id,
                                requester,
                                reply: RemoteCallOutcome::Replied(reply),
                                cause: call_cause,
                            };
                            if let Err(reason) = route_remote(
                                self.shard.id(),
                                QueuedRemoteEnvelope::CallReply(remote_reply),
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
            ErasedEffect::RestartChildren => {
                self.restart_children(isolate_id, cause, round_messages);
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
        }
    }

    fn reject_call_context(
        &mut self,
        isolate_id: IsolateId,
        cause: tina_runtime::CauseId,
        context: MessageCallContext,
        reason: CallRejectedReason,
        route_remote: &mut impl FnMut(ShardId, QueuedRemoteEnvelope) -> Result<(), SendRejectedReason>,
    ) {
        match context {
            MessageCallContext::Local { call_id } => {
                self.push_call_rejected_event(isolate_id, cause, call_id, reason);
                if !self.complete_isolate_call(call_id, cause, CallOutcome::Rejected(reason)) {
                    let reason = match self.recently_cancelled_cause(call_id) {
                        Some(c) => tina_runtime::call_reply_reason_for_cause(c),
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
                cause: call_cause,
                ..
            } => {
                self.push_call_rejected_event(isolate_id, cause, call_id, reason);
                let remote_reply = RemoteCallReply {
                    call_id,
                    requester,
                    reply: RemoteCallOutcome::Rejected(reason),
                    cause: call_cause,
                };
                if let Err(rejected) = route_remote(
                    self.shard.id(),
                    QueuedRemoteEnvelope::CallReply(remote_reply),
                ) {
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

    fn push_call_rejected_event(
        &mut self,
        isolate_id: IsolateId,
        cause: tina_runtime::CauseId,
        call_id: CallId,
        reason: CallRejectedReason,
    ) {
        let kind = match reason {
            CallRejectedReason::ReplyAbandoned => RuntimeEventKind::CallReplyAbandoned { call_id },
            CallRejectedReason::HandlerPanicked | CallRejectedReason::UnsupportedMessage => {
                RuntimeEventKind::CallRejected { call_id, reason }
            }
        };
        self.push_event(isolate_id, Some(cause), kind);
    }

    fn execute_reply_to(
        &mut self,
        isolate_id: IsolateId,
        cause: tina_runtime::CauseId,
        handle: DeferredReplyHandle,
        message: Box<dyn Any>,
        route_remote: &mut impl FnMut(ShardId, QueuedRemoteEnvelope) -> Result<(), SendRejectedReason>,
    ) {
        let shared = tina::runtime_internal::handle_shared(&handle).clone();
        let Some(record) = self.promoted_slots.take_by_handle(&shared) else {
            // Slot already terminal: caller closed and we already
            // emitted Rejected{CallerClosed}. Drop the payload.
            return;
        };

        let payload_type_id = (*message).type_id();
        if payload_type_id != record.shared.expected_reply_type_id() {
            record.shared.set_state(tina::DeferredSlotState::Closed);
            self.push_event(
                isolate_id,
                Some(cause),
                RuntimeEventKind::DeferredReplyRejected {
                    slot_id: record.slot_id,
                    call_id: record.call_id,
                    reason: tina_runtime::DeferredReplyRejectedReason::TypeMismatch,
                },
            );
            return;
        }

        match record.routing {
            deferred::DeferredRouting::Local => {
                if self.complete_isolate_call(record.call_id, cause, CallOutcome::Replied(message))
                {
                    record.shared.set_state(tina::DeferredSlotState::Replied);
                    self.push_event(
                        isolate_id,
                        Some(cause),
                        RuntimeEventKind::DeferredReplySent {
                            slot_id: record.slot_id,
                            call_id: record.call_id,
                        },
                    );
                } else {
                    let reason = match self.recently_cancelled_cause(record.call_id) {
                        Some(c) => tina_runtime::deferred_reply_reason_for_cause(c),
                        None => tina_runtime::DeferredReplyRejectedReason::CallerClosed,
                    };
                    record.shared.set_state(tina::DeferredSlotState::Closed);
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
                let remote_reply = RemoteCallReply {
                    call_id: record.call_id,
                    requester,
                    reply: RemoteCallOutcome::Replied(message),
                    cause: request_cause,
                };
                match route_remote(
                    self.shard.id(),
                    QueuedRemoteEnvelope::CallReply(remote_reply),
                ) {
                    Ok(()) => {
                        record.shared.set_state(tina::DeferredSlotState::Replied);
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
                            SendRejectedReason::Full => {
                                tina_runtime::DeferredReplyRejectedReason::ReplyPathFull
                            }
                            SendRejectedReason::Closed => {
                                tina_runtime::DeferredReplyRejectedReason::RequesterShardClosed
                            }
                        };
                        record.shared.set_state(tina::DeferredSlotState::Closed);
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

    fn register_entry<I, Msg, Outbound>(
        &mut self,
        isolate: I,
        parent: Option<IsolateId>,
        mailbox_capacity: usize,
    ) -> RegisteredAddress
    where
        I: Isolate<
                Message = Msg,
                Shard = S,
                Send = TinaOutbound<Outbound>,
                Call = RuntimeCall<Msg>,
            > + 'static,
        I::Spawn: IntoErasedSpawn<S> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, I::Message> + 'static,
        I::Reply: 'static,
        Msg: 'static,
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
            inbox: LocalInbox::new(mailbox_capacity),
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

    fn spawn_isolate<I, Msg, Outbound>(
        &mut self,
        parent: IsolateId,
        isolate: I,
        mailbox_capacity: usize,
        bootstrap_message: Option<Msg>,
    ) -> SpawnOutcome<S>
    where
        I: Isolate<
                Message = Msg,
                Shard = S,
                Send = TinaOutbound<Outbound>,
                Call = RuntimeCall<Msg>,
            > + 'static,
        I::Spawn: IntoErasedSpawn<S> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, I::Message> + 'static,
        I::Reply: 'static,
        Msg: 'static,
        Outbound: 'static,
    {
        if mailbox_capacity == 0 {
            panic!("spawn requested mailbox capacity 0, which is out of scope for this slice");
        }

        let child =
            self.register_entry::<I, Msg, Outbound>(isolate, Some(parent), mailbox_capacity);

        SpawnOutcome {
            child,
            mailbox_capacity,
            restart_recipe: None,
            bootstrap_message: bootstrap_message.map(|message| Box::new(message) as Box<dyn Any>),
        }
    }

    fn record_child(&mut self, parent: IsolateId, outcome: SpawnOutcome<S>) {
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

    fn enqueue_bootstrap_message(
        &mut self,
        child: RegisteredAddress,
        message: Box<dyn Any>,
        cause: tina_runtime::CauseId,
    ) {
        let entry = self
            .entries
            .iter()
            .find(|entry| entry.id == child.isolate && entry.generation == child.generation)
            .unwrap_or_else(|| panic!("bootstrap referenced unknown child {:?}", child.isolate));
        entry
            .inbox
            .push(message, self.step_ordinal, None)
            .unwrap_or_else(|_| {
                panic!(
                    "simulator failed to enqueue bootstrap message for child {:?}",
                    child.isolate
                )
            });
        self.push_event(
            child.isolate,
            Some(cause),
            RuntimeEventKind::MailboxAccepted,
        );
    }

    fn restart_children(
        &mut self,
        parent: IsolateId,
        cause: tina_runtime::CauseId,
        round_messages: &mut [Option<DeliveredMessage>],
    ) {
        for child_record_index in 0..self.child_records.len() {
            if self.child_records[child_record_index].parent == parent {
                self.restart_child_record(parent, child_record_index, cause, round_messages);
            }
        }
    }

    fn supervise_panic(
        &mut self,
        failed_child: RegisteredAddress,
        cause: tina_runtime::CauseId,
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

    fn restart_child_record(
        &mut self,
        parent: IsolateId,
        child_record_index: usize,
        cause: tina_runtime::CauseId,
        round_messages: &mut [Option<DeliveredMessage>],
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

    fn checked_registered_address<M: 'static, R>(
        &self,
        address: Address<M, R>,
        operation: &str,
    ) -> RegisteredAddress {
        if address.shard() != self.shard.id() {
            panic!(
                "{operation} targeted a parent on another shard: target shard {} != simulator shard {}",
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

    fn entry_index(&self, address: RegisteredAddress) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| entry.id == address.isolate && entry.generation == address.generation)
    }

    fn entry_by_isolate(&self, isolate: IsolateId) -> Option<&RegisteredEntry<S>> {
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

    fn dispatch_call(
        &mut self,
        call: ErasedCall,
        requester: RegisteredAddress,
        cause: tina_runtime::CauseId,
        continuation_context: Option<MessageCallContext>,
        route_remote: &mut impl FnMut(ShardId, QueuedRemoteEnvelope) -> Result<(), SendRejectedReason>,
    ) {
        let call_id = self.ids.next_call_id();
        let call_kind: CallKind = match &call.kind {
            ErasedCallKind::Backend { request, .. } => call_kind(request),
            ErasedCallKind::ObservedSend { .. } => CallKind::ObservedSend,
            ErasedCallKind::IsolateCall { .. } => CallKind::IsolateCall,
            ErasedCallKind::CancelCall { .. } => CallKind::CancelCall,
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
            ErasedCallKind::Backend {
                request,
                translator,
            } => self.dispatch_backend_call(dispatch_context, call_kind, request, translator),
            ErasedCallKind::ObservedSend { send, translator } => {
                self.dispatch_observed_send(dispatch_context, send, translator, route_remote)
            }
            ErasedCallKind::IsolateCall {
                send,
                timeout,
                translator,
                expected_reply_type_id,
                handle_shared,
            } => self.dispatch_isolate_call(
                dispatch_context,
                send,
                timeout,
                translator,
                expected_reply_type_id,
                handle_shared,
                route_remote,
            ),
            ErasedCallKind::CancelCall {
                handle_shared,
                translator,
            } => self.dispatch_cancel_call(dispatch_context, handle_shared, translator),
        }
    }

    fn dispatch_backend_call(
        &mut self,
        context: CallDispatchContext,
        call_kind: CallKind,
        request: CallInput,
        translator: ErasedTranslator,
    ) {
        let persistence = request.persistence_trace_info();
        if persistence == Some(PersistenceTraceInfo::Recovery) {
            self.push_event(
                context.requester.isolate,
                Some(context.cause),
                RuntimeEventKind::RecoveryStarted,
            );
        }
        self.in_flight_calls.push(InFlightCall {
            call_id: context.call_id,
            call_kind,
            requester: context.requester,
            cause: context.cause,
            persistence,
            continuation_context: context.continuation_context,
        });
        self.translators.push(StoredTranslator {
            call_id: context.call_id,
            translator: Some(translator),
        });

        let call_id = context.call_id;
        match request {
            CallInput::Sleep { after } => {
                let insertion_order = self.next_timer_ordinal;
                let deadline = self.virtual_now
                    + after
                    + self.fault_delay(self.config.faults.timer_wake, insertion_order, 0x5449_4d45);
                self.next_timer_ordinal += 1;
                self.timers.push(TimerEntry {
                    call_id,
                    deadline,
                    insertion_order,
                });
            }
            CallInput::TcpBind { addr } => self.handle_tcp_bind(context.call_id, addr),
            CallInput::TcpAccept { listener } => self.handle_tcp_accept(context.call_id, listener),
            CallInput::TcpConnect { addr } => self.handle_tcp_connect(context.call_id, addr),
            CallInput::TcpRead { stream, max_len } => {
                self.handle_tcp_read(call_id, stream, max_len)
            }
            CallInput::TcpWrite { stream, bytes } => self.handle_tcp_write(call_id, stream, bytes),
            CallInput::TcpListenerClose { listener } => {
                self.handle_tcp_listener_close(call_id, listener)
            }
            CallInput::TcpStreamClose { stream } => self.handle_tcp_stream_close(call_id, stream),
            CallInput::UdpBind { addr } => self.handle_udp_bind(call_id, addr),
            CallInput::UdpSendTo {
                socket,
                peer,
                bytes,
            } => self.handle_udp_send_to(call_id, socket, peer, bytes),
            CallInput::UdpRecvFrom { socket, max_len } => {
                self.handle_udp_recv_from(call_id, socket, max_len)
            }
            CallInput::UdpSocketClose { socket } => self.handle_udp_socket_close(call_id, socket),
            CallInput::DnsLookup {
                host,
                port,
                timeout,
            } => self.handle_dns_lookup(call_id, host, port, timeout),
            CallInput::TlsConnect {
                addr,
                server_name,
                root_certificates,
                timeout,
            } => self.handle_tls_connect(call_id, addr, server_name, root_certificates, timeout),
            CallInput::TlsBind { addr, .. } => self.handle_tls_bind(call_id, addr),
            CallInput::TlsAccept { listener, timeout } => {
                self.handle_tls_accept(call_id, listener, timeout)
            }
            CallInput::TlsListenerClose { listener } => {
                self.handle_tls_listener_close(call_id, listener)
            }
            CallInput::TlsRead {
                stream,
                max_len,
                timeout,
            } => self.handle_tls_read(call_id, stream, max_len, timeout),
            CallInput::TlsWrite {
                stream,
                bytes,
                timeout,
            } => self.handle_tls_write(call_id, stream, bytes, timeout),
            CallInput::TlsClose { stream, timeout } => {
                self.handle_tls_close(call_id, stream, timeout)
            }
            CallInput::SignalWait { name, timeout } => {
                self.handle_signal_wait(call_id, name, timeout)
            }
            CallInput::ProcessRun {
                command,
                args,
                timeout,
                stdout_limit,
                stderr_limit,
            } => {
                self.handle_process_run(call_id, command, args, timeout, stdout_limit, stderr_limit)
            }
            CallInput::FileOpen { path, options } => self.handle_file_open(call_id, path, options),
            CallInput::FileReadAt { file, len, offset } => {
                self.handle_file_read_at(call_id, file, len, offset)
            }
            CallInput::FileWriteAt {
                file,
                bytes,
                offset,
            } => self.handle_file_write_at(call_id, file, bytes, offset),
            CallInput::FileFsync { file } => self.handle_file_fsync(call_id, file),
            CallInput::FileSize { file } => self.handle_file_size(call_id, file),
            CallInput::FileClose { file } => self.handle_file_close(call_id, file),
            CallInput::Mkdir { path, .. } => self.handle_mkdir(call_id, path),
            CallInput::PathMetadata { path } => self.handle_path_metadata(call_id, path),
            CallInput::RenameReplace { from, to } => self.handle_rename_replace(call_id, from, to),
            CallInput::RemoveFile { path } => self.handle_remove_file(call_id, path),
            CallInput::ReadDir { path } => self.handle_read_dir(call_id, path),
            CallInput::SyncParent { path } => self.handle_sync_parent(call_id, path),
            CallInput::SnapshotCommit {
                path,
                bytes,
                last_journal_index,
            } => self.handle_snapshot_commit(call_id, path, bytes, last_journal_index),
            CallInput::SnapshotLoad { path } => self.handle_snapshot_load(call_id, path),
            CallInput::JournalAppend {
                path,
                record_index,
                bytes,
            } => self.handle_journal_append(call_id, path, record_index, bytes),
            CallInput::JournalReplay { path } => self.handle_journal_replay(call_id, path),
        }
    }

    fn dispatch_observed_send(
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
                match reason {
                    SendRejectedReason::Full => SendOutcome::Full,
                    SendRejectedReason::Closed => SendOutcome::Closed,
                }
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

    #[allow(clippy::too_many_arguments)]
    fn dispatch_isolate_call(
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

        if target_shard != self.shard.id() {
            let call_context = MessageCallContext::Remote {
                call_id: context.call_id,
                requester: context.requester,
                cause: context.cause,
                expected_reply_type_id,
            };
            match route_remote(
                self.shard.id(),
                QueuedRemoteEnvelope::Send(QueuedRemoteSend {
                    send,
                    call_context: Some(call_context),
                    cause: send_attempted.into(),
                }),
            ) {
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
                    self.pending_isolate_calls.push(PendingIsolateCall {
                        call_id: context.call_id,
                        requester: context.requester,
                        cause: context.cause,
                        deadline: self.virtual_now + timeout,
                        insertion_order,
                        continuation_context: context.continuation_context,
                        translator: Some(translator),
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
                        IsolateCallDeliveryContext {
                            call_id: context.call_id,
                            requester: context.requester,
                            cause: context.cause,
                            continuation_context: context.continuation_context,
                            visible_at_step: self.step_ordinal,
                        },
                        outcome,
                        translator,
                    );
                }
            }
            return;
        }

        let delivery = self.dispatch_local_send_with_context(
            send,
            Some(MessageCallContext::Local {
                call_id: context.call_id,
            }),
        );

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
                self.pending_isolate_calls.push(PendingIsolateCall {
                    call_id: context.call_id,
                    requester: context.requester,
                    cause: context.cause,
                    deadline: self.virtual_now + timeout,
                    insertion_order,
                    continuation_context: context.continuation_context,
                    translator: Some(translator),
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
                    IsolateCallDeliveryContext {
                        call_id: context.call_id,
                        requester: context.requester,
                        cause: context.cause,
                        continuation_context: context.continuation_context,
                        visible_at_step: self.step_ordinal,
                    },
                    outcome,
                    translator,
                );
            }
        }
    }

    fn handle_tcp_bind(&mut self, call_id: CallId, addr: SocketAddr) {
        if self
            .listeners
            .iter()
            .any(|listener| listener.bind_addr == addr && !listener.closed)
        {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::Io));
            return;
        }

        let Some(script) = self
            .config
            .tcp
            .listeners
            .iter()
            .find(|listener| listener.bind_addr == addr)
            .cloned()
        else {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::Io));
            return;
        };

        let listener = ListenerState {
            id: ListenerId::new(self.next_listener_id),
            bind_addr: script.bind_addr,
            local_addr: script.local_addr,
            backlog_capacity: script.backlog_capacity,
            future_peers: script
                .peers
                .into_iter()
                .map(|peer| {
                    let inbound_len = peer.inbound_chunks.iter().map(Vec::len).sum::<usize>();
                    if inbound_len > peer.inbound_capacity {
                        panic!(
                            "scripted peer {} exceeds inbound_capacity {} with {} bytes",
                            peer.peer_addr, peer.inbound_capacity, inbound_len
                        );
                    }
                    if peer.write_cap == 0 {
                        panic!("scripted peer {} configured write_cap 0", peer.peer_addr);
                    }
                    ScriptedPeerState {
                        accept_after_step: peer.accept_after_step,
                        peer_addr: peer.peer_addr,
                        inbound_chunks: VecDeque::from(peer.inbound_chunks),
                        read_chunk_cap: peer.read_chunk_cap,
                        write_cap: peer.write_cap,
                        output_capacity: peer.output_capacity,
                        output: Vec::new(),
                    }
                })
                .collect(),
            ready_peers: VecDeque::new(),
            closed: false,
        };
        self.next_listener_id += 1;
        let listener_id = listener.id;
        let local_addr = listener.local_addr;
        self.listeners.push(listener);
        self.deliver_completion(
            call_id,
            CallOutput::TcpBound {
                listener: listener_id,
                local_addr,
            },
        );
    }

    fn handle_tcp_connect(&mut self, call_id: CallId, addr: SocketAddr) {
        let Some(listener_index) = self.ensure_connect_listener(addr) else {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::Io));
            return;
        };

        if self.listeners[listener_index].closed {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        }

        self.promote_ready_peers(listener_index);
        if let Some(peer) = self.listeners[listener_index].ready_peers.pop_front() {
            let (stream, local_addr, peer_addr) = self.open_connected_stream_from_peer(peer, addr);
            self.schedule_tcp_completion(
                call_id,
                TcpResourceKey::TcpConnect(call_id),
                CallOutput::TcpConnected {
                    stream,
                    local_addr,
                    peer_addr,
                },
            );
            return;
        }

        if self.listeners[listener_index].future_peers.is_empty() {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::Io));
            return;
        }

        let insertion_order = self.next_tcp_completion_ordinal;
        self.next_tcp_completion_ordinal += 1;
        self.pending_connects.push(PendingConnect {
            call_id,
            addr,
            insertion_order,
        });
    }

    fn handle_tcp_accept(&mut self, call_id: CallId, listener: ListenerId) {
        let Some(listener_index) = self.listener_index(listener) else {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        };

        if self.listeners[listener_index].closed {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        }

        let resource = TcpResourceKey::ListenerAccept(listener);
        if self.resource_has_active_pending(resource) {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::ResourceBusy));
            return;
        }

        self.promote_ready_peers(listener_index);
        if let Some(peer) = self.listeners[listener_index].ready_peers.pop_front() {
            let (stream, peer_addr) = self.open_stream_from_peer(peer);
            self.schedule_tcp_completion(
                call_id,
                resource,
                CallOutput::TcpAccepted { stream, peer_addr },
            );
            return;
        }

        let insertion_order = self.next_tcp_completion_ordinal;
        self.next_tcp_completion_ordinal += 1;
        self.pending_accepts.push(PendingAccept {
            call_id,
            listener,
            insertion_order,
        });
    }

    fn handle_tcp_read(&mut self, call_id: CallId, stream: tina_runtime::StreamId, max_len: usize) {
        let Some(stream_index) = self.stream_index(stream) else {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        };

        let resource = TcpResourceKey::StreamRead(stream);
        if self.resource_has_active_pending(resource) {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::ResourceBusy));
            return;
        }

        let Some(result) = self.stream_read_result(stream_index, max_len) else {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        };

        self.schedule_tcp_completion(call_id, resource, result);
    }

    fn handle_tcp_write(
        &mut self,
        call_id: CallId,
        stream: tina_runtime::StreamId,
        bytes: Vec<u8>,
    ) {
        let Some(stream_index) = self.stream_index(stream) else {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        };

        let resource = TcpResourceKey::StreamWrite(stream);
        if self.resource_has_active_pending(resource) {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::ResourceBusy));
            return;
        }

        let Some(result) = self.stream_write_result(stream_index, &bytes) else {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        };

        self.schedule_tcp_completion(call_id, resource, result);
    }

    fn handle_tcp_listener_close(&mut self, call_id: CallId, listener: ListenerId) {
        let Some(listener_index) = self.listener_index(listener) else {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        };

        if self.listeners[listener_index].closed {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        }

        // Close wins. Pending accepts are cancelled and traced.
        self.cancel_backend_calls_for_resource(TcpResourceKey::ListenerAccept(listener));

        self.listeners[listener_index].closed = true;
        self.deliver_completion(call_id, CallOutput::TcpListenerClosed);
    }

    fn handle_tcp_stream_close(&mut self, call_id: CallId, stream: tina_runtime::StreamId) {
        let Some(stream_index) = self.stream_index(stream) else {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        };

        if self.streams[stream_index].closed {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        }

        // Close wins. Pending reads/writes are cancelled and traced.
        self.cancel_backend_calls_for_resource(TcpResourceKey::StreamRead(stream));
        self.cancel_backend_calls_for_resource(TcpResourceKey::StreamWrite(stream));

        self.streams[stream_index].closed = true;
        self.deliver_completion(call_id, CallOutput::TcpStreamClosed);
    }

    fn handle_udp_bind(&mut self, call_id: CallId, addr: SocketAddr) {
        if self
            .udp_sockets
            .iter()
            .any(|socket| socket.bind_addr == addr && !socket.closed)
        {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::Io));
            return;
        }

        let Some(script) = self
            .config
            .udp
            .sockets
            .iter()
            .find(|socket| socket.bind_addr == addr)
            .cloned()
        else {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::Io));
            return;
        };

        let ready_len = script
            .inbound_datagrams
            .iter()
            .filter(|datagram| datagram.deliver_after_step <= self.step_ordinal)
            .count();
        if ready_len > script.recv_capacity {
            panic!(
                "scripted UDP socket {} exceeds recv_capacity {} with {} ready datagrams",
                script.local_addr, script.recv_capacity, ready_len
            );
        }

        let socket = UdpSocketState {
            id: UdpSocketId::new(self.next_udp_socket_id),
            bind_addr: script.bind_addr,
            local_addr: script.local_addr,
            recv_capacity: script.recv_capacity,
            future_datagrams: script
                .inbound_datagrams
                .into_iter()
                .map(|datagram| ScriptedUdpDatagramState {
                    deliver_after_step: datagram.deliver_after_step,
                    peer_addr: datagram.peer_addr,
                    bytes: datagram.bytes,
                })
                .collect(),
            ready_datagrams: VecDeque::new(),
            closed: false,
        };
        self.next_udp_socket_id += 1;
        let socket_id = socket.id;
        let local_addr = socket.local_addr;
        self.udp_sockets.push(socket);
        self.deliver_completion(
            call_id,
            CallOutput::UdpBound {
                socket: socket_id,
                local_addr,
            },
        );
    }

    fn handle_udp_send_to(
        &mut self,
        call_id: CallId,
        socket: UdpSocketId,
        peer: SocketAddr,
        bytes: Vec<u8>,
    ) {
        let Some(source_index) = self.udp_socket_index(socket) else {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        };
        if self.udp_sockets[source_index].closed {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        }

        let source_addr = self.udp_sockets[source_index].local_addr;
        if let Some(target_index) = self
            .udp_sockets
            .iter()
            .position(|entry| !entry.closed && entry.local_addr == peer)
        {
            if self.udp_sockets[target_index].ready_datagrams.len()
                + self.udp_sockets[target_index].future_datagrams.len()
                >= self.udp_sockets[target_index].recv_capacity
            {
                self.deliver_completion(call_id, CallOutput::Failed(CallError::ResourceBusy));
                return;
            }

            self.udp_sockets[target_index]
                .future_datagrams
                .push_back(ScriptedUdpDatagramState {
                    deliver_after_step: self.step_ordinal + 1,
                    peer_addr: source_addr,
                    bytes: bytes.clone(),
                });
        }

        self.schedule_udp_completion(
            call_id,
            TcpResourceKey::UdpSend(call_id),
            CallOutput::UdpSent { count: bytes.len() },
        );
    }

    fn handle_udp_recv_from(&mut self, call_id: CallId, socket: UdpSocketId, max_len: usize) {
        let Some(socket_index) = self.udp_socket_index(socket) else {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        };
        if self.udp_sockets[socket_index].closed {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        }

        let resource = TcpResourceKey::UdpRecv(socket);
        if self.resource_has_active_pending(resource) {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::ResourceBusy));
            return;
        }

        self.promote_ready_udp_datagrams(socket_index);
        if let Some(result) = self.udp_recv_result(socket_index, max_len) {
            self.schedule_udp_completion(call_id, resource, result);
            return;
        }

        let insertion_order = self.next_tcp_completion_ordinal;
        self.next_tcp_completion_ordinal += 1;
        self.pending_udp_recvs.push(PendingUdpRecv {
            call_id,
            socket,
            max_len,
            insertion_order,
        });
    }

    fn handle_udp_socket_close(&mut self, call_id: CallId, socket: UdpSocketId) {
        let Some(socket_index) = self.udp_socket_index(socket) else {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        };
        if self.udp_sockets[socket_index].closed {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        }
        // Close wins. Pending recv is cancelled and traced.
        self.cancel_backend_calls_for_resource(TcpResourceKey::UdpRecv(socket));
        self.udp_sockets[socket_index].closed = true;
        self.deliver_completion(call_id, CallOutput::UdpSocketClosed);
    }

    fn handle_dns_lookup(&mut self, call_id: CallId, host: String, port: u16, timeout: Duration) {
        if timeout.is_zero() {
            self.deliver_completion_at(
                call_id,
                CallOutput::Failed(CallError::Timeout),
                self.step_ordinal + 1,
            );
            return;
        }

        let Some(script) = self
            .config
            .dns
            .lookups
            .iter()
            .find(|lookup| lookup.host == host && lookup.port == port)
            .cloned()
        else {
            self.deliver_completion_at(
                call_id,
                CallOutput::Failed(CallError::Io),
                self.step_ordinal + 1,
            );
            return;
        };

        let result = match script.result {
            ScriptedDnsResult::Resolved(addrs) => CallOutput::DnsResolved { addrs },
            ScriptedDnsResult::Failed => CallOutput::Failed(CallError::Io),
            ScriptedDnsResult::Timeout => CallOutput::Failed(CallError::Timeout),
        };
        self.schedule_dns_completion(
            call_id,
            TcpResourceKey::Dns(call_id),
            script.complete_after_step.max(self.step_ordinal + 1),
            result,
        );
    }

    fn handle_tls_connect(
        &mut self,
        call_id: CallId,
        addr: SocketAddr,
        server_name: String,
        _root_certificates: Vec<Vec<u8>>,
        timeout: Duration,
    ) {
        if timeout.is_zero() {
            self.deliver_completion_at(
                call_id,
                CallOutput::Failed(CallError::Timeout),
                self.step_ordinal + 1,
            );
            return;
        }

        let Some(script) = self
            .config
            .tls
            .connects
            .iter()
            .find(|connect| connect.addr == addr && connect.server_name == server_name)
            .cloned()
        else {
            self.deliver_completion_at(
                call_id,
                CallOutput::Failed(CallError::Io),
                self.step_ordinal + 1,
            );
            return;
        };

        if self
            .pending_tcp_completions
            .iter()
            .filter(|completion| completion.resource.is_tls())
            .count()
            >= self.config.tls.pending_completion_capacity
        {
            self.schedule_tls_completion(
                call_id,
                TcpResourceKey::TlsConnect(call_id),
                self.step_ordinal + 1,
                CallOutput::Failed(CallError::TlsFull),
            );
            return;
        }

        let result = match script.result {
            ScriptedTlsConnectResult::Connected { reads, writes } => {
                let stream = TlsStreamId::new(self.next_tls_stream_id);
                self.next_tls_stream_id += 1;
                self.tls_streams.push(TlsStreamState {
                    id: stream,
                    reads: VecDeque::from(reads),
                    writes: VecDeque::from(writes),
                    closed: false,
                    pending_connect_call: Some(call_id),
                });
                CallOutput::TlsConnected { stream }
            }
            ScriptedTlsConnectResult::Failed => CallOutput::Failed(CallError::Io),
            ScriptedTlsConnectResult::Certificate => CallOutput::Failed(CallError::TlsCertificate),
            ScriptedTlsConnectResult::Name => CallOutput::Failed(CallError::TlsName),
            ScriptedTlsConnectResult::Timeout => CallOutput::Failed(CallError::Timeout),
        };
        self.schedule_tls_completion(
            call_id,
            TcpResourceKey::TlsConnect(call_id),
            script.complete_after_step.max(self.step_ordinal + 1),
            result,
        );
    }

    fn handle_tls_bind(&mut self, call_id: CallId, addr: SocketAddr) {
        if self
            .tls_listeners
            .iter()
            .any(|listener| listener.local_addr == addr && !listener.closed)
        {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::Io));
            return;
        }
        let listener = TlsListenerId::new(self.next_tls_listener_id);
        self.next_tls_listener_id += 1;
        let local_addr = if addr.port() == 0 {
            SocketAddr::new(addr.ip(), 40_000 + listener.get() as u16)
        } else {
            addr
        };
        self.tls_listeners.push(TlsListenerState {
            id: listener,
            local_addr,
            closed: false,
        });
        self.deliver_completion(
            call_id,
            CallOutput::TlsBound {
                listener,
                local_addr,
            },
        );
    }

    fn handle_tls_accept(&mut self, call_id: CallId, listener: TlsListenerId, timeout: Duration) {
        if timeout.is_zero() {
            self.deliver_completion_at(
                call_id,
                CallOutput::Failed(CallError::Timeout),
                self.step_ordinal + 1,
            );
            return;
        }
        let Some(listener_index) = self.tls_listener_index(listener) else {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        };
        if self.tls_listeners[listener_index].closed {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::TlsClosed));
            return;
        }
        let resource = TcpResourceKey::TlsAccept(listener);
        if self.resource_has_active_pending(resource) {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::ResourceBusy));
            return;
        }
        if self
            .pending_tcp_completions
            .iter()
            .filter(|completion| completion.resource.is_tls())
            .count()
            >= self.config.tls.pending_completion_capacity
        {
            self.schedule_tls_completion(
                call_id,
                resource,
                self.step_ordinal + 1,
                CallOutput::Failed(CallError::TlsFull),
            );
            return;
        }
        let stream = TlsStreamId::new(self.next_tls_stream_id);
        self.next_tls_stream_id += 1;
        self.tls_streams.push(TlsStreamState {
            id: stream,
            reads: VecDeque::new(),
            writes: VecDeque::new(),
            closed: false,
            pending_connect_call: Some(call_id),
        });
        self.schedule_tls_completion(
            call_id,
            resource,
            self.step_ordinal + 1,
            CallOutput::TlsAccepted {
                stream,
                peer_addr: "127.0.0.1:50000".parse().expect("scripted peer addr"),
            },
        );
    }

    fn handle_tls_listener_close(&mut self, call_id: CallId, listener: TlsListenerId) {
        let Some(listener_index) = self.tls_listener_index(listener) else {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        };
        if self.tls_listeners[listener_index].closed {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::TlsClosed));
            return;
        }
        if self.resource_has_active_pending(TcpResourceKey::TlsAccept(listener)) {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::ResourceBusy));
            return;
        }
        self.tls_listeners[listener_index].closed = true;
        self.deliver_completion(call_id, CallOutput::TlsListenerClosed);
    }

    fn handle_tls_read(
        &mut self,
        call_id: CallId,
        stream: TlsStreamId,
        max_len: usize,
        timeout: Duration,
    ) {
        if timeout.is_zero() {
            self.deliver_completion_at(
                call_id,
                CallOutput::Failed(CallError::Timeout),
                self.step_ordinal + 1,
            );
            return;
        }

        let Some(stream_index) = self.tls_stream_index(stream) else {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        };
        if self.tls_streams[stream_index].closed {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        }
        if self.tls_streams[stream_index]
            .pending_connect_call
            .is_some()
        {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        }
        let resource = TcpResourceKey::TlsStream(stream);
        if self.resource_has_active_pending(resource) {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::ResourceBusy));
            return;
        }

        let result = match self.tls_streams[stream_index].reads.pop_front() {
            Some(ScriptedTlsReadResult::Bytes(bytes)) => CallOutput::TlsRead {
                bytes: bytes.into_iter().take(max_len).collect(),
            },
            Some(ScriptedTlsReadResult::Eof) => CallOutput::TlsRead { bytes: Vec::new() },
            Some(ScriptedTlsReadResult::Failed) => CallOutput::Failed(CallError::Io),
            Some(ScriptedTlsReadResult::Timeout) => CallOutput::Failed(CallError::Timeout),
            None => CallOutput::Failed(CallError::Io),
        };
        self.schedule_tls_completion(call_id, resource, self.step_ordinal + 1, result);
    }

    fn handle_tls_write(
        &mut self,
        call_id: CallId,
        stream: TlsStreamId,
        bytes: Vec<u8>,
        timeout: Duration,
    ) {
        if timeout.is_zero() {
            self.deliver_completion_at(
                call_id,
                CallOutput::Failed(CallError::Timeout),
                self.step_ordinal + 1,
            );
            return;
        }

        let Some(stream_index) = self.tls_stream_index(stream) else {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        };
        if self.tls_streams[stream_index].closed {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        }
        if self.tls_streams[stream_index]
            .pending_connect_call
            .is_some()
        {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        }
        let resource = TcpResourceKey::TlsStream(stream);
        if self.resource_has_active_pending(resource) {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::ResourceBusy));
            return;
        }

        let result = match self.tls_streams[stream_index].writes.pop_front() {
            Some(ScriptedTlsWriteResult::Wrote(count)) => CallOutput::TlsWrote {
                count: count.min(bytes.len()),
            },
            Some(ScriptedTlsWriteResult::Failed) => CallOutput::Failed(CallError::Io),
            Some(ScriptedTlsWriteResult::Timeout) => CallOutput::Failed(CallError::Timeout),
            None => CallOutput::Failed(CallError::Io),
        };
        self.schedule_tls_completion(call_id, resource, self.step_ordinal + 1, result);
    }

    fn handle_tls_close(&mut self, call_id: CallId, stream: TlsStreamId, timeout: Duration) {
        if timeout.is_zero() {
            self.deliver_completion_at(
                call_id,
                CallOutput::Failed(CallError::Timeout),
                self.step_ordinal + 1,
            );
            return;
        }

        let Some(stream_index) = self.tls_stream_index(stream) else {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        };
        if self.tls_streams[stream_index].closed {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        }
        if self.tls_streams[stream_index]
            .pending_connect_call
            .is_some()
        {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        }
        let resource = TcpResourceKey::TlsStream(stream);
        if self.resource_has_active_pending(resource) {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::ResourceBusy));
            return;
        }

        self.tls_streams[stream_index].closed = true;
        self.schedule_tls_completion(
            call_id,
            resource,
            self.step_ordinal + 1,
            CallOutput::TlsClosed,
        );
    }

    fn handle_signal_wait(&mut self, call_id: CallId, name: String, timeout: Duration) {
        if timeout.is_zero() {
            self.deliver_completion_at(
                call_id,
                CallOutput::Failed(CallError::Timeout),
                self.step_ordinal + 1,
            );
            return;
        }

        let Some(script) = self
            .config
            .signal
            .events
            .iter()
            .find(|event| event.name == name)
            .cloned()
        else {
            self.deliver_completion_at(
                call_id,
                CallOutput::Failed(CallError::Io),
                self.step_ordinal + 1,
            );
            return;
        };

        let result = match script.result {
            ScriptedSignalResult::Received => CallOutput::SignalReceived { name: script.name },
            ScriptedSignalResult::Failed => CallOutput::Failed(CallError::Io),
            ScriptedSignalResult::Timeout => CallOutput::Failed(CallError::Timeout),
        };
        self.schedule_signal_completion(
            call_id,
            TcpResourceKey::Signal(call_id),
            script.deliver_after_step.max(self.step_ordinal + 1),
            result,
        );
    }

    fn handle_process_run(
        &mut self,
        call_id: CallId,
        command: String,
        args: Vec<String>,
        timeout: Duration,
        stdout_limit: usize,
        stderr_limit: usize,
    ) {
        if timeout.is_zero() {
            self.deliver_completion_at(
                call_id,
                CallOutput::Failed(CallError::Timeout),
                self.step_ordinal + 1,
            );
            return;
        }

        let Some(script) = self
            .config
            .process
            .runs
            .iter()
            .find(|run| run.command == command && run.args == args)
            .cloned()
        else {
            self.deliver_completion_at(
                call_id,
                CallOutput::Failed(CallError::Io),
                self.step_ordinal + 1,
            );
            return;
        };

        let result = match script.result {
            ScriptedProcessResult::Exited {
                code,
                stdout,
                stderr,
            } => {
                let stdout_truncated = stdout.len() > stdout_limit;
                let stderr_truncated = stderr.len() > stderr_limit;
                CallOutput::ProcessExited {
                    status: ProcessStatus { code },
                    stdout: stdout.into_iter().take(stdout_limit).collect(),
                    stderr: stderr.into_iter().take(stderr_limit).collect(),
                    stdout_truncated,
                    stderr_truncated,
                }
            }
            ScriptedProcessResult::Failed => CallOutput::Failed(CallError::Io),
            ScriptedProcessResult::Timeout => CallOutput::Failed(CallError::Timeout),
            ScriptedProcessResult::KillUncertain => CallOutput::Failed(CallError::KillUncertain),
        };
        self.schedule_process_completion(
            call_id,
            TcpResourceKey::Process(call_id),
            script.complete_after_step.max(self.step_ordinal + 1),
            result,
        );
    }

    fn handle_file_open(
        &mut self,
        call_id: CallId,
        path: PathBuf,
        options: tina_runtime::FileOpenOptions,
    ) {
        if !options.read && !options.write {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::Io));
            return;
        }
        if (options.create || options.truncate) && !options.write {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::Io));
            return;
        }
        if !self.file_storage.contains_key(&path) && !options.create {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::Io));
            return;
        }
        if options.create && !self.parent_directory_exists(&path) {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::Io));
            return;
        }

        if options.truncate || !self.file_storage.contains_key(&path) {
            self.file_storage.insert(path.clone(), Vec::new());
        }
        let id = tina_runtime::FileId::new(self.next_file_id);
        self.next_file_id += 1;
        self.files.push(FileState {
            id,
            path,
            options,
            closed: false,
        });
        self.deliver_completion(call_id, CallOutput::FileOpened { file: id });
    }

    fn handle_file_read_at(
        &mut self,
        call_id: CallId,
        file: tina_runtime::FileId,
        len: usize,
        offset: u64,
    ) {
        let Some(index) = self.file_index(file) else {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        };
        if self.file_has_active_pending(file) {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::ResourceBusy));
            return;
        }
        if !self.files[index].options.read {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::Io));
            return;
        }
        let start = offset as usize;
        let bytes = self
            .file_storage
            .get(&self.files[index].path)
            .unwrap_or_else(|| panic!("open file has no simulated storage"))
            .get(start..)
            .unwrap_or(&[])
            .iter()
            .take(len)
            .copied()
            .collect();
        self.schedule_tcp_completion(
            call_id,
            TcpResourceKey::FileRead(file),
            CallOutput::FileRead { bytes },
        );
    }

    fn handle_file_write_at(
        &mut self,
        call_id: CallId,
        file: tina_runtime::FileId,
        bytes: Vec<u8>,
        offset: u64,
    ) {
        let Some(index) = self.file_index(file) else {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        };
        if self.file_has_active_pending(file) {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::ResourceBusy));
            return;
        }
        if !self.files[index].options.write {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::Io));
            return;
        }
        let path = self.files[index].path.clone();
        let start = offset as usize;
        let end = start.saturating_add(bytes.len());
        let storage = self
            .file_storage
            .get_mut(&path)
            .unwrap_or_else(|| panic!("open file has no simulated storage"));
        if storage.len() < end {
            storage.resize(end, 0);
        }
        storage[start..end].copy_from_slice(&bytes);
        self.schedule_tcp_completion(
            call_id,
            TcpResourceKey::FileWrite(file),
            CallOutput::FileWrote { count: bytes.len() },
        );
    }

    fn handle_file_fsync(&mut self, call_id: CallId, file: tina_runtime::FileId) {
        if self.file_index(file).is_none() {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        }
        let index = self.file_index(file).expect("file checked above");
        if !self.files[index].options.write {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::Io));
            return;
        }
        if self.file_has_active_pending(file) {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::ResourceBusy));
            return;
        }
        self.schedule_tcp_completion(
            call_id,
            TcpResourceKey::FileFsync(file),
            CallOutput::FileSynced,
        );
    }

    fn handle_file_size(&mut self, call_id: CallId, file: tina_runtime::FileId) {
        let Some(index) = self.file_index(file) else {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        };
        if self.file_has_active_pending(file) {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::ResourceBusy));
            return;
        }
        let size = self
            .file_storage
            .get(&self.files[index].path)
            .unwrap_or_else(|| panic!("open file has no simulated storage"))
            .len() as u64;
        self.schedule_tcp_completion(
            call_id,
            TcpResourceKey::FileSize(file),
            CallOutput::FileSize { size },
        );
    }

    fn handle_file_close(&mut self, call_id: CallId, file: tina_runtime::FileId) {
        if self.file_has_active_pending(file) {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::ResourceBusy));
            return;
        }
        let Some(index) = self.file_index(file) else {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        };
        self.files[index].closed = true;
        self.deliver_completion(call_id, CallOutput::FileClosed);
    }

    fn handle_mkdir(&mut self, call_id: CallId, path: PathBuf) {
        if self.directories.iter().any(|existing| existing == &path) {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::Io));
            return;
        }
        if !self.parent_directory_exists(&path) {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::Io));
            return;
        }
        self.directories.push(path);
        self.schedule_tcp_completion(
            call_id,
            TcpResourceKey::Mkdir(call_id),
            CallOutput::DirectoryCreated,
        );
    }

    fn handle_path_metadata(&mut self, call_id: CallId, path: PathBuf) {
        let metadata = if let Some(bytes) = self.file_storage.get(&path) {
            PathMetadata {
                kind: PathKind::File,
                len: Some(bytes.len() as u64),
            }
        } else if self.directories.iter().any(|directory| directory == &path) {
            PathMetadata {
                kind: PathKind::Directory,
                len: None,
            }
        } else {
            PathMetadata::missing()
        };
        self.schedule_tcp_completion(
            call_id,
            TcpResourceKey::PathOp(call_id),
            CallOutput::PathMetadata { metadata },
        );
    }

    fn handle_rename_replace(&mut self, call_id: CallId, from: PathBuf, to: PathBuf) {
        if !self.parent_directory_exists(&to) {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::Io));
            return;
        }
        if let Some(bytes) = self.file_storage.remove(&from) {
            self.file_storage.insert(to, bytes);
            self.schedule_tcp_completion(
                call_id,
                TcpResourceKey::PathOp(call_id),
                CallOutput::PathRenamed,
            );
            return;
        }
        if let Some(index) = self
            .directories
            .iter()
            .position(|directory| directory == &from)
        {
            if self.file_storage.contains_key(&to)
                || self.directories.iter().any(|directory| directory == &to)
            {
                self.deliver_completion(call_id, CallOutput::Failed(CallError::Io));
                return;
            }
            self.directories[index] = to;
            self.schedule_tcp_completion(
                call_id,
                TcpResourceKey::PathOp(call_id),
                CallOutput::PathRenamed,
            );
            return;
        }
        self.deliver_completion(call_id, CallOutput::Failed(CallError::NotFound));
    }

    fn handle_remove_file(&mut self, call_id: CallId, path: PathBuf) {
        if self.file_storage.remove(&path).is_some() {
            self.schedule_tcp_completion(
                call_id,
                TcpResourceKey::PathOp(call_id),
                CallOutput::FileRemoved,
            );
        } else {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::NotFound));
        }
    }

    fn handle_read_dir(&mut self, call_id: CallId, path: PathBuf) {
        if !self.directories.iter().any(|directory| directory == &path)
            && path != std::path::Path::new("/")
            && path != std::path::Path::new("/tmp")
        {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::NotFound));
            return;
        }
        let mut entries = Vec::new();
        for file in self.file_storage.keys() {
            if file.parent() == Some(path.as_path()) {
                entries.push(file.clone());
            }
        }
        for directory in &self.directories {
            if directory.parent() == Some(path.as_path()) {
                entries.push(directory.clone());
            }
        }
        entries.sort();
        entries.dedup();
        self.schedule_tcp_completion(
            call_id,
            TcpResourceKey::PathOp(call_id),
            CallOutput::DirectoryRead { entries },
        );
    }

    fn handle_sync_parent(&mut self, call_id: CallId, path: PathBuf) {
        if self.parent_directory_exists(&path) {
            self.schedule_tcp_completion(
                call_id,
                TcpResourceKey::PathOp(call_id),
                CallOutput::ParentSynced,
            );
        } else {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::NotFound));
        }
    }

    fn handle_snapshot_commit(
        &mut self,
        call_id: CallId,
        path: PathBuf,
        bytes: Vec<u8>,
        last_journal_index: u64,
    ) {
        let storage_ordinal = self.next_storage_ordinal();
        if !self.parent_directory_exists(&path) {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::Io));
            return;
        }
        if self.config.storage.fail_snapshot_commit_at == Some(storage_ordinal) {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::Io));
            return;
        }
        let snapshot = tina_runtime::SnapshotImage {
            bytes,
            last_journal_index,
        };
        self.file_storage
            .insert(path, tina_runtime::persistence::encode_snapshot(&snapshot));
        if self.config.storage.commit_uncertain_snapshot_at == Some(storage_ordinal) {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::CommitUncertain));
            return;
        }
        self.deliver_completion(call_id, CallOutput::SnapshotCommitted);
    }

    fn handle_snapshot_load(&mut self, call_id: CallId, path: PathBuf) {
        let result = match self.file_storage.get(&path) {
            Some(bytes) => tina_runtime::persistence::decode_snapshot(bytes).map(Some),
            None => Ok(None),
        };
        self.deliver_completion(
            call_id,
            match result {
                Ok(snapshot) => CallOutput::SnapshotLoaded { snapshot },
                Err(reason) => CallOutput::Failed(reason),
            },
        );
    }

    fn handle_journal_append(
        &mut self,
        call_id: CallId,
        path: PathBuf,
        record_index: u64,
        bytes: Vec<u8>,
    ) {
        let storage_ordinal = self.next_storage_ordinal();
        if !self.parent_directory_exists(&path) {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::Io));
            return;
        }
        if self.config.storage.fail_journal_append_at == Some(storage_ordinal) {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::Io));
            return;
        }
        if let Some(existing) = self.file_storage.get(&path) {
            let replay = match tina_runtime::persistence::replay_journal_bytes(existing) {
                Ok(replay) => replay,
                Err(reason) => {
                    self.deliver_completion(call_id, CallOutput::Failed(reason));
                    return;
                }
            };
            if replay.warning.is_some()
                || replay
                    .records
                    .last()
                    .is_some_and(|last| record_index <= last.index)
            {
                self.deliver_completion(call_id, CallOutput::Failed(CallError::CorruptRecord));
                return;
            }
        }
        let record = tina_runtime::JournalRecord {
            index: record_index,
            bytes,
        };
        self.file_storage
            .entry(path.clone())
            .or_default()
            .extend(tina_runtime::persistence::encode_journal_record(&record));
        if self.config.storage.truncate_journal_tail_at == Some(storage_ordinal) {
            if let Some(stored) = self.file_storage.get_mut(&path) {
                stored.pop();
            }
        }
        if self.config.storage.corrupt_journal_record_at == Some(storage_ordinal)
            && let Some(stored) = self.file_storage.get_mut(&path)
            && let Some(last) = stored.last_mut()
        {
            *last ^= 0xff;
        }
        self.deliver_completion(call_id, CallOutput::JournalAppended { record_index });
    }

    fn next_storage_ordinal(&mut self) -> u64 {
        let ordinal = self.next_storage_ordinal;
        self.next_storage_ordinal += 1;
        ordinal
    }

    fn handle_journal_replay(&mut self, call_id: CallId, path: PathBuf) {
        let result = match self.file_storage.get(&path) {
            Some(bytes) => tina_runtime::persistence::replay_journal_bytes(bytes),
            None => Ok(tina_runtime::JournalReplay {
                records: Vec::new(),
                warning: None,
            }),
        };
        self.deliver_completion(
            call_id,
            match result {
                Ok(replay) => CallOutput::JournalReplayed { replay },
                Err(reason) => CallOutput::Failed(reason),
            },
        );
    }

    fn parent_directory_exists(&self, path: &std::path::Path) -> bool {
        let Some(parent) = path.parent() else {
            return true;
        };
        if parent.as_os_str().is_empty()
            || parent == std::path::Path::new("/")
            || parent == std::path::Path::new("/tmp")
        {
            return true;
        }
        self.directories
            .iter()
            .any(|directory| directory.as_path() == parent)
    }

    fn resource_has_active_pending(&self, resource: TcpResourceKey) -> bool {
        let queued_accept = match resource {
            TcpResourceKey::ListenerAccept(listener) => self
                .pending_accepts
                .iter()
                .any(|pending| pending.listener == listener),
            TcpResourceKey::UdpRecv(socket) => self
                .pending_udp_recvs
                .iter()
                .any(|pending| pending.socket == socket),
            TcpResourceKey::TcpConnect(_)
            | TcpResourceKey::StreamRead(_)
            | TcpResourceKey::StreamWrite(_)
            | TcpResourceKey::UdpSend(_)
            | TcpResourceKey::Dns(_)
            | TcpResourceKey::TlsConnect(_)
            | TcpResourceKey::TlsAccept(_)
            | TcpResourceKey::TlsStream(_)
            | TcpResourceKey::Signal(_)
            | TcpResourceKey::Process(_)
            | TcpResourceKey::FileRead(_)
            | TcpResourceKey::FileWrite(_)
            | TcpResourceKey::FileFsync(_)
            | TcpResourceKey::FileSize(_)
            | TcpResourceKey::Mkdir(_)
            | TcpResourceKey::PathOp(_) => false,
        };
        queued_accept
            || self
                .pending_tcp_completions
                .iter()
                .any(|pending| pending.resource == resource)
    }

    fn udp_socket_index(&self, socket: UdpSocketId) -> Option<usize> {
        self.udp_sockets.iter().position(|state| state.id == socket)
    }

    fn file_index(&self, file: tina_runtime::FileId) -> Option<usize> {
        self.files
            .iter()
            .position(|state| state.id == file && !state.closed)
    }

    fn tls_stream_index(&self, stream: TlsStreamId) -> Option<usize> {
        self.tls_streams.iter().position(|state| state.id == stream)
    }

    fn tls_listener_index(&self, listener: TlsListenerId) -> Option<usize> {
        self.tls_listeners
            .iter()
            .position(|state| state.id == listener)
    }

    fn file_has_active_pending(&self, file: tina_runtime::FileId) -> bool {
        self.resource_has_active_pending(TcpResourceKey::FileRead(file))
            || self.resource_has_active_pending(TcpResourceKey::FileWrite(file))
            || self.resource_has_active_pending(TcpResourceKey::FileFsync(file))
            || self.resource_has_active_pending(TcpResourceKey::FileSize(file))
    }

    fn schedule_tcp_completion(
        &mut self,
        call_id: CallId,
        resource: TcpResourceKey,
        result: CallOutput,
    ) {
        let pending_non_udp = self
            .pending_tcp_completions
            .iter()
            .filter(|completion| {
                !completion.resource.is_udp()
                    && !completion.resource.is_dns()
                    && !completion.resource.is_tls()
                    && !completion.resource.is_signal()
                    && !completion.resource.is_process()
            })
            .count();
        if pending_non_udp >= self.config.tcp.pending_completion_capacity {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::Io));
            return;
        }

        let insertion_order = self.next_tcp_completion_ordinal;
        self.next_tcp_completion_ordinal += 1;
        let mut ready_at_step = self.step_ordinal + 1 + self.tcp_delay_steps(insertion_order);
        if let Some(previous_ready) = self
            .pending_tcp_completions
            .iter()
            .filter(|pending| pending.resource == resource)
            .map(|pending| pending.ready_at_step)
            .max()
        {
            ready_at_step = ready_at_step.max(previous_ready);
        }
        self.pending_tcp_completions.push(PendingTcpCompletion {
            call_id,
            ready_at_step,
            insertion_order,
            resource,
            result,
        });
    }

    fn schedule_udp_completion(
        &mut self,
        call_id: CallId,
        resource: TcpResourceKey,
        result: CallOutput,
    ) {
        let pending_udp = self
            .pending_tcp_completions
            .iter()
            .filter(|completion| completion.resource.is_udp())
            .count();
        if pending_udp >= self.config.udp.pending_completion_capacity {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::Io));
            return;
        }

        let insertion_order = self.next_tcp_completion_ordinal;
        self.next_tcp_completion_ordinal += 1;
        self.pending_tcp_completions.push(PendingTcpCompletion {
            call_id,
            ready_at_step: self.step_ordinal + 1,
            insertion_order,
            resource,
            result,
        });
    }

    fn schedule_dns_completion(
        &mut self,
        call_id: CallId,
        resource: TcpResourceKey,
        ready_at_step: u64,
        result: CallOutput,
    ) {
        let pending_dns = self
            .pending_tcp_completions
            .iter()
            .filter(|completion| completion.resource.is_dns())
            .count();
        if pending_dns >= self.config.dns.pending_completion_capacity {
            let insertion_order = self.next_tcp_completion_ordinal;
            self.next_tcp_completion_ordinal += 1;
            self.pending_tcp_completions.push(PendingTcpCompletion {
                call_id,
                ready_at_step: self.step_ordinal + 1,
                insertion_order,
                resource,
                result: CallOutput::Failed(CallError::DnsFull),
            });
            return;
        }

        let insertion_order = self.next_tcp_completion_ordinal;
        self.next_tcp_completion_ordinal += 1;
        self.pending_tcp_completions.push(PendingTcpCompletion {
            call_id,
            ready_at_step,
            insertion_order,
            resource,
            result,
        });
    }

    fn schedule_tls_completion(
        &mut self,
        call_id: CallId,
        resource: TcpResourceKey,
        ready_at_step: u64,
        result: CallOutput,
    ) {
        let pending_tls = self
            .pending_tcp_completions
            .iter()
            .filter(|completion| completion.resource.is_tls())
            .count();
        if pending_tls >= self.config.tls.pending_completion_capacity {
            let insertion_order = self.next_tcp_completion_ordinal;
            self.next_tcp_completion_ordinal += 1;
            self.pending_tcp_completions.push(PendingTcpCompletion {
                call_id,
                ready_at_step: self.step_ordinal + 1,
                insertion_order,
                resource,
                result: CallOutput::Failed(CallError::TlsFull),
            });
            return;
        }

        let insertion_order = self.next_tcp_completion_ordinal;
        self.next_tcp_completion_ordinal += 1;
        self.pending_tcp_completions.push(PendingTcpCompletion {
            call_id,
            ready_at_step,
            insertion_order,
            resource,
            result,
        });
    }

    fn schedule_signal_completion(
        &mut self,
        call_id: CallId,
        resource: TcpResourceKey,
        ready_at_step: u64,
        result: CallOutput,
    ) {
        let pending_signal = self
            .pending_tcp_completions
            .iter()
            .filter(|completion| completion.resource.is_signal())
            .count();
        if pending_signal >= self.config.signal.pending_completion_capacity {
            let insertion_order = self.next_tcp_completion_ordinal;
            self.next_tcp_completion_ordinal += 1;
            self.pending_tcp_completions.push(PendingTcpCompletion {
                call_id,
                ready_at_step: self.step_ordinal + 1,
                insertion_order,
                resource,
                result: CallOutput::Failed(CallError::SignalFull),
            });
            return;
        }

        let insertion_order = self.next_tcp_completion_ordinal;
        self.next_tcp_completion_ordinal += 1;
        self.pending_tcp_completions.push(PendingTcpCompletion {
            call_id,
            ready_at_step,
            insertion_order,
            resource,
            result,
        });
    }

    fn schedule_process_completion(
        &mut self,
        call_id: CallId,
        resource: TcpResourceKey,
        ready_at_step: u64,
        result: CallOutput,
    ) {
        let pending_process = self
            .pending_tcp_completions
            .iter()
            .filter(|completion| completion.resource.is_process())
            .count();
        if pending_process >= self.config.process.pending_completion_capacity {
            let insertion_order = self.next_tcp_completion_ordinal;
            self.next_tcp_completion_ordinal += 1;
            self.pending_tcp_completions.push(PendingTcpCompletion {
                call_id,
                ready_at_step: self.step_ordinal + 1,
                insertion_order,
                resource,
                result: CallOutput::Failed(CallError::ProcessFull),
            });
            return;
        }

        let insertion_order = self.next_tcp_completion_ordinal;
        self.next_tcp_completion_ordinal += 1;
        self.pending_tcp_completions.push(PendingTcpCompletion {
            call_id,
            ready_at_step,
            insertion_order,
            resource,
            result,
        });
    }

    fn listener_index(&self, listener: ListenerId) -> Option<usize> {
        self.listeners.iter().position(|state| state.id == listener)
    }

    fn connect_listener_index(&self, addr: SocketAddr) -> Option<usize> {
        self.listeners.iter().position(|state| {
            !state.closed && (state.bind_addr == addr || state.local_addr == addr)
        })
    }

    fn ensure_connect_listener(&mut self, addr: SocketAddr) -> Option<usize> {
        if let Some(index) = self.connect_listener_index(addr) {
            return Some(index);
        }

        let script = self
            .config
            .tcp
            .listeners
            .iter()
            .find(|listener| listener.bind_addr == addr || listener.local_addr == addr)
            .cloned()?;
        let listener = self.listener_from_script(script);
        let index = self.listeners.len();
        self.listeners.push(listener);
        Some(index)
    }

    fn stream_index(&self, stream: tina_runtime::StreamId) -> Option<usize> {
        self.streams.iter().position(|state| state.id == stream)
    }

    fn listener_from_script(&mut self, script: ScriptedListenerConfig) -> ListenerState {
        let listener = ListenerState {
            id: ListenerId::new(self.next_listener_id),
            bind_addr: script.bind_addr,
            local_addr: script.local_addr,
            backlog_capacity: script.backlog_capacity,
            future_peers: script
                .peers
                .into_iter()
                .map(|peer| {
                    let inbound_len = peer.inbound_chunks.iter().map(Vec::len).sum::<usize>();
                    if inbound_len > peer.inbound_capacity {
                        panic!(
                            "scripted peer {} exceeds inbound_capacity {} with {} bytes",
                            peer.peer_addr, peer.inbound_capacity, inbound_len
                        );
                    }
                    if peer.write_cap == 0 {
                        panic!("scripted peer {} configured write_cap 0", peer.peer_addr);
                    }
                    ScriptedPeerState {
                        accept_after_step: peer.accept_after_step,
                        peer_addr: peer.peer_addr,
                        inbound_chunks: VecDeque::from(peer.inbound_chunks),
                        read_chunk_cap: peer.read_chunk_cap,
                        write_cap: peer.write_cap,
                        output_capacity: peer.output_capacity,
                        output: Vec::new(),
                    }
                })
                .collect(),
            ready_peers: VecDeque::new(),
            closed: false,
        };
        self.next_listener_id += 1;
        listener
    }

    fn promote_ready_peers(&mut self, listener_index: usize) {
        let current_step = self.step_ordinal;
        while self.listeners[listener_index]
            .future_peers
            .front()
            .is_some_and(|peer| peer.accept_after_step <= current_step)
            && self.listeners[listener_index].ready_peers.len()
                < self.listeners[listener_index].backlog_capacity
        {
            let peer = self.listeners[listener_index]
                .future_peers
                .pop_front()
                .unwrap_or_else(|| panic!("listener future peer disappeared"));
            self.listeners[listener_index].ready_peers.push_back(peer);
        }
    }

    fn open_stream_from_peer(
        &mut self,
        peer: ScriptedPeerState,
    ) -> (tina_runtime::StreamId, SocketAddr) {
        let stream = tina_runtime::StreamId::new(self.next_stream_id);
        self.next_stream_id += 1;
        let peer_addr = peer.peer_addr;
        self.streams.push(StreamState {
            id: stream,
            peer_addr,
            inbound_chunks: peer.inbound_chunks,
            read_chunk_cap: peer.read_chunk_cap,
            write_cap: peer.write_cap,
            output_capacity: peer.output_capacity,
            output: peer.output,
            closed: false,
        });
        (stream, peer_addr)
    }

    fn open_connected_stream_from_peer(
        &mut self,
        peer: ScriptedPeerState,
        remote_addr: SocketAddr,
    ) -> (tina_runtime::StreamId, SocketAddr, SocketAddr) {
        let stream = tina_runtime::StreamId::new(self.next_stream_id);
        self.next_stream_id += 1;
        let local_addr = peer.peer_addr;
        self.streams.push(StreamState {
            id: stream,
            peer_addr: remote_addr,
            inbound_chunks: peer.inbound_chunks,
            read_chunk_cap: peer.read_chunk_cap,
            write_cap: peer.write_cap,
            output_capacity: peer.output_capacity,
            output: peer.output,
            closed: false,
        });
        (stream, local_addr, remote_addr)
    }

    fn stream_read_result(&mut self, stream_index: usize, max_len: usize) -> Option<CallOutput> {
        let stream = self.streams.get_mut(stream_index)?;
        if stream.closed {
            return None;
        }

        let mut bytes = Vec::new();
        let read_cap = stream.read_chunk_cap.unwrap_or(max_len);
        let target_len = max_len.min(read_cap);
        if target_len == 0 {
            return Some(CallOutput::TcpRead { bytes });
        }

        while bytes.len() < target_len {
            let Some(mut chunk) = stream.inbound_chunks.pop_front() else {
                break;
            };
            let remaining = target_len - bytes.len();
            if chunk.len() <= remaining {
                bytes.extend_from_slice(&chunk);
            } else {
                bytes.extend_from_slice(&chunk[..remaining]);
                chunk.drain(..remaining);
                stream.inbound_chunks.push_front(chunk);
            }
        }

        Some(CallOutput::TcpRead { bytes })
    }

    fn stream_write_result(&mut self, stream_index: usize, bytes: &[u8]) -> Option<CallOutput> {
        let stream = self.streams.get_mut(stream_index)?;
        if stream.closed {
            return None;
        }

        let remaining_capacity = stream.output_capacity.saturating_sub(stream.output.len());
        let count = bytes.len().min(stream.write_cap).min(remaining_capacity);
        stream.output.extend_from_slice(&bytes[..count]);
        Some(CallOutput::TcpWrote { count })
    }

    fn promote_ready_udp_datagrams(&mut self, socket_index: usize) {
        let current_step = self.step_ordinal;
        while self.udp_sockets[socket_index]
            .future_datagrams
            .front()
            .is_some_and(|datagram| datagram.deliver_after_step <= current_step)
            && self.udp_sockets[socket_index].ready_datagrams.len()
                < self.udp_sockets[socket_index].recv_capacity
        {
            let datagram = self.udp_sockets[socket_index]
                .future_datagrams
                .pop_front()
                .unwrap_or_else(|| panic!("UDP future datagram disappeared"));
            self.udp_sockets[socket_index]
                .ready_datagrams
                .push_back(datagram);
        }
    }

    fn udp_recv_result(&mut self, socket_index: usize, max_len: usize) -> Option<CallOutput> {
        let datagram = self.udp_sockets[socket_index].ready_datagrams.pop_front()?;
        let truncated = datagram.bytes.len() > max_len;
        let bytes = datagram.bytes.into_iter().take(max_len).collect();
        Some(CallOutput::UdpReceived {
            peer_addr: datagram.peer_addr,
            bytes,
            truncated,
        })
    }

    fn harvest_tcp(&mut self) {
        self.activate_pending_connects();
        self.activate_pending_accepts();
        self.activate_pending_udp_recvs();

        let mut ready = Vec::new();
        let mut still_pending = Vec::new();
        for completion in std::mem::take(&mut self.pending_tcp_completions) {
            if completion.ready_at_step <= self.step_ordinal {
                ready.push(completion);
            } else {
                still_pending.push(completion);
            }
        }
        self.pending_tcp_completions = still_pending;
        self.order_ready_tcp_completions(&mut ready);

        let mut batch_offset = 0;
        let mut last_registration_index = None;
        for completion in ready {
            let completed_tls_stream = match &completion.result {
                CallOutput::TlsConnected { stream } | CallOutput::TlsAccepted { stream, .. } => {
                    Some(*stream)
                }
                _ => None,
            };
            if let Some(stream) = completed_tls_stream {
                if let Some(stream_index) = self.tls_stream_index(stream) {
                    if self.tls_streams[stream_index].pending_connect_call
                        == Some(completion.call_id)
                    {
                        self.tls_streams[stream_index].pending_connect_call = None;
                    }
                }
            }
            let registration_index = self.requester_registration_index(completion.call_id);
            if let Some(previous) = last_registration_index {
                if registration_index <= previous {
                    batch_offset += 1;
                }
            }
            self.deliver_completion_at(
                completion.call_id,
                completion.result,
                self.step_ordinal + batch_offset,
            );
            last_registration_index = Some(registration_index);
        }
    }

    fn activate_pending_accepts(&mut self) {
        let mut survivors = Vec::new();
        let mut pending = std::mem::take(&mut self.pending_accepts);
        pending.sort_by_key(|entry| entry.insertion_order);
        for pending_accept in pending {
            let Some(listener_index) = self.listener_index(pending_accept.listener) else {
                self.deliver_completion(
                    pending_accept.call_id,
                    CallOutput::Failed(CallError::InvalidResource),
                );
                continue;
            };
            if self.listeners[listener_index].closed {
                self.deliver_completion(
                    pending_accept.call_id,
                    CallOutput::Failed(CallError::InvalidResource),
                );
                continue;
            }

            self.promote_ready_peers(listener_index);
            if let Some(peer) = self.listeners[listener_index].ready_peers.pop_front() {
                let (stream, peer_addr) = self.open_stream_from_peer(peer);
                self.schedule_tcp_completion(
                    pending_accept.call_id,
                    TcpResourceKey::ListenerAccept(pending_accept.listener),
                    CallOutput::TcpAccepted { stream, peer_addr },
                );
            } else {
                survivors.push(pending_accept);
            }
        }
        self.pending_accepts = survivors;
    }

    fn activate_pending_connects(&mut self) {
        let mut survivors = Vec::new();
        let mut pending = std::mem::take(&mut self.pending_connects);
        pending.sort_by_key(|entry| entry.insertion_order);
        for pending_connect in pending {
            let Some(listener_index) = self.ensure_connect_listener(pending_connect.addr) else {
                self.deliver_completion(pending_connect.call_id, CallOutput::Failed(CallError::Io));
                continue;
            };
            if self.listeners[listener_index].closed {
                self.deliver_completion(
                    pending_connect.call_id,
                    CallOutput::Failed(CallError::InvalidResource),
                );
                continue;
            }

            self.promote_ready_peers(listener_index);
            if let Some(peer) = self.listeners[listener_index].ready_peers.pop_front() {
                let (stream, local_addr, peer_addr) =
                    self.open_connected_stream_from_peer(peer, pending_connect.addr);
                self.schedule_tcp_completion(
                    pending_connect.call_id,
                    TcpResourceKey::TcpConnect(pending_connect.call_id),
                    CallOutput::TcpConnected {
                        stream,
                        local_addr,
                        peer_addr,
                    },
                );
            } else {
                survivors.push(pending_connect);
            }
        }
        self.pending_connects = survivors;
    }

    fn activate_pending_udp_recvs(&mut self) {
        let mut survivors = Vec::new();
        let mut pending = std::mem::take(&mut self.pending_udp_recvs);
        pending.sort_by_key(|entry| entry.insertion_order);
        for pending_recv in pending {
            let Some(socket_index) = self.udp_socket_index(pending_recv.socket) else {
                self.deliver_completion(
                    pending_recv.call_id,
                    CallOutput::Failed(CallError::InvalidResource),
                );
                continue;
            };
            if self.udp_sockets[socket_index].closed {
                self.deliver_completion(
                    pending_recv.call_id,
                    CallOutput::Failed(CallError::InvalidResource),
                );
                continue;
            }
            self.promote_ready_udp_datagrams(socket_index);
            if let Some(result) = self.udp_recv_result(socket_index, pending_recv.max_len) {
                self.schedule_udp_completion(
                    pending_recv.call_id,
                    TcpResourceKey::UdpRecv(pending_recv.socket),
                    result,
                );
            } else {
                survivors.push(pending_recv);
            }
        }
        self.pending_udp_recvs = survivors;
    }

    fn order_ready_tcp_completions(&self, ready: &mut [PendingTcpCompletion]) {
        ready.sort_by(|left, right| {
            left.ready_at_step
                .cmp(&right.ready_at_step)
                .then_with(|| left.insertion_order.cmp(&right.insertion_order))
        });

        if let TcpCompletionFaultMode::ReorderReady { one_in } = self.config.faults.tcp_completion {
            if one_in == 0 {
                return;
            }

            let mut start = 0;
            let mut batch_index = 0_u64;
            while start < ready.len() {
                let ready_at_step = ready[start].ready_at_step;
                let mut end = start + 1;
                while end < ready.len() && ready[end].ready_at_step == ready_at_step {
                    end += 1;
                }

                let selector = self
                    .config
                    .seed
                    .wrapping_add(0x5443_5052)
                    .wrapping_add(batch_index)
                    % one_in;
                if selector == 0 && end - start > 1 {
                    ready[start..end].sort_by(|left, right| {
                        right
                            .resource
                            .cmp(&left.resource)
                            .then_with(|| left.insertion_order.cmp(&right.insertion_order))
                    });
                }

                start = end;
                batch_index += 1;
            }
        }
    }

    fn tcp_delay_steps(&self, insertion_order: u64) -> u64 {
        match self.config.faults.tcp_completion {
            TcpCompletionFaultMode::None | TcpCompletionFaultMode::ReorderReady { .. } => 0,
            TcpCompletionFaultMode::DelayBySteps { one_in, steps } => {
                if one_in == 0 || steps == 0 {
                    return 0;
                }
                let selector = self
                    .config
                    .seed
                    .wrapping_add(0x5443_5044)
                    .wrapping_add(insertion_order)
                    % one_in;
                if selector == 0 { steps } else { 0 }
            }
        }
    }

    fn harvest_timers(&mut self, now: Duration) {
        let mut batch_offset = 0;
        let mut last_registration_index = None;
        while let Some(index) = self
            .timers
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.deadline <= now)
            .min_by(|(_, left), (_, right)| {
                left.deadline
                    .cmp(&right.deadline)
                    .then_with(|| left.insertion_order.cmp(&right.insertion_order))
            })
            .map(|(index, _)| index)
        {
            let entry = self.timers.remove(index);
            let registration_index = self.requester_registration_index(entry.call_id);
            if let Some(previous) = last_registration_index {
                if registration_index <= previous {
                    batch_offset += 1;
                }
            }
            self.deliver_completion_at(
                entry.call_id,
                CallOutput::TimerFired,
                self.step_ordinal + batch_offset,
            );
            last_registration_index = Some(registration_index);
        }
    }

    fn harvest_isolate_call_timeouts(&mut self, now: Duration) {
        while let Some(index) = self
            .pending_isolate_calls
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.deadline <= now)
            .min_by(|(_, left), (_, right)| {
                left.deadline
                    .cmp(&right.deadline)
                    .then_with(|| left.insertion_order.cmp(&right.insertion_order))
            })
            .map(|(index, _)| index)
        {
            let mut entry = self.pending_isolate_calls.remove(index);
            let translator = entry.translator.take().unwrap_or_else(|| {
                panic!("translator for call {:?} already consumed", entry.call_id)
            });
            if let Some(shared) = &entry.handle_shared {
                shared.set_state(tina::CallHandleState::Settled);
            }
            // Record the timeout cause so late callee replies surface
            // as `CallerTimedOut` rather than the generic
            // `NoPendingCall` / `CallerClosed`.
            self.record_cancelled_call(entry.call_id, tina::CancelCause::CallerTimedOut);
            self.close_deferred_slot_for_call_with_reason(
                entry.call_id,
                tina_runtime::DeferredReplyRejectedReason::CallerTimedOut,
            );
            self.deliver_isolate_call_outcome(
                IsolateCallDeliveryContext {
                    call_id: entry.call_id,
                    requester: entry.requester,
                    cause: entry.cause,
                    continuation_context: entry.continuation_context,
                    visible_at_step: self.step_ordinal,
                },
                CallOutcome::Timeout,
                translator,
            );
        }
    }

    fn record_cancelled_call(&mut self, call_id: CallId, cause: tina::CancelCause) {
        if self.cancelled_calls.len() == tina_runtime::CANCELLED_CALL_RING_CAPACITY {
            self.cancelled_calls.pop_front();
        }
        self.cancelled_calls.push_back((call_id, cause));
    }

    fn recently_cancelled_cause(&self, call_id: CallId) -> Option<tina::CancelCause> {
        self.cancelled_calls
            .iter()
            .find_map(|(id, cause)| (*id == call_id).then_some(*cause))
    }

    /// Cancels every pending isolate call whose caller is the stopping
    /// isolate. Same shape as the runtime helper.
    fn cancel_pending_isolate_calls_for_owner(
        &mut self,
        owner_isolate: IsolateId,
        owner_generation: AddressGeneration,
        _stopped_cause: tina_runtime::CauseId,
    ) {
        // Single-pass partition: O(n) over pending calls.
        let original = std::mem::take(&mut self.pending_isolate_calls);
        let (mut owned, kept): (Vec<_>, Vec<_>) = original.into_iter().partition(|entry| {
            entry.requester.isolate == owner_isolate
                && entry.requester.generation == owner_generation
        });
        self.pending_isolate_calls = kept;
        for entry in owned.iter_mut() {
            self.record_cancelled_call(entry.call_id, tina::CancelCause::OwnerStopped);
        }
        for mut entry in owned {
            let original_cause = entry.cause;
            let _ = entry.translator.take();
            if let Some(shared) = &entry.handle_shared {
                shared.set_state(tina::CallHandleState::Cancelled);
            }
            self.close_deferred_slot_for_call_with_reason(
                entry.call_id,
                tina_runtime::DeferredReplyRejectedReason::OwnerStopped,
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

    fn close_deferred_slot_for_call_with_reason(
        &mut self,
        call_id: CallId,
        reason: tina_runtime::DeferredReplyRejectedReason,
    ) {
        if let Some(record) = self.promoted_slots.take_by_local_call_id(call_id) {
            record.shared.set_state(tina::DeferredSlotState::Closed);
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

    fn dispatch_cancel_call(
        &mut self,
        context: CallDispatchContext,
        handle_shared: std::sync::Arc<tina::CallHandleShared>,
        translator: ErasedCancelCallTranslator,
    ) {
        let outcome = match handle_shared.state() {
            tina::CallHandleState::Settled => tina::CancelOutcome::AlreadyCompleted,
            tina::CallHandleState::Cancelled => tina::CancelOutcome::AlreadyCancelled,
            tina::CallHandleState::Pending => {
                let raw_call_id = handle_shared.call_id().expect(
                    "cancel_call dispatched before its call_with_handle's effect ran. \
                     Issue cancel from a separate handler, or store the handle and \
                     cancel later.",
                );
                let stamped_shard = handle_shared.shard_id().expect(
                    "shard_id is stamped together with call_id — call_id was Some but \
                     shard_id was None.",
                );
                if stamped_shard != self.shard.id().get() {
                    tina::CancelOutcome::WrongShard
                } else {
                    let call_id = CallId::new(raw_call_id);
                    match self
                        .pending_isolate_calls
                        .iter()
                        .position(|entry| entry.call_id == call_id)
                    {
                        Some(index) => {
                            let mut entry = self.pending_isolate_calls.remove(index);
                            let original_cause = entry.cause;
                            let _ = entry.translator.take();
                            handle_shared.set_state(tina::CallHandleState::Cancelled);
                            self.record_cancelled_call(call_id, tina::CancelCause::CallerCancelled);
                            self.close_deferred_slot_for_call_with_reason(
                                call_id,
                                tina_runtime::DeferredReplyRejectedReason::CallerCancelled,
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
        };

        let message_any = translator(outcome);
        let Some(entry_index) = self.entries.iter().position(|entry| {
            entry.id == context.requester.isolate
                && entry.generation == context.requester.generation
        }) else {
            self.push_event(
                context.requester.isolate,
                Some(context.cause),
                RuntimeEventKind::CallCompletionRejected {
                    call_id: context.call_id,
                    call_kind: tina_runtime::CallKind::CancelCall,
                    reason: tina_runtime::CallCompletionRejectedReason::RequesterClosed,
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
                    call_kind: tina_runtime::CallKind::CancelCall,
                    reason: tina_runtime::CallCompletionRejectedReason::RequesterClosed,
                },
            );
            return;
        }
        match self.entries[entry_index].inbox.push(
            message_any,
            self.step_ordinal,
            context.continuation_context,
        ) {
            Ok(()) => {
                self.push_event(
                    context.requester.isolate,
                    Some(context.cause),
                    RuntimeEventKind::CallCompleted {
                        call_id: context.call_id,
                        call_kind: tina_runtime::CallKind::CancelCall,
                    },
                );
            }
            Err(TrySendError::Full(_)) => {
                self.push_event(
                    context.requester.isolate,
                    Some(context.cause),
                    RuntimeEventKind::CallCompletionRejected {
                        call_id: context.call_id,
                        call_kind: tina_runtime::CallKind::CancelCall,
                        reason: tina_runtime::CallCompletionRejectedReason::MailboxFull,
                    },
                );
            }
            Err(TrySendError::Closed(_)) => {
                self.push_event(
                    context.requester.isolate,
                    Some(context.cause),
                    RuntimeEventKind::CallCompletionRejected {
                        call_id: context.call_id,
                        call_kind: tina_runtime::CallKind::CancelCall,
                        reason: tina_runtime::CallCompletionRejectedReason::RequesterClosed,
                    },
                );
            }
        }
    }

    fn build_message_caller(
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
        let expected_reply_type_id = match routing {
            CallRouting::Local => self
                .pending_isolate_calls
                .iter()
                .find(|p| p.call_id == call_id)
                .map(|p| p.expected_reply_type_id)
                .unwrap_or_else(std::any::TypeId::of::<()>),
            CallRouting::Remote { .. } => remote_expected_reply_type_id
                .expect("remote call context carries the expected reply type"),
        };
        Some(MessageCaller::new(
            std::rc::Rc::clone(&self.deferred_registry),
            call_id.get(),
            isolate_id,
            routing,
            expected_reply_type_id,
        ))
    }

    fn promote_captures(&mut self, isolate_id: IsolateId, cause: tina_runtime::CauseId) -> bool {
        let captures = self.deferred_registry.drain_pending();
        if captures.is_empty() {
            return false;
        }
        for capture in captures {
            let slot_id = tina_runtime::DeferredSlotId::new(capture.slot_id);
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
                    cause: tina_runtime::CauseId::new(tina_runtime::EventId::new(remote_cause)),
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

    fn sweep_dropped_deferred_slots(&mut self) {
        let dropped = self.promoted_slots.sweep_dropped();
        for record in dropped {
            self.drop_promoted_deferred_slot(record, None);
        }
    }

    fn drop_pending_deferred_captures(&mut self, cause: tina_runtime::CauseId) -> bool {
        let captures = self.deferred_registry.drain_pending();
        let captured_any = !captures.is_empty();
        for capture in captures {
            capture.shared.set_state(tina::DeferredSlotState::Closed);
            let slot_id = tina_runtime::DeferredSlotId::new(capture.slot_id);
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

    fn drop_promoted_deferred_slot(
        &mut self,
        record: deferred::DeferredSlotRecord,
        cause: Option<tina_runtime::CauseId>,
    ) {
        if record.shared.state() != tina::DeferredSlotState::Open {
            return;
        }
        record.shared.set_state(tina::DeferredSlotState::Closed);
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

    fn deliver_completion(&mut self, call_id: CallId, result: CallOutput) {
        self.deliver_completion_at(call_id, result, self.step_ordinal);
    }

    fn deliver_observed_send_outcome(
        &mut self,
        call_id: CallId,
        requester: RegisteredAddress,
        cause: tina_runtime::CauseId,
        outcome: SendOutcome,
        translator: Box<dyn FnOnce(SendOutcome) -> Box<dyn Any>>,
        continuation_context: Option<MessageCallContext>,
    ) {
        let call_kind = CallKind::ObservedSend;
        let message = translator(outcome);

        let entry_index = self.entries.iter().position(|entry| {
            entry.id == requester.isolate && entry.generation == requester.generation
        });
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

        match self.entries[entry_index]
            .inbox
            .push(message, self.step_ordinal, continuation_context)
        {
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

    fn complete_isolate_call(
        &mut self,
        call_id: CallId,
        cause: tina_runtime::CauseId,
        outcome: CallOutcome<Box<dyn Any>>,
    ) -> bool {
        let Some(index) = self
            .pending_isolate_calls
            .iter()
            .position(|entry| entry.call_id == call_id)
        else {
            return false;
        };
        let mut pending = self.pending_isolate_calls.remove(index);
        let translator = pending
            .translator
            .take()
            .unwrap_or_else(|| panic!("translator for call {call_id:?} already consumed"));
        if let Some(shared) = &pending.handle_shared {
            shared.set_state(tina::CallHandleState::Settled);
        }
        self.deliver_isolate_call_outcome(
            IsolateCallDeliveryContext {
                call_id,
                requester: pending.requester,
                cause,
                continuation_context: pending.continuation_context,
                visible_at_step: self.step_ordinal,
            },
            outcome,
            translator,
        );
        true
    }

    fn deliver_isolate_call_outcome(
        &mut self,
        context: IsolateCallDeliveryContext,
        outcome: CallOutcome<Box<dyn Any>>,
        translator: ErasedIsolateCallTranslator,
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
                context.requester.isolate,
                Some(context.cause),
                RuntimeEventKind::CallFailed {
                    call_id: context.call_id,
                    call_kind: CallKind::IsolateCall,
                    reason,
                },
            );
        }

        let message = translator(outcome);
        let Some(entry_index) = self.entries.iter().position(|entry| {
            entry.id == context.requester.isolate
                && entry.generation == context.requester.generation
        }) else {
            self.push_event(
                context.requester.isolate,
                Some(context.cause),
                RuntimeEventKind::CallCompletionRejected {
                    call_id: context.call_id,
                    call_kind: CallKind::IsolateCall,
                    reason: CallCompletionRejectedReason::RequesterClosed,
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
                    call_kind: CallKind::IsolateCall,
                    reason: CallCompletionRejectedReason::RequesterClosed,
                },
            );
            return;
        }

        match self.entries[entry_index].inbox.push(
            message,
            context.visible_at_step,
            context.continuation_context,
        ) {
            Ok(()) => {
                if failure_reason.is_none() {
                    self.push_event(
                        context.requester.isolate,
                        Some(context.cause),
                        RuntimeEventKind::CallCompleted {
                            call_id: context.call_id,
                            call_kind: CallKind::IsolateCall,
                        },
                    );
                }
            }
            Err(TrySendError::Full(_)) => {
                self.push_event(
                    context.requester.isolate,
                    Some(context.cause),
                    RuntimeEventKind::CallCompletionRejected {
                        call_id: context.call_id,
                        call_kind: CallKind::IsolateCall,
                        reason: CallCompletionRejectedReason::MailboxFull,
                    },
                );
            }
            Err(TrySendError::Closed(_)) => {
                self.push_event(
                    context.requester.isolate,
                    Some(context.cause),
                    RuntimeEventKind::CallCompletionRejected {
                        call_id: context.call_id,
                        call_kind: CallKind::IsolateCall,
                        reason: CallCompletionRejectedReason::RequesterClosed,
                    },
                );
            }
        }
    }

    fn deliver_completion_at(&mut self, call_id: CallId, result: CallOutput, visible_at_step: u64) {
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
        self.push_persistence_completion_events(&in_flight, &result, failure_reason);

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

        match self.entries[entry_index].inbox.push(
            message,
            visible_at_step,
            in_flight.continuation_context,
        ) {
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

    fn push_persistence_completion_events(
        &mut self,
        in_flight: &InFlightCall,
        result: &CallOutput,
        failure_reason: Option<CallError>,
    ) {
        let Some(persistence) = in_flight.persistence else {
            return;
        };
        match (persistence, failure_reason, result) {
            (PersistenceTraceInfo::SnapshotCommit, None, _) => {
                self.push_event(
                    in_flight.requester.isolate,
                    Some(in_flight.cause),
                    RuntimeEventKind::SnapshotCommitted,
                );
            }
            (PersistenceTraceInfo::SnapshotCommit, Some(reason), _) => {
                self.push_event(
                    in_flight.requester.isolate,
                    Some(in_flight.cause),
                    RuntimeEventKind::SnapshotCommitFailed { reason },
                );
            }
            (PersistenceTraceInfo::JournalAppend { record_index }, None, _) => {
                self.push_event(
                    in_flight.requester.isolate,
                    Some(in_flight.cause),
                    RuntimeEventKind::JournalAppended { record_index },
                );
            }
            (PersistenceTraceInfo::JournalAppend { record_index }, Some(reason), _) => {
                self.push_event(
                    in_flight.requester.isolate,
                    Some(in_flight.cause),
                    RuntimeEventKind::JournalAppendFailed {
                        record_index,
                        reason,
                    },
                );
            }
            (PersistenceTraceInfo::Recovery, None, _) => {
                self.push_event(
                    in_flight.requester.isolate,
                    Some(in_flight.cause),
                    RuntimeEventKind::RecoveryFinished,
                );
            }
            (PersistenceTraceInfo::Recovery, Some(reason), _) => {
                self.push_event(
                    in_flight.requester.isolate,
                    Some(in_flight.cause),
                    RuntimeEventKind::RecoveryFailed { reason },
                );
            }
        }
    }

    fn dispatch_local_send(&mut self, send: ErasedSend) -> Result<(), SendRejectedReason> {
        self.dispatch_local_send_with_context(send, None)
    }

    fn dispatch_local_send_with_context(
        &mut self,
        send: ErasedSend,
        call_context: Option<MessageCallContext>,
    ) -> Result<(), SendRejectedReason> {
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
            return Err(SendRejectedReason::Closed);
        };

        if entry.generation != send.target_generation || entry.stopped.get() {
            return Err(SendRejectedReason::Closed);
        }

        let send_ordinal = self.next_send_ordinal;
        self.next_send_ordinal += 1;
        let visible_at_step = self.step_ordinal
            + 1
            + self.local_send_delay_rounds(self.config.faults.local_send, send_ordinal);

        entry
            .inbox
            .push(send.message, visible_at_step, call_context)
            .map_err(|reason| match reason {
                TrySendError::Full(_) => SendRejectedReason::Full,
                TrySendError::Closed(_) => SendRejectedReason::Closed,
            })
    }

    fn harvest_remote_envelope(
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

    fn harvest_remote_send(&mut self, queued: QueuedRemoteSend) -> Option<QueuedRemoteEnvelope> {
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
            return remote_call_outcome_envelope(queued.call_context, RemoteCallOutcome::Closed);
        };

        if entry.generation != send.target_generation || entry.stopped.get() {
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

        match entry
            .inbox
            .push(send.message, self.step_ordinal + 1, queued.call_context)
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

    fn harvest_remote_call_reply(&mut self, reply: RemoteCallReply) {
        let outcome = match reply.reply {
            RemoteCallOutcome::Replied(reply) => CallOutcome::Replied(reply),
            RemoteCallOutcome::Full => CallOutcome::Full,
            RemoteCallOutcome::Closed => CallOutcome::Closed,
            RemoteCallOutcome::Rejected(reason) => CallOutcome::Rejected(reason),
        };
        if !self.complete_isolate_call(reply.call_id, reply.cause, outcome) {
            let reason = match self.recently_cancelled_cause(reply.call_id) {
                Some(c) => tina_runtime::call_reply_reason_for_cause(c),
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

    fn fault_delay(&self, mode: FaultMode, ordinal: u64, tag: u64) -> Duration {
        match mode {
            FaultMode::None => Duration::ZERO,
            FaultMode::DelayBy { one_in, by } => {
                if one_in == 0 {
                    return Duration::ZERO;
                }
                let selector = self.config.seed.wrapping_add(tag).wrapping_add(ordinal) % one_in;
                if selector == 0 { by } else { Duration::ZERO }
            }
        }
    }

    fn local_send_delay_rounds(&self, mode: LocalSendFaultMode, ordinal: u64) -> u64 {
        match mode {
            LocalSendFaultMode::None => 0,
            LocalSendFaultMode::DelayByRounds { one_in, rounds } => {
                if one_in == 0 || rounds == 0 {
                    return 0;
                }
                let selector = self
                    .config
                    .seed
                    .wrapping_add(0x5345_4e44)
                    .wrapping_add(ordinal)
                    % one_in;
                if selector == 0 { rounds } else { 0 }
            }
        }
    }

    fn stop_entry(
        &mut self,
        index: usize,
        isolate_id: IsolateId,
        cause: tina_runtime::CauseId,
    ) -> tina_runtime::EventId {
        self.stop_entry_with_precollected(index, isolate_id, cause, None)
    }

    fn stop_entry_with_precollected(
        &mut self,
        index: usize,
        isolate_id: IsolateId,
        cause: tina_runtime::CauseId,
        precollected: Option<DeliveredMessage>,
    ) -> tina_runtime::EventId {
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
        self.entries[index].inbox.close();
        let stopped = self.push_event(isolate_id, Some(cause), RuntimeEventKind::IsolateStopped);
        self.entries[index].stopped_event.set(Some(stopped));
        self.cancel_backend_calls_for_requester(RegisteredAddress {
            shard: self.shard.id(),
            isolate: isolate_id,
            generation: self.entries[index].generation,
        });
        self.cancel_pending_isolate_calls_for_owner(
            isolate_id,
            self.entries[index].generation,
            stopped.into(),
        );

        // Drain any deferred reply slots this isolate captured.
        for record in self.promoted_slots.take_by_isolate(isolate_id) {
            self.drop_promoted_deferred_slot(record, Some(stopped.into()));
        }
        if precollected.is_some() {
            self.push_event(
                isolate_id,
                Some(stopped.into()),
                RuntimeEventKind::MessageAbandoned,
            );
        }
        while self.entries[index].inbox.pop_visible(u64::MAX).is_some() {
            self.push_event(
                isolate_id,
                Some(stopped.into()),
                RuntimeEventKind::MessageAbandoned,
            );
        }
        stopped
    }

    /// Drops simulator state for calls cancelled by resource close.
    /// Translator is not run; caller's continuation does not fire.
    /// Trace records `ResourceClosed`.
    fn cancel_backend_calls_for_resource(&mut self, resource: TcpResourceKey) {
        let cancelled_call_ids: Vec<CallId> = self
            .pending_tcp_completions
            .iter()
            .filter(|pending| pending.resource == resource)
            .map(|pending| pending.call_id)
            .chain(
                self.pending_accepts
                    .iter()
                    .filter(|pending| {
                        matches!(
                            resource,
                            TcpResourceKey::ListenerAccept(l) if l == pending.listener
                        )
                    })
                    .map(|pending| pending.call_id),
            )
            .chain(
                self.pending_udp_recvs
                    .iter()
                    .filter(|pending| {
                        matches!(
                            resource,
                            TcpResourceKey::UdpRecv(s) if s == pending.socket
                        )
                    })
                    .map(|pending| pending.call_id),
            )
            .collect();

        for call_id in cancelled_call_ids {
            // Drop the backend op from whichever queue owns it.
            self.pending_tcp_completions
                .retain(|pending| pending.call_id != call_id);
            self.pending_accepts
                .retain(|pending| pending.call_id != call_id);
            self.pending_udp_recvs
                .retain(|pending| pending.call_id != call_id);

            // Drop simulator call state too, or quiescence never arrives.
            let in_flight_index = self
                .in_flight_calls
                .iter()
                .position(|entry| entry.call_id == call_id);
            if let Some(index) = in_flight_index {
                let call = self.in_flight_calls.remove(index);
                self.translators.retain(|stored| stored.call_id != call_id);
                self.push_event(
                    call.requester.isolate,
                    Some(call.cause),
                    RuntimeEventKind::CallCompletionRejected {
                        call_id: call.call_id,
                        call_kind: call.call_kind,
                        reason: CallCompletionRejectedReason::ResourceClosed,
                    },
                );
            }
        }
    }

    fn cancel_backend_calls_for_requester(&mut self, requester: RegisteredAddress) {
        let mut index = 0;
        while index < self.in_flight_calls.len() {
            if self.in_flight_calls[index].requester != requester {
                index += 1;
                continue;
            }

            let call = self.in_flight_calls.remove(index);
            self.cancel_pending_backend_work(call.call_id);
            self.remove_backend_translator(call.call_id);
            self.push_event(
                call.requester.isolate,
                Some(call.cause),
                RuntimeEventKind::CallCompletionRejected {
                    call_id: call.call_id,
                    call_kind: call.call_kind,
                    reason: CallCompletionRejectedReason::RequesterClosed,
                },
            );
        }
    }

    fn cancel_pending_backend_work(&mut self, call_id: CallId) {
        self.timers.retain(|timer| timer.call_id != call_id);
        self.pending_accepts
            .retain(|pending| pending.call_id != call_id);
        self.pending_connects
            .retain(|pending| pending.call_id != call_id);
        self.pending_udp_recvs
            .retain(|pending| pending.call_id != call_id);
        self.tls_streams
            .retain(|stream| stream.pending_connect_call != Some(call_id));
        self.pending_tcp_completions
            .retain(|completion| completion.call_id != call_id);
    }

    fn remove_backend_translator(&mut self, call_id: CallId) {
        let translator_index = self
            .translators
            .iter()
            .position(|entry| entry.call_id == call_id)
            .unwrap_or_else(|| panic!("missing simulator translator for call {call_id:?}"));
        self.translators.remove(translator_index);
    }

    fn push_event(
        &mut self,
        isolate: IsolateId,
        cause: Option<tina_runtime::CauseId>,
        kind: RuntimeEventKind,
    ) -> tina_runtime::EventId {
        let id = self.ids.next_event_id();
        let event = RuntimeEvent::new(id, cause, self.shard.id(), isolate, kind);
        // Observer first. Panic kills the sim thread by design.
        if let Some(obs) = &self.trace_observer {
            obs.on_event(&event);
        }
        self.trace.push(event);
        id
    }

    fn has_pending_messages(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| !entry.stopped.get() && entry.inbox.has_pending())
    }

    fn observed_peer_output(&self) -> Vec<ObservedPeerOutput> {
        self.streams
            .iter()
            .map(|stream| ObservedPeerOutput {
                peer_addr: stream.peer_addr,
                bytes: stream.output.clone(),
            })
            .collect()
    }

    fn requester_registration_index(&self, call_id: CallId) -> usize {
        let requester = self
            .in_flight_calls
            .iter()
            .find(|entry| entry.call_id == call_id)
            .map(|entry| entry.requester)
            .unwrap_or_else(|| panic!("missing in-flight requester for call {call_id:?}"));
        self.entries
            .iter()
            .position(|entry| {
                entry.id == requester.isolate && entry.generation == requester.generation
            })
            .unwrap_or_else(|| panic!("missing registered requester for timer call {call_id:?}"))
    }
}

fn call_kind(request: &CallInput) -> CallKind {
    match request {
        CallInput::TcpBind { .. } => CallKind::TcpBind,
        CallInput::TcpAccept { .. } => CallKind::TcpAccept,
        CallInput::TcpConnect { .. } => CallKind::TcpConnect,
        CallInput::TcpRead { .. } => CallKind::TcpRead,
        CallInput::TcpWrite { .. } => CallKind::TcpWrite,
        CallInput::TcpListenerClose { .. } => CallKind::TcpListenerClose,
        CallInput::TcpStreamClose { .. } => CallKind::TcpStreamClose,
        CallInput::UdpBind { .. } => CallKind::UdpBind,
        CallInput::UdpSendTo { .. } => CallKind::UdpSendTo,
        CallInput::UdpRecvFrom { .. } => CallKind::UdpRecvFrom,
        CallInput::UdpSocketClose { .. } => CallKind::UdpSocketClose,
        CallInput::DnsLookup { .. } => CallKind::DnsLookup,
        CallInput::TlsConnect { .. } => CallKind::TlsConnect,
        CallInput::TlsBind { .. } => CallKind::TlsBind,
        CallInput::TlsAccept { .. } => CallKind::TlsAccept,
        CallInput::TlsListenerClose { .. } => CallKind::TlsListenerClose,
        CallInput::TlsRead { .. } => CallKind::TlsRead,
        CallInput::TlsWrite { .. } => CallKind::TlsWrite,
        CallInput::TlsClose { .. } => CallKind::TlsClose,
        CallInput::SignalWait { .. } => CallKind::SignalWait,
        CallInput::ProcessRun { .. } => CallKind::ProcessRun,
        CallInput::FileOpen { .. } => CallKind::FileOpen,
        CallInput::FileReadAt { .. } => CallKind::FileReadAt,
        CallInput::FileWriteAt { .. } => CallKind::FileWriteAt,
        CallInput::FileFsync { .. } => CallKind::FileFsync,
        CallInput::FileSize { .. } => CallKind::FileSize,
        CallInput::FileClose { .. } => CallKind::FileClose,
        CallInput::Mkdir { .. } => CallKind::Mkdir,
        CallInput::PathMetadata { .. } => CallKind::PathMetadata,
        CallInput::RenameReplace { .. } => CallKind::RenameReplace,
        CallInput::RemoveFile { .. } => CallKind::RemoveFile,
        CallInput::ReadDir { .. } => CallKind::ReadDir,
        CallInput::SyncParent { .. } => CallKind::SyncParent,
        CallInput::SnapshotCommit { .. } => CallKind::SnapshotCommit,
        CallInput::SnapshotLoad { .. } => CallKind::SnapshotLoad,
        CallInput::JournalAppend { .. } => CallKind::JournalAppend,
        CallInput::JournalReplay { .. } => CallKind::JournalReplay,
        CallInput::Sleep { .. } => CallKind::Sleep,
    }
}

impl<S> IntoErasedSpawn<S> for Infallible
where
    S: Shard,
{
    fn into_erased_spawn(self) -> Box<dyn ErasedSpawn<S>> {
        match self {}
    }
}

impl<S, ParentMessage> IntoErasedSpawnObserved<S, ParentMessage> for Infallible
where
    S: Shard,
{
    fn into_erased_spawn_observed(self) -> Box<dyn ErasedSpawnObserved<S>> {
        match self {}
    }
}

struct SpawnObservedAdapter<Spawn, ParentMessage, ChildMessage, ChildReply> {
    inner: tina::SpawnObserved<Spawn, ParentMessage, ChildMessage, ChildReply>,
}

impl<Spawn, ParentMessage, ChildMessage, ChildReply, S> ErasedSpawnObserved<S>
    for SpawnObservedAdapter<Spawn, ParentMessage, ChildMessage, ChildReply>
where
    Spawn: IntoErasedSpawn<S> + 'static,
    ParentMessage: 'static,
    ChildMessage: 'static,
    ChildReply: 'static,
    S: Shard,
{
    fn spawn_observed(
        self: Box<Self>,
        sim: &mut Simulator<S>,
        parent: IsolateId,
    ) -> SpawnObservedOutcome<S> {
        let (spawn, continuation) = self.inner.into_parts();
        match spawn.into_erased_spawn().try_spawn_observed(sim, parent) {
            Ok(outcome) => {
                let child_address = Address::<ChildMessage, ChildReply>::new_with_generation(
                    outcome.child.shard,
                    outcome.child.isolate,
                    outcome.child.generation,
                );
                let message = continuation(Ok(ChildRef::new(child_address)));
                SpawnObservedOutcome {
                    spawn: Some(outcome),
                    continuation: Some(Box::new(message)),
                }
            }
            Err(error) => {
                let message = continuation(Err(error));
                SpawnObservedOutcome {
                    spawn: None,
                    continuation: Some(Box::new(message)),
                }
            }
        }
    }
}

impl<Spawn, ParentMessage, ChildMessage, ChildReply, S> IntoErasedSpawnObserved<S, ParentMessage>
    for tina::SpawnObserved<Spawn, ParentMessage, ChildMessage, ChildReply>
where
    Spawn: IntoErasedSpawn<S> + 'static,
    ParentMessage: 'static,
    ChildMessage: 'static,
    ChildReply: 'static,
    S: Shard,
{
    fn into_erased_spawn_observed(self) -> Box<dyn ErasedSpawnObserved<S>> {
        Box::new(SpawnObservedAdapter { inner: self })
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

impl<I, S, Msg, Outbound> ErasedSpawn<S> for SpawnAdapter<I, Outbound>
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
    fn spawn(self: Box<Self>, sim: &mut Simulator<S>, parent: IsolateId) -> SpawnOutcome<S> {
        sim.spawn_isolate::<I, Msg, Outbound>(
            parent,
            self.isolate,
            self.mailbox_capacity,
            self.bootstrap_message,
        )
    }

    fn try_spawn_observed(
        self: Box<Self>,
        sim: &mut Simulator<S>,
        parent: IsolateId,
    ) -> Result<SpawnOutcome<S>, SpawnObservedError> {
        if self.mailbox_capacity == 0 {
            return Err(SpawnObservedError::ZeroMailboxCapacity);
        }
        Ok(self.spawn(sim, parent))
    }
}

impl<I, S, Msg, Outbound> IntoErasedSpawn<S> for ChildDefinition<I>
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
    fn into_erased_spawn(self) -> Box<dyn ErasedSpawn<S>> {
        let (isolate, mailbox_capacity, bootstrap_message) = self.into_parts();
        Box::new(SpawnAdapter::<I, Outbound> {
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

impl<I, S, Msg, Outbound> ErasedSpawn<S> for RestartableSpawnAdapter<I, Outbound>
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
    fn spawn(self: Box<Self>, sim: &mut Simulator<S>, parent: IsolateId) -> SpawnOutcome<S> {
        let adapter = Rc::new(*self);
        let isolate = (adapter.factory)();
        let mailbox_capacity = adapter.mailbox_capacity;
        let bootstrap_message = adapter.bootstrap_factory.as_ref().map(|factory| factory());
        let mut outcome = sim.spawn_isolate::<I, Msg, Outbound>(
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
        sim: &mut Simulator<S>,
        parent: IsolateId,
    ) -> Result<SpawnOutcome<S>, SpawnObservedError> {
        if self.mailbox_capacity == 0 {
            return Err(SpawnObservedError::ZeroMailboxCapacity);
        }
        Ok(self.spawn(sim, parent))
    }
}

impl<I, S, Msg, Outbound> ErasedRestartRecipe<S> for RestartableSpawnAdapter<I, Outbound>
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
    fn create(&self, sim: &mut Simulator<S>, parent: IsolateId) -> SpawnOutcome<S> {
        let isolate = (self.factory)();
        let bootstrap_message = self.bootstrap_factory.as_ref().map(|factory| factory());
        sim.spawn_isolate::<I, Msg, Outbound>(
            parent,
            isolate,
            self.mailbox_capacity,
            bootstrap_message,
        )
    }
}

impl<I, S, Msg, Outbound> IntoErasedSpawn<S> for RestartableChildDefinition<I>
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
    fn into_erased_spawn(self) -> Box<dyn ErasedSpawn<S>> {
        let (factory, mailbox_capacity, bootstrap_factory) = self.into_parts();
        Box::new(RestartableSpawnAdapter::<I, Outbound> {
            factory,
            mailbox_capacity,
            bootstrap_factory,
            marker: PhantomData,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::convert::Infallible;
    use std::rc::Rc;
    use tina::{
        ChildDefinition, ChildRef, Outbound, SpawnObservedError, batch, noop, send, spawn,
        spawn_observed, stop,
    };

    #[test]
    fn round_message_scratch_reserve_covers_more_than_initial_capacity() {
        let mut scratch = Vec::with_capacity(INITIAL_ENTRY_CAPACITY);
        scratch.clear();
        reserve_round_message_scratch(&mut scratch, INITIAL_ENTRY_CAPACITY + 4);
        assert!(
            scratch.capacity() >= INITIAL_ENTRY_CAPACITY + 4,
            "scratch reserve must cover the entry count before push-time growth"
        );
    }

    #[test]
    fn simulator_spawn_observed_delivers_child_ref_and_parent_uses_address() {
        let seen = Rc::new(RefCell::new(Vec::new()));
        let child_ref = Rc::new(RefCell::new(None));
        let spawn_error = Rc::new(RefCell::new(None));
        let mut sim = Simulator::new(NumberedShard(9), SimulatorConfig::default());
        let parent = sim.register_with_mailbox_capacity::<
            SimObservedParent,
            SimObservedParentMsg,
            SimObservedChildMsg,
        >(
            SimObservedParent {
                seen: Rc::clone(&seen),
                child_ref: Rc::clone(&child_ref),
                spawn_error,
            },
            8,
        );

        assert!(sim.try_send(parent, SimObservedParentMsg::Start).is_ok());
        assert_eq!(sim.step(), 1);
        assert!(child_ref.borrow().is_none());

        assert_eq!(sim.step(), 1);
        let child = (*child_ref.borrow()).expect("child ref delivered to parent");
        assert_eq!(child.address.shard(), ShardId::new(9));
        assert_eq!(child.generation, child.address.generation());

        assert_eq!(sim.step(), 1);
        assert_eq!(&*seen.borrow(), &[7]);
    }

    #[test]
    fn simulator_spawn_observed_reports_zero_capacity_without_spawning_child() {
        let seen = Rc::new(RefCell::new(Vec::new()));
        let child_ref = Rc::new(RefCell::new(None));
        let spawn_error = Rc::new(RefCell::new(None));
        let mut sim = Simulator::new(NumberedShard(9), SimulatorConfig::default());
        let parent = sim.register_with_mailbox_capacity::<
            SimObservedParent,
            SimObservedParentMsg,
            SimObservedChildMsg,
        >(
            SimObservedParent {
                seen,
                child_ref: Rc::clone(&child_ref),
                spawn_error: Rc::clone(&spawn_error),
            },
            8,
        );

        assert!(
            sim.try_send(parent, SimObservedParentMsg::StartInvalid)
                .is_ok()
        );
        assert_eq!(sim.step(), 1);
        assert!(child_ref.borrow().is_none());
        assert!(spawn_error.borrow().is_none());
        assert!(
            !sim.trace()
                .iter()
                .any(|event| matches!(event.kind(), RuntimeEventKind::Spawned { .. }))
        );

        assert_eq!(sim.step(), 1);
        assert_eq!(
            *spawn_error.borrow(),
            Some(SpawnObservedError::ZeroMailboxCapacity)
        );
    }

    #[derive(Debug, Clone, Copy)]
    struct NumberedShard(u32);

    impl Shard for NumberedShard {
        fn id(&self) -> ShardId {
            ShardId::new(self.0)
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum SimTimerMsg {
        Start,
        Fired,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum SimStepEvent {
        Tick,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum SimRemoteEvent {
        Kick,
        KickTwice,
        KickThrice,
        Arrived,
        StopNow,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum SimShardLocalSupervisionEvent {
        SpawnChild,
        Panic,
        Noop,
    }

    #[derive(Debug)]
    struct SimSleeper<S> {
        delay: Duration,
        marker: PhantomData<S>,
    }

    #[derive(Debug)]
    struct SimRecorder<S> {
        marker: PhantomData<S>,
    }

    #[derive(Debug)]
    struct SimRemoteSender<S> {
        target: Address<SimRemoteEvent>,
        marker: PhantomData<S>,
    }

    #[derive(Debug)]
    struct SimRemoteSink<S> {
        marker: PhantomData<S>,
    }

    #[derive(Debug)]
    struct SimShardLocalParent<S> {
        marker: PhantomData<S>,
    }

    #[derive(Debug)]
    struct SimShardLocalChild<S> {
        marker: PhantomData<S>,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum SimObservedChildMsg {
        Record(u8),
    }

    #[derive(Debug)]
    enum SimObservedParentMsg {
        Start,
        StartInvalid,
        ChildStarted(Result<ChildRef<SimObservedChildMsg>, SpawnObservedError>),
    }

    #[derive(Debug)]
    struct SimObservedChild {
        seen: Rc<RefCell<Vec<u8>>>,
    }

    impl Isolate for SimObservedChild {
        type Message = SimObservedChildMsg;
        type Reply = ();
        type Send = Outbound<Infallible>;
        type Spawn = Infallible;
        type SpawnObserved = Infallible;
        type Call = RuntimeCall<Self::Message>;
        type Shard = NumberedShard;

        fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                SimObservedChildMsg::Record(value) => {
                    self.seen.borrow_mut().push(value);
                    noop()
                }
            }
        }
    }

    #[derive(Debug)]
    struct SimObservedParent {
        seen: Rc<RefCell<Vec<u8>>>,
        child_ref: Rc<RefCell<Option<ChildRef<SimObservedChildMsg>>>>,
        spawn_error: Rc<RefCell<Option<SpawnObservedError>>>,
    }

    impl Isolate for SimObservedParent {
        type Message = SimObservedParentMsg;
        type Reply = ();
        type Send = Outbound<SimObservedChildMsg>;
        type Spawn = ChildDefinition<SimObservedChild>;
        type SpawnObserved =
            tina::SpawnObserved<Self::Spawn, Self::Message, SimObservedChildMsg, ()>;
        type Call = RuntimeCall<Self::Message>;
        type Shard = NumberedShard;

        fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                SimObservedParentMsg::Start => spawn_observed(ChildDefinition::new(
                    SimObservedChild {
                        seen: Rc::clone(&self.seen),
                    },
                    4,
                ))
                .reply(SimObservedParentMsg::ChildStarted),
                SimObservedParentMsg::StartInvalid => spawn_observed(ChildDefinition::new(
                    SimObservedChild {
                        seen: Rc::clone(&self.seen),
                    },
                    0,
                ))
                .reply(SimObservedParentMsg::ChildStarted),
                SimObservedParentMsg::ChildStarted(Ok(child)) => {
                    *self.child_ref.borrow_mut() = Some(child);
                    send(child.address, SimObservedChildMsg::Record(7))
                }
                SimObservedParentMsg::ChildStarted(Err(error)) => {
                    *self.spawn_error.borrow_mut() = Some(error);
                    noop()
                }
            }
        }
    }

    impl<S> Isolate for SimSleeper<S>
    where
        S: Shard + 'static,
    {
        type Message = SimTimerMsg;
        type Reply = ();
        type Send = Outbound<Infallible>;
        type Spawn = Infallible;
        type SpawnObserved = std::convert::Infallible;
        type Call = RuntimeCall<SimTimerMsg>;
        type Shard = S;

        fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                SimTimerMsg::Start => Effect::Call(RuntimeCall::new(
                    CallInput::Sleep { after: self.delay },
                    |result| match result {
                        CallOutput::TimerFired => SimTimerMsg::Fired,
                        other => panic!("expected TimerFired, got {other:?}"),
                    },
                )),
                SimTimerMsg::Fired => Effect::Noop,
            }
        }
    }

    impl<S> Isolate for SimRecorder<S>
    where
        S: Shard + 'static,
    {
        type Message = SimStepEvent;
        type Reply = ();
        type Send = Outbound<Infallible>;
        type Spawn = Infallible;
        type SpawnObserved = std::convert::Infallible;
        type Call = RuntimeCall<SimStepEvent>;
        type Shard = S;

        fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                SimStepEvent::Tick => Effect::Noop,
            }
        }
    }

    impl<S> Isolate for SimRemoteSender<S>
    where
        S: Shard + 'static,
    {
        type Message = SimRemoteEvent;
        type Reply = ();
        type Send = Outbound<SimRemoteEvent>;
        type Spawn = Infallible;
        type SpawnObserved = std::convert::Infallible;
        type Call = RuntimeCall<SimRemoteEvent>;
        type Shard = S;

        fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                SimRemoteEvent::Kick => send(self.target, SimRemoteEvent::Arrived),
                SimRemoteEvent::KickTwice => batch([
                    send(self.target, SimRemoteEvent::Arrived),
                    send(self.target, SimRemoteEvent::Arrived),
                ]),
                SimRemoteEvent::KickThrice => batch([
                    send(self.target, SimRemoteEvent::Arrived),
                    send(self.target, SimRemoteEvent::Arrived),
                    send(self.target, SimRemoteEvent::Arrived),
                ]),
                SimRemoteEvent::Arrived => noop(),
                SimRemoteEvent::StopNow => stop(),
            }
        }
    }

    impl<S> Isolate for SimRemoteSink<S>
    where
        S: Shard + 'static,
    {
        type Message = SimRemoteEvent;
        type Reply = ();
        type Send = Outbound<Infallible>;
        type Spawn = Infallible;
        type SpawnObserved = std::convert::Infallible;
        type Call = RuntimeCall<SimRemoteEvent>;
        type Shard = S;

        fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                SimRemoteEvent::Arrived
                | SimRemoteEvent::Kick
                | SimRemoteEvent::KickTwice
                | SimRemoteEvent::KickThrice => noop(),
                SimRemoteEvent::StopNow => stop(),
            }
        }
    }

    impl<S> Isolate for SimShardLocalParent<S>
    where
        S: Shard + 'static,
    {
        type Message = SimShardLocalSupervisionEvent;
        type Reply = ();
        type Send = Outbound<Infallible>;
        type Spawn = tina::RestartableChildDefinition<SimShardLocalChild<S>>;
        type SpawnObserved = std::convert::Infallible;
        type Call = RuntimeCall<SimShardLocalSupervisionEvent>;
        type Shard = S;

        fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                SimShardLocalSupervisionEvent::SpawnChild => {
                    spawn(tina::RestartableChildDefinition::new(
                        || SimShardLocalChild {
                            marker: PhantomData,
                        },
                        4,
                    ))
                }
                SimShardLocalSupervisionEvent::Panic => {
                    panic!("parent should not panic in this test")
                }
                SimShardLocalSupervisionEvent::Noop => noop(),
            }
        }
    }

    impl<S> Isolate for SimShardLocalChild<S>
    where
        S: Shard + 'static,
    {
        type Message = SimShardLocalSupervisionEvent;
        type Reply = ();
        type Send = Outbound<Infallible>;
        type Spawn = Infallible;
        type SpawnObserved = std::convert::Infallible;
        type Call = RuntimeCall<SimShardLocalSupervisionEvent>;
        type Shard = S;

        fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                SimShardLocalSupervisionEvent::Panic => {
                    panic!("child panicked for restart test")
                }
                SimShardLocalSupervisionEvent::SpawnChild | SimShardLocalSupervisionEvent::Noop => {
                    noop()
                }
            }
        }
    }

    struct AddressLocalRemoteFailureRun {
        artifact: MultiShardReplayArtifact,
        failure: Option<CheckerFailure>,
    }

    struct AddressLocalRemoteFailureChecker {
        destination: ShardId,
        rejected: IsolateId,
        live: IsolateId,
        marker_shard: ShardId,
        marker: IsolateId,
        saw_address_rejection: bool,
        saw_live_accept_after_rejection: bool,
    }

    impl Checker for AddressLocalRemoteFailureChecker {
        fn name(&self) -> &'static str {
            "address_local_remote_failure"
        }

        fn on_event(&mut self, event: &RuntimeEvent) -> CheckerDecision {
            match event.kind() {
                RuntimeEventKind::SendRejected {
                    target_isolate,
                    reason: SendRejectedReason::Closed,
                    ..
                } if event.shard() == self.destination && target_isolate == self.rejected => {
                    self.saw_address_rejection = true;
                }
                RuntimeEventKind::MailboxAccepted
                    if self.saw_address_rejection
                        && event.shard() == self.destination
                        && event.isolate() == self.live =>
                {
                    self.saw_live_accept_after_rejection = true;
                }
                RuntimeEventKind::HandlerStarted
                    if self.saw_address_rejection
                        && event.shard() == self.marker_shard
                        && event.isolate() == self.marker
                        && !self.saw_live_accept_after_rejection =>
                {
                    return CheckerDecision::Fail(
                        "destination shard did not accept later live traffic after address-local remote failure"
                            .into(),
                    );
                }
                _ => {}
            }

            CheckerDecision::Continue
        }
    }

    fn run_address_local_remote_failure_checker_workload(
        include_live_send: bool,
        config: SimulatorConfig,
        multishard: MultiShardSimulatorConfig,
    ) -> AddressLocalRemoteFailureRun {
        let mut sim = MultiShardSimulator::with_config(
            [NumberedShard(11), NumberedShard(22)],
            config,
            multishard,
        );

        let unknown = Address::new_with_generation(
            ShardId::new(22),
            IsolateId::new(999),
            AddressGeneration::new(0),
        );
        let live_sink = sim.register_with_capacity_on::<SimRemoteSink<NumberedShard>, _, _>(
            ShardId::new(22),
            SimRemoteSink {
                marker: PhantomData,
            },
            4,
        );
        let marker = sim.register_with_capacity_on::<SimRemoteSink<NumberedShard>, _, _>(
            ShardId::new(11),
            SimRemoteSink {
                marker: PhantomData,
            },
            4,
        );
        let bad_sender = sim.register_with_capacity_on::<SimRemoteSender<NumberedShard>, _, _>(
            ShardId::new(11),
            SimRemoteSender {
                target: unknown,
                marker: PhantomData,
            },
            4,
        );
        let good_sender = sim.register_with_capacity_on::<SimRemoteSender<NumberedShard>, _, _>(
            ShardId::new(11),
            SimRemoteSender {
                target: live_sink,
                marker: PhantomData,
            },
            4,
        );

        sim.try_send(bad_sender, SimRemoteEvent::Kick).unwrap();
        if include_live_send {
            sim.try_send(good_sender, SimRemoteEvent::Kick).unwrap();
        }

        sim.step();
        sim.step();
        sim.try_send(marker, SimRemoteEvent::Kick).unwrap();

        let mut checker = AddressLocalRemoteFailureChecker {
            destination: ShardId::new(22),
            rejected: unknown.isolate(),
            live: live_sink.isolate(),
            marker_shard: ShardId::new(11),
            marker: marker.isolate(),
            saw_address_rejection: false,
            saw_live_accept_after_rejection: false,
        };
        let failure = sim.run_until_quiescent_checked(&mut checker);

        AddressLocalRemoteFailureRun {
            artifact: sim.replay_artifact(),
            failure,
        }
    }

    #[test]
    fn sibling_simulators_can_share_global_event_and_call_id_sources() {
        let ids = IdSource::new();
        let mut first =
            Simulator::with_ids(NumberedShard(11), SimulatorConfig::default(), ids.clone());
        let mut second = Simulator::with_ids(NumberedShard(22), SimulatorConfig::default(), ids);

        let first_sleeper = first.register(SimSleeper::<NumberedShard> {
            delay: Duration::from_millis(5),
            marker: PhantomData,
        });
        let second_sleeper = second.register(SimSleeper::<NumberedShard> {
            delay: Duration::from_millis(7),
            marker: PhantomData,
        });

        first.try_send(first_sleeper, SimTimerMsg::Start).unwrap();
        second.try_send(second_sleeper, SimTimerMsg::Start).unwrap();

        assert_eq!(first.step(), 1);
        assert_eq!(second.step(), 1);

        let first_event_ids: Vec<_> = first.trace().iter().map(|event| event.id().get()).collect();
        let second_event_ids: Vec<_> = second
            .trace()
            .iter()
            .map(|event| event.id().get())
            .collect();
        assert_eq!(first_event_ids, vec![1, 2, 3, 4]);
        assert_eq!(second_event_ids, vec![5, 6, 7, 8]);

        let first_call_ids: Vec<_> = first
            .trace()
            .iter()
            .filter_map(|event| match event.kind() {
                RuntimeEventKind::CallDispatchAttempted { call_id, .. } => Some(call_id.get()),
                _ => None,
            })
            .collect();
        let second_call_ids: Vec<_> = second
            .trace()
            .iter()
            .filter_map(|event| match event.kind() {
                RuntimeEventKind::CallDispatchAttempted { call_id, .. } => Some(call_id.get()),
                _ => None,
            })
            .collect();
        assert_eq!(first_call_ids, vec![1]);
        assert_eq!(second_call_ids, vec![2]);
    }

    #[test]
    fn multishard_simulator_routes_ingress_and_steps_in_ascending_shard_order() {
        let mut sim = MultiShardSimulator::new(
            [NumberedShard(22), NumberedShard(11)],
            SimulatorConfig::default(),
        );

        assert_eq!(sim.shard_ids(), vec![ShardId::new(11), ShardId::new(22)]);

        let shard_twenty_two = sim.register_with_capacity_on::<SimRecorder<NumberedShard>, _, _>(
            ShardId::new(22),
            SimRecorder {
                marker: PhantomData,
            },
            4,
        );
        let shard_eleven = sim.register_with_capacity_on::<SimRecorder<NumberedShard>, _, _>(
            ShardId::new(11),
            SimRecorder {
                marker: PhantomData,
            },
            4,
        );

        assert_eq!(shard_twenty_two.shard(), ShardId::new(22));
        assert_eq!(shard_eleven.shard(), ShardId::new(11));

        sim.try_send(shard_twenty_two, SimStepEvent::Tick).unwrap();
        sim.try_send(shard_eleven, SimStepEvent::Tick).unwrap();

        assert_eq!(sim.step(), 2);

        let handler_shards: Vec<_> = sim
            .trace()
            .into_iter()
            .filter_map(|event| match event.kind() {
                RuntimeEventKind::HandlerStarted => Some(event.shard().get()),
                _ => None,
            })
            .collect();
        assert_eq!(handler_shards, vec![11, 22]);
    }

    #[test]
    fn cross_shard_simulation_becomes_visible_on_the_next_global_step() {
        let mut sim = MultiShardSimulator::new(
            [NumberedShard(11), NumberedShard(22)],
            SimulatorConfig::default(),
        );

        let sink = sim.register_with_capacity_on::<SimRemoteSink<NumberedShard>, _, _>(
            ShardId::new(22),
            SimRemoteSink {
                marker: PhantomData,
            },
            4,
        );
        let sender = sim.register_with_capacity_on::<SimRemoteSender<NumberedShard>, _, _>(
            ShardId::new(11),
            SimRemoteSender {
                target: sink,
                marker: PhantomData,
            },
            4,
        );

        sim.try_send(sender, SimRemoteEvent::Kick).unwrap();

        assert_eq!(sim.step(), 1);
        let step_one_trace = sim.trace();
        let step_one_sink_handlers = step_one_trace
            .iter()
            .filter(|event| {
                event.shard() == ShardId::new(22)
                    && matches!(event.kind(), RuntimeEventKind::HandlerStarted)
            })
            .count();
        assert_eq!(step_one_sink_handlers, 0);

        assert_eq!(sim.step(), 1);
        let trace = sim.trace();
        assert!(trace.iter().any(|event| {
            event.shard() == ShardId::new(11)
                && matches!(event.kind(), RuntimeEventKind::SendAccepted { .. })
        }));
        assert!(trace.iter().any(|event| {
            event.shard() == ShardId::new(22)
                && matches!(event.kind(), RuntimeEventKind::MailboxAccepted)
        }));
    }

    #[test]
    fn cross_shard_simulation_queue_overflow_rejects_at_source_time() {
        let mut sim = MultiShardSimulator::with_config(
            [NumberedShard(11), NumberedShard(22)],
            SimulatorConfig::default(),
            MultiShardSimulatorConfig {
                shard_pair_capacity: 1,
            },
        );

        let sink = sim.register_with_capacity_on::<SimRemoteSink<NumberedShard>, _, _>(
            ShardId::new(22),
            SimRemoteSink {
                marker: PhantomData,
            },
            4,
        );
        let sender = sim.register_with_capacity_on::<SimRemoteSender<NumberedShard>, _, _>(
            ShardId::new(11),
            SimRemoteSender {
                target: sink,
                marker: PhantomData,
            },
            4,
        );

        sim.try_send(sender, SimRemoteEvent::KickTwice).unwrap();

        assert_eq!(sim.step(), 1);

        let trace = sim.trace();
        let full_rejections = trace
            .iter()
            .filter(|event| {
                event.shard() == ShardId::new(11)
                    && matches!(
                        event.kind(),
                        RuntimeEventKind::SendRejected {
                            reason: SendRejectedReason::Full,
                            ..
                        }
                    )
            })
            .count();
        assert_eq!(full_rejections, 1);
    }

    #[test]
    fn cross_shard_simulation_closed_target_rejects_on_destination_harvest() {
        let mut sim = MultiShardSimulator::new(
            [NumberedShard(11), NumberedShard(22)],
            SimulatorConfig::default(),
        );

        let sink = sim.register_with_capacity_on::<SimRemoteSink<NumberedShard>, _, _>(
            ShardId::new(22),
            SimRemoteSink {
                marker: PhantomData,
            },
            4,
        );
        let sender = sim.register_with_capacity_on::<SimRemoteSender<NumberedShard>, _, _>(
            ShardId::new(11),
            SimRemoteSender {
                target: sink,
                marker: PhantomData,
            },
            4,
        );

        sim.try_send(sender, SimRemoteEvent::Kick).unwrap();
        sim.try_send(sink, SimRemoteEvent::StopNow).unwrap();

        assert_eq!(sim.step(), 2);
        assert_eq!(sim.step(), 0);

        let trace = sim.trace();
        let closed_rejections = trace
            .iter()
            .filter(|event| {
                event.shard() == ShardId::new(22)
                    && matches!(
                        event.kind(),
                        RuntimeEventKind::SendRejected {
                            reason: SendRejectedReason::Closed,
                            ..
                        }
                    )
            })
            .count();
        assert_eq!(closed_rejections, 1);
    }

    #[test]
    fn cross_shard_simulation_unknown_isolate_rejects_on_destination_harvest() {
        let mut sim = MultiShardSimulator::new(
            [NumberedShard(11), NumberedShard(22)],
            SimulatorConfig::default(),
        );

        let unknown = Address::new_with_generation(
            ShardId::new(22),
            IsolateId::new(999),
            AddressGeneration::new(0),
        );
        let sender = sim.register_with_capacity_on::<SimRemoteSender<NumberedShard>, _, _>(
            ShardId::new(11),
            SimRemoteSender {
                target: unknown,
                marker: PhantomData,
            },
            4,
        );

        sim.try_send(sender, SimRemoteEvent::Kick).unwrap();

        assert_eq!(sim.step(), 1);
        assert_eq!(sim.step(), 0);

        let trace = sim.trace();
        let destination_closed_rejections = trace
            .iter()
            .filter(|event| {
                event.shard() == ShardId::new(22)
                    && matches!(
                        event.kind(),
                        RuntimeEventKind::SendRejected {
                            target_isolate,
                            reason: SendRejectedReason::Closed,
                            ..
                        } if target_isolate == unknown.isolate()
                    )
            })
            .count();
        assert_eq!(destination_closed_rejections, 1);
    }

    #[test]
    fn cross_shard_simulation_unknown_isolate_does_not_poison_destination_shard() {
        let mut sim = MultiShardSimulator::new(
            [NumberedShard(11), NumberedShard(22)],
            SimulatorConfig::default(),
        );

        let unknown = Address::new_with_generation(
            ShardId::new(22),
            IsolateId::new(999),
            AddressGeneration::new(0),
        );
        let live_sink = sim.register_with_capacity_on::<SimRemoteSink<NumberedShard>, _, _>(
            ShardId::new(22),
            SimRemoteSink {
                marker: PhantomData,
            },
            4,
        );
        let bad_sender = sim.register_with_capacity_on::<SimRemoteSender<NumberedShard>, _, _>(
            ShardId::new(11),
            SimRemoteSender {
                target: unknown,
                marker: PhantomData,
            },
            4,
        );
        let good_sender = sim.register_with_capacity_on::<SimRemoteSender<NumberedShard>, _, _>(
            ShardId::new(11),
            SimRemoteSender {
                target: live_sink,
                marker: PhantomData,
            },
            4,
        );

        sim.try_send(bad_sender, SimRemoteEvent::Kick).unwrap();
        sim.try_send(good_sender, SimRemoteEvent::Kick).unwrap();

        assert_eq!(sim.step(), 2);
        assert_eq!(sim.step(), 1);

        let trace = sim.trace();
        let unknown_rejection = trace
            .iter()
            .find(|event| {
                matches!(
                    event.kind(),
                    RuntimeEventKind::SendRejected {
                        target_isolate,
                        reason: SendRejectedReason::Closed,
                        ..
                    } if event.shard() == ShardId::new(22)
                        && target_isolate == unknown.isolate()
                )
            })
            .expect("unknown remote target should reject on destination shard");
        let live_accept = trace
            .iter()
            .find(|event| {
                event.shard() == ShardId::new(22)
                    && event.isolate() == live_sink.isolate()
                    && matches!(event.kind(), RuntimeEventKind::MailboxAccepted)
            })
            .expect("later valid traffic to same shard should still be accepted");

        assert!(unknown_rejection.id() < live_accept.id());

        let live_handler_count = trace
            .iter()
            .filter(|event| {
                event.shard() == ShardId::new(22)
                    && event.isolate() == live_sink.isolate()
                    && matches!(event.kind(), RuntimeEventKind::HandlerStarted)
            })
            .count();
        assert_eq!(live_handler_count, 1);
    }

    #[test]
    fn multishard_checker_accepts_address_local_remote_failure_then_good_traffic() {
        let run = run_address_local_remote_failure_checker_workload(
            true,
            SimulatorConfig::default(),
            MultiShardSimulatorConfig::default(),
        );

        assert!(run.failure.is_none());
        assert!(run.artifact.checker_failure().is_none());
    }

    #[test]
    fn multishard_checker_failure_replays_for_address_local_liveness_bug() {
        let first = run_address_local_remote_failure_checker_workload(
            false,
            SimulatorConfig::default(),
            MultiShardSimulatorConfig::default(),
        );
        let replayed = run_address_local_remote_failure_checker_workload(
            false,
            first.artifact.simulator_config().clone(),
            first.artifact.multishard_config(),
        );

        let failure = first
            .failure
            .as_ref()
            .expect("checker should catch missing later live traffic");
        assert_eq!(failure.checker_name(), "address_local_remote_failure");
        assert_eq!(first.artifact.checker_failure(), Some(failure));
        assert_eq!(
            first.artifact.event_record(),
            replayed.artifact.event_record()
        );
        assert_eq!(first.artifact.final_time(), replayed.artifact.final_time());
        assert_eq!(
            first.artifact.checker_failure(),
            replayed.artifact.checker_failure()
        );
        assert_eq!(first.failure, replayed.failure);
    }

    #[test]
    fn cross_shard_simulation_destination_mailbox_full_rejects_on_harvest() {
        let mut sim = MultiShardSimulator::new(
            [NumberedShard(11), NumberedShard(22)],
            SimulatorConfig::default(),
        );

        let sink = sim.register_with_capacity_on::<SimRemoteSink<NumberedShard>, _, _>(
            ShardId::new(22),
            SimRemoteSink {
                marker: PhantomData,
            },
            1,
        );
        let remote_sender = sim.register_with_capacity_on::<SimRemoteSender<NumberedShard>, _, _>(
            ShardId::new(11),
            SimRemoteSender {
                target: sink,
                marker: PhantomData,
            },
            4,
        );
        let local_filler = sim.register_with_capacity_on::<SimRemoteSender<NumberedShard>, _, _>(
            ShardId::new(22),
            SimRemoteSender {
                target: sink,
                marker: PhantomData,
            },
            4,
        );

        sim.try_send(remote_sender, SimRemoteEvent::Kick).unwrap();
        sim.try_send(local_filler, SimRemoteEvent::Kick).unwrap();

        assert_eq!(sim.step(), 2);
        assert_eq!(sim.step(), 1);

        let trace = sim.trace();
        let source_accepts = trace
            .iter()
            .filter(|event| {
                event.shard() == ShardId::new(11)
                    && matches!(event.kind(), RuntimeEventKind::SendAccepted { .. })
            })
            .count();
        assert_eq!(source_accepts, 1);

        let destination_full_rejections = trace
            .iter()
            .filter(|event| {
                event.shard() == ShardId::new(22)
                    && matches!(
                        event.kind(),
                        RuntimeEventKind::SendRejected {
                            reason: SendRejectedReason::Full,
                            ..
                        }
                    )
            })
            .count();
        assert_eq!(destination_full_rejections, 1);
    }

    #[test]
    fn cross_shard_simulation_preserves_fifo_from_one_source() {
        let mut sim = MultiShardSimulator::new(
            [NumberedShard(11), NumberedShard(22)],
            SimulatorConfig::default(),
        );

        let sink = sim.register_with_capacity_on::<SimRemoteSink<NumberedShard>, _, _>(
            ShardId::new(22),
            SimRemoteSink {
                marker: PhantomData,
            },
            8,
        );
        let sender = sim.register_with_capacity_on::<SimRemoteSender<NumberedShard>, _, _>(
            ShardId::new(11),
            SimRemoteSender {
                target: sink,
                marker: PhantomData,
            },
            4,
        );

        sim.try_send(sender, SimRemoteEvent::KickThrice).unwrap();

        assert_eq!(sim.step(), 1);
        assert_eq!(sim.step(), 1);

        let trace = sim.trace();
        let source_attempts: Vec<_> = trace
            .iter()
            .filter_map(|event| match event.kind() {
                RuntimeEventKind::SendDispatchAttempted { .. }
                    if event.shard() == ShardId::new(11) =>
                {
                    Some(event.id().get())
                }
                _ => None,
            })
            .collect();
        let destination_causes: Vec<_> = trace
            .iter()
            .filter_map(|event| match event.kind() {
                RuntimeEventKind::MailboxAccepted if event.shard() == ShardId::new(22) => {
                    event.cause().map(|cause| cause.event().get())
                }
                _ => None,
            })
            .collect();
        assert_eq!(destination_causes, source_attempts);
    }

    #[test]
    fn cross_shard_simulation_preserves_fifo_per_isolate_pair_with_multiple_sources_and_targets() {
        let mut sim = MultiShardSimulator::new(
            [NumberedShard(11), NumberedShard(44)],
            SimulatorConfig::default(),
        );

        let sink_a = sim.register_with_capacity_on::<SimRemoteSink<NumberedShard>, _, _>(
            ShardId::new(44),
            SimRemoteSink {
                marker: PhantomData,
            },
            8,
        );
        let sink_b = sim.register_with_capacity_on::<SimRemoteSink<NumberedShard>, _, _>(
            ShardId::new(44),
            SimRemoteSink {
                marker: PhantomData,
            },
            8,
        );
        let sender_a = sim.register_with_capacity_on::<SimRemoteSender<NumberedShard>, _, _>(
            ShardId::new(11),
            SimRemoteSender {
                target: sink_a,
                marker: PhantomData,
            },
            4,
        );
        let sender_b = sim.register_with_capacity_on::<SimRemoteSender<NumberedShard>, _, _>(
            ShardId::new(11),
            SimRemoteSender {
                target: sink_b,
                marker: PhantomData,
            },
            4,
        );

        sim.try_send(sender_a, SimRemoteEvent::KickThrice).unwrap();
        sim.try_send(sender_b, SimRemoteEvent::KickTwice).unwrap();

        assert_eq!(sim.step(), 2);
        assert_eq!(sim.step(), 2);

        let trace = sim.trace();
        let sender_a_attempts: Vec<_> = trace
            .iter()
            .filter_map(|event| match event.kind() {
                RuntimeEventKind::SendDispatchAttempted { .. }
                    if event.shard() == ShardId::new(11)
                        && event.isolate() == sender_a.isolate() =>
                {
                    Some(event.id().get())
                }
                _ => None,
            })
            .collect();
        let sender_b_attempts: Vec<_> = trace
            .iter()
            .filter_map(|event| match event.kind() {
                RuntimeEventKind::SendDispatchAttempted { .. }
                    if event.shard() == ShardId::new(11)
                        && event.isolate() == sender_b.isolate() =>
                {
                    Some(event.id().get())
                }
                _ => None,
            })
            .collect();
        let sink_a_causes: Vec<_> = trace
            .iter()
            .filter_map(|event| match event.kind() {
                RuntimeEventKind::MailboxAccepted
                    if event.shard() == ShardId::new(44) && event.isolate() == sink_a.isolate() =>
                {
                    event.cause().map(|cause| cause.event().get())
                }
                _ => None,
            })
            .collect();
        let sink_b_causes: Vec<_> = trace
            .iter()
            .filter_map(|event| match event.kind() {
                RuntimeEventKind::MailboxAccepted
                    if event.shard() == ShardId::new(44) && event.isolate() == sink_b.isolate() =>
                {
                    event.cause().map(|cause| cause.event().get())
                }
                _ => None,
            })
            .collect();

        assert_eq!(sink_a_causes, sender_a_attempts);
        assert_eq!(sink_b_causes, sender_b_attempts);
    }

    #[test]
    fn cross_shard_simulation_harvests_sources_in_ascending_shard_order() {
        let mut sim = MultiShardSimulator::new(
            [
                NumberedShard(33),
                NumberedShard(22),
                NumberedShard(11),
                NumberedShard(44),
            ],
            SimulatorConfig::default(),
        );

        let sink = sim.register_with_capacity_on::<SimRemoteSink<NumberedShard>, _, _>(
            ShardId::new(44),
            SimRemoteSink {
                marker: PhantomData,
            },
            8,
        );
        let sender11 = sim.register_with_capacity_on::<SimRemoteSender<NumberedShard>, _, _>(
            ShardId::new(11),
            SimRemoteSender {
                target: sink,
                marker: PhantomData,
            },
            4,
        );
        let sender22 = sim.register_with_capacity_on::<SimRemoteSender<NumberedShard>, _, _>(
            ShardId::new(22),
            SimRemoteSender {
                target: sink,
                marker: PhantomData,
            },
            4,
        );
        let sender33 = sim.register_with_capacity_on::<SimRemoteSender<NumberedShard>, _, _>(
            ShardId::new(33),
            SimRemoteSender {
                target: sink,
                marker: PhantomData,
            },
            4,
        );

        sim.try_send(sender33, SimRemoteEvent::Kick).unwrap();
        sim.try_send(sender22, SimRemoteEvent::Kick).unwrap();
        sim.try_send(sender11, SimRemoteEvent::Kick).unwrap();

        assert_eq!(sim.step(), 3);
        assert_eq!(sim.step(), 1);

        let trace = sim.trace();
        let source_order: Vec<_> = trace
            .iter()
            .filter_map(|event| match event.kind() {
                RuntimeEventKind::SendDispatchAttempted { .. }
                    if matches!(event.shard().get(), 11 | 22 | 33) =>
                {
                    Some((event.shard().get(), event.id().get()))
                }
                _ => None,
            })
            .collect();
        let destination_causes: Vec<_> = trace
            .iter()
            .filter_map(|event| match event.kind() {
                RuntimeEventKind::MailboxAccepted if event.shard() == ShardId::new(44) => {
                    event.cause().map(|cause| cause.event().get())
                }
                _ => None,
            })
            .collect();
        let expected: Vec<_> = source_order.into_iter().map(|(_, id)| id).collect();
        assert_eq!(destination_causes, expected);
    }

    #[test]
    fn multishard_simulation_supervision_keeps_children_on_parent_shard() {
        let mut sim = MultiShardSimulator::new(
            [NumberedShard(11), NumberedShard(22)],
            SimulatorConfig::default(),
        );

        let parent = sim
            .register_with_capacity_on::<SimShardLocalParent<NumberedShard>, _, Infallible>(
                ShardId::new(22),
                SimShardLocalParent {
                    marker: PhantomData,
                },
                4,
            );
        sim.supervise(
            parent,
            SupervisorConfig::new(tina::RestartPolicy::OneForOne, tina::RestartBudget::new(3)),
        );

        sim.try_send(parent, SimShardLocalSupervisionEvent::SpawnChild)
            .unwrap();
        assert_eq!(sim.step(), 1);

        let trace = sim.trace();
        let child = trace
            .iter()
            .find_map(|event| match event.kind() {
                RuntimeEventKind::Spawned { child_isolate }
                    if event.shard() == ShardId::new(22) =>
                {
                    Some(child_isolate)
                }
                _ => None,
            })
            .expect("child spawn should be recorded on the parent shard");
        assert!(
            trace.iter().all(|event| {
                !matches!(event.kind(), RuntimeEventKind::Spawned { .. })
                    || event.shard() == ShardId::new(22)
            }),
            "multi-shard supervision must not create children on another shard"
        );

        let child_address = Address::new(ShardId::new(22), child);
        sim.try_send(child_address, SimShardLocalSupervisionEvent::Panic)
            .unwrap();
        assert_eq!(sim.step(), 1);

        let trace = sim.trace();
        let restart = trace
            .iter()
            .find_map(|event| match event.kind() {
                RuntimeEventKind::RestartChildCompleted {
                    old_isolate,
                    new_isolate,
                    ..
                } if event.shard() == ShardId::new(22) && old_isolate == child => Some(new_isolate),
                _ => None,
            })
            .expect("supervised restart should complete on the parent shard");
        assert_ne!(restart, child);
        assert!(
            trace.iter().all(|event| {
                !matches!(
                    event.kind(),
                    RuntimeEventKind::SupervisorRestartTriggered { .. }
                        | RuntimeEventKind::RestartChildAttempted { .. }
                        | RuntimeEventKind::RestartChildCompleted { .. }
                ) || event.shard() == ShardId::new(22)
            }),
            "restart events must stay on the parent shard"
        );

        sim.try_send(
            Address::new(ShardId::new(22), restart),
            SimShardLocalSupervisionEvent::Noop,
        )
        .unwrap();
    }
}
