# Specimen Findings — Current Product Work

This file is the current action list. Examples are specimens: they
show how Tokio and Tina code feel for the same kind of job. When the same
Tina pain appears across specimens, it becomes runtime/API work here.

The active list below is what Tina still needs. Earlier rounds that
have closed are summarized further down so external references stay
valid; the long-form history lives in
[`FINDINGS_HISTORY.md`](FINDINGS_HISTORY.md).

## Active

### 2026-07-12 Copied service path flow migration

The canonical `system_copied_service_path` now uses `LocalSystem` and a
request-aware raw flow for its held work. The flow carries the original
`RequestContext`, durable record id, `SharedLease`, and exhaustive `SleepReply`
without qids, `GuardedPendingReplies`, a redundant pending-capacity knob,
take/reinsert logic, or manual service envelopes. The shared scope remains the
honest bound on parked requests. Callers already closed when their queued turn
runs never cross the durable admission boundary; later caller loss, timer
failure, and owner stop all settle the move-only lease.

Load callers and the Stats host call distinguish every `CallOutcome`; typed
work failures retain their `CallError` class, including `TimerFull`, and outer
host errors remain separate. The application facade is fallible. Registration,
reporting, and proof failures cannot bypass terminal shutdown, and the final
scope snapshot proves admitted equals released with zero current authority.
This applies the request-aware flow to the first service new users are told to
copy.

Post-merge stress exposed a lower driver defect in the new owner-stop proof:
empty storage, TLS, Unix, and TCP lanes still invoked whole-loop backend
cancellation. A timer-only shutdown could therefore fail spuriously with
`DriverShutdownFailed` even though no I/O completion slot existed. Empty lanes
now skip backend cancellation while non-empty lanes retain the bounded
drain/quarantine contract. A tracked-backend regression proves zero cancel
calls for empty shared I/O; retained-completion quarantine tests and 200
consecutive copied-path owner-stop runs prove both sides of the boundary.

This driver hotfix is independent of address provenance; the provenance work
only stamps and validates routing identity and does not alter backend
cancellation or completion draining.

### 2026-07-12 Split-service outbound facade prerequisite

**Surfaced by:** `ergonomics_playground`'s debounced batch client.

A mixed event/request client could use the typed `send_event` and
`call_request` helpers, but its isolate declaration still had to expose the
private routing shape as
`Outbound<ServiceMessage<BatcherEvent, BatcherRequest>>`. Tina now exports
`ServiceOutbound<Event, Request>` as the canonical associated type for that
capability. It is a transparent alias, so it adds no conversion layer and does
not weaken the separate event/request address rails. Runtime and compile-fail
proofs use the public spelling, and the motivating batching migration can now
contain no direct service-envelope vocabulary.

### 2026-07-12 Debounced batch shared-work migration

The `ergonomics_playground` batch probe now models the actual operation: many
callers join one bounded batch. `SharedWork<BatchId, BatchReply>` replaces the
monotonic qid, `PendingReplies`, `(qid, value)` sidecar rows, and manual drain
correlation. One raw typed `flow!` step carries the batch id and exhaustive
`SleepReply`; `TimerFull` is a distinct `TimerFailed(CallError)` reply rather
than being collapsed into application `Full`.

Adversarial review caught a second bound hidden by that simplification:
`SharedWork` bounds live parked callers, while the batch values are accepted
operations. A timed-out caller can be reclaimed before the window closes, so
operation admission now checks the batch-value cap before parking authority.
The regression proves timeout settlement, in-window overload, next-window
refill, and exact terminal accounting; a live one-timer test proves real
`TimerFull` classification rather than only testing the report classifier.

The simulator client records and classifies every `CallOutcome` instead of
discarding non-reply terminals. Drain closes every waiter, clears the active
batch, and makes the physically armed late timer harmless. No batch-specific
framework helper was added: `SharedWork::reply_all_clone` and
`drain_all_with` already produce the smaller, honest application form. Its
split-service declaration now uses `ServiceOutbound`, so the motivating
example contains no direct service-envelope vocabulary.

### 2026-07-12 Request-aware raw flow prerequisite

`flow!` now accepts `-> raw request T` for typed timer and runtime-I/O
continuations that must keep the original `RequestContext`. The generated
variant owns caller authority, move-only captures, and the raw typed outcome;
it does not coerce `Result<T, CallError>` into the broader isolate-call
`CallOutcome<T>`. `then_service_event_with_request` supplies the private split
service envelope for typed calls and sleeps.

The soak-shaped compile proof threads an HTTP lease and then a DB lease through
two timer steps without a qid, `GuardedPendingReplies`, or take/reinsert cycle.
Live and simulator tests use the same service authoring and prove exhaustive
timer results, exact lease release, caller timeout, and owner-stop cancellation
while caller authority is captured. Migrating `system_soak_http_db` remains a
separate example cohort.

Adversarial review also closed two boundary defects. The contextual `request`
qualifier no longer steals an existing plain raw type path such as
`request::Outcome`. More importantly, a local request that times out after it
is queued but before its handler turn now supplies a closed `RequestContext`
if the handler captures it; the runtime preserves the established typed late
reply trace without minting fresh caller authority. Live/simulator proofs now
also preserve an exact raw `CallError::InvalidResource` from typed file I/O and
cover caller-gone and owner-stop capture settlement on both backends.

### 2026-07-12 Unix write-all split-service continuation prerequisite

`UnixWriteAll` now has `next_service_event` and `advance_service_event`, the
domain-event siblings of `next_effect` and `advance`. They delegate to the same
partial-progress state machine, hide only `ServiceMessage::Event`, and preserve
the complete `UnixWriteOwnedReply` plus original `Vec` allocation on success or
failure. Adjacent one-shot Unix operations already inherit
`TypedCall::then_service_event`; no broader loop abstraction was added because
the motivating custom-codec migration only needs write-all.

One event-only writer service runs unchanged on explicit `Runtime` and
`Simulator`. Simulator coverage forces two-byte partial writes through bounded
peer pressure, proves exact completion count and allocation identity, preserves
peer close as `CallError::Io`, and proves owner stop cancels a genuinely parked
write with no report or in-flight authority left. Unix peer-buffer Full is a
parking condition rather than a user-facing `Full` terminal outcome; the test
therefore proves bounded park-and-resume instead of inventing a false variant.
The refill proof also saturates a one-slot service-event mailbox when the
parked write resumes and proves the continuation is retained through overflow.

Adversarial review found that both Unix and adjacent TCP write-all helpers
accepted a plausible fabricated or stale owned reply without proving that a
write was armed or that the reply carried the original allocation. Both now
track one in-flight write, validate allocation identity, reject unarmed/stale
advance calls with `InvariantViolation`, and leave genuine in-flight work armed
when a stale reply is rejected.

This closes the framework prerequisite found by
`tina-extension-custom-codec`; the extension migration is recorded below.

### 2026-07-12 Typed sharded request-service table prerequisite

`ShardRequestServiceTable<Request, Reply, Event = Infallible>` preserves
canonical request-only service capabilities, and split-service request lanes
when explicitly selected, through `new`, `from_placement`,
`try_from_placement`, `address_for`, and key-owner lookup. It shares the
existing placement-order, typed missing-shard, and fallible-registration
contracts without exposing the internal `ServiceMessage` envelope. This is the
narrow prerequisite surfaced and now applied by
`specimen_sharded_fanout_read`.

Adversarial review factored the raw and typed tables onto one invariant
implementation, rejected mislabeled capabilities whose actual address shard
does not match the entry shard, and made both fallible and infallible placement
builders return all already-registered capabilities on failure. Direct tests
exercise registration and typed lookup on explicit, threaded, LocalSystem, and
simulated multi-shard owners; generation tests prove tables remain snapshots
until rebuilt after restart.

### 2026-07-12 Soak HTTP/DB request-aware flow migration

`system_soak_http_db` now applies the request-aware raw flow directly. The
service carries the original `RequestContext`, HTTP lease, and DB lease through
two exhaustive `SleepReply` stages without qids, `GuardedPendingReplies`,
take/reinsert cycles, or manual `ServiceMessage` construction. The former
`pending_capacity`, `PendingFull`, and `PendingDuplicate` surface was
implementation state rather than business pressure; the HTTP shared scope now
provides the honest bound on parked work.

The host uses `call_blocking_request` and exhaustively counts Replied, Full,
Closed, Timeout, and Rejected outcomes. Timer failure remains a distinct reply.
Every run verifies both shared scopes return to zero after terminal shutdown;
a focused caller-timeout smoke proves a parked HTTP lease is cancelled and
released. This closes the motivating example for the request-aware flow
prerequisite.

Adversarial review moved the live host to fallible `LocalSystem`, releases an
HTTP lease immediately when the caller was already gone before its handler
turn, and guarantees registration, worker, classification, and capacity-report
failures still pass through bounded terminal shutdown. Live proofs cover caller
timeout in both the HTTP and DB stages, timer-lane Full as an exact
`TimerFailed(TimerFull)`, gateway mailbox Full, completion-only slow events,
concurrent workers, and zero HTTP/DB authority after shutdown. Unit accounting
keeps every call terminal and every outer threaded-host error distinct.

### 2026-07-12 Extension corpus canonicalization ledger

Every crate under `examples/extensions` was read by hand and run with
`--all-targets`. Four extension proofs needed no isolate-authoring migration;
adversarial review still corrected pressure and validation defects rather than
declaring their existing shapes canonical by inspection:

| Example | Current friction | Desired form | API sufficient | Framework prerequisite | Example branch | Tests | Status |
|---|---|---|---|---|---|---|---|
| `tina-extension-capacity-surface` | None; owned report data joins `CapacitySummary` directly. | Current `CapacitySurfaceReport` constructors and typed assertions. | yes | none | `agent/extensions-canonical` | 1 unit test | canonical |
| `tina-extension-compile-fail` | None; public/private ownership boundaries are compile-fail doctests. | Current public constructors with unforgeable private state. | yes | none | `agent/extensions-canonical` | 4 doctests + count guard | canonical |
| `tina-extension-fake-bridge` | Closed: in-flight accounting happened after enqueue, so a fast worker could underflow the counter and the queue admitted one more job than the reported installed cap. | Reserve total queued-plus-active capacity before dispatch; roll back failed dispatch exactly. | yes | none | `agent/extensions-canonical` | 3 unit tests, including 100 fast-worker iterations | canonical; docs migrated to event handle vocabulary |
| `tina-extension-service-policy` | Closed: a zero limit still admitted the first request for a new key; a zero window had no stable retry contract. | Fallible configuration before policy use, then exhaustive decisions from caller-supplied time. | yes | none | `agent/extensions-canonical` | 2 unit tests | canonical |
| `tina-extension-custom-codec` | Closed: both actors were event-only but used generic message authoring and collapsed Unix errors. | Event-only isolates/registration/sends, envelope-free typed continuations, and exact staged Unix failures. | yes | `UnixWriteAll::next_service_event` and `advance_service_event` (PR #331). | `agent/extensions-canonical` | 8 unit/simulator tests | canonical |

The custom codec README and extension user guide now show the correct public
`SyncCodec::feed` signature (`-> usize`). Fake-bridge documentation now teaches
typed event-only registration and `try_send_event` rather than a generic
message address. No example-local envelope adapter or duplicate write loop was
added. After PR #331, the custom codec resumed and now uses event-only service
handles throughout. `CodecIoFailure` preserves endpoint, bind/accept/connect/
read/write/close stage, and exact `CallError`; codec Full/Malformed policy
outcomes remain separate from transport failure. Adversarial failure probes
exercise every staged rail outcome on both endpoints where applicable, and the
one-shot server now closes both its stream and listener instead of relying on
simulator teardown to hide the listener. The fake bridge now reserves its
installed in-flight cap before dispatch, and the custom policy rejects
self-contradictory zero configurations. The extension sweep is now complete.

### 2026-07-12 Runtime address provenance prerequisite

Address identity now includes an opaque `SystemIncarnation` ahead of shard,
isolate, and generation. Every live or simulated owner stamps one incarnation
across all of its shards, while independently constructed owners receive
distinct nonzero incarnations. Deterministic owners can configure the value
explicitly, including matching live/simulator fixtures without relying on
process-global construction order. Address capability wrappers, contexts,
typed continuations, erased sends, remote envelopes, observation keys, and
host call routing all preserve and validate the stamp before shard or isolate
routing.

Typed threaded and call surfaces report `ForeignSystem` without claiming call
or observation authority. Explicit-step and simulated ingress use a routing-
level `IngressSendError::ForeignSystem` that returns message ownership without
misreporting a mailbox closure. Focused tests cover coincident
foreign address tuples, exact message drop settlement, same-owner cross-shard
identity, preferred `LocalSystem` routing, deterministic replay, configured
live/simulator parity, stale post-restart addresses, and replacement delivery
within the original system incarnation. This prerequisite prevents an address
from one example-owned runtime from accidentally targeting a coincident tuple
in another as the corpus moves onto `LocalSystem`.

### 2026-07-12 Lock-manager keyed FIFO canonicalization

Migrated `system_lock_manager` from the historical
`PendingReplies<u64, LockReply>` + monotonic waiter ids + per-lock
`VecDeque<u64>` sidecar onto `SharedWork<String, LockReply>`. The specimen now
uses `with_key_limit`, `wait`, and `take_next`; the helper owns FIFO order,
global and per-key admission, caller-gone reclamation, and exact occupancy.
`SharedWorkError::Full` and `KeyFull` remain distinct as
`Busy(GlobalFull)` and `Busy(KeyFull)` rather than collapsing terminal
pressure. The host is now a fallible `LocalSystem`, and lease continuations
carry and exhaustively handle `SleepReply`.

Direct live coverage proves FIFO hand-off, capacity-one caller-timeout
reclamation and refill, distinct global/per-key Full rails, keyspace Full,
release and expiry hand-off, renew plus stale timer suppression, stale
release/renew rejection, zero final waiter/key occupancy, and clean bounded
shutdown. Focused unit probes prove a current-generation timer failure retires
an unenforceable lease, a stale failure cannot revoke the current holder, and a
caller that closes after FIFO selection leaves at most a lease-bounded ghost
holder before expiry rollback. This fully applies closed finding 21 to its
remaining motivating specimen; no new framework gap surfaced.

### 2026-07-12 Bounded scatter/gather operation prerequisite

`ScatterGather<K, R, Q>` now owns the original `RequestContext`, ordered target
rows, and cancelable child authority through `CallJoinSet`. `start_service`
accepts a fully `BoundedItems`-validated target list and a typed call factory;
the factory receives the configured per-target timeout, so the documented
deadline cannot drift from the executed call. Missing targets, replies, Full,
Closed, Timeout, Rejected, and aggregate timeout remain distinct and preserve
caller order.

Every reply, aggregate timeout, and cancel acknowledgement carries the public
operation token, so a bounded collection can route concurrent aggregates
without private qids or colliding per-operation branch generations. Aggregate
expiry marks only still-pending rows, emits a bounded cancellation
batch, and withholds caller authority until every cancel acknowledgement is
recorded. Generation tokens reject duplicate and late overwrites. The aggregate
timer also carries an operation token, so a physically non-cancelable timer from
a completed request cannot expire a newer request in the same coordinator.
Start failures return the untouched `RequestContext`, and over-cap or duplicate
input is rejected before the call factory or effect batch exists. One
coordinator implementation is exercised unchanged on `Runtime`,
`ThreadedRuntime`, `Simulator`, `MultiShardRuntime`, and
`ThreadedMultiShardRuntime`; owner stop with child authority pending closes the
original caller.

`ScatterGatherOperations<K, R, Q>` closes the concurrent coordinator gap. It
owns a fixed-capacity operation collection, rejects `Full` before building a
call, and routes the unified `ScatterGatherEvent<K, R>` vocabulary. Application
coordinators now need one event variant, one bounded field, one inferred
`start_service` call, and one inferred `advance_service` call; they no longer
spell reply/cancel/timer variants, qids, token lookup, or find/remove logic.

`specimen_scatter_gather` now applies this prerequisite directly. Its
coordinator contains only the worker list plus `ScatterGatherOperations`; one
`Scatter` event replaces qids, `PendingReplies`, partial rows, manual batches,
and terminal folding. The completed typed report reaches the driver without
collapsing target outcomes. A capacity-one live probe proves one admitted
operation, typed `Full` for every excess caller, exact caller settlement,
same-runtime refill, target-order and reply-identity validation, and clean
shutdown.

`specimen_sharded_fanout_read` now applies both prerequisites. Shard counters
are request-only services stored in `ShardRequestServiceTable`; the coordinator
has one request, one scatter event, and one capacity-one operations owner.
`ReplyAdapter`, `Bind`, `Start`, `pending_targets`, manual sorting, raw outbound
sends, and service-envelope types are gone. The host uses
`call_blocking_request`, matches every terminal outcome, and rejects partial,
reordered, misrouted, or wrong-value reports before producing the public sum.
Together the two specimens
close the motivating scatter/gather example cohort.

### 2026-07-11 Bounded shutdown truth across the example corpus

Migrated production examples away from exclusive-`Arc` teardown and
transport-only shutdown success. Shared runtimes capture a
`ThreadedShutdownHandle` at construction, use
`request_and_wait_report(total_timeout)`, drop the remaining owner only after
terminal observation, and require `LocalSystemTerminalReport::ensure_clean()`.
Owned runtimes use `shutdown_report().ensure_clean()`. Explicit auxiliary
server, worker, stop-signal, and join failures are propagated instead of
discarded; WebSocket room shutdown returns a typed timeout with the last
snapshot when close settlement misses its bound.

The perf corpus now records leak cleanliness only after Tina terminal truth or
Tokio stop-and-join truth succeeds, preserving any earlier failed surface
observation. `scripts/examples_shutdown_truth_guard.sh`, wired into
`verify-guards`, rejects exclusive-owner and transport-only runtime shutdown,
discarded synchronous or Tokio task joins, ignored service stop sends, and
ignored bridge drain reports. The guard strips literals and comments before
matching so documentation cannot masquerade as lifecycle code.

**Still open:** broader `LocalSystem` host-facade migration remains a separate
ergonomics cohort; this slice establishes truthful shutdown behavior for the
current hosts.

### 2026-07-11 Fallible production startup propagation

Migrated every production-shaped example host from the panic convenience
constructors to `ThreadedRuntime::try_*` or `LocalSystem::try_build`, preserving
`StartupError` and its source chain through the host's existing error return.
Public server/demo helpers that previously returned an initialized host or
panicked now return `Result`; test-only fixtures unwrap explicitly at the test
boundary. `scripts/examples_startup_api_guard.sh`, wired into `verify-guards`,
prevents the infallible constructors from returning to production example
sources.

**Still open:** this closes panic-on-startup behavior, not the broader
`LocalSystem` host-facade migration. Raw `ThreadedRuntime::try_*` hosts remain
the next applied-ergonomics probe; bridge-heavy examples should expose whether
`LocalSystem::into_threaded_runtime` is sufficient or a facade API is missing.

### 2026-07-12 Remaining raw Isolate → macro (rooms / fanout / grpc / mini_saas)

Converted the last hand-rolled `impl Isolate` / `isolate_types!` blocks:

- `specimen_sharded_fanout_read` ShardCounter (`send` + `AppShard`) + ScatterCoord (`tina::isolate`, `Io=Infallible`)
- `specimen_grpc_counter` StreamingEchoSource
- `specimen_websocket_room` Gateway + Room
- `system_realtime_rooms` Room + Gateway (dropped manual `CallableIsolate` stamps; macro now owns them)
- `mini_saas_api` NotifySink + Controller

**Still open (not raw Isolate):**
- Bind/Start paired-registration ceremony in scatter fanout (finding 3)
- LocalSystem / fallible startup migration for production-shaped hosts
- event-only / request-only form sweep where placeholders remain


### 2026-07-11 Raw `impl Isolate` → macro cohort (local I/O + sqlite + cross-shard)

- `specimen_local_io_codec_ipc` — Ingest/Seeder/CopyPump, AdminServer/Client,
  KeyspaceServer/Client, live Unix Probe all on `#[tina_runtime::isolate]`
- `specimen_sqlite_counter` Caller/QueryCaller
- `specimen_cross_shard_child_ownership` Worker

Still raw: websocket/realtime rooms, mini_saas Controller/NotifySink,
sharded_fanout_read, grpc StreamingEchoSource (specialized Io/Send/protocol
shapes).


Finding numbers are stable across phases — when a finding closes it
moves to the [Closed](#closed) section below with the same number.

### 2026-07-11 Raw `impl Isolate` → macro cohort (partial)

Converted remaining mechanical raw/`isolate_types!` blocks onto
`#[tina::isolate]` / `#[tina_runtime::isolate]`:

- `tina-extension-custom-codec` CodecServer + CodecClient (`shard = CodecShard`)
- `specimen_http_body_streaming` StreamingService
- `specimen_webhook_publisher` Driver
- `ergonomics_playground` QuoteClient / BatchClient / CacheClient

**Still raw (next slice):** `specimen_local_io_codec_ipc/*`,
`specimen_sharded_fanout_read`, `specimen_sqlite_counter`,
`specimen_grpc_counter`, `specimen_cross_shard_child_ownership`,
`specimen_websocket_room`, `system_realtime_rooms`, `mini_saas_api`
Controller/NotifySink. Some of these own non-default `Io`/`Send`/shard
shapes and need careful macro attributes rather than a rename.

### 2026-07-11 Envelope-free continuation cohort

Closed the remaining application-level `ServiceMessage::Event` /
`ServiceMessage::Request` construction in the examples corpus by
migrating onto the landed helpers (`then_service_event`,
`reply_service_event`, `call_request`, `call_cancelable_request`,
`send_event`, `register_split_service`) and two small missing set/scope
helpers:

- `CallGroup` / `CallJoinSet` / `CallSelectSet::start_cancelable_service_event`
- `RequestScope::cancel_into_service_event_effect`

**Migrated (no remaining envelope construction in effect/call sites):**

- specimens: `backpressure_chain`, `cancellation_chain`,
  `multi_turn_request_context`, `request_scope_fanout`, `scatter_gather`,
  `two_stage_pipeline` (comment only), `worker_pool`
- systems: `api_gateway_limits`, `bounded_object_lane`, `cache_with_fill`,
  `copied_service_path`, `job_queue`, `lock_manager`, `metrics_shipper`,
  `scoped_request_tree`, `soak_http_db`, `webhook_relay`,
  `ergonomics_playground` (also switched races/batch/cache probes onto
  typed `ServiceRequestAddress` + `register_split_service`)

**Still open after this cohort (not envelope construction):**

- Raw `impl Isolate` blocks (extensions custom-codec, local I/O specimens,
  websocket/room gateways, driver clients in ergonomics_playground) —
  next cohort: macro/`#[isolate]` form where lanes allow.
- Production-shaped hosts still on bare `ThreadedRuntime::new` rather
  than `LocalSystem` + fallible startup — next cohort.
- `specimen_sharded_fanout_read` Bind/Start paired-registration ceremony
  (finding 3) — still a framework gap.
- Type aliases like `SoakMsg = ServiceMessage<…>` remain only where an
  `HttpListener` (or similar rail) needs the envelope type parameter.

### 2026-07-09 Examples Canonicalization Pass

Swept the example crates to the current canonical Tina shapes. Every
touched crate still builds `--tests --offline` and its existing
tests/goldens pass unchanged (canonicalization must not change observed
behavior). What moved, and what was deliberately left:

**Canonicalized:**

- **`tina::flow!`** — `specimen_two_stage_pipeline` (closes finding 11;
  also deleted the qid/`PendingReplies` correlation table).
- **Split-service `#[isolate(event=.., request=.., reply=..)]`** —
  `system_job_queue` (Worker; closes finding 25),
  `system_metrics_shipper`, `system_webhook_relay`,
  `system_api_gateway_limits`, `system_bounded_object_lane`,
  `ergonomics_playground` (5 isolates), and specimens
  `specimen_cancellation_chain`, `specimen_scatter_gather`,
  `specimen_bounded_batcher`, `specimen_worker_pool`, and `ServiceC` in
  `specimen_backpressure_chain`.
- **`register_with_capacity_and_bootstrap[_on]`** — `system_job_queue`,
  `system_session_auth` (closes finding 24), `perf_native` (3 h2 client
  sites), `system_realtime_rooms`.

**Deliberately left (reason):**

- `system_soak_http_db` — `flow!` cannot type its `sleep().then()`
  continuations (finding 29, negative result recorded there).
- `system_scoped_request_tree` — split-service breaks its generic
  `HttpListener<S, TreeMsg>` ingress: the `From<HttpRequest>` impl the
  listener needs would land on the foreign `ServiceMessage<..>` alias
  (orphan rule, E0117). Migrating re-architects the inbound path.
- `system_tenant_rate_limiter`, `specimen_rate_limited_worker`,
  `specimen_idempotent_retry`, `system_live_replay_bugbox` — all-request
  or single-variant message sets; no event/request split to make, and
  the reject arm (where present) documents a real policy invariant.
- `ServiceB`/`ServiceA` in `specimen_backpressure_chain` — blocked by
  finding 36 (`RequestCall` has no `now()`).
- `QuoteGateway` race in `ergonomics_playground` — stays on `CallGroup`;
  it needs a business-success classifier (`|q| q.available`) that
  `CallJoinSet`/`CallSelectSet` cannot carry.
- `specimen_sharded_fanout_read` (Bind/Start is open runtime gap,
  finding 3), `specimen_dynamic_worker_pool` /
  `specimen_supervised_worker` (already canonical: self-address ctor /
  `spawn_observed`), `specimen_pool_cancel_reclaim` /
  `specimen_cancellation_chain` pending shape (already on
  `PendingCallSet`, finding 8), `specimen_graceful_pool_shutdown` /
  `specimen_graceful_drain_server` / `specimen_webhook_publisher`
  (README frames the manual shape as the lesson, or register→observe
  ordering is load-bearing).

**New rough edges:** finding 36 — `RequestCall::now()` is missing (now
fixed). Finding 38 — the HTTP/2 rail's `Http2ServiceMessage` lacks the twin
`FromHttpRequest for ServiceMessage` impl, so a split-service isolate cannot
yet serve over HTTP/2 (surfaced migrating `system_scoped_request_tree` over
HTTP/1; PR #277 fixed the HTTP/1 `HttpListener` path only). CLOSED (PR #279):
the twin `Http2ServiceMessage for ServiceMessage` impl landed with an
e2e split-service-over-h2 test.

**API-gap fixes landed (2026-07-09):** the four crates left above were all
unblocked and migrated to canonical form:
- `system_soak_http_db` → `flow!` now has `-> raw T` steps for non-call
  continuations (PR #276, closes finding 29).
- `system_scoped_request_tree` → `tina-http`'s new `FromHttpRequest` trait
  routes around the orphan rule (PR #277).
- `ServiceB`/`ServiceA` in `specimen_backpressure_chain` → `RequestCall::now()`
  added (PR #275, closes finding 36).
- `QuoteGateway` in `ergonomics_playground` → `record_classified_reply` on
  `CallSelectSet` carries a business-success predicate (PR #278).

**Not swept this pass (follow-up):** the ~30 remaining `specimen_*`
crates and `examples/extensions/*` were triaged (no split/bootstrap
anti-patterns found via grep) but not each individually migrated;
`examples/extensions/tina-extension-custom-codec` has two raw
`impl Isolate` blocks a future pass could look at.

### 2026-07-09 Examples Canonicalization Pass (by-hand follow-up)

Hand-read the remaining reject-arm / mixed-lane isolates and migrated
the ones that still carried finding-25 anti-patterns. Every touched crate
builds `--tests --offline` and existing smoke tests pass unchanged.

**Canonicalized (split-service + drop hand-written reject arms):**

- `specimen_multi_turn_request_context` — Probe / Db / Service from raw
  `impl Isolate` + reject arms → `#[tina_runtime::isolate(event=..,
  request=..)]`; Client → `#[isolate(message=..)]`; call sites use
  `call_request` / `SplitServiceHandle::from_address` (sim has no
  `register_split_service`).
- `specimen_two_stage_pipeline` — Pipeline finishes the earlier `flow!`
  migration with split form; stage continuations wrap as
  `ServiceMessage::Event(PipelineEvent::Stage(...))`.
- `specimen_request_scope_fanout` — Worker `handle`/`handle_call` mix on
  one `Wake` message → `WorkerRequest::Run` / `WorkerEvent::Wake`.
- `system_soak_http_db` — Soak `Request`/`Flow` → split; parks via
  `call.capture` + `insert_deferred_guarded` (same shape as
  `system_api_gateway_limits`); host uses `register_split_service`.
- `system_session_auth` — SessionBucket Bootstrap/Sweep events vs
  Login/Touch/Logout/Stats requests; bootstrap prefill becomes
  `ServiceMessage::Event(Bootstrap)`.
- `system_metrics_shipper` — Shipper Tick/FlushDone events vs
  Submit/Stats/Stop requests; `reply_and` for size-flush + arm-tick.
- `system_job_queue` — Queue finishes the earlier Worker-only split;
  Bootstrap/spawn/call-return events vs Submit/Cancel/Stats requests;
  `register_with_capacity_and_bootstrap` keeps working with the Event
  envelope.
- `perf_native` — ChainService Run request / PingReturned event.

**Still deliberately left (reason):**

- `mini_saas_api` NotifySink / Controller — large HTTP ingress surface;
  Controller carries multi-flow `NotifyFlow` + body/capacity ceremony;
  split needs a careful `FromHttpRequest` path, not a drive-by rename.
- `system_scoped_request_tree` — already unblocked by `FromHttpRequest`
  (PR #277) but the TreeMsg split is a separate re-architecture of the
  generic `HttpListener<S, TreeMsg>` parameter; not pure example polish.
- `system_tenant_rate_limiter` — reject arm is for unreachable policy
  decisions under Shed, not an event/request lane mix.
- Pure request/reply isolates (`HttpRequest` counters, keyspace stores)
  and pure fire-and-forget drivers — nothing to split; reject arm is
  absent by construction.
- `examples/extensions/tina-extension-custom-codec` — two raw
  `impl Isolate` blocks that are pure event loops; macro conversion is
  mechanical, not a lane-correctness fix. Left for a formatting pass.
- Driver `register` + `try_send(Begin)` sites — host-owned kick messages
  are not the register-and-bootstrap footgun (finding 24); the service
  does not always need Bootstrap before other work.

### 2026-05-23 Status Pass

The recent Wave A / post-122 / Phase 120 work closed a lot of old pain:
native HTTP/2/gRPC client parity, local I/O/codec/Unix IPC, admission and
rate policy, resource lifetime, durable outbox, ecosystem hooks, and
supervision/fairness reports are now landed and recorded in `CHANGELOG.md`.
Phase 120 also made the copied service path explicit:
`system_copied_service_path`, its companion proof, and a smoke-copy crate now
show the blessed service shape without asking readers to stitch ten specimens
together.

What is still active after reading the specimens and systems:

- **Admission across parked work.** Closed for local concurrency by
  `ConcurrencyPendingReplies`: one bounded owner holds the local
  `ConcurrencyLimit`, parked caller, and optional auxiliary RAII guard.
  `system_api_gateway_limits` uses it with `SharedCapacityReservation`, so
  owner-stop and caller-gone cleanup no longer depend on dropping an explicit
  local permit. Multi-stage guard replacement in `system_soak_http_db` remains
  intentionally explicit because it changes which external budget is held.
- **Race / cancel / retry ceremony.** `ergonomics_playground` and
  `system_job_queue` show the model is correct. `CallGroup::start_cancelable`
  now removes the branch-start token/handle ceremony, and Phase 120 added
  `CallJoinSet` / `CallSelectSet` for the common join-all and select-next
  cases. Re-binding a cancelable caller after worker crash remains
  intentionally unsolved.
- **Cross-isolate setup.** Scatter/gather and paired registration still make
  users write bind/start adapter plumbing for the happy path.
- **Runtime observation while running.** Several protocol/IPC specimens still
  want "observe accumulated facts" without `Arc<Mutex<_>>` side channels.
  Trace projection may be the right blessed path; if not, build a typed
  observation helper.
- **Local I/O companions.** Phase 117 shipped the rails; `UnixWriteAll`,
  `UnixReadToEof`, and the unified `FileCopyBounded` drive path now cover most
  of the boring companions. Framed writers remain open.
- **Session/control-message lifecycle.** Phase 127 added the native WebSocket
  client session and tightened session protocol facts. Phase 120 added typed
  `WebSocketSessionMsg::AppControl` for app-injected `Start` / `Tick` /
  `Drain` messages so systems do not smuggle control through peer text.
  Remaining rough edges are pooled/reconnecting client managers and broader
  protocol hardening.
- **Live trace to sim.** Phase 128 made projection/capture/shrink the copied
  path, and Phase 120 added `RunCapture` plus `capture_run` / `save_bug` /
  `replay_bug` / `shrink_bug` workflow wrappers. Remaining rough edges are
  adding more supported live facts and using the workflow in more
  production-shaped systems. Phase 143 adds the overload-shaped names
  (`capture_overload_run`, `save_overload_bug`, `replay_overload_bug`) and
  bounded capacity assertions; protocol-specific overload facts remain the
  next expansion point.

Some older entries below are partly historical and say "shipped" inside the
section. Keep their numbers stable until the next cleanup pass moves those
paragraphs to `FINDINGS_HISTORY.md`.

### Admission and rate policy ergonomics

**Surfaced by:** `system_tenant_rate_limiter`, `system_api_gateway_limits`,
`specimen_rate_limited_worker`, `specimen_idempotent_retry`.

What felt good:

- One decision shape (`AdmissionDecision`) reads identically across every
  limiter — gateway, tenant rate limiter, pacing worker, retry relay all
  match the same `Admitted | Full | RateLimited { retry_after } | Wait |
  Degrade | Closed | TimedOut`. Learn it once, use it everywhere.
- Passing `now` in explicitly (`try_admit(&key, ctx.now())`) feels like
  boilerplate until replay. The sim test runs the exact same line under
  virtual time and gets byte-identical decisions across runs *and across
  seeds*. The boilerplate buys determinism nothing else can.
- `retry_after` is exact, not approximate — time-based tests assert
  `== 100ms` and `== ["k=ok", "k=rate(100ms)"]` with no jitter tolerance.
- Move-only permits + RAII release: parking a
  `SharedCapacityReservation` as a `GuardedPendingReplies` guard makes
  owner-stop release fall out for free (`current == 0` after shutdown
  is the proof).
- `FullHandling` composition keeps retry visibly caller-owned, and
  "idempotency key named on the message" is the right home for the safety
  claim.

What felt rough:

- **Closed: local concurrency across parked work.**
  `ConcurrencyPendingReplies` owns both the `ConcurrencyLimit` and guarded
  pending slots, rather than changing `ConcurrencyPermit`'s deliberately loud
  drop semantics. Reply releases as completed; caller-gone sweep, drain,
  rollback, and owner drop retire without completion. Because permits never
  leave the owner, wrong-gate release is unrepresentable and no `Arc`/atomic
  back-reference is required. Its report exposes policy current, parked
  current, completion/retirement, duplicate, reclaim, and both Full counters;
  `counts_agree()` makes ownership drift directly testable.
- **Charging two shared budgets per request used to be manual two-phase with
  rollback.** Closed by `SharedCapacityReservation::try_reserve([...])`, which
  admits every charge or drops earlier leases before returning the full scope.
  `ConcurrencyLimit::with_shared_scope` still takes only one shared scope.
- **The exhaustive 7-variant match is honest but verbose.** A policy that
  can only produce 2–3 variants (`RateLimit<()>` in the pacing worker)
  still forces a pile of `_ => unreachable!()`. `into_admitted()` saves the
  tests; handlers usually want a per-variant reply and eat the match.
  **Build (maybe):** a decision→reply mapping helper, or narrower decision
  subsets per policy.
- `PressureAction` on `RateLimit` governs only the *table-full* path, not
  the per-key rate decision (which always returns `RateLimited`). Correct,
  but `on_table_pressure` needs a comment so a reader doesn't expect it to
  reshape rate-limit rejections.
- `evict_key_for_capacity` is a footgun the type system can't guard —
  convention + the `evicted_count()` counter only. And the
  `KeyedLimit`-has-no-eviction / `RateLimit`-does asymmetry (live permits
  would dangle) takes a beat to internalize.

### Protocol facts to replay (Phase 112)

What felt good:

- Adding `Fact = ProtocolFact` to a protocol isolate is one line on the
  macro form. The `IntoRuntimeFact` bound at registration catches a
  typoed fact type as a compile error instead of a runtime mystery.
- `TraceProjection::protocol_facts()` and the named siblings let test
  code compare only protocol behaviour without touching the broader
  trace shape.
- The compile-fail fixtures pin the diagnostic shape: an ordinary
  isolate emitting a `ProtocolFact` shows "expected `Infallible`,
  found `ProtocolFact`" right at the call site, which is the shape a
  future reader will recognise.

What felt rough:

- Threading a mutable `effects: &mut Vec<Effect<Self>>` through five
  layers of response helpers (`enqueue_response`,
  `queue_or_send_response`, `send_pending_response`,
  `flush_pending_responses`, `handle_window_update`) is the price of
  emitting facts at the point each truth happens. The alternative
  shape — buffering facts on the isolate and draining at handler
  return — was tried and reverted: it added a hidden `pending_facts`
  field, separated emission from truth, and was a worse spelling. The
  thread-through version is verbose but makes the call sites honest.

### 2. ScatterCoord setup is heavy for the happy path

**Surfaced by:** `specimen_sharded_fanout_read`.

A bounded scatter/gather over three shards needs:

- coord isolate registration with `ScatterCoordMsg::{Bind, Start, Reply}`;
- a `ReplyAdapter<ShardReply, ScatterCoordMsg, S>` registration and
  `From<ShardReply> for ScatterCoordMsg` impl;
- a `Bind { bridge }` send before the `Start`;
- caller-owned `pending_targets` / `outcomes` bookkeeping until every
  target is in.

That is the right *shape* for the rich pressure form (per-target timer,
aggregate timer, partial outcomes), but the ceremony is the same for the
"three shards, all reply, sum the results" case. The per-call-site setup is
roughly the size of the actual scatter/gather logic.

**Build:** a small `scatter_gather!` builder or a
`ScatterCoord::register(table, config, on_complete)` helper that wires the
adapter, the bind/start handshake, and the `pending_targets` /
`outcomes` accumulator at the same shard the coord lives on. Must keep the
typed partial-outcome surface — convenience may not collapse `Full` /
`Closed` / `Timeout` into one bucket.

### 3. Self-address at registration time

**Surfaced by:** `specimen_sharded_fanout_read`,
`specimen_dynamic_worker_pool`.

The `ReplyAdapter` pattern needs the coord's own address to wire the
adapter, and the coord needs the adapter's address before it can fan out.
Today the answer is a `Bind { bridge }` (or `Begin { self_addr }`) message
before `Start`. That works but adds a variant whose only job is to land
"you, isolate, look here for your replies" into the isolate's state.

**Build:** a way for an isolate to learn its own typed address at register
time — for example, a constructor closure parameter `|self_addr| {
ScatterCoord { ..., self_addr } }`. Avoids the bind-before-start handshake
and removes the `Option<Address<...>>` field that's only `None` for one
turn.

Self-address half shipped on the single-shard runtimes:
`Runtime::register_with_capacity_using(cap, |self_addr| ...)` and
the threaded mirror. `specimen_dynamic_worker_pool` migrated to it;
the chicken-and-egg `Begin { self_addr }` variant is gone.
Multi-shard parity (`MultiShardRuntime` /
`ThreadedMultiShardRuntime` / simulator) is deferred until a
multi-shard example needs it.

Still open: the cross-isolate handshake half — `Bind { bridge }` in
`specimen_sharded_fanout_read` is *not* about self-address, it's about
two isolates needing each other's addresses at registration. That
needs a paired-registration primitive or a different shape.

### 7. Reqwest-bridge flatten edge: useful but per-call-site

**Surfaced by:** `specimen_webhook_publisher`.

The `tina-reqwest-bridge` ergonomics polish shipped
`flatten_outcome(outcome) -> Result<R, ReqwestCallError>` as an
opt-in flat-error helper. Building a specimen that uses all three
call shapes (`send_request`, raw `call(addr, ReqwestMsg::Send(...))`,
and `send_request` + `flatten_outcome` at the reply translator) made
it clear that flattening is *useful* — the consumer-side match drops
from five arms to three without losing the bridge-vs-worker layer
naming — but the call-site syntax for shape 3 is denser than for
shapes 1 and 2:

```rust
.then(DriverMsg::PostedViaSendRequest)                // shape 1: bare ctor
.then(DriverMsg::PostedViaRawCall)                     // shape 2: bare ctor
.then(|outcome| DriverMsg::PostedFlattened(flatten_outcome(outcome))) // shape 3: closure
```

A first-time reader has to look at shape 3 twice. Mixing layered
and flat call sites in the same isolate without a comment explaining
why some are layered is confusing.

**Build:**

- Keep `flatten_outcome` opt-in. Do not default it.
- Document explicitly: "pick layered or flat per call-site cluster,
  not per-isolate-mixed-mode."
- Consider a derive-style helper that produces a continuation enum
  variant + a bare-function translator from one declaration, so
  shape-3 call sites read the same as shapes 1/2. Not urgent —
  punt until a non-pedagogical user actually mixes the two and
  flinches.

### 8. External cancellation API — first form shipped

**Surfaced by:** `specimen_cancellation_chain`.

**Resolved (Tina cancellation phase):** Tina now ships
`call_cancelable(addr, msg, t).then(...)` returning a caller-owned
`CallHandle`, plus `cancel_call(handle).then(...)` that closes one
pending isolate call's wait. The handle is move-only and not `Clone`,
and is stamped with `(call_id, shard_id)` on dispatch so a cancel
issued from a different shard is rejected with a typed
`CancelOutcome::WrongShard` instead of silently no-op'ing.
Cancellation is visible truth: `CancelOutcome` (`Cancelled` /
`AlreadyCompleted` / `AlreadyCancelled` / `WrongShard`) is
`#[must_use]`. Late callee replies surface with a cause-specific
rejection reason from a bounded recently-cancelled ring:
`CallReplyRejected { CallerCancelled / CallerTimedOut / OwnerStopped
/ RuntimeStopped }` or the deferred-path equivalent; ring-evicted
fall-through is the generic `NoPendingCall` / `CallerClosed`.

**Resolved (Tina pending-call helper phase):** the bounded
[`PendingCallSet<K, R>`](../tina/src/pending_call_set.rs) helper now
ships in `tina`. Specimens that previously hand-rolled
`Vec<CallHandle<R>>` use it: `specimen_cancellation_chain` keys the
table by worker index, `specimen_pool_cancel_reclaim` keys by waiter
index. Insert returns `Full` / `DuplicateKey` as typed errors —
duplicate-key is rejected even when the prior handle has settled,
because an auto-sweep would create a silent ABA bug if a `Returned`
continuation for the prior call were already queued in the user's
mailbox. Forgetting `remove(&key)` therefore *does* leak slots until
the set is dropped, drained, or `sweep_terminal()`-pruned — that
leak is loud (eventual `Full`); silent ABA would not be. No `Drop`
magic, no background timer; the drain-and-cancel pattern stays in
user code — the helper does not own the workflow. End-to-end fill
-> cancel -> refill and fill -> timeout -> refill proofs in
`tina-runtime/tests/pending_call_set.rs`.

**Still open:** runtime-level `runtime.cancel_isolate(addr)` (third
form — closes every call an isolate owns) is a small wrapper around
`cancel_call` and `PendingCallSet::drain`; will land when a real
service consumer asks for it.

### 9. Drain helper for `PendingReplies` at service stop

**Surfaced by:** `specimen_graceful_pool_shutdown`,
`specimen_graceful_drain_server`.

`PendingReplies::drain()` returns `Vec<(K, DeferredReply<R>)>`,
which the user has to map into `Effect::Batch(reply_to(slot,
value))` calls plus a final `stop()`. The service-stop pattern
is identical at every call site:

```rust
let mut effects: Vec<_> =
    self.pending.drain().into_iter().map(|(_, slot)| reply_to(slot, R::Closed)).collect();
effects.push(stop());
Effect::Batch(effects)
```

The same area also wants a *deadline* — a drain that says "finish
in-flight work, but give up after T". Today that's a hand-rolled
`DrainDeadlineFired` continuation message scheduled via `sleep`
plus a check in the isolate's "is it done" predicate that returns
true on deadline-fired even when `pending > 0`. The
`tina-tokio-bridge::BridgeShutdownReport::drained_within_timeout`
flag is the bridge-side version of the same idea.

**Build:**

- ~~`pending.drain_into_effect(R::Closed) -> Effect<I>` (or
  similarly named) that returns the matching `Effect::Batch` in
  one call, with the trailing `stop()` opt-in via a sibling
  `drain_into_stop_effect(R::Closed)`.~~ Shipped:
  `PendingReplies::drain_replies` / `drain_replies_with` /
  `drain_replies_into_effect` / `drain_replies_into_stop` /
  `drain_replies_with_into_effect` /
  `drain_replies_with_into_stop`, all typed so a
  `PendingReplies<K, R>` only produces `Effect<I>` when
  `I::Reply = R`. `specimen_graceful_pool_shutdown` used
  `pending.drain_replies_into_stop::<Self>(R::Closed)` before
  its 067 migration; it now relies on
  `WorkerPoolMsg::Close(CloseMode::Drain)` for the same
  parked-callers-get-`Closed` outcome. The helper is still
  load-bearing for `PendingReplies`-shaped frontends. The
  deadline half of this finding (DrainGate) folds into finding
  15 (Deadline as first-class context).
- An isolate-state `DrainGate` helper that holds the deadline +
  the pending-count predicate, with an `is_done` /
  `drained_within_timeout` accessor that the handler reuses.

### 11. Multi-stage pipeline ergonomics

**Surfaced by:** `specimen_two_stage_pipeline`.

A 3-stage pipeline reads as 4 enum variants in `PipelineMsg`
(Submit + Parsed + Validated + Executed), each with its own match
arm. The Tokio side reads as `parse(i).await?; validate(p).await?;
execute(v).await?` — three lines. The Tina version is correct and
trace-visible at every stage, but the variant count grows
linearly with stage count.

**Decision:** do not build a pipeline helper yet. The long form is
not merely noise: it names each suspension point and each
per-stage `Full` / `Closed` / `Timeout` edge. A helper that makes
Tina look like fake `async` would be worse for humans and LLMs.

**Revisit only if:** a non-pedagogical pipeline repeats enough
boilerplate that a helper can delete plumbing while keeping every
stage, timeout, and partial-progress fact visible. The raw
match-state-machine form remains semantic truth.

**Update (verified on this audit):** `tina::flow!` (`tina-macros/src/lib.rs`)
now generates exactly this shape — a named continuation enum + dispatcher
per linear step, with no runtime behavior added (each step is still an
ordinary ` .then_with_request` continuation) — and ships in `mini_saas_api`
and `specimen_multi_turn_request_context`. It plausibly satisfies the
revisit condition above, but `specimen_two_stage_pipeline` — the specimen
that surfaced this finding — still hand-writes `PipelineMsg` by hand
(`examples/specimen_two_stage_pipeline/src/tina_impl.rs`). Not closing
until that specimen (or an equivalent) is migrated and proves the fit.

**Closed (2026-07 examples canonicalization pass):**
`specimen_two_stage_pipeline` now declares `PipelineFlow` with
`tina::flow!` for its `Parsed` / `Validated` / `Executed` steps. The
migration proves more than the boilerplate deletion this finding asked
for: threading `req: RequestContext<PipelineReply>` directly through each
step also removed the `qid`-keyed `PendingReplies<u64, PipelineReply>`
table the hand-written version needed purely to correlate a continuation
back to its caller — `flow!`'s req-threading makes that correlation table
unnecessary, not just its dispatch boilerplate. Both existing smoke tests
(`tina_smoke`, `tokio_smoke`) pass unchanged, including the exact
completed/parse-failed/validate-failed counts `assert_report_invariants`
checks. `flow!` is still linear-only by design (see finding 29's negative
result below for the sleep-driven shape it does not cover); an N-stage
fan-out pipeline remains hand-written by design.

### 12. Rust footgun replication: shared receiver in worker pool

**Surfaced by:** `specimen_graceful_pool_shutdown` (Tokio side).

Not a Tina finding per se — but worth recording as the *kind of
footgun* Tina structurally avoids. The Tokio shutdown path needs
both `JoinSet::abort_all` AND `drop(rx_arc)`. Forgetting the
second leaves buffered jobs (and their reply oneshots) alive,
blocking queued callers forever. The test passes under low burst
because all jobs were in flight.

Tina's `pending.drain()` + `Effect::Batch(reply_to)` makes this
class of bug structurally impossible: every captured slot has one
container, and shutdown is one effect away.

This is a positive observation about Tina's model. The build is
documentation, not new product work — call it out in the user
guide's lifecycle chapter as a contrast with the Tokio shape.

### 14. Spawn API surfaces the child's address

**Surfaced by:** `specimen_dynamic_worker_pool`,
`specimen_supervised_worker`.

`spawn(ChildDefinition::new(...))` still returns nothing and stays
the fire-and-forget primitive. Phase 084 adds the explicit observed
form:

```rust
spawn_observed(ChildDefinition::new(worker, cap))
    .then(ParentMsg::ChildStarted)
```

The continuation receives
`Result<ChildRef<ChildMsg, ChildReply>, SpawnObservedError>` as an
ordinary later parent message. The parent can store the typed child
address, send follow-up messages, and treat restart-created
incarnations as new/stale generation truth.

The error half covers spawn construction rejection, for example a zero
mailbox capacity. If delivering the continuation to the parent is itself
rejected because the parent's bounded mailbox is full or closed, the runtime
records the normal send rejection in the trace; there is no hidden queue to
force a second message through the failed path.

Before this, the parent did not learn the child's `Address`. That
meant the parent could not:

- ask the runtime "is this specific child still alive?" via
  `observe_isolate_complete(child_addr)`;
- send the child a follow-up message;
- aggregate "missing partials" as a typed timeout (the parent
  doesn't know which child is missing).

The old supervised-worker workaround had the child send a
`Boot(self_addr)` message back to a shared `Arc<Mutex<...>>` slot.
`specimen_supervised_worker` now uses `spawn_observed` for the
initial address instead.

**Still open:** join/stop child convenience and typed restart
refresh as parent messages. Existing `observe_child_restarted`
carries the new isolate id/generation, but it is a host waiter and
does not yet deliver a typed replacement `ChildRef` to the parent.

A *host-side* alternative —
`runtime.observe_child_started::<M>(parent).wait(timeout)?` —
was considered and rejected for now: the existing
`RuntimeEventKind::Spawned { child_isolate }` event has no
`TypeId` for the child's `Message`, so a typed waiter would
either need a new field on `Spawned` (a runtime-event change)
or a caller-asserted `M` (not honest under the LLM rule). Pick
the typed-event vs. continuation form when the supervisor/spawn
API gets revisited.

### 19. Pool consumer ergonomics — host-side acquire and scenario runner

**Surfaced by:** `specimen_pool_cancel_reclaim` (and to a lesser extent
`specimen_graceful_pool_shutdown`).

The cancel-reclaim specimen is ~245 lines tina vs ~113 lines tokio.
Roughly 115 of the gap is a `Driver` isolate that exists *only*
because `cancel_call(handle)` requires being inside an isolate's
`handle()`. Roughly 34 lines are a host-side
`try_send` + `std::thread::sleep` dance to step the driver through
seven scripted stages. Both costs go away in real services (the
service's own handler is the isolate, and there are no scripted
stages), but they hurt readability of test-shaped specimens.

Two helpers would cut the gap further when a real consumer pulls them
into existence:

- A host-side `runtime.acquire_owned(pool_addr, timeout)` analogous
  to `observe_result`. Lets test code acquire from outside an isolate
  context, eliminating the coordination Driver. Real risk: creates a
  second pool-interaction model from the host side. Defer until a
  real consumer (HTTP keepalive, DB pool) pulls on it.
- A host-side scenario runner —
  `runtime.scenario(addr).send(M).then_wait(D).send(N).run()` — that
  collapses the `try_send` + `sleep` dance. Real risk: becomes fake
  async choreography that hides ordering bugs. Test sugar only;
  defer until a second test specimen wants the same shape.

**Decision:** both are watch-list, not next-up. The
result-flavored `acquire_result_effect` / `release_result_effect`
helpers (shipped with 067) gave the same payoff for the no-loss case
and remain the right place to push first.

**Revisit when:** an HTTP keepalive consumer or a third pool
specimen wants either shape.

### 20. HTTP body-streaming ergonomics — first round shipped

**Surfaced by:** `specimen_http_body_streaming`.

Two ergonomic gaps showed up in the first specimen and got fixed
in this slice:

- **Hand-rolling an `Isolate` just to yield bytes.** A single-route
  streamed response needed two custom isolates: a chunk source
  with `tina::isolate_types!` + `ResponseChunkMsg`/`ResponseChunkReply`
  arms, plus the request handler. Wrapping any
  `Iterator<Item = Vec<u8>>` is now `IterBodySource::new(iter)`;
  no `Isolate` impl. The handler still names the framing
  (`stream_known_length` / `stream_chunked`), so the choice is
  visible without macro magic.
- **Framing was a struct literal, not a typed choice.** Callers
  built `ResponseStream { content_length: ..., source }` with no
  hint that an "unknown length" shape existed. Loud constructors
  (`HttpResponse::stream_known_length` and
  `HttpResponse::stream_chunked`) make the call site name the
  framing; a chunked response is `Transfer-Encoding: chunked` on
  the wire with the connection writing the terminator on `Eof`.
- **Cancel signal from connection back to source — closed.** Verified
  on this audit: `ResponseChunkMsg::Cancel` ships
  (`tina-http/src/streaming.rs`), the connection sends it on abandon
  (`tina-http/src/connection.rs`, `tina-http/src/http2/server.rs`,
  `tina-http/src/http2/client.rs`), and `cancel_response_source`
  (`tina-http/src/scope.rs`) is the scoped host-side helper.

What still needs work but is deferred:

- **Live metrics ticks.** `BodyMetrics::snapshot()` is callable
  from any thread at any time (the counter is `Arc`-backed), but
  there is no built-in periodic emit. A `runtime.metrics_tick(D)`-
  shaped helper or a generic capacity-tick channel belongs in the
  observability slice, not here.
- **Chunked decoding on the HTTP/1 client.** Server can emit
  chunked; the client still rejects real chunked bodies. Verified on
  this audit: `tina-http/src/parse.rs`'s response parser still treats
  `transfer_encoding_chunked` as `content_length = 0` (a body-forbidden
  shape), it does not decode a chunked body. Symmetric support is a
  separate slice with its own decoder + tests.

**Status:** shipped (server-side chunked emit, `IterBodySource`,
loud-API constructors, `body_io_error_count` proves mid-stream
client close, cancel signal to source).

### 28. Service-level scope registry mirroring `register_with_capacity`

**Surfaced by:** `system_api_gateway_limits`.

`SharedCapacityScope` is shard-local. A service today builds one with
`SharedCapacityScope::new("gateway.in_flight", "weight", 4)` and clones
the handle into every isolate that needs to admit. That works, but a
service builder may want `register_scope("name", unit, max)` next to
`register_with_capacity` so the discovery report and the lifecycle are
owned by the runtime, not by user code.

**Build:** a runtime-side `SharedScopeRegistry` keyed by name with
register/get/snapshot. Reuse the existing `CapacitySummary` shape so
the runtime can produce one merged discovery line per shard.

### 29. Effect chaining over multiple runtime calls inside one logical request

**CLOSED (2026-07-09, PR #276):** `flow!` gained `-> raw T` steps that carry a
non-call continuation (e.g. a `sleep().then()` timer wake-up yielding `Result`);
`system_soak_http_db` is migrated onto it. Kept here (not yet moved to
FINDINGS_HISTORY) per the ledger's stable-number convention.

**Surfaced by:** `system_soak_http_db`.

A request rail that admits HTTP, sleeps, releases, admits DB, sleeps,
replies needs two custom message variants (`HttpReleased`, `DbReleased`)
so the post-sleep state mutation can land in `handle`. The pattern is
the same across systems: "after this timer wakes, do the next stage".
Today every system rebuilds the variants by hand.

**Build:** an effect combinator like `sleep(d).then_in_isolate(|this:
&mut Self| this.start_db(...))` that wires the message envelope and
the post-wake state mutation in one place.

**Update (verified on this audit):** `tina::flow!` (`tina-macros/src/lib.rs`)
now generates a continuation enum + dispatcher for a named linear step
sequence and ships in `mini_saas_api` (`tina_impl/controller.rs`) and
`specimen_multi_turn_request_context`. It looks like the answer to this
finding's shape, but `system_soak_http_db` — the specimen that actually
surfaced this pain — still hand-rolls `HttpReleased` / `DbReleased`
(`examples/systems/system_soak_http_db/src/lib.rs`). Not closing until a
migrated `system_soak_http_db` (or an equivalent multi-hop-with-timers
case) proves `flow!` covers this exact shape.

**Checked (2026-07 examples canonicalization pass): `flow!` does not cover
this shape, and forcing it would be dishonest.** Every `flow!` step is
generated as `(RequestContext<Reply>, ..captures.., CallOutcome<T>)` —
the outcome slot is hard-coded to `tina_runtime::CallOutcome<T>`, the type
an isolate-to-isolate `call(...)` returns. `system_soak_http_db`'s
`HttpReleased` / `DbReleased` continuations are not isolate calls; they are
runtime-owned `sleep(d).then_with_request(req, ...)` wake-ups, which yield
`Result<(), CallError>` — a different, narrower outcome type with no `Full`
/ `Closed`/`Rejected` variants a real dependency call would have. The two
shapes are not interchangeable: writing a `flow!` step for a sleep wake-up
would need a hand-written `Result<(), CallError> -> CallOutcome<()>` shim at
every step, which reintroduces the exact boilerplate `flow!` exists to
remove. Replacing the sleeps with fake isolate calls to make the macro fit
was considered and rejected — it would invent architecture (two dummy
worker isolates) that does not exist in the real "admit, wait, release"
shape this specimen demonstrates, just to satisfy a macro's type signature.
`system_soak_http_db` is left on its hand-rolled `SoakMsg::{HttpReleased,
DbReleased}` form. **Revised build:** `flow!` (or a sibling macro) would
need a second outcome shape — a timer-wake step whose outcome slot is
`Result<(), CallError>` instead of `CallOutcome<T>` — before this finding
can close through the macro path. No such macro exists today.

### 30. DST adapter for `SharedCapacityScope` / `BoundedEventSink`

**Surfaced by:** `system_api_gateway_limits`, `system_soak_http_db`,
phase 107 findings.

The new observability primitives live outside the `RuntimeEvent`
trace, so DST replay does not currently carry their facts forward.
Existing trace-based pressure (`PressureSummary::from_events`) still
works; the new shared-scope full counts and event-sink drops do not.
The `ServicePressureReport` shape already encodes
`Unavailable { reason }` so a sim that does not have these primitives
yet stays honest.

**Build:** a small adapter in `tina-sim` that snapshots scope/sink
counters into the trace at well-defined points (admit, release,
drop, push, drain) so a replay can reconstruct `assert_no_full`
semantics. Or expose the snapshots as `LiveReplayFact` entries so
they ride alongside the existing fact stream.

### 32. AWS bridge surface duplication across services

**Surfaced by:** adding DynamoDB / SNS / Secrets Manager workers to
`tina-aws-bridge`.

Each AWS service worker repeats the same scaffolding: `OwnedRuntime`
wrapper, `*MetricsInner` struct with the same eight counters plus
service-specific ones, `note_admit_kind` / `note_terminal_kind` /
`in_flight_kinds`, `*Closer::close_and_drain` polling loop, and the
admit/poll/timeout state machine. Five services share roughly 80% of
their lifecycle code. The phase plan explicitly forbade a shared bridge
base crate to keep the per-service stories independent, so all five live
side-by-side with copy-pasted plumbing.

**Build:** when the bridge surface stops growing in shape, factor out
the common state machine into an internal `bridge_core` module within
`tina-aws-bridge` (still not a separate crate). The factoring needs to
preserve each service's per-error tally semantics — counters like
`DynamoMetrics::conditional_check_failed` or
`SecretsMetrics::decryption_failed` are service-specific. A trait that
the per-service module implements (validate, run_request, classify_sdk
error, tally_terminal) is probably the right shape.

Reference: the canonical bridge shape now lives in
[`docs/tina-user-guide/30-bridge-author-kit.md`](../docs/tina-user-guide/30-bridge-author-kit.md).
Any internal AWS refactor must keep those eight steps user-visible —
no hidden queues, no hidden classifier collapse, no late-result
silent rollup.

### 35. Local I/O / codec / IPC rails feel low-level next to the file loops (Phase 117)

**Surfaced by:** `specimen_local_io_codec_ipc` (file-ingest, file-copy,
admin-socket, framed-keyspace, live-unix), plus the live Unix echo and
`unix_simulation` tests.

What felt good:

- The `.then(Msg::Variant)` continuation model reads top-to-bottom as a
  pure state machine; every resume point is a named enum variant. The
  whole admin server fits in your head.
- The simulator carries the IPC story end-to-end: the admin and keyspace
  protocols were written and run deterministically with no real socket,
  then the *same* `unix_*` calls run live. That live/sim parity is the
  payoff.
- Typed outcomes (`Ok(bytes)` / empty-is-EOF / `Err(CallError)`) are
  uniform across TCP/Unix/file rails — one `CallError`, no per-rail
  relearning.
- The codec *decode* side is the right shape: `feed(bytes)` then loop
  `next_frame()`, with `FrameDecision::{Full, Malformed}` forcing the bad
  cases. `FileReadChunks`' `Eof` vs `CapReached` report is honest and
  genuinely useful.

What felt rough — each is a `Build`:

- **Unix loop helpers were missing.** Closed by `UnixWriteAll` and
  `UnixReadToEof`, mirroring the TCP helpers and surfacing `Ok(0)` stuck writes
  as `CallError::Io` instead of hot-spinning.

- **The codec owns decode but not encode+write.** `LineFramer` /
  `LengthDelimitedFramer` parse beautifully, but to *send* a framed reply
  the specimens manually `encode_into` and then drive the write loop.
  There is no framed *writer* that turns frames into the write state
  machine, so the codec is half a round-trip. **Build:** a framed-writer
  companion (e.g. a `LengthDelimitedWriter` / line writer that produces
  the bounded write effect), so encode+write reads like decode.

- **`FileCopyBounded`'s two-method API was clunky.** Closed by the unified
  `next_effect(...)` / `advance(FileCopyProgress, ...)` path. The old
  `next_leg` / `record_*` gears remain when a caller wants the mechanism.

- **No blessed way to observe an isolate mid-run.** Every IPC specimen
  smuggles results out through `Arc<Mutex<Vec<...>>>` because the isolate
  owns its state and `stop_with` + `observe_result` only cover the
  *final* value, not the running observations a protocol test wants. This
  reintroduces the retired Round-1 `Arc<Mutex<...>>` side-channel pattern
  and produced a real bug: `Arc::try_unwrap` returned `Default` while the
  isolate still held a reference, making a test pass on empty data. This
  echoes active finding 6 (bless an observation handle). **Build:** a
  sim/runtime helper for "observe an isolate's accumulated facts" that is
  not a shared-mutable side channel — or document the trace-projection
  path as the sanctioned alternative for protocol assertions.

## Closed

Findings shipped by recent phases. Numbers are kept stable so
existing README references stay valid.

### 2026-07-12 Worker-pool caller authority canonicalization

`specimen_worker_pool` no longer invents a `qid` or parks each caller in a
`PendingReplies` sidecar for a workflow with exactly one child call per
request. The frontend now uses
`RequestCall::defer(call_request(...)).reply_service_event(...)`, which moves
the typed `RequestContext` directly into the worker completion event. This
deletes the synthetic pending-full and duplicate-key outcomes while preserving
distinct worker `Full`, `Closed`, `Timeout`, and `Rejected` outcomes. The live
host also preserves the timer continuation's typed `CallError` instead of
collapsing every timer failure into one flag, and uses `LocalSystem` for
startup, registration, result observation, typed ingress, and terminal
shutdown. No framework prerequisite was needed.

### 2026-07-12 Bounded batcher synthetic reply correlation

`specimen_bounded_batcher` now uses `SharedWork<u64, BatcherReply>` keyed by
the honest batch generation. `SharedWork::wait` owns each `RequestCall`, and
`reply_all_clone` settles every live waiter for a flushed generation. This
removes the monotonic `qid`, `PendingReplies<qid, _>`, and parallel `qids`
vector without replacing them with example-local glue. The item vector is the
only batch payload state; caller timeout reclaims reply capacity without
silently retracting accepted work.

The specimen also moved from a driver isolate over raw `ThreadedRuntime` to
typed host requests through fallibly built `LocalSystem`, exhaustive
`CallOutcome` and outer host-control accounting, and bounded terminal-report shutdown. Direct tests
cover size/timer flushes, global `Full`, caller-gone reclamation and refill,
post-`Full` refill, stale success/error invalidation, typed timer-failure settlement and refill, exact
capacity counters, and clean shutdown. This also closes the old timer-error
`noop` path that abandoned accepted callers until their individual deadlines.

**Framework result:** the current `SharedWork` FIFO/all-waiter API is
sufficient. No new batch-specific abstraction or example-local adapter is
needed.

### 13. Tina-owned database client (`tina-sqlx-bridge`) — closed

**Surfaced by:** `specimen_sqlite_counter`.

There was no native or bridged path for "Tina service talks to a
database." The honest first-form shape used in the specimen was one
isolate that owns a `rusqlite::Connection` and runs each query inline
in `handle`, which blocks the shard thread for the query's duration —
fine for SQLite, dishonest for a remote DB with millisecond latency.

**Closed. Verified on this audit:** both requested shapes ship as full
crates. `tina-sqlx-bridge` (`tina-sqlx-bridge/src/{lib,worker,helpers,
metrics,types}.rs`) covers the async/remote-DB path with a
Tokio-owned worker, bounded ingress, and a `PgMetricsHandle`.
`tina-sqlite-bridge` (`tina-sqlite-bridge/src/{lib,worker,helpers,
metrics,types,budget}.rs`) covers the sync path with `SqliteError::*`
variants and `SqliteMetricsHandle`. `specimen_postgres_counter` and
`specimen_sqlite_counter` use them directly.

### 15. Deadline as first-class context — closed

**Surfaced by:** `specimen_backpressure_chain`.

A multi-hop chain had to thread a deadline (or remaining-budget
duration) through every call by hand, with the outer hop's call
timeout kept slightly longer than the inner's so slack didn't
accumulate silently.

**Closed. Verified on this audit:** [`Deadline`](../tina/src/context.rs)
ships with the explicit-`now` constructor `Deadline::from_instant(now,
after)` plus `Context::now()` / `Context::deadline_after(after)` as the
runtime/sim-aware sugar (`tina/src/context.rs`). The runtime stamps
`Context::now()` from its monotonic `Clock` before each handler turn;
the simulator stamps it from a stable virtual-clock anchor. There is no
`Deadline::after(Duration)` shortcut, since it would call
`Instant::now()` internally and silently break DST/replay.

`Deadline` is a budget value: it does not retry, extend, or cancel
work. `remaining(now)` returns `Option<Duration>`, `remaining_or_zero
(now)` returns the duration for use as a call timeout. Proved live in
`tina-runtime/tests/deadline.rs` and deterministically in
`tina-sim/tests/deadline.rs`. `specimen_backpressure_chain` propagates
a `Deadline` through A -> B so each hop sees the remaining budget
against its own `now`.

### 16. Multi-worker TLS lane (or split accept/stream lanes) — closed

**Surfaced by:** `specimen_native_https`, `tina-http/tests/client_tls_smoke.rs`.

**Closed.** TLS no longer runs on worker threads at all. The runtime owns a
rustls connection (sans-I/O) per `TlsStreamId` and drives the
handshake/read/write/close state machine on the shard thread as Betelgeuse
harvests TCP completions — TLS is a layer over the runtime's own TCP rail, not a
second socket stack. The single TLS worker that head-of-line-blocked accepts and
deadlocked a same-runtime client+server is gone. `local_system_tls_quiet_stream_does_not_block_second_connection`
still pins the quiet-stream story, and `local_system_tls_client_and_server_share_one_runtime`
runs a Tina TLS client and server on one shard in one runtime — the exact case
this finding called impossible. The substrate guard
(`tina-runtime/tests/tls_substrate_guard.rs`) pins the absence of any
`tina-tls-*` worker thread or private socket stack.

**Still true:** `tls_lane_capacity` is a hard cap — now the shard-total count of
in-flight TLS ops, not magic unbounded concurrency. Handshake asymmetric crypto
runs on the shard thread, an accepted tradeoff: visible and boundable by accept
rate rather than hidden on a serial worker that deadlocks.

**Verified on this audit:** `TlsStreamId` and the driver/call TLS state
machine live in `tina-runtime/src/driver/tls.rs` and
`tina-runtime/src/call/tls.rs`; `tina-runtime/tests/tls_substrate_guard.rs`
exists and is exactly the guard test named above.

### 17. Private Unix-domain socket worker thread — closed

**Note:** this shares finding number 17 with "Host-thread `call_blocking`"
below — a pre-existing duplicate in the ledger's numbering, not introduced
by this pass. Flagged for a human to renumber; left as-is here since
inventing a new number was out of scope for this audit.

**Surfaced by:** `specimen_local_io_codec_ipc` (`live_unix_smoke`, `admin_socket`),
`tina-runtime/tests/local_system.rs` (`unix_live_echo`).

**Closed.** Unix-domain sockets no longer run on a private worker thread over
`std::os::unix::net`. The runtime drives bind/accept/connect/read/write/close on
the shard thread as completions on the same per-shard Betelgeuse loop TCP and TLS
already ride — Unix sockets are sockets, so they follow the same substrate rule.
The narrow Unix addressing the substrate lacked (`bind_unix` / `connect_unix` and
the socket-file unlink lifecycle) was added to vendored Betelgeuse rather than
left in a hidden worker. The lane keeps TCP's discipline: one accept/read/write
lane each, `ResourceBusy` on duplicates, close-wins cancellation, tombstoned
shutdown. The capability report now classifies it completion-backed, and the
rail-inventory guard (`scripts/rail_inventory_guard.sh`) fails the build if a
worker thread or blocking std socket reappears in a runtime rail off-inventory.

**Still true:** DNS (platform resolver) and process spawn/wait stay bounded
blocking lanes on purpose — they are OS lifecycle / library calls with no
portable completion opcode, and the capability report carries their written
reason. A narrow rename/remove/readdir/metadata storage fallback is the only
remaining off-shard storage worker.

**Verified on this audit:** `scripts/rail_inventory_guard.sh` exists and
greps `tina-runtime/src/driver` for `thread::spawn` / `os::unix::net` /
blocking `std::fs` calls against a written inventory
(`.intent/runtime-rail-inventory.txt`); the only live
`std::os::unix::net` hit left in `tina-runtime/src/driver` is the
documented process-spawn exception in `driver/process.rs`.
`UnixWriteAll` / `UnixReadToEof` ship in `tina-runtime/src/unix_loops.rs`.

### 21. Per-bucket FIFO wait list next to a global `PendingReplies` — closed

`tina_runtime::SharedWork<K, R>` is now the user-facing copy path:
"many callers wait for one result", one global cap, optional per-key
cap (`with_key_limit`), FIFO per key, ticketed `reply_one`, and
`reply_all_clone` / `reply_all_with` / `close_all_clone` /
`close_all_with` / `drain_all_with` for multi-waiter replies. Stale
tickets are rejected; tickets are move-only with crate-private fields.
`request_effect_after_shared_wait(&ticket, effect)` is the only path
that produces a `RequestEffect` after admission.

`SharedWork` is a thin wrapper over `WaitList`; the lower-level
`WaitList` name remains public for call sites that read better under
the mechanism name. `system_cache_with_fill` and the
`ergonomics_playground` single-flight cache probe both copy from
`SharedWork` now.
*(Update: `WaitList` has since been made private; `SharedWork` is the
only public name.)*

**Verified on this audit:** `SharedWork<K, R>` is defined in
`tina-runtime/src/shared_work.rs`; no remaining ask from the
historical finding below is open.

*(Historical finding kept below for context.)*

### 21-historical. Per-bucket FIFO wait list next to a global `PendingReplies`

**Surfaced by:** `system_cache_with_fill`, `system_lock_manager`.

Both specimens want "one bounded global pending box, plus a FIFO
wait list per cache key / lock key, plus a hand-off loop that skips
slots whose caller went away." Each writes the same shape by hand:

- `pending: PendingReplies<u64, Reply>` keyed by a monotonic waiter id;
- per-bucket `VecDeque<u64>` of waiter ids inside the bucket's state;
- on hand-off / fill-done, pop a waiter id from the queue, `take` from
  pending, and if the slot is gone (caller cancelled / timed out) loop
  to the next id.

The cap accounting splits awkwardly: the global cap lives on
`PendingReplies`; the per-bucket cap lives in handler code; the
"skip reclaimed" loop is repeated.

**Build:** a small `WaitList<K, R>` (or `KeyedPendingReplies<K, R>`)
helper that owns both caps, takes the inbound `CallContext`, and
exposes a single `pop_next(&K) -> Option<DeferredReply<R>>` that walks
past reclaimed slots. Must keep typed admission errors
(`Full` / `BucketFull`) so callers can reply `Busy` distinctly.
Revisit only after a third specimen needs the same shape so the helper
shape is informed by three call sites, not two.

### 22. Internal-event variants need a `handle_call` rejection arm — closed

**Surfaced by:** `system_cache_with_fill`, `system_lock_manager`.

Specimens that mix caller-authority messages (`Acquire`, `Get`) with
runtime-owned continuations (`LeaseExpired`, `FillDone`) used to write
a `handle_call` arm whose only job was
`call.reject(CallRejectedReason::UnsupportedMessage)` for every
internal variant, repeated per isolate.

**Closed. Verified on this audit:** the `#[tina_runtime::isolate(event =
Event, request = Request, reply = Reply)]` split-service form
(`tina-macros/src/lib.rs`, `build_isolate`) generates `ServiceMessage
<Event, Request>` (`tina/src/address.rs`) and auto-generates the
rejection arm on both sides — an `Event` delivered to the generated
`handle_call` is rejected with `UnsupportedMessage` and a `Request`
delivered to `handle` is rejected the same way, with no user-written
match arm. Compile-fail fixtures pin the type-level half
(`tina-runtime/tests/safety_rails_compile_fail/split_event_on_request_lane.rs`);
the live test `split_service_routes_events_and_requests_on_separate_capabilities`
in `tina-runtime/tests/safety_rails.rs` passes (verified: `cargo test -p
tina-runtime --test safety_rails`, 10/10 ok). `system_cache_with_fill`
and `system_lock_manager` — the two specimens that surfaced this
finding — both use the split form today with no hand-written rejection
arm left in either file.

### 23. Mailbox-first service ergonomics — Phase 101 shipped — closed

**Note:** this shares finding number 23 with "Host-side `call_blocking`"
below — a pre-existing duplicate in the ledger's numbering, not
introduced by this pass. Flagged for a human to renumber.

**Surfaced by:** `system_metrics_shipper`, `system_bounded_object_lane`,
the recurring-tick / single-flight / drain / Full-handling repetition
across system specimens.

Shipped helpers:

- `tina::time::RecurringTick` — fixed-period service ticks with
  `Skip` / `Bounded(n)` / `Delay` catch-up policies; explicit
  `RecurringTickToken` for stale-tick detection. `system_metrics_shipper`
  now uses it for time-window flushes.
- `tina_runtime::LocalPermitGate` — fixed-capacity, move-only `Permit`,
  explicit release/retire; reports
  capacity/current/full_count/high_water/retired_count/completed_count/
  invalid_release_count. `system_bounded_object_lane` and the metrics
  shipper's single-flight flush slot both run on it.
- `tina_runtime::DrainState` — small admit/complete/cancel/drop
  counter state plus `begin/finish/can_stop`. Late completions counted
  separately. Resource close still belongs to the service.
- `runtime.register_with_capacity_and_bootstrap[_on]` — prefills the
  mailbox with the bootstrap message before inserting the isolate entry.
  No cleanup-after-registration path; typed `RegisterBootstrapError` on
  prefill refusal. Available on `Runtime`, `ThreadedRuntime`,
  `MultiShardRuntime`, `ThreadedMultiShardRuntime`.
- `tina_runtime::FullHandling` — decision-only state for the
  "on Full, shed or retry-with-backoff" shape; the service still
  schedules the visible Tina sleep.

Out of scope here: lifecycle `on_start` callbacks (not shipped,
register-and-bootstrap covers the common footgun without breaking
mailbox truth), broad retry frameworks (FullHandling is the only one).

**Verified on this audit:** `RecurringTick` in `tina/src/time.rs`;
`LocalPermitGate` in `tina-runtime/src/local_permit.rs`; `DrainState`
in `tina-runtime/src/drain_state.rs`; `FullHandling` in
`tina-runtime/src/full_handling.rs`. `system_bounded_object_lane` and
`system_metrics_shipper` both import and use `LocalPermitGate` /
`DrainState` directly. `register_with_capacity_and_bootstrap` exists in
`tina-runtime/src/{registration,threaded,multi_shard,
threaded_multi_shard}.rs`, though see finding 24 below for the caveat
that neither surfacing example has migrated onto it yet.

### 23. Host-side `call_blocking` on `ThreadedMultiShardRuntime` *(closed by phase 102)*

`ThreadedMultiShardRuntime::call_blocking(addr, msg, timeout)` now
ships and routes by `addr.shard()` — same convention as `try_send` and
`observe_result`. Bounded admission: a full worker command queue
surfaces as `ThreadedRuntimeError::CommandFull` instead of a host
hang. Single-shard `ThreadedRuntime::call_blocking` got the same
bounded-admission treatment. `system_session_auth` was migrated to
real multi-shard placement (one bucket isolate per shard, host routes
by `ShardPlacement`); the in-isolate fallback note is gone.

No `call_blocking_on(shard, addr, ...)` ships — passing the shard
twice is a place to introduce a mismatch bug. A future host-to-shard
variant only earns its place when a real caller needs "call as if
from shard A into target shard B" and has a remote-path proof.

**Verified on this audit:** `call_blocking` exists on both
`tina-runtime/src/threaded.rs` (single-shard) and
`tina-runtime/src/threaded_multi_shard.rs` (multi-shard), each routed
by `addr.shard()`; no `call_blocking_on` exists anywhere in
`tina-runtime/src`, matching the "no host-to-shard variant" claim.

The preferred `LocalSystem` and `LocalMultiShardSystem` facades now forward
only the two host-call shapes justified by their public registrations:
`call_blocking` for `register_root[_on]` and `call_blocking_request` for
request/split service handles. The multi-shard facade keeps address-owned
routing and the same unknown-shard panic convention. Separate host-wait
budgeting remains a lower-level threaded-runtime control rather than widening
the app facade.

### 24. Register-and-bootstrap helper for start-up effects — closed

**Surfaced by:** `system_job_queue`, `system_session_auth`.

Both specimens have a startup effect (job_queue spawns N worker children;
session_auth schedules the first sweep timer). The ceremony this finding
complained about: define a public `Msg::Bootstrap` variant, handle it in
`handle`, and after `register_with_capacity` remember a separate
`try_send(addr, Msg::Bootstrap)` — forgettable, and the failure mode is
silent.

**Closed at the library level. Verified on this audit:**
`register_with_capacity_and_bootstrap` (and `_on` / `_using` siblings)
ship in `tina-runtime/src/registration.rs`,
`tina-runtime/src/threaded.rs`, `tina-runtime/src/multi_shard.rs`, and
`tina-runtime/src/threaded_multi_shard.rs`, prefilling the mailbox with
the bootstrap message before the address is returned, with a typed
`RegisterBootstrapError` on prefill refusal (`tina-runtime/src/errors.rs`).

**Closed at the example level (2026-07 examples canonicalization pass):**
both surfacing specimens now use the bootstrap-prefill form.
`system_job_queue` registers its `Queue` isolate with
`register_with_capacity_and_bootstrap::<Queue, WorkerMsg>(Queue::new(...),
mailbox, QueueMsg::Bootstrap)` — this also let the `Queue::self_addr` field
(dead code; it fed the old `register_with_capacity_using` closure and was
never read) drop out entirely. `system_session_auth` registers each
per-shard `SessionBucket` with
`register_with_capacity_and_bootstrap_on::<SessionBucket,
Infallible>(shard_id, ..., SessionAuthMsg::Bootstrap)`. Both crates'
existing smoke tests pass unchanged (`system_job_queue`: 4/4;
`system_session_auth`: 1/1), proving the prefill-then-register ordering
does not change observable behavior.

### 25. Request/reply variants in `handle` compile but reject at runtime — closed

**Surfaced by:** `system_cache_with_fill`, `system_job_queue` (Worker
isolate).

A request/reply isolate used to route incoming messages through
`handle` for fire-and-forget variants and `handle_call` for
caller-authority variants, with both handlers sharing the same
`Message` type — a variant belonged on one side by convention only,
and putting it on the wrong side compiled cleanly but rejected at
runtime.

**Closed. Verified on this audit:** the isolate macro accepts
`event = Event, request = Request` in place of `message = Message`
(`tina-macros/src/lib.rs`, keys parsed at line ~93-94, `split_service`
branch in `build_isolate`) and expands to `ServiceMessage<Event,
Request>` (`tina/src/address.rs`). This makes the split
unrepresentable at the type level exactly as the finding's `Build`
section asked: an `Event` can never reach the generated `handle_call`
match, a `Request` can never reach `handle`, and the compile-fail
fixture `split_event_on_request_lane.rs` pins a real `E0308`
diagnostic (`expected Request, found Event`) at the call site, not a
runtime rejection. Live coverage: `cargo test -p tina-runtime --test
safety_rails` passes 10/10, including
`split_service_routes_events_and_requests_on_separate_capabilities`.
`system_cache_with_fill` and `system_lock_manager` both use the split
form today. **Migrated (2026-07 examples canonicalization pass):**
`system_job_queue`'s `Worker` isolate
(`examples/systems/system_job_queue/src/lib.rs`) now uses `event =
WorkerEvent, request = WorkerRequest, reply = WorkerReply` — `WorkerEvent`
carries `Cancel`/`Wake` (fire-and-forget), `WorkerRequest` carries the one
caller-authority `Process` message — with no hand-written rejection arm on
either side. The queue-side call sites wrap messages explicitly
(`tina::ServiceMessage::Request(WorkerRequest::Process { .. })` for
`call_cancelable`, `tina::ServiceMessage::Event(WorkerEvent::Cancel(id))`
for the opportunistic wake send), since `send`/`call_cancelable` take a
plain `Address<M, R>` and the split form's `M` is
`ServiceMessage<Event, Request>` — there is no split-service-typed
`call_cancelable` helper today, only `send_event` for the send side. All
4 existing smoke tests still pass unchanged.

### 27. Lease handoff into a `PendingReplies` slot — Phase 110 shipped — closed

`tina_runtime::GuardedPendingReplies<K, R, G>` pairs the parked caller
with one RAII `G` guard, drops it exactly once on reply / drain /
caller-gone sweep, and returns it back to the caller on failed
admission. `system_api_gateway_limits` now parks a
`SharedCapacityReservation` directly in the slot, so there is no
sidecar charge table.

**Verified on this audit:** `GuardedPendingReplies` is defined in
`tina-runtime/src/guarded_pending.rs`;
`examples/systems/system_api_gateway_limits/src/lib.rs` declares
`pending: GuardedPendingReplies<u64, GatewayReply,
SharedCapacityReservation>` directly — no sidecar lease map.

*(Historical finding kept below for context.)*

### 27-historical. Lease handoff into a `PendingReplies` slot

**Surfaced by:** `system_api_gateway_limits`, `system_soak_http_db`.

A request that admits against a `SharedCapacityScope` and then parks
its reply in `PendingReplies` has to carry the `SharedLease` in a
sidecar `HashMap<qid, SharedLease>` so the lease outlives the
post-sleep handler. Both new specimens do this manually. The mapping
between "this qid" and "this lease" is invariant under the slot
lifecycle and would compose cleanly into the slot itself.

**Build:** a slot variant — `PendingReplies::try_insert_with_lease(qid,
slot, lease)` — or a generic `SharedLease`-carrying wrapper that
`reply_to` consumes. Either form removes the parallel map.

### 31. `SleepReply` leaks into user-defined message variants — Phase 110 shipped — closed

`tina_runtime::sleep(d).then_event(move || Msg::Wake { id })` is the
sleep-only sugar: the user enum has no `SleepReply` field, and the
helper does not exist on non-timer `TypedCall<()>` so file/process/TCP
close errors stay visible. The phase still ships `sleep_then(d, m)` and
`sleep(d).then(...)` for the cases that *do* want the timer reply.

**Verified on this audit:** `then_event` is defined in
`tina-runtime/src/call/time.rs`.

*(Historical finding kept below for context.)*

### 31-historical. `SleepReply` leaks into user-defined message variants

**Surfaced by:** `system_api_gateway_limits`, `system_soak_http_db`.

Every variant a specimen builds for a post-sleep wake-up carries
`result: SleepReply` even when the handler never inspects it. The
gateway's `HoldDone { qid, route, result: SleepReply }` and the
soak's `HttpReleased { qid, ..., result: SleepReply }` are both
shaped this way. The field is dead weight in the user's message
enum, but the `sleep(d).then(move |r| Msg { result: r, ... })`
signature requires it.

**Build:** either (a) accept `then(move |_| Msg { ... })` without
the placeholder field as the blessed shape and add a `then_no_result`
variant, or (b) drop the `Result` from `SleepReply` for the
infallible-sleep case so the carrying variant is a unit. The wider
form is right for cancellation-aware sleeps; for the typical "wake
me up later, I don't care if you were nudged" the unit form would
keep the user's enum clean.

### 33. Bridge classifier vocabulary lives in `tina-aws-bridge` — shipped — closed

`tina_runtime::bridge::BridgeOutcomeClass` (with
`BridgeRetryable` / `BridgeUnavailable` / `BridgeFatal`) is the shared
shape every bridge classifier projects onto. Each per-bridge
classifier (reqwest, AWS workers) is still free to expose richer
per-bridge reasons, but the shared `bridge_class()` projection makes
mixed-bridge classification a typed fold instead of caller-private
re-mapping. The bridge-author copy path in
[`docs/tina-user-guide/30-bridge-author-kit.md`](../docs/tina-user-guide/30-bridge-author-kit.md)
step 7 names this contract.

**Verified on this audit:** `BridgeOutcomeClass` is defined in
`tina-runtime/src/bridge.rs`; `bridge_class()` projections exist in
`tina-aws-bridge/src/classifier.rs` and `tina-reqwest-bridge`.

*(Historical finding kept below for context.)*

### 33-historical. Bridge classifier vocabulary lives in `tina-aws-bridge`

**Surfaced by:** `system_webhook_relay`, classifier extension traits.

`BridgeOutcomeClass` / `TransientReason` / `FatalReason` were useful
outside AWS too — the reqwest bridge already has `ReqwestOutcomeClass`
with its own per-bridge vocabulary. A relay or retry-driver needs
*both* shapes to classify mixed outcomes (one outbound HTTP, one SQS)
the same way, so callers re-classify into a private enum.

**Build:** decide whether the bridge classifier should be in
`tina-runtime` (shared by all bridges) or whether each bridge keeps
its own private vocabulary and callers map at the boundary. The plan
forbids a shared bridge crate, but the *classifier vocabulary* is
plain data and could live alongside `CallOutcome` without coupling
the bridges themselves.

### 36. Whole-service copied path — Phase 120 shipped

**Surfaced by:** `mini_saas_api`, `system_metrics_shipper`,
`system_job_queue`, `system_realtime_rooms`, and the post-Wave-A system
specimens.

Closed by `system_copied_service_path`,
`system_copied_service_path_companion`, and
`system_copied_service_path_smoke`. A reader can now copy one ordinary service
skeleton and see the normal Tina path for request entry, bounded replies,
session app-control messages, service limits, reports, owner-stop shutdown,
live capture/replay/shrink workflow, fairness/load assertions, and join/select
helpers.

The important product choice: the copied path did not hide request/reply
authority in callbacks and did not build a fake async/select framework.
`CallJoinSet` and `CallSelectSet` keep named branch identity, bounded
pending/results, explicit loser cancellation, partial reports, and late-reply
truth visible. The companion proof and smoke-copy crate exist so a cheap model
or tired human can tell whether the path is actually copyable.

**Correction (external review, P0):** the claim above did not hold. The
skeleton built `CopiedServiceReport` from constants — no isolate, runtime,
listener, or shutdown ever ran — and its own smoke test failed
(`assert_no_leaked_capacity_at_shutdown` panicked with `leak=unchecked`
because the run never supplied a real leak check). `system_copied_service_path`
is rebuilt around one real `#[tina_runtime::isolate]` on a real
`ThreadedRuntime`: bounded admission via `SharedCapacityScope`, a durable-state
ledger step, real concurrent callers through `tina_proof_harness::load`, and a
leak check that reads the scope's real post-shutdown state. Skipping the
release (`Gateway::hold_done`'s `drop(lease)`) now makes the smoke test fail
for a real reason. `system_copied_service_path_companion` and
`system_copied_service_path_smoke` were deleted — they only re-verified the
same fake fields (`session_control`, `replay_roles`, `join`/`select`
capacities) and added no coverage beyond the rebuilt crate's own smoke test.
Systems examples are now gated in CI (`.github/workflows/`) and in
`Makefile`'s example-verification target, so this class of bug fails a PR
instead of shipping silently.

The config/budget half of the copied path is now closed too. Services used to
scatter caps through handlers and `register_*` literals, so a reader could not
see all knobs before the service ran. `tina_runtime::budget::ServiceBudgetManifest`
makes boundedness copyable: one object declares every cap with kind/unit/replay
impact, validates before startup with typed errors, builds rows from existing
configs through adapters, joins configured caps with observed pressure, and
exports the replay-affecting caps a saved DST case depends on. `mini_saas_api`
declares all its caps in one `src/budget.rs` manifest and reads them back from
there; `tests/budget.rs` proves the documented caps are exactly the manifest
rows and that every live surface has a row. Still deliberately manual: time
deadlines and retry-budget *durations* (the unit vocabulary is count and weight,
not time) and per-isolate mailbox depth the runtime does not sample.

### 36. `RequestCall` has no `now()`, blocking split-service migration for time-reading request handlers — closed

**Note:** shares finding number 36 with "Whole-service copied path" above —
a pre-existing duplicate in the ledger's numbering, not a re-use by this
entry.

**Closed.** `RequestCall::now()` (`tina/src/context.rs`) now delegates to
the inner `CallContext::now()`, borrow-only (`&self`), so a handler can
read the clock for deadline math ahead of `.defer(...)` without losing
caller authority. `specimen_backpressure_chain`'s `ServiceB` and `ServiceA`
are now on the split `#[tina_runtime::isolate(event = .., request = ..,
reply = ..)]` form (`examples/specimen_backpressure_chain/src/tina_impl.rs`),
each reading `call.now()` before `call.defer(...)`, dropping the manual
`handle`/`handle_call` pair and its hand-written `UnsupportedMessage`
reject arm. `cargo test -p tina --test request_call_now` proves the
accessor's value and borrow behavior; `cargo build --tests` +
`cargo test` on the specimen pass unchanged (2/2).

### 37. Accept-loop bad-peer survivability — Phase 120 hostile review

**Surfaced by:** `system_realtime_rooms/tests/bad_peer.rs`,
`tina-http/tests/server_bad_input.rs`.

The realtime-room bad-peer suite exposed a real listener survivability bug.
The plain HTTP listener treated any `tcp_accept` error as fatal and closed the
listener. A peer reset or half-close can surface as accept-side
`CallError::Io`, so one bad peer could shut the front door and make the next
peer observe `ConnectionRefused`.

Closed by re-arming the HTTP/1 and h2c accept loops on accept-side
`CallError::Io` while preserving fatal handling for non-`Io` internal contract
errors. The proof is user-facing, not pretty-wire-output-facing: reset,
half-close, and malformed peers may observe reply bytes, EOF, or reset
depending on OS close timing, but later fresh connections must still be
accepted and served.

### 26. Call-shaped sends from `handle_call` deliver completions back as calls — closed by phase 114

Closed by the live-runtime regression
`tina-runtime/tests/runtime_call_completion_from_handle_call.rs::runtime_call_returned_from_handle_call_completes_as_event`.

The test pins the user-truth resolution chosen in option (a) of the
original finding: when an isolate's `handle_call` returns a
runtime-owned call effect (`sleep(...).then_with_request(req, ...)` in
the regression, but applies to any `.then` continuation), the
completion arrives as an ordinary internal-event message at `handle`,
not back at `handle_call`. The original caller receives the deferred
reply through `reply_to_request`, and the trace records no
`CallRejected { UnsupportedMessage }` event for the continuation.

This is the path `system_realtime_rooms` would have wanted: send-shaped
effects emitted from `handle_call` no longer carry hidden routing back
into `handle_call`. If a future change reintroduces the hidden routing,
this regression test catches it on the live threaded runtime path
(non-split isolate, no fixtures, hermetic timer).

### 34. `call.defer(async_bridge).reply(...)` from `handle_call` — Phase 104 proof

The suspected runtime gap was re-tested before Phase 104 merged. The
general runtime path already works (`handle_call` defers through a
multi-turn callee and preserves the original caller). Phase 104 now
pins the AWS-shaped version directly with hermetic S3 and SQS bridge
tests:

- `tina-aws-bridge/tests/bridge.rs::handle_call_defer_through_s3_bridge_replies_to_original_caller`
- `tina-aws-bridge/tests/sqs_bridge.rs::handle_call_defer_through_sqs_bridge_replies_to_original_caller`

Both tests put a relay/lane isolate in front of the AWS bridge, issue
`call.defer(send_s3/send_sqs(...)).reply(...)` from `handle_call`, let
the AWS bridge complete through its async SDK task + `sleep().then(Poll)`
loop, and assert the original caller receives the final reply. The public
`run_against_s3` / `run_against_sqs` paths remain available for larger
system specimens, but the panic report is closed.

### 17. Host-thread `call_blocking` — Phase 068 follow-up

Surfaced by `specimen_native_https` and native HTTP/TLS tests.
`ThreadedRuntime::call_blocking(addr, msg, timeout)` now performs
the ordinary typed Tina call through a temporary driver isolate and
returns `CallOutcome<R>` to the host thread. The HTTPS specimen and
the direct TLS client/server tests use it; tests that intentionally
need a concurrent in-flight call still keep an explicit driver.

### 18. Trace query helpers — Phase 068 follow-up

Surfaced by TLS regression tests that repeatedly scanned for
`RuntimeEventKind::CallCompleted` / `CallFailed` by hand.
`RuntimeTraceExt` now adds `count_completed`, `any_completed`,
`count_failed`, `any_failed`, `count_failed_with`, and
`count_completion_rejected` on trace slices. The helpers summarize
existing trace facts only; they do not infer hidden causality.

### 1. `observe_result` on `ThreadedMultiShardRuntime` — Phase 062 Rock 1

Surfaced by `specimen_sharded_fanout_read`, `specimen_sharded_keyspace`.
`runtime.observe_result::<Report, _, _>(addr)` now exists on the
multi-shard threaded shell with the same single-claim semantics as
the single-shard form. Both 053 specimens use it directly; the
`Arc<Mutex<Option<Report>>>` polling is gone.

### 4. Synchronous `try_send_outcome` — Phase 062 Rocks 3 & 4

Surfaced by `specimen_rate_limited_worker`,
`specimen_hot_key_fairness`. `runtime.try_send_outcome(addr, msg,
&outcomes)` plus a shared `HostBurstOutcomes` accumulator removes
the per-send observer closure, the Arc-cloned counters, and the
manual observed barrier. `runtime.send_observed_until(addr,
deadline, backoff, || msg)` covers the "control message through a
saturated mailbox" pattern with a typed
`SendObservedUntilError::{Timeout, Closed, WorkerStopped}`.

Per-send precision still rides on the worker-thread observer: true
synchronous-in-the-host mailbox inspection would violate SPSC and
expose the worker's address->mailbox registry to the host thread,
so the helper removes bookkeeping, not the worker roundtrip.

### 5. Single-in-flight gate for timer-driven workers — Phase 062 Rock 5

Surfaced by `specimen_rate_limited_worker`,
`specimen_hot_key_fairness`, and reinforced by
`specimen_periodic_batcher` / `specimen_graceful_drain_server`.
`tina_runtime::SingleCallGate` names the "at most one timer/call in
flight, plus N queued" invariant. `submit()` returns `true` when
the caller should schedule; `complete()` returns `true` when more
work is queued and the next timer should be scheduled. The gate is
plain data — it does not own the timer or the trace; the caller
still writes `sleep(...).then(...)` so every event is visible.

### 6. Bridge call retry classifier — Phase 062 Rock 6

Surfaced by `specimen_retrying_outbound_http`,
`specimen_webhook_fanout`. `ReqwestOutcomeExt::classify` returns
`ReqwestOutcomeClass::{Succeeded, Transient(reason),
Fatal(reason)}` with typed reason payloads. The raw layered
`ReqwestCallOutcome` and `flatten_outcome` are unchanged; the
classifier is opt-in sugar. `specimen_retrying_outbound_http` and
`specimen_webhook_fanout` now match three arms instead of six.

### 10. Retry helper at the service edge — Phase 062 Rock 4

Closed by the same Rock as finding 4. `send_observed_until` covers
both shapes — burst-message ingress and one-shot control-message
delivery through a saturated mailbox.

## How To Add A Finding

Only add to this file when the finding implies Tina product work.

```md
### N. Short product-shaped title

**Surfaced by:** `example_name`, `other_example`.

What repeated pain we saw.

**Build:** concrete primitive, API, doc, or test work.
```

Per-example flavor belongs in the example README. Resolved
archaeology belongs in `FINDINGS_HISTORY.md`.

Numbers are stable: when a finding closes, move it down to
[Closed](#closed) and keep its number so external references
(README links, commit messages, prior PRs) stay valid.

## Resolved Or Retired Round 1 (Phase 053 + 059)

Round 1 closed in Phase 059 + Phase 053. Those nine items are
archived verbatim in [`FINDINGS_HISTORY.md`](FINDINGS_HISTORY.md).
Short summary of patterns no new code should copy:

- hand-rolled mailbox factories: use `DefaultMailboxFactory` /
  `DefaultThreadedMailboxFactory`;
- `Arc<Mutex<Option<SocketAddr>>>` for listener bind address: use
  `observe_next_bound()`;
- trace fingerprinting via `Debug`: use `RuntimeEvent::stable_hash()` /
  `stable_trace_hash(...)`;
- one-off shard types for single-shard programs: use `SingleShard` or omit
  `shard = ...`;
- `Arc::try_unwrap` bridge shutdown dances: use the bridge host lifecycle;
- `Arc::try_unwrap(runtime)` host shutdown dances on threaded runtimes:
  use `runtime.shutdown_handle()` and the cloneable
  `ThreadedShutdownHandle` (`request_shutdown` is nonblocking and
  idempotent; `wait_report(timeout)` returns the cached terminal
  report; see [docs/tina-user-guide/14-lifecycle-and-shutdown.md](../docs/tina-user-guide/14-lifecycle-and-shutdown.md));
- old shared comparison harnesses: examples are specimens, tests are proof;
- `Arc<Outcome>` / `Arc<Mutex<Vec<_>>>` for an isolate's *final* app
  value: use `stop_with(value)` +
  `runtime.observe_result::<T>(addr)?` (works on single-shard and
  multi-shard threaded runtimes; see active finding 1's closure
  above);
- per-comparison shard types: use `SingleShard` for one-shard programs and
  `tina_runtime::sharded::ShardPlacement` / `ShardServiceTable` for
  multi-shard placement.
