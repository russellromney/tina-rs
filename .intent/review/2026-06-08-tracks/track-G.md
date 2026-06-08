# Track G — Determinism, simulation, and proof-harness truth

HEAD 49c3580. Read-only review. Focus crates: `tina-sim`,
`tina-proof-harness`, `tina-tracing`, plus replay/trace code in
`tina-runtime`.

## Summary by risk boundary

The saved-case / `ReplayCase` machinery is genuinely sound: the runners
re-run the simulator and compare against pinned `(event_count,
trace_hash)` constants, the hash is an explicit FNV-1a field walk (not
`DefaultHasher`), `mailboxes` is a `BTreeMap` (stable iteration), fault
selection is splitmix64-deterministic from the seed, and the sim is
single-threaded so event-id assignment is contiguous. The seed-sweep,
shrink, and `check_captured_replay` paths all compare real data.

The real determinism hole is at the **live multishard** boundary, where
the project's "sort by event id makes the trace hash stable" claim is
false: event ids in the threaded multishard runtime come from a single
`AtomicU64` shared across OS threads with `Relaxed` ordering, so id
assignment is decided by thread race, not logical order. The same
`IdSource` design is deterministic in the simulator and nondeterministic
in `ThreadedMultiShardRuntime`. This makes the documented
`LiveTrace::snapshot` / `compare_live_shape` regression path flaky for
multishard, and makes the standard DST trace invariants
(`events_are_monotonic`, `causes_point_backward`) inapplicable to live
multishard traces even though they hold in sim. No green proof currently
asserts on a multishard live hash, so the bug is latent — a footgun the
docs actively invite a user to step on, not an already-failing test.

Secondary: `leak_clean` defaults to `true` when no leak check is
supplied; `TraceRetention::Bounded` silently truncates the hash/pressure
surface read via `runtime.trace()`; `sweep_seeds` skips the runner
identity guards that `observe_replay_case`/`discover_constants` run.

## Ranked findings

### G1 — [High / High] tina-runtime/src/lib.rs:281-297 + threaded_multi_shard.rs:234-296 — shared `AtomicU64(Relaxed)` event-id counter makes live multishard trace ids race-assigned, breaking the documented "sort by id → stable hash" claim

- **Invariant**: "Replay hashes are deterministic where they are used as
  proof" and "Live and simulator behavior match where the project says
  they match." `LiveTrace::snapshot` (tina-proof-harness/src/live_replay.rs:94)
  sorts events by `event.id()` and hashes with `stable_trace_hash`, and
  its docstring + the test
  `snapshot_hash_sorts_by_event_id_for_multishard_arrival_order`
  (live_replay.rs:254) claim this yields a stable hash regardless of
  multishard arrival order.
- **Concrete bug**: `IdSource { next_event_id: Arc<AtomicU64> }` is
  cloned into every shard worker thread
  (`threaded_multi_shard.rs:234` `let ids = IdSource::new();` then
  `let ids = ids.clone();` per worker, line 244) and each shard's
  `Runtime::push_event` (dispatch.rs:2281) calls
  `self.ids.next_event_id()` = `fetch_add(1, Relaxed)`. With shards on
  separate OS threads, the integer assigned to a given logical event
  depends on which thread won the `fetch_add` race. Sorting the captured
  `Vec` by id therefore orders events by their *racy* id, not by logical
  causality, so two runs of the identical multishard workload produce
  different id→event mappings, different sorted orders, and different
  `stable_trace_hash` values. Sorting "fixes" the Mutex arrival order but
  not the id assignment.
- **Why it happens in real use**: any specimen that captures a
  `LiveTrace` against a `ThreadedRuntime::*multi*` topology and uses
  `compare_live_shape(saved_shape, ...)` as a regression check will see
  the hash flap between runs. The single-shard bugbox specimen avoids it
  only because it has one shard.
- **Sim/live divergence**: the *same* `IdSource` is shared in the sim
  multishard coordinator (tina-sim/src/multi_shard.rs:105-113) but
  `step()` drives shards sequentially on one thread (multi_shard.rs:303),
  so ids are contiguous and the hash is stable. The contract that DST
  proves in sim does not transfer to the live multishard runtime.
- **Repro idea**: build a 2-shard `ThreadedMultiShardRuntime`, run a
  fixed cross-shard workload twice with a `LiveTrace` each, assert the
  two `snapshot().trace_hash` values are equal. Expect intermittent
  inequality under load / multiple cores. (Single-threaded sim version of
  the same workload stays equal — that contrast is the proof.)
- **Fix**: do not rely on a global racy counter for replay-identity ids
  in the threaded runtime. Either (a) make event ids per-shard
  (`shard_ordinal << 48 | local_seq`) so each shard's local sequence is
  deterministic and the merged trace can be ordered by
  `(shard, local_seq)`; or (b) have `LiveTrace::snapshot` sort by a
  deterministic key (`(shard, isolate, cause, kind, local_seq)`) rather
  than the global id; or (c) restrict the "stable hash" claim in the
  `snapshot` docstring and the `compare_live_shape` doc to single-shard
  runtimes and fail closed (panic/return mismatch) when more than one
  shard id appears in the captured trace. At minimum the
  `snapshot_hash_sorts_by_event_id_for_multishard_arrival_order` test
  name and the docstring are an over-claim and must be corrected.
- **LLM-pattern?**: yes — plausible "just sort by id" idiom that is
  correct for a single deterministic stream and silently wrong for a
  multi-thread interleaving.

### G2 — [Medium / High] tina-sim/src/dst/invariants.rs:127,145 — standard DST invariants (`events_are_monotonic`, `causes_point_backward`) assume contiguous, same-slice ids; false on live multishard traces

- **Invariant**: a trace invariant that "holds" must hold for the traces
  it is actually run against.
- **Concrete bug**: `events_are_monotonic` requires
  `event.id().get() == previous + 1` for the whole slice;
  `causes_point_backward` requires `cause.event() < event.id()` *and*
  that the cause event exists in the same slice. Both are true for a
  single-shard sim trace (contiguous ids, single Vec). On a live
  multishard trace: (a) ids are interleaved by thread race so they are
  not `+1` contiguous in capture/sort order; (b) cross-shard causes carry
  an `EventId` minted on the *originating* shard (dispatch.rs:418
  `CauseId::new(EventId::new(remote_cause))`) which, due to the racy
  counter, can be numerically ordered arbitrarily relative to the caused
  event, and which lives in a *different shard's* `trace` Vec unless the
  caller merged all shards. Running `InvariantSuite::standard()` over a
  per-shard live trace would report false `causes_point_backward`
  violations; over a merged live trace it can report false
  `events_are_monotonic` violations.
- **Why it happens**: the invariants read as general ("the standard Tina
  trace invariant set") and a specimen author may apply them to a
  `LiveTrace::events()` from a multishard runtime.
- **Repro idea**: feed `events_are_monotonic` a 2-shard live capture; it
  fails on the first cross-shard id gap even though nothing is wrong.
- **Fix**: scope these invariants to "one shard's contiguous id stream,"
  enforce single-shard input (assert all `event.shard()` equal), or
  rewrite monotonicity as per-shard-local monotonic and causality as
  "cause exists somewhere in the merged trace and is logically earlier"
  rather than `cause.id() < event.id()`. Same root cause as G1.
- **LLM-pattern?**: yes — invariant written against the deterministic sim
  shape and assumed universal.

### G3 — [Low / High] tina-proof-harness/src/load.rs:250 — `leak_clean` defaults to `true` when no leak check is supplied; a "leak-clean" report can prove nothing

- **Invariant**: a report field named `leak_clean` should mean "leak
  checked and clean," not "leak not checked."
- **Concrete bug**: `let leak_clean = leak_check.map(|f| f()).unwrap_or(true);`
  When the caller passes `None` for `leak_check`, `LoadReport.leak_clean`
  is `true` and the `Display` line prints a clean leak result. A reader
  of the report cannot distinguish "verified no leak" from "never
  looked."
- **Why it happens**: load harnesses are frequently invoked without a
  leak closure during quick soaks.
- **Fix**: make `leak_clean: Option<bool>` (or add a
  `leak_checked: bool`) so an unchecked run renders as `leak=unchecked`
  rather than `leak_clean=true`.
- **LLM-pattern?**: yes — `unwrap_or(true)` defaulting an assertion to
  "pass."

### G4 — [Low / Medium] tina-runtime/src/dispatch.rs:2288-2306 — `TraceRetention::Bounded`/`Off` silently truncates the trace, so a `stable_trace_hash`/`PressureSummary` read via `runtime.trace()` no longer reflects the full run

- **Invariant**: a pinned trace hash / pressure summary must reflect the
  events that actually happened.
- **Concrete bug**: under `Bounded(capacity)`, `push_event` drops the
  oldest events (`trace_start += 1; trace_dropped += 1`) and under `Off`
  drops everything (`trace_dropped += 1`). A specimen that computes
  `stable_trace_hash(runtime.trace().iter())` or
  `PressureSummary::from_events(runtime.trace())` after a bounded run gets
  a hash/summary over only the retained suffix — pressure events that
  scrolled off are invisible, so a "clean soak" can hide real
  backpressure. The `LiveTrace` observer path is safe (it captures in
  `on_event`, before retention), but `runtime.trace()`/`event_record()`
  readers are not.
- **Mitigation already present**: default retention is `Full`
  (lib.rs:608, threaded.rs:109), so this only bites when a user opts into
  bounded/off retention and then hashes the in-memory trace.
- **Fix**: document loudly that hashing/summarizing `runtime.trace()` is
  only valid under `Full`; or expose `trace_dropped()` and have
  `PressureSummary`/hash helpers refuse (or flag) a truncated trace.
- **LLM-pattern?**: partial — the truncation is intentional, the truth
  gap is in letting truncated traces feed proof helpers.

### G5 — [Low / Medium] tina-sim/src/dst/sweep.rs:116 — `sweep_seeds` calls `runner(&case)` directly, skipping the case/runner identity guards that `observe_replay_case` enforces

- **Invariant**: a discovered "failing case" must be the case the runner
  actually ran.
- **Concrete bug**: `discover_constants` runs each case through
  `observe_replay_case` (which debug-asserts case/history coherence and
  report identity), but `sweep_seeds` calls the runner directly. A buggy
  runner that returns a `ReplayReport` for a different case
  (wrong name/seed/config) is not caught; the sweep then writes that
  report's `event_count`/`trace_hash` onto `failing_case`, producing a
  pinned case whose constants came from a mismatched run.
- **Fix**: route `sweep_seeds` through `observe_replay_case` (or call
  `report_identity_error`/`case_history_coherence_error`) before trusting
  the report, matching `discover_constants`.
- **LLM-pattern?**: yes — guard applied in one helper and forgotten in
  the sibling.

### G6 — [Low / Low] tina-sim/src/sim_impl.rs:5655 — `fault_selector` ignores `tag` when `ordinal == 0`, correlating the first fault decision across unrelated fault categories

- **Invariant**: independent fault streams (timer / local-send / tcp)
  should be statistically independent given the seed.
- **Concrete bug**: for `ordinal == 0` the function returns
  `seed % modulus` regardless of `tag`, so the very first candidate of
  every fault category shares one selector value. With equal `one_in`
  the first timer fault, first local-send fault, and first tcp fault all
  fire (or not) together. Deterministic, so no replay break — just a
  weaker fault distribution at ordinal 0.
- **Fix**: run ordinal 0 through `splitmix64(seed ^ tag.rotate_left(17))`
  like every other ordinal instead of the early `return seed % modulus`.
- **LLM-pattern?**: mild — special-casing the zero index.

## Disproven / checked-clean

- **Wall-clock leakage into the trace hash** — DISPROVEN.
  `Simulator.virtual_anchor = Instant::now()` (tina-sim/src/lib.rs:194)
  is real wall-clock, but `RuntimeEvent` carries no time field and the
  trace hash walks only id/cause/shard/isolate/kind (trace.rs:808-820),
  so the anchor never enters the hash. The docstring (lib.rs:105-120) is
  honest about the cross-run anchor difference and confines it to user
  code that embeds raw `Instant`s. `MonotonicClock` (clock.rs) is only
  the live runtime's now-source; not in the hash. Test cover:
  `same_seed_local_send_failure_replays_same_checker_failure`
  (faulted_replay.rs:315) re-runs and asserts identical `event_record()`.
- **HashMap iteration into a hash** — DISPROVEN. `ReplayConfig.mailboxes`
  is `BTreeMap` (dst/mod.rs:198); `topology_roles` collected from
  `mailboxes.keys()` (replay_case.rs:248) is therefore ordered;
  `replay_config_hash` encodes it in iteration order deterministically.
  `live_fact_sets_match` sorts both sides before comparing
  (replay_case.rs:1261). No `std::collections::HashMap`/`HashSet` feeds
  any hash in the focus crates.
- **`stable_trace_hash` non-determinism** — DISPROVEN for the
  single-thread paths. It is FNV-1a over each event's explicit
  `stable_hash` field walk with hand-assigned variant tags
  (trace.rs:1044-1647), independent of `DefaultHasher`. Order-sensitive by
  design; callers that need order-insensitivity sort first. Sim
  `push_event` is single-threaded so its ids are contiguous (the
  multishard *live* exception is G1).
- **Saved replay artifact loaded but never compared ("proof that proves
  nothing")** — DISPROVEN for the main path. `assert_replay_case` /
  `check_replay_case` re-run the runner and compare `event_count` +
  `trace_hash` against pinned constants and panic on drift
  (replay_case.rs:1488-1565); `saved_replay_cases.rs` pins real numbers
  (`BURST_OVERFLOW_TRACE_HASH = 0xe22d12a51cd8cf10`, full_rejections = 5)
  and `changing_the_seed_changes_the_trace_hash` proves the seed property
  is non-trivial. `check_captured_replay` enumerates every changed fact
  category and fails closed on unsupported live facts. One narrow
  exception is a *plumbing* unit test, not a behavior proof:
  `live_replay_projected_comparison_names_ignored_event_kinds`
  (saved_replay_cases.rs:509) feeds the runner the same `events` slice
  that built the capture, so it self-compares and always passes — but it
  is testing the projection wiring, not simulator behavior, and is
  labeled as such. Not a finding.
- **Poisoned-lock behavior changing outcomes** — checked. `LiveTrace`
  uses `Mutex` with `.expect("...poisoned")` everywhere
  (live_replay.rs:49,81,95,159); a poisoned lock panics rather than
  silently changing a result. Acceptable for a test harness.
- **Reports overwriting earlier errors (last-write-wins)** — checked.
  `PressureSummary::from_events` and the HostBurst snapshots are additive
  counters; `CapturedReplayMismatch.changes` is an accumulating `Vec`
  (replay_case.rs:1156-1195), not last-write-wins.

## Areas needing deeper review (out of time / scope)

- Cross-shard cause-id semantics in the live runtime (dispatch.rs:418):
  whether a merged multishard `LiveTrace` ever produces a `causes_point_backward`
  violation that is logically spurious, under real thread interleaving.
- `ThreadedMultiShardRuntime` shutdown trace completeness: whether all
  shards' `on_event` callbacks are flushed before `shutdown()` returns,
  so a `LiveTrace` snapshot taken post-shutdown is complete (relevant to
  any future multishard hash pinning).
- `tina-tracing` `install.rs`/`live.rs` were not deeply read; the
  observer itself (observer.rs) is stateless and clean.

## Suggested tests

- **Property (the G1 proof)**: 2-shard `ThreadedMultiShardRuntime`, fixed
  cross-shard workload, two `LiveTrace` captures → assert equal
  `snapshot().trace_hash`. Pair with the single-thread sim version of the
  same workload asserting equality, to show the contrast.
- **Invariant guard**: `events_are_monotonic` / `causes_point_backward`
  fed a synthetic 2-shard trace with non-contiguous ids → assert they
  either pass (after a per-shard fix) or explicitly reject multishard
  input, not silently false-fail.
- **Retention truth**: run a bursting workload under
  `TraceRetention::Bounded(4)`, hash `runtime.trace()` and compare to the
  `LiveTrace`-observed full hash → demonstrate the truncation gap (G4).
- **Sweep guard**: a `sweep_seeds` runner that returns a report for the
  wrong case name → assert it is rejected like `discover_constants` (G5).

## Track coverage map

| Finding | Crate / file |
|---|---|
| G1 trace-hash race | tina-runtime lib.rs/threaded_multi_shard.rs, tina-proof-harness live_replay.rs |
| G2 invariants assume contiguous ids | tina-sim dst/invariants.rs |
| G3 leak_clean defaults true | tina-proof-harness load.rs |
| G4 bounded-retention hash truncation | tina-runtime dispatch.rs |
| G5 sweep skips identity guard | tina-sim dst/sweep.rs |
| G6 fault_selector ordinal-0 tag collapse | tina-sim sim_impl.rs |
