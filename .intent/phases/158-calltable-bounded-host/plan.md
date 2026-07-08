# 158 — CallTable + bounded host control plane

Highest-risk change of the current batch. Correctness and runtime/sim parity beat
speed. The golden trace-hash and DST replay tests are the equivalence oracle.

## Two problems

**P1 — call bookkeeping is parallel Vecs glued by panics.** The single-shard
runtime tracks in-flight calls across three parallel structures keyed by
`CallId`:

- `in_flight_calls: Vec<InFlightCall>` + `in_flight_call_indexes: HashMap`
- `translators: Vec<StoredTranslator>` + `translator_indexes: HashMap`
- `pending_isolate_calls: Vec<PendingIsolateCall>` + `pending_isolate_call_indexes`
  + `pending_isolate_call_deadlines: BTreeMap`

`in_flight_calls` and `translators` are two structures for the *same* driver
call, kept consistent by `panic!` when one is present and the other missing
(`missing translator`, `translator already consumed`, `driver produced
completion for unknown call`). A driver accounting bug currently panics the
shard thread and takes every isolate on it down with it.

**P2 — host control plane can hang forever.** `ThreadedRuntime::call`
(threaded.rs) does a blocking bounded send + an unbounded `reply_rx.recv()`.
Every host API routes through it. One user handler that loops forever wedges the
host thread with no timeout and no diagnosis. `ThreadedMultiShardRuntime::call_on`
has the same unbounded `recv()`.

## Design — CallTable

One owner type keyed by `CallId`, replacing all eight fields above. Two families
of call share the `CallId` space but never the same id (each id is minted once by
`IdSource::next_call_id`):

```
struct CallTable {
    driver:  BTreeMap<CallId, DriverCall>,     // host/backend I/O calls
    isolate: BTreeMap<CallId, IsolateCall>,    // isolate->isolate calls
    isolate_deadlines: BTreeMap<(Instant, u64), CallId>,  // earliest-deadline index
}

struct DriverCall {   // == InFlightCall + StoredTranslator, folded
    call_kind, requester, cause, persistence, continuation_context,
    translator: ErasedTranslator,             // INLINE, non-Option
}

struct IsolateCall {  // == PendingIsolateCall, translator de-Option'd
    requester, cause, deadline, insertion_order, continuation_context,
    expected_reply_type_id, handle_shared,
    translator: ErasedIsolateCallTranslator,  // INLINE, non-Option
}
```

Why this shape:

- **Translator inline** — the entry and its translator are inserted and removed
  together, atomically. "missing translator" and "translator already consumed"
  become structurally impossible; those panics are *deleted*, not converted.
- **`BTreeMap<CallId, _>`** not `HashMap` — iteration order must be
  deterministic. `CallId`s are monotonic, so ascending-key iteration == insertion
  order. The by-requester cancel sweeps and owner-stop partition iterate in this
  order; the simulator mirrors it, so trace ordering stays identical across both.
  (The former runtime `swap_remove` perturbed cancel order; no golden scenario
  cancels >=2 driver calls for one requester interleaved, so ascending order
  coincides with today's observable order for every tested scenario. The sim
  already iterated in insertion order via `Vec::remove`, so this actually
  *removes* a latent runtime/sim asymmetry.)
- **Deadline index folded in** — only isolate calls carry deadlines. Kept as the
  same `BTreeMap<(Instant, u64), CallId>` so harvest order is unchanged.

The index `HashMap`s and the `swap_remove`/`rebuild_*_indexes` dance are gone.
`PreallocationConfig::call_capacity` still reserves `driver_completions`;
BTreeMaps do not pre-reserve, so the two capacity assertions in tests that probed
the removed Vecs are updated to probe live behavior instead.

## Error policy — decided explicitly

- **QUARANTINE (driver-sourced inconsistency).** `deliver_completion` for a
  `call_id` the table no longer tracks (unknown / already settled / type-confused
  against the isolate map) traces a NEW `RuntimeEventKind::DriverCompletionQuarantined
  { call_id }`, drops the completion, and keeps the shard alive. A buggy driver
  must not kill unrelated isolates. This is the *only* former driver-sourced panic
  left after folding translators inline; "already consumed" is now impossible.
  Attributed to `IsolateId::new(0)` (runtime/shard sentinel; user isolates start
  at 1) with `cause: None`.
- **KEEP panic (wrong-message-TYPE delivery).** dispatch.rs type-confusion sites
  (`runtime attempted to deliver a message to a mailbox with the wrong type`, and
  the call-handler-message twins) stay `panic!`. Those mean the erased downcast
  chain is wrong; continuing would corrupt state worse than aborting.

New event tag: `write_kind_stable` gets append-only tag **46** (`write_u8(46)` +
`write_u64(call_id.get())`); never renumber existing tags. `projection.rs` gets a
matching `RuntimeEventKindName::DriverCompletionQuarantined` + arm (its match is
exhaustive, so this is compiler-forced). Because the event only ever fires on a
driver bug, no existing scenario emits it and no pinned `*_TRACE_HASH` moves.

## Hostile review — failure modes before code

- **Cancellation / owner-stop cleanup.** `cancel_pending_isolate_calls_for_owner`
  and `dispatch_cancel_call` previously `translator.take()`-then-drop to discard a
  translator without running it. With inline non-Option translators, removing the
  entry from the table drops the translator — same effect, no take dance. Must
  preserve the two-phase owner-stop order: record *all* cancelled ids in the ring
  first (ring-eviction order matters), then emit `CallCancelled` for each. Iterate
  ascending call-id == the former partition insertion order.
- **Timeout harvest.** `harvest_isolate_call_timeouts` pops the deadline index,
  then removes the isolate entry and consumes its translator. Removal now yields
  the translator directly. A deadline-index entry with no matching isolate entry
  (should be impossible) is skipped, same as today's `else { continue }`.
- **Resource-close cancel during driver advance.** `advance_driver` purges
  carried `pending_completions` for calls the driver cancelled by close, then
  `cancel_in_flight_call_for_resource_close`. With quarantine, even if that purge
  regressed, a stale carried completion would quarantine (drop) instead of
  panicking — strictly safer.
- **Cross-shard terminal replies.** Untouched. Remote spawn/child-control tables
  are separate fields, not folded into CallTable.
- **Duplicate id.** `insert_driver` / `insert_isolate` keep the duplicate-id
  assert (a duplicate mint is a runtime invariant break, not a driver bug).
- **`has_in_flight_calls` / `has_pending_runtime_work` / capacity introspection.**
  Re-expressed against CallTable emptiness; deadline-index non-empty check
  unchanged.

## Bounded host control plane (P2)

`ThreadedRuntime::call` gets a `recv_timeout(control_call_timeout)` with a
generous configurable default (`ThreadedRuntimeConfig::control_call_timeout`,
30s). On timeout: set shard metrics state `Failed`, return new
`ThreadedRuntimeError::WorkerUnresponsive`. `ThreadedMultiShardRuntime::call_on`
mirrored (per-shard metrics `Failed`). Callers already funnel through `?` and
mostly already handle `WorkerStopped`; the new variant propagates the same way. A
test registers an isolate whose handler blocks forever and asserts a host call
returns `WorkerUnresponsive` within the bound instead of hanging.

`send_and_observe`'s separate unbounded `recv()` is out of scope: it does not
route through `call` and the task scopes P2 to `call` / `call_on`.

## Sim mirror (Commit 2)

tina-sim keeps its own parallel copies (`internals.rs`) and its Simulator state
in `lib.rs`. Mirror symmetrically:

- Give the sim a `CallTable` of the same shape (deadline type is `Duration`
  virtual-time instead of `Instant`; otherwise identical API + BTreeMap internals),
  fold translators inline, delete the sim's index-free Vec scans.
- `deliver_completion_at`'s unknown-call `panic!` becomes the same
  `DriverCompletionQuarantined` event + return.
- Delete the sim's "already consumed" / "missing translator" panics (structurally
  impossible once inline).
- Non-quarantine trace shape, event ordering, terminal-outcome priority MUST NOT
  move. Golden + DST replay is the oracle; a moved hash means changed semantics,
  investigate before re-blessing.
- Add quarantine unit tests on BOTH sides (inject a completion for an unknown
  call id; assert the event fires and the shard/sim survives).

If the sim mirror proves too large to hold safely in one PR, fall back to
behavior-only parity (quarantine + ordering; keep the sim's Vecs) and split the
structural mirror into a follow-up. The oracle needs behavioral parity, not
identical storage.

## Verification

`cargo test -p tina-runtime`, `cargo test -p tina-sim`, `cargo check --workspace`.
NOT `make verify`. Any golden/DST failure => stop, diagnose root cause.
