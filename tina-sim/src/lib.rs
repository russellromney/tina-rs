#![deny(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

//! Single-shard virtual-time simulator for `tina-rs`.
//!
//! The simulator starts with a narrow deterministic slice:
//!
//! - one shard
//! - virtual monotonic time
//! - the shipped `Sleep { after }` / `TimerFired` contract
//! - deterministic replay artifacts
//!
//! Seeded perturbation makes replayed divergence useful:
//!
//! - seeded perturbation over local-send and timer-wake delivery
//! - a small checker surface that can halt a run with a structured reason
//! - replay artifacts that preserve checker failures alongside the event record
//!
//! Single-shard spawn and supervision replay are modeled:
//!
//! - public `ChildDefinition` / `RestartableChildDefinition` execution
//! - runtime-owned direct parent-child lineage and restartable child records
//! - direct-child `RestartChildren` and supervised panic restart
//! - bootstrap re-delivery, stale identity rejection, and budget exhaustion
//!   through the live runtime event vocabulary
//!
//! Scripted single-shard TCP simulation covers:
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

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[cfg(test)]
use std::marker::PhantomData;
#[cfg(test)]
use tina::{Address, AddressGeneration, Context, Effect, Isolate, IsolateId, ShardId};
use tina::{DeferredSlotRegistry, Shard};
use tina_runtime::{CallId, RuntimeEvent};
#[cfg(test)]
use tina_runtime::{CallInput, CallOutput, RuntimeCall, RuntimeEventKind, SendRejectedReason};
#[cfg(test)]
use tina_supervisor::SupervisorConfig;

mod config;
mod deferred;
mod internals;
mod multi_shard;

use internals::*;

pub use config::{
    Checker, CheckerDecision, CheckerFailure, DurableImage, FaultConfig, FaultMode,
    LocalSendFaultMode, MultiShardReplayArtifact, ObservedPeerOutput, ReplayArtifact,
    SchedulerFaultMode, ScriptedDnsConfig, ScriptedDnsLookupConfig, ScriptedDnsResult,
    ScriptedListenerConfig, ScriptedPeerConfig, ScriptedProcessConfig, ScriptedProcessResult,
    ScriptedProcessRunConfig, ScriptedSignalConfig, ScriptedSignalEventConfig,
    ScriptedSignalResult, ScriptedStorageFaultConfig, ScriptedTcpConfig, ScriptedTlsConfig,
    ScriptedTlsConnectConfig, ScriptedTlsConnectResult, ScriptedTlsReadResult,
    ScriptedTlsWriteResult, ScriptedUdpConfig, ScriptedUdpDatagramConfig, ScriptedUdpSocketConfig,
    SimulatorConfig, TcpCompletionFaultMode, UnixSimConfig,
};
pub use multi_shard::{MultiShardSimulator, MultiShardSimulatorConfig};

pub mod dst;
mod sim_impl;

#[cfg(test)]
use sim_impl::{fault_selector, fill_dispatch_order};

/// Narrow single-shard simulator for timer-driven `tina-rs` workloads.
pub struct Simulator<S>
where
    S: Shard,
{
    pub(crate) system_incarnation: tina::SystemIncarnation,
    pub(crate) shard: S,
    pub(crate) config: SimulatorConfig,
    pub(crate) entries: Vec<RegisteredEntry<S>>,
    pub(crate) child_records: Vec<ChildRecord<S>>,
    pub(crate) supervisors: Vec<SupervisorRecord>,
    pub(crate) next_isolate_id: u64,
    pub(crate) next_listener_id: u64,
    pub(crate) next_stream_id: u64,
    pub(crate) next_udp_socket_id: u64,
    pub(crate) next_tls_listener_id: u64,
    pub(crate) next_tls_stream_id: u64,
    pub(crate) next_file_id: u64,
    pub(crate) next_unix_listener_id: u64,
    pub(crate) next_unix_stream_id: u64,
    pub(crate) ids: IdSource,
    pub(crate) trace: Vec<RuntimeEvent>,
    pub(crate) virtual_now: Duration,
    /// `Instant` anchor that pairs with `virtual_now` to give the
    /// simulator a same-shape "now" as the live runtime's `Clock`.
    /// Stamped once at construction (from `Instant::now()`) and never
    /// mutated; handlers see `virtual_anchor + virtual_now`, saturated through
    /// [`tina::Deadline`] when virtual time exceeds `Instant`'s range.
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
    pub(crate) virtual_anchor: std::time::Instant,
    pub(crate) step_ordinal: u64,
    pub(crate) timers: Vec<TimerEntry>,
    pub(crate) next_timer_ordinal: u64,
    pub(crate) next_send_ordinal: u64,
    pub(crate) next_tcp_completion_ordinal: u64,
    pub(crate) next_storage_ordinal: u64,
    pub(crate) listeners: Vec<ListenerState>,
    pub(crate) streams: Vec<StreamState>,
    pub(crate) udp_sockets: Vec<UdpSocketState>,
    pub(crate) tls_listeners: Vec<TlsListenerState>,
    pub(crate) tls_streams: Vec<TlsStreamState>,
    pub(crate) files: Vec<FileState>,
    pub(crate) unix_listeners: Vec<UnixListenerState>,
    pub(crate) unix_streams: Vec<UnixStreamState>,
    pub(crate) pending_unix_accepts: Vec<PendingUnixAccept>,
    pub(crate) pending_unix_connects: Vec<PendingUnixConnect>,
    pub(crate) pending_unix_reads: Vec<PendingUnixRead>,
    pub(crate) pending_unix_writes: Vec<PendingUnixWrite>,
    pub(crate) file_storage: BTreeMap<PathBuf, Vec<u8>>,
    pub(crate) directories: Vec<PathBuf>,
    pub(crate) pending_accepts: Vec<PendingAccept>,
    pub(crate) pending_connects: Vec<PendingConnect>,
    pub(crate) pending_udp_recvs: Vec<PendingUdpRecv>,
    pub(crate) pending_tcp_completions: Vec<PendingTcpCompletion>,
    /// Single owner of in-flight call bookkeeping, keyed by `CallId`. Mirrors
    /// `tina_runtime::CallTable`; folds the former parallel
    /// `in_flight_calls`/`translators`/`pending_isolate_calls` Vecs.
    pub(crate) call_table: CallTable,
    pub(crate) pending_remote_spawns: Vec<PendingRemoteSpawn>,
    pub(crate) round_messages: Vec<Option<DeliveredMessage>>,
    /// Reusable scratch holding this round's isolate dispatch order over
    /// the fixed registration-order base enumeration. Seed-driven when
    /// `FaultConfig::scheduler` is `PermuteReadyOrder`; identity
    /// otherwise. Reused across rounds like `round_messages` so the
    /// default path allocates once, not per step.
    pub(crate) dispatch_order: Vec<usize>,
    pub(crate) next_isolate_call_ordinal: u64,
    pub(crate) last_checker_failure: Option<CheckerFailure>,
    pub(crate) deferred_registry: std::rc::Rc<DeferredSlotRegistry>,
    pub(crate) promoted_slots: deferred::PromotedSlots,
    /// Bounded ring of recently-cancelled calls plus the cause that
    /// closed each one. Mirrors the runtime so late callee replies
    /// surface as the matching cause-specific rejection reason
    /// (`CallerCancelled` / `CallerTimedOut` / `OwnerStopped` /
    /// `RuntimeStopped`) instead of the generic
    /// `NoPendingCall` / `CallerClosed`.
    pub(crate) cancelled_calls: std::collections::VecDeque<(CallId, tina::CancelCause)>,
    pub(crate) cancelled_call_cause_evictions: u64,
    /// Live trace observer. Fires before in-memory push so DST replay
    /// sees the same stream a live operator does.
    pub(crate) trace_observer: Option<Arc<dyn tina_runtime::TraceObserver>>,
}

impl<S> Simulator<S>
where
    S: Shard,
{
    /// Returns the provenance stamped into addresses issued by this owner.
    pub const fn system_incarnation(&self) -> tina::SystemIncarnation {
        self.system_incarnation
    }

    /// Creates a new simulator with virtual time starting at zero.
    pub fn new(shard: S, config: SimulatorConfig) -> Self {
        Self::with_ids(shard, config, IdSource::new())
    }

    pub(crate) fn with_ids(shard: S, config: SimulatorConfig, ids: IdSource) -> Self {
        assert!(
            !config
                .system_incarnation
                .is_some_and(tina::SystemIncarnation::is_unscoped),
            "simulator system incarnation must be nonzero"
        );
        let system_incarnation = config
            .system_incarnation
            .unwrap_or_else(tina_runtime::fresh_system_incarnation);
        Self::with_ids_and_system(shard, config, ids, system_incarnation)
    }

    pub(crate) fn with_ids_and_system(
        shard: S,
        config: SimulatorConfig,
        ids: IdSource,
        system_incarnation: tina::SystemIncarnation,
    ) -> Self {
        // Advance the user-visible `IsolateId` counter past the system isolates
        // a live `ThreadedRuntime` registers at startup (e.g. its host-call
        // dispatcher pool). The simulator never registers those isolates
        // itself, but skipping their id range keeps user-isolate ids in
        // parity between live and sim so the captured trace replays exactly.
        let reserved_system_isolates = u64::try_from(config.reserved_system_isolates)
            .expect("reserved system isolate count must fit in u64");
        let next_isolate_id = reserved_system_isolates
            .checked_add(1)
            .expect("reserved system isolate count leaves no user id space");
        Self {
            system_incarnation,
            shard,
            config,
            entries: Vec::with_capacity(INITIAL_ENTRY_CAPACITY),
            child_records: Vec::with_capacity(INITIAL_CHILD_RECORD_CAPACITY),
            supervisors: Vec::with_capacity(INITIAL_SUPERVISOR_CAPACITY),
            next_isolate_id,
            next_listener_id: 1,
            next_stream_id: 1,
            next_udp_socket_id: 1,
            next_tls_listener_id: 1,
            next_tls_stream_id: 1,
            next_file_id: 1,
            next_unix_listener_id: 1,
            next_unix_stream_id: 1,
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
            unix_listeners: Vec::with_capacity(INITIAL_TCP_RESOURCE_CAPACITY),
            unix_streams: Vec::with_capacity(INITIAL_TCP_RESOURCE_CAPACITY),
            pending_unix_accepts: Vec::with_capacity(INITIAL_CALL_CAPACITY),
            pending_unix_connects: Vec::with_capacity(INITIAL_CALL_CAPACITY),
            pending_unix_reads: Vec::with_capacity(INITIAL_CALL_CAPACITY),
            pending_unix_writes: Vec::with_capacity(INITIAL_CALL_CAPACITY),
            file_storage: BTreeMap::new(),
            directories: Vec::with_capacity(INITIAL_TCP_RESOURCE_CAPACITY),
            pending_accepts: Vec::with_capacity(INITIAL_CALL_CAPACITY),
            pending_connects: Vec::with_capacity(INITIAL_CALL_CAPACITY),
            pending_udp_recvs: Vec::with_capacity(INITIAL_CALL_CAPACITY),
            pending_tcp_completions: Vec::with_capacity(INITIAL_CALL_CAPACITY),
            call_table: CallTable::new(),
            pending_remote_spawns: Vec::new(),
            round_messages: Vec::with_capacity(INITIAL_ENTRY_CAPACITY),
            dispatch_order: Vec::with_capacity(INITIAL_ENTRY_CAPACITY),
            next_isolate_call_ordinal: 0,
            last_checker_failure: None,
            deferred_registry: std::rc::Rc::new(DeferredSlotRegistry::new()),
            promoted_slots: deferred::PromotedSlots::default(),
            cancelled_calls: std::collections::VecDeque::with_capacity(
                tina_runtime::CANCELLED_CALL_RING_CAPACITY,
            ),
            cancelled_call_cause_evictions: 0,
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

    /// Returns how many recently-cancelled call causes were evicted
    /// from the bounded attribution ring.
    ///
    /// Late replies for evicted calls still reject honestly, but they
    /// fall back to generic caller-closed/no-pending reasons because
    /// the exact cancellation cause is no longer retained.
    pub const fn cancelled_call_cause_evictions(&self) -> u64 {
        self.cancelled_call_cause_evictions
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
        self.call_table.has_driver_calls()
            || !self.timers.is_empty()
            || !self.pending_tcp_completions.is_empty()
            || !self.pending_accepts.is_empty()
            || !self.pending_connects.is_empty()
            || !self.pending_udp_recvs.is_empty()
            || self.call_table.has_isolate_calls()
            || !self.pending_unix_accepts.is_empty()
            || !self.pending_unix_connects.is_empty()
            || !self.pending_unix_reads.is_empty()
            || !self.pending_unix_writes.is_empty()
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
    fn fault_selector_is_deterministic_and_tag_separated() {
        let first: Vec<_> = (0..32)
            .map(|ordinal| fault_selector(7, 0xAA, ordinal, 17))
            .collect();
        let second: Vec<_> = (0..32)
            .map(|ordinal| fault_selector(7, 0xAA, ordinal, 17))
            .collect();
        let other_tag: Vec<_> = (0..32)
            .map(|ordinal| fault_selector(7, 0xBB, ordinal, 17))
            .collect();

        assert_eq!(first, second, "same seed/tag stream must replay exactly");
        assert_ne!(
            first, other_tag,
            "different fault tags should not share a trivially correlated stream"
        );
    }

    #[test]
    fn driver_completion_for_unknown_call_is_quarantined_not_panicked() {
        // Parity mirror of the runtime: a completion for a call the table no
        // longer tracks is quarantined (traced, dropped), not panicked.
        let mut sim = Simulator::new(NumberedShard(0), SimulatorConfig::default());

        sim.deliver_completion(CallId::new(4242), CallOutput::TcpListenerClosed);

        let quarantined = sim
            .trace()
            .iter()
            .filter(|event| {
                matches!(
                    event.kind(),
                    RuntimeEventKind::DriverCompletionQuarantined { call_id }
                        if call_id == CallId::new(4242)
                )
            })
            .count();
        assert_eq!(quarantined, 1, "exactly one quarantine event");
        assert!(!sim.has_in_flight_calls(), "simulator survived and is idle");
    }

    #[test]
    fn unknown_completion_through_real_timer_harvest_is_quarantined_not_panicked() {
        // Stronger than the direct-`deliver_completion` test above: a timer
        // whose call the table never tracked (a driver accounting bug) fires
        // through the REAL `step -> harvest_timers -> deliver_completion_at`
        // sink. The simulator must quarantine (trace + drop) and survive.
        let mut sim = Simulator::new(NumberedShard(0), SimulatorConfig::default());
        sim.timers.push(crate::internals::TimerEntry {
            call_id: CallId::new(4242),
            deadline: std::time::Duration::from_millis(10),
            insertion_order: 0,
        });

        sim.advance_time(std::time::Duration::from_millis(20));
        sim.step();

        let quarantined = sim
            .trace()
            .iter()
            .filter(|event| {
                matches!(
                    event.kind(),
                    RuntimeEventKind::DriverCompletionQuarantined { call_id }
                        if call_id == CallId::new(4242)
                )
            })
            .count();
        assert_eq!(
            quarantined, 1,
            "an unknown timer through the real harvest sink must quarantine exactly once"
        );
        assert!(
            sim.timers.is_empty(),
            "the fired timer is removed from the queue"
        );
        assert!(
            !sim.has_in_flight_calls(),
            "simulator survived the quarantine"
        );
    }

    #[test]
    fn dispatch_order_is_a_deterministic_permutation() {
        use config::SchedulerFaultMode;

        let mut order = Vec::new();

        // Mode off => identity, regardless of seed. This is the property
        // that keeps every pinned golden trace byte-identical.
        for seed in [0u64, 7, 0xC0FFEE, u64::MAX] {
            fill_dispatch_order(&mut order, SchedulerFaultMode::None, seed, 3, 5, 8);
            assert_eq!(order, (0..8).collect::<Vec<_>>());
        }

        // Perturb on but seed 0 => still identity.
        fill_dispatch_order(
            &mut order,
            SchedulerFaultMode::PermuteReadyOrder,
            0,
            3,
            5,
            8,
        );
        assert_eq!(order, (0..8).collect::<Vec<_>>());

        // Perturb on, non-zero seed => a valid permutation (bijection over
        // 0..len: every dispatch slot appears exactly once, so no isolate
        // loses its turn and none is dispatched twice).
        fill_dispatch_order(
            &mut order,
            SchedulerFaultMode::PermuteReadyOrder,
            42,
            3,
            5,
            8,
        );
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..8).collect::<Vec<_>>());
        assert_ne!(order, (0..8).collect::<Vec<_>>(), "seed 42 reorders");

        // Deterministic: same (mode, seed, shard, step, len) => same order.
        let mut again = Vec::new();
        fill_dispatch_order(
            &mut again,
            SchedulerFaultMode::PermuteReadyOrder,
            42,
            3,
            5,
            8,
        );
        assert_eq!(order, again);

        // Distinct step ordinals give distinct streams (round-to-round
        // interleaving actually moves, not one fixed shuffle).
        let mut next_round = Vec::new();
        fill_dispatch_order(
            &mut next_round,
            SchedulerFaultMode::PermuteReadyOrder,
            42,
            3,
            6,
            8,
        );
        assert_ne!(order, next_round);
    }

    #[test]
    fn fault_selector_mixes_tag_into_first_ordinal() {
        assert_ne!(
            fault_selector(7, 0xAA, 0, 17),
            fault_selector(7, 0xBB, 0, 17),
            "first fault decision must not collapse every tag to seed % modulus"
        );
    }

    #[test]
    fn cancelled_call_cause_ring_overflow_is_visible() {
        let mut simulator = Simulator::new(NumberedShard(0), SimulatorConfig::default());

        for raw_id in 0..(tina_runtime::CANCELLED_CALL_RING_CAPACITY as u64 + 3) {
            simulator
                .record_cancelled_call(CallId::new(raw_id), tina::CancelCause::CallerCancelled);
        }

        assert_eq!(simulator.cancelled_call_cause_evictions(), 3);
        assert_eq!(simulator.recently_cancelled_cause(CallId::new(0)), None);
        assert_eq!(
            simulator.recently_cancelled_cause(CallId::new(
                tina_runtime::CANCELLED_CALL_RING_CAPACITY as u64 + 2
            )),
            Some(tina::CancelCause::CallerCancelled)
        );
    }

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
        type Io = RuntimeCall<Self::Message>;
        type Fact = ::std::convert::Infallible;
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
        type Io = RuntimeCall<Self::Message>;
        type Fact = ::std::convert::Infallible;
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
                .then(SimObservedParentMsg::ChildStarted),
                SimObservedParentMsg::StartInvalid => spawn_observed(ChildDefinition::new(
                    SimObservedChild {
                        seen: Rc::clone(&self.seen),
                    },
                    0,
                ))
                .then(SimObservedParentMsg::ChildStarted),
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

    // Cross-shard observed spawn: the child must be `Send` to cross shards,
    // so it holds no `Rc`. The owner keeps the learned address in an `Rc`
    // (the owner never crosses a shard boundary).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum SimCrossChildMsg {
        Ping,
    }

    #[derive(Debug)]
    struct SimCrossChild;

    impl Isolate for SimCrossChild {
        type Message = SimCrossChildMsg;
        type Reply = ();
        type Send = Outbound<Infallible>;
        type Spawn = Infallible;
        type SpawnObserved = Infallible;
        type Io = RuntimeCall<Self::Message>;
        type Fact = ::std::convert::Infallible;
        type Shard = NumberedShard;

        fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                SimCrossChildMsg::Ping => noop(),
            }
        }
    }

    #[derive(Debug)]
    enum SimCrossParentMsg {
        SpawnOn(ShardId),
        ChildStarted(Result<ChildRef<SimCrossChildMsg>, SpawnObservedError>),
    }

    #[derive(Debug)]
    struct SimCrossParent {
        learned: Rc<RefCell<Option<ChildRef<SimCrossChildMsg>>>>,
    }

    impl Isolate for SimCrossParent {
        type Message = SimCrossParentMsg;
        type Reply = ();
        type Send = Outbound<SimCrossChildMsg>;
        type Spawn = Infallible;
        type SpawnObserved = Infallible;
        type SpawnObservedRemote = tina::SpawnObservedRemote<
            ChildDefinition<SimCrossChild>,
            Self::Message,
            SimCrossChildMsg,
            (),
        >;
        type Io = RuntimeCall<Self::Message>;
        type Fact = ::std::convert::Infallible;
        type Shard = NumberedShard;

        fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                SimCrossParentMsg::SpawnOn(shard) => {
                    spawn_observed(ChildDefinition::new(SimCrossChild, 4))
                        .on_shard(shard)
                        .then(SimCrossParentMsg::ChildStarted)
                }
                SimCrossParentMsg::ChildStarted(Ok(child)) => {
                    *self.learned.borrow_mut() = Some(child);
                    // Address the cross-shard child through the learned ref.
                    send(child.address, SimCrossChildMsg::Ping)
                }
                SimCrossParentMsg::ChildStarted(Err(_)) => noop(),
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
        type Io = RuntimeCall<SimTimerMsg>;
        type Fact = ::std::convert::Infallible;
        type Shard = S;

        fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                SimTimerMsg::Start => Effect::Io(RuntimeCall::new(
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
        type Io = RuntimeCall<SimStepEvent>;
        type Fact = ::std::convert::Infallible;
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
        type Io = RuntimeCall<SimRemoteEvent>;
        type Fact = ::std::convert::Infallible;
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
        type Io = RuntimeCall<SimRemoteEvent>;
        type Fact = ::std::convert::Infallible;
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
        type Io = RuntimeCall<SimShardLocalSupervisionEvent>;
        type Fact = ::std::convert::Infallible;
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
        type Io = RuntimeCall<SimShardLocalSupervisionEvent>;
        type Fact = ::std::convert::Infallible;
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

        let unknown = Address::new_with_generation_in(
            sim.system_incarnation(),
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

        let unknown = Address::new_with_generation_in(
            sim.system_incarnation(),
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

        let unknown = Address::new_with_generation_in(
            sim.system_incarnation(),
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
    fn multishard_simulation_spawns_observed_child_on_another_shard_and_learns_address() {
        fn run() -> (Option<ChildRef<SimCrossChildMsg>>, Vec<RuntimeEvent>) {
            let learned = Rc::new(RefCell::new(None));
            let mut sim = MultiShardSimulator::new(
                [NumberedShard(11), NumberedShard(22)],
                SimulatorConfig::default(),
            );
            let parent = sim.register_with_capacity_on::<SimCrossParent, _, SimCrossChildMsg>(
                ShardId::new(11),
                SimCrossParent {
                    learned: Rc::clone(&learned),
                },
                4,
            );
            sim.try_send(parent, SimCrossParentMsg::SpawnOn(ShardId::new(22)))
                .unwrap();
            sim.run_until_quiescent();
            let child = *learned.borrow();
            (child, sim.trace().to_vec())
        }

        let (child, trace) = run();
        let (replay_child, replay_trace) = run();
        assert_eq!(
            trace, replay_trace,
            "cross-shard spawn must replay identically"
        );

        // The owner learned the child's address, and it lives on shard 22.
        let child = child.expect("parent learns the cross-shard child address");
        assert_eq!(child.address.shard(), ShardId::new(22));
        assert_eq!(
            replay_child.map(|c| c.address.shard()),
            Some(ShardId::new(22))
        );

        // The child was created on shard 22, not the owner's shard 11.
        assert!(
            trace.iter().any(|event| event.shard() == ShardId::new(22)
                && matches!(event.kind(), RuntimeEventKind::Spawned { .. })),
            "child must be spawned on the target shard"
        );
        // The owner (shard 11) recorded the ChildStarted truth naming shard 22.
        assert!(
            trace.iter().any(|event| event.shard() == ShardId::new(11)
                && matches!(
                    event.kind(),
                    RuntimeEventKind::ChildStarted { child_shard, .. }
                        if child_shard == ShardId::new(22)
                )),
            "owner must record ChildStarted for the cross-shard child"
        );
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

        let child_address = Address::new_in(sim.system_incarnation(), ShardId::new(22), child);
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
            Address::new_in(sim.system_incarnation(), ShardId::new(22), restart),
            SimShardLocalSupervisionEvent::Noop,
        )
        .unwrap();
    }
}
