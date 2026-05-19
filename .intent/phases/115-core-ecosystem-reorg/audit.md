# Phase 115 — Visibility audit for the runtime/sim splits

This is the prep-work audit for splitting `tina-runtime/src/lib.rs` and
`tina-sim/src/lib.rs`. The actual moves were blocked on visibility
plumbing; this document records the decisions so the next session can
execute the split mechanically without re-reading every method.

## Top-line decisions

- Every private field on `Runtime<S, F>` and `Simulator<S>` becomes
  `pub(crate)`. They name internal runtime state; none of them is part
  of the public surface, and crate-internal visibility doesn't widen
  beyond what the rest of the crate could already see by `impl`-ing in
  the same file.
- Every private support type and trait that the `impl<S, F> Runtime`
  block reaches for becomes `pub(crate)`. Same justification.
- Every private method on `Runtime`/`Simulator` that another method
  calls becomes `pub(crate)`. Public methods stay `pub`.
- Constructors and pure host-facing helpers stay in the parent file.
  Internal worker methods move to a submodule per the plan.

The audit deliberately uses `pub(crate)` (not `pub(super)`) so the
visibility decision survives later module reshuffles inside the crate.

## Runtime — field audit

All 24 fields on `Runtime<S, F>`. None is part of the public surface.

| Field | Visibility plan | Notes |
|---|---|---|
| `shard: S` | `pub(crate)` | read-only in most paths |
| `mailbox_factory: F` | `pub(crate)` | used by spawn paths |
| `entries: Vec<RegisteredEntry<S, F>>` | `pub(crate)` | address book; touched by registration + dispatch |
| `child_records: Vec<ChildRecord<S, F>>` | `pub(crate)` | lineage; accessor methods already exist |
| `supervisors: Vec<SupervisorRecord>` | `pub(crate)` | supervision policy table |
| `next_isolate_id: u64` | `pub(crate)` | monotonic counter, single writer |
| `ids: IdSource` | `pub(crate)` | id source, single writer |
| `trace: Vec<RuntimeEvent>` | `pub(crate)` | event ring; `push_event` is the only mutator |
| `trace_start: usize` | `pub(crate)` | trace bookkeeping |
| `trace_retention: TraceRetention` | `pub(crate)` | config |
| `trace_dropped: u64` | `pub(crate)` | counter |
| `driver: Box<dyn RuntimeDriver>` | `pub(crate)` | I/O backend |
| `in_flight_calls: Vec<InFlightCall>` | `pub(crate)` | dispatch state |
| `translators: Vec<StoredTranslator>` | `pub(crate)` | type-erasure store |
| `clock: Box<dyn Clock>` | `pub(crate)` | time source |
| `pending_isolate_calls: Vec<PendingIsolateCall>` | `pub(crate)` | isolate-call wait list |
| `round_messages: Vec<Option<DeliveredMessage>>` | `pub(crate)` | per-step scratch |
| `driver_completions: Vec<DriverCompletion>` | `pub(crate)` | driver result buffer |
| `next_isolate_call_ordinal: u64` | `pub(crate)` | counter |
| `observation: ObservationRegistry` | `pub(crate)` | observer registry |
| `trace_observer: StoredObserver` | `pub(crate)` | single observer slot |
| `deferred_registry: Rc<DeferredSlotRegistry>` | `pub(crate)` | shared with `MessageCaller` |
| `promoted_slots: PromotedSlots` | `pub(crate)` | promoted-slot table |
| `cancelled_calls: VecDeque<(CallId, CancelCause)>` | `pub(crate)` | bounded ring; `record_cancelled_call` only writer |
| `cancelled_call_cause_evictions: u64` | `pub(crate)` | counter |

**Constants** (currently `const`, no change): `INITIAL_ENTRY_CAPACITY`,
`INITIAL_CHILD_RECORD_CAPACITY`, `INITIAL_SUPERVISOR_CAPACITY`,
`CANCELLED_CALL_RING_CAPACITY` (already `pub`).

**Type aliases** to make `pub(crate)`:
`ErasedTranslator`, `ErasedIsolateCallTranslator`.

## Runtime — private support types

These currently sit at module scope inside `lib.rs`. Each becomes
`pub(crate)` so submodules can name them in their signatures and
construct them.

| Type | Kind | Use |
|---|---|---|
| `InFlightCall` | struct | Tracks one outstanding driver call. Used by `in_flight_calls` field. |
| `CallDispatchContext` | struct | Per-call dispatch context |
| `DeliveredMessage` | struct | Per-step inbox row |
| `RegisteredEntry<S, F>` | struct | One row of the address book |
| `ChildRecord<S, F>` | struct | Lineage record |
| `SupervisorRecord` | struct | Supervision policy row |
| `RegisteredAddress` | struct | Internal address index |
| `MessageCallContext` | struct | Per-message call routing context |
| `ErasedSend` | struct | Type-erased pending send |
| `QueuedRemoteSend` / `QueuedRemoteEnvelope` | structs | Cross-shard send transport |
| `RemoteCallReply` | struct | Cross-shard call reply transport |
| `SendableRemoteCallReply` | struct | `Send`-bound transport variant |
| `SendableQueuedRemoteSend` / `SendableQueuedRemoteEnvelope` | structs | `Send`-bound transport variants |
| `ErasedMessage` | enum | Type-erased message body |
| `ErasedEffect<S, F>` | enum | Type-erased effect |
| `MailboxAdapter<M, Msg>` | struct | Type-erased mailbox |
| `AnyMailboxAdapter` | struct | Same, fully erased |
| `HandlerAdapter<I, Outbound>` | struct | Type-erased handler |
| `SendableHandlerAdapter<I, Outbound>` | struct | `Send`-bound handler |
| `SpawnAdapter<I, Outbound>` | struct | Type-erased spawn request |
| `RestartableSpawnAdapter<I, Outbound>` | struct | Restartable spawn |
| `SpawnObservedAdapter<...>` | struct | Spawn-observed adapter |
| `SpawnOutcome<S, F>` | struct | Spawn result |
| `SpawnObservedOutcome<S, F>` | struct | Spawn-observed result |
| `StoredTranslator` | struct | Boxed call-result translator |
| `PendingIsolateCall` | struct | Pending isolate→isolate call |

**Traits**, all to become `pub(crate)`:

| Trait | Use |
|---|---|
| `ErasedMailbox` | Type-erased mailbox shape |
| `ErasedHandler<S, F>` | Type-erased handler shape |
| `ErasedSpawn<S, F>` | Type-erased spawn |
| `ErasedSpawnObserved<S, F>` | Type-erased spawn-observed |
| `ErasedRestartRecipe<S, F>` | Restart recipe |
| `IntoErasedSpawn<S, F>` | Conversion into erased spawn |
| `IntoErasedSpawnObserved<S, F, ParentMessage>` | Conversion into observed spawn |

## Runtime — method binning

152 methods on `Runtime` (impl block + helper impls). Each maps to one
of: parent (stays in `lib.rs`), `mod registration`, `mod dispatch`,
`mod remote`, or `mod host_call`. Methods called by another method
become `pub(crate)`.

### Stays in `lib.rs` (constructors + cross-cutting)

- `new`, `with_betelgeuse_io_loop`, `with_clock`, `with_clock_and_ids`,
  `with_clock_and_ids_and_driver`,
  `with_clock_and_ids_and_driver_and_preallocation`

### → `mod registration` (~22 methods, all `pub(crate)` if not already `pub`)

Public:
- `register`, `register_with_capacity`,
  `register_with_capacity_and_bootstrap`,
  `register_with_capacity_using`,
  `register_service`, `register_service_send_only`,
  `register_split_service`,
  `supervise`, `try_supervise`

Private (become `pub(crate)`):
- `register_entry`, `register_sendable_with_capacity`,
  `register_sendable_with_capacity_and_bootstrap`
  (already `pub(crate)`), `register_sendable_entry`
- `spawn_isolate`, `record_child`
- `entry_index`, `entry_by_isolate`, `child_record_index_by_child`,
  `supervisor_index`, `try_registered_address`
- `enqueue_bootstrap_message`
- `enqueue_entry_message`, `recv_entry_message`

### → `mod dispatch` (~50 methods, the biggest bin)

Public:
- `step`

Private (become `pub(crate)`):
- `step_with_remote`
- `build_message_caller`
- `promote_captures`, `sweep_dropped_deferred_slots`,
  `drop_pending_deferred_captures`,
  `drop_promoted_deferred_slot`
- `execute_effect`, `reject_call_context`,
  `push_call_rejected_event`, `execute_reply_to`
- `dispatch_call`, `dispatch_driver_call`,
  `dispatch_observed_send`, `deliver_observed_send_outcome`,
  `dispatch_isolate_call`, `dispatch_cancel_call`
- `harvest_isolate_call_timeouts`,
  `cancel_pending_isolate_calls_for_owner`
- `record_cancelled_call`, `recently_cancelled_cause`,
  `close_deferred_slot_for_call_with_reason`,
  `complete_isolate_call`, `deliver_isolate_call_outcome`
- `advance_driver`, `deliver_completion`,
  `push_persistence_completion_events`
- `cancel_in_flight_call_for_resource_close`,
  `cancel_in_flight_calls_for_shutdown`,
  `cancel_driver_calls_for_requester`, `remove_translator`
- `stop_entry`, `stop_entry_with_precollected`,
  `stop_entry_with_result`, `stop_entry_full`
- `restart_children`, `supervise_panic`, `restart_child_record`
- `push_event`, `enforce_trace_retention`, `active_trace_len`,
  `compact_trace_prefix`, `compact_trace_prefix_if_empty_or_large`
- `gc_stopped_entries`, `can_gc_stopped_entry`
- `notify_signal`

### → `mod remote` (~6 methods)

Private (become `pub(crate)`):
- `dispatch_local_send`, `dispatch_local_send_with_context`
- `harvest_remote_envelope`, `harvest_remote_send`,
  `harvest_remote_call_reply`,
  `complete_remote_isolate_call`

### → `mod host_call` (~15 methods)

Public:
- `try_send`, `try_send_event`
- `observe_next_bound`, `observe_next_tls_bound`,
  `observe_isolate_complete`, `observe_operation_done`,
  `observe_child_restarted`, `observe_result`
- `has_in_flight_calls`, `trace`, `pressure_summary`
- `set_trace_retention`, `set_trace_observer`

Private (become `pub(crate)`):
- `trace_storage_len`, `entry_count`, `io_pending_count`,
  `resource_report`
- `lineage_snapshot`, `child_record_snapshot`,
  `supervisor_snapshot` (already `pub(crate)`)

## Simulator — same audit, same answer

`Simulator<S>` has 27 fields and 175 methods. Pattern is identical:

- All 27 fields → `pub(crate)`. Notable ones: `shard`, `config`,
  `entries`, `child_records`, `supervisors`, three `next_*_id`
  counters, `ids`, `trace`, `virtual_now`, `virtual_anchor`,
  `step_ordinal`, `timers`, listener/stream/socket/tls/file state
  vectors, `pending_*` queues, `in_flight_calls`, `translators`,
  `pending_isolate_calls`, `round_messages`, `cancelled_calls`,
  `deferred_registry`, `promoted_slots`, `trace_observer`.
- Private support types (`InFlightCall`, `RegisteredEntry`,
  `ChildRecord`, `SupervisorRecord`, `ListenerState`, `StreamState`,
  `UdpSocketState`, `TlsListenerState`, `TlsStreamState`, `FileState`,
  `PendingAccept`, `PendingConnect`, `PendingUdpRecv`,
  `PendingTcpCompletion`, `TimerEntry`, `DeliveredMessage`,
  `RegisteredAddress`, `MessageCallContext`, `SpawnOutcome`,
  `SpawnAdapter`, `RestartableSpawnAdapter`, `SpawnObservedAdapter`,
  `StoredTranslator`, `PendingIsolateCall`, `CheckerFailure`)
  → `pub(crate)`.
- Method bins follow the plan: `mod simulator` (step + execute_effect
  + reject/reply/push_event family), `mod resources` (listener /
  stream / socket / file / pending-accept management), `mod calls`
  (dispatch_call / dispatch_backend_call / dispatch_observed_send /
  dispatch_isolate_call / harvest_isolate_call_timeouts /
  complete_isolate_call / driver call lifecycle), `mod projection`
  (trace projection helpers, currently in `dst::projection` —
  re-export if there is overlap, otherwise keep in mod.rs).

The sim's `internals.rs` already exists and already exposes the
`pub(crate) trait` shapes for the `Erased*` family. The visibility
work is therefore smaller here than on the runtime — the main
exposure that's missing is the field set on `Simulator` itself.

## Execution checklist for the next session

1. **One-pass visibility widening** (10–15 min):
   - In `tina-runtime/src/lib.rs`, mark every `Runtime<S, F>` field as
     `pub(crate)`.
   - Mark every private support struct / trait listed above as
     `pub(crate)`.
   - Mark every helper method that another method calls as
     `pub(crate)`. (`cargo check` will name each missing one if any
     are forgotten.)
   - Same pass in `tina-sim/src/lib.rs`.

2. **Create the submodule files** (each as `pub(crate)` impl blocks
   on `Runtime<S, F>` / `Simulator<S>`):
   - `tina-runtime/src/registration.rs`
   - `tina-runtime/src/dispatch.rs`
   - `tina-runtime/src/remote.rs`
   - `tina-runtime/src/host_call.rs`
   - `tina-sim/src/simulator.rs`
   - `tina-sim/src/resources.rs`
   - `tina-sim/src/calls.rs`

3. **Move methods one bin at a time, run `cargo check -p ...` between
   bins.** Order matters: do `host_call` first (smallest), then
   `remote`, `registration`, `dispatch`. Each successful compile
   confirms the visibility pass was complete enough for that bin.

4. **Move the support types** alongside the dispatch/spawn machinery
   that owns them (e.g., `Erased*` adapters with `mod dispatch`,
   `Queued*Remote*` with `mod remote`). The fields that hold instances
   of those types stay on the struct in `lib.rs`; only the type
   definitions and their non-`Runtime` impls move.

5. **Run the full test suite per crate after each bin lands.**
   `cargo test -p tina-runtime --tests`, `cargo test -p tina-sim`,
   `cargo clippy ... -- -D warnings`.

6. **No behavior change.** Anything that wants to be more than a move
   (rename, sign change, doc rewrite) goes in a separate commit.

## What this audit deliberately does **not** do

- Does not propose making anything new `pub` (i.e., visible outside
  the crate). The crate's public surface is unchanged.
- Does not propose accessor methods in place of `pub(crate)` fields.
  Inside this crate every method already has direct field access; the
  audit just keeps that access working after the file split. If a
  future refactor wants to put invariants behind accessors, that is a
  separate phase.
- Does not propose changing any field type, default, or layout. The
  goal is exactly: same code, more files.
