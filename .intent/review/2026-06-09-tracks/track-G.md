# Track G: determinism, simulation, and proof harness — 2026-06-09

Reviewed at HEAD `0cd6a31` (= origin/main). Scope: `tina-sim/src/` (dst/, invariants,
sweep, replay), `tina-proof-harness/src/`, trace/replay accessors in
`tina-runtime` (the `tina/src/live_replay.rs` named in the brief does not exist;
the live-replay surface lives in `tina-proof-harness/src/live_replay.rs` and the
trace accessors in `tina-runtime/src/{lib,threaded,threaded_multi_shard,local_system}.rs`),
and `tina-tracing` excluding install.rs/live.rs shutdown-flush (sibling agent).

Special attention to the G1–G5 fixes from PR #231 per the brief: are they
complete, and did per-shard event-id namespacing break any consumer that
assumed global ids?

## Summary by risk boundary

- **G1 fail-closed is real on the front door, absent on the side door.**
  `LiveTrace::snapshot_complete` / `compare_live_shape_complete` /
  `RunCapture::finish` all refuse multishard and lossy traces. But the
  *capture builder* (`LiveReplayCaptureBuilder`, re-exported as `capture_run`,
  `capture_live_run`, `capture_overload_run`) hashes raw arrival-order events
  with no multishard or dropped-events gate and pins that hash into the saved
  replay artifact (TG-1).
- **G4 fail-closed is real on the inner Runtime, absent on the public
  wrappers.** `Runtime::trace_for_proof()` refuses a retention-truncated
  trace, but `ThreadedRuntime::complete_trace`,
  `ThreadedMultiShardRuntime::complete_trace`, and
  `TraceSnapshot::complete(...)` read the retained suffix and label it
  complete; `trace_for_proof` has zero non-test callers (TG-2).
- **Shrinkers trust the caller's bug exists.** None of the three shrink
  helpers run `still_fails` against the *original* case, so a
  non-reproducing capture exits as a "shrunk" case with constants refreshed
  from a passing run (TG-3).
- **Per-shard id namespacing (G1 fix) did not break any proof consumer**, but
  it quietly degraded the timeline export's cross-shard semantics and left
  one span-pairing key under-scoped (TG-5).
- G2, G3, G5 fixes verified correct on current HEAD (see disproven section).

## Findings

### TG-1 — Multishard/lossy live traces reach a pinned hash via the capture builder
1. **Severity:** Medium
2. **Confidence:** High
3. **File/line:** `tina-sim/src/dst/replay_case.rs:734-749` (`with_trace`),
   `:830-881` (`finish` → `project_trace_shape(&events, …)`);
   `tina-sim/src/dst/projection.rs:534-539` (`Exact` → `TraceShape::from_events`,
   no sort, no shard check); re-exports: `tina-proof-harness/src/live_replay.rs:484`
   (`capture_run`), `tina-sim/src/dst/overload.rs:31` (`capture_overload_run`);
   in-tree reachability: `examples/systems/system_live_replay_bugbox/src/lib.rs:441-456`
   feeds raw live `ThreadedRuntime` events into the builder.
4. **Violated invariant:** "the proof path fails closed on a multishard (or
   lossy) live trace" — the G1 fix's own contract. Grep proof: zero occurrences
   of `shards()`/`Multishard` in `replay_case.rs`.
5. **Concrete bug:** `LiveReplayCaptureBuilder::finish` computes
   `expected = project_trace_shape(events, projection)` over the events in
   **capture-arrival order** with no gate. For a live multishard trace,
   arrival order is the cross-thread mutex race the G1 review proved
   non-deterministic, and `stable_trace_hash` is order-sensitive and hashes
   per-shard event ids. The saved `LiveReplayCapture` / saved-case file
   (`expected_trace_hash=` line) pins a hash that flaps run-to-run. There is
   also an internal inconsistency: `LiveTrace::snapshot()` sorts by
   `(shard, id)` before hashing while the builder hashes unsorted, so the same
   logical capture produces two different "expected" hashes depending on the
   path used (`RunCapture::finish` validates `inputs.expected` against the
   *sorted exact* snapshot, then stores the *unsorted projected* shape — they
   only coincide for single-shard in-order captures).
6. **Why it happens in real use:** the overload-bugbox workflow
   (`capture_overload_run` → `save_overload_bug` → `replay_overload_bug`) is
   exactly what a service author reaches for after a live multishard overload
   incident. The capture saves cleanly; every later `check_captured_replay`
   fails with `Hash`/`EventCount` diverged, misattributing the divergence to
   the simulator ("sim did not reproduce the live story") when the artifact
   itself was never stable. Fail-visible, not fail-open — but the harness
   creates a different failure shape than it claims, which is this track's
   core sin.
7. **Repro/failing test idea:** run the 2-shard ping workload from
   `tina-proof-harness/tests/multishard_trace_determinism.rs`, feed
   `trace.events()` into `capture_live_run("x").with_trace(...)...finish()`
   twice; assert the two `capture.expected` values are equal — flaps.
8. **Fix:** in `LiveReplayCaptureBuilder::finish` (and
   `LiveReplayCapture::from_events_with_options`), compute the distinct shard
   set of `events`; if >1 shard, fail closed (new
   `LiveReplayCaptureBuildError::Multishard`) or auto-record an
   `UnsupportedLiveFact` + `TraceCompleteness::Partial` so `replay_blocked`
   is true. Accept an `upstream_dropped_events: u64` the same way
   `RunCapture::finish` does. Do **not** "fix" by sorting inside
   `project_trace_shape` — sim runners pin emission-order hashes and would all
   break.
9. **LLM-pattern?** Yes — the classic "cap/check honored on one path, ignored
   on its symmetric twin": the gate landed on `RunCapture` but not on the
   lower-level builder that the convenience wrappers actually expose.

### TG-2 — `complete_trace()` / `TraceSnapshot::complete` launder retention-truncated traces
1. **Severity:** Medium
2. **Confidence:** High
3. **File/line:** `tina-runtime/src/threaded.rs:1291-1316` (`trace()` wraps
   `complete_trace()` in `TraceSnapshot::complete`, `complete_trace` =
   `self.call(|runtime| runtime.trace().to_vec())`);
   `tina-runtime/src/threaded_multi_shard.rs:592-613` (same shape per shard);
   contrast `tina-runtime/src/lib.rs:801-807` (`trace_for_proof`, the G4
   accessor) and `:791` (`trace_is_truncated`).
4. **Violated invariant:** "truncated trace is detectable and refused by the
   proof accessor" (G4 resolution). Field-name truth: a method named
   `complete_trace` and a constructor named `TraceSnapshot::complete` promise
   completeness the code never checks.
5. **Concrete bug:** under `TraceRetention::Bounded(n)` / `Off` (settable via
   `set_trace_retention`, surfaced in `LiveShardReport.trace_retention`), the
   inner `runtime.trace()` is a retained suffix and `trace_dropped() > 0`. The
   threaded wrappers ignore both: `complete_trace()` returns `Ok(suffix)`,
   `trace()` returns a snapshot whose `is_complete()` is true, and
   `TraceSnapshot::complete_events()` "proves" completeness.
   `trace_for_proof()` — the accessor the G4 test pins — has **no callers**
   outside `lib.rs` and its test. Downstream, the Chrome-trace export writes
   `"complete": missing_shards.is_empty()` (`tina-tracing/src/timeline.rs:275`)
   and `PressureSummary::from_events` silently summarizes the suffix.
6. **Why it happens in real use:** bounded retention is precisely the
   production configuration; an operator exporting a timeline or hashing
   `complete_trace()` after an incident gets a partial trace labeled complete.
   In-tree, `tina-sim/tests/timmerhus_dst.rs:343-357` derives assertions from
   `complete_trace()` counts (default Full retention today, so latent).
7. **Repro/failing test idea:** `ThreadedRuntime` with
   `TraceRetention::Bounded(3)`, push >3 events, assert
   `runtime.complete_trace()` errs or that `runtime.trace().is_complete()` is
   false — both currently pass as "complete".
8. **Fix:** thread `trace_dropped` through the worker call:
   `complete_trace()` returns the existing typed error (or a new
   `ThreadedRuntimeError::TraceTruncated { dropped }`) when any shard reports
   `trace_dropped() > 0`; give `TraceSnapshot` a `dropped_events: u64` field
   and make `complete_events()` refuse when non-zero; timeline metadata gains
   `"trace_dropped"`.
9. **LLM-pattern?** Yes — fail-closed accessor added at one layer, every
   public wrapper above it left reading the unguarded path; the fix's test
   proves the helper, not the user-visible surface.

### TG-3 — Shrink helpers never verify the original case fails
1. **Severity:** Low-Medium
2. **Confidence:** High
3. **File/line:** `tina-sim/src/dst/shrink.rs:172-225` (`shrink_replay_case`:
   initial `runner(&current_case)` result is never passed to `still_fails`),
   `:348-417` (`shrink_captured_replay`: initial run checked only for
   fact-set equality, not failure), `tina-proof-harness/src/byte_replay.rs:230-265`
   (`ProtocolByteReplayCase::shrink`: same).
4. **Violated invariant:** doc claims "Deletion-shrinks … while the bug still
   reproduces"; the precondition "the bug reproduces at all" is unchecked.
5. **Concrete bug:** if the input case does not exhibit the bug (stale
   capture, environment-dependent failure, already-fixed regression), every
   candidate deletion has `still_fails == false`, nothing is removed, and the
   helper returns a `ShrinkReport`/`ShrinkCapturedReport` whose
   `expected_event_count`/`expected_trace_hash` are refreshed **from a passing
   run** and whose `reason` still asserts the bug ("kept failing"). Saved via
   `write_saved_replay_case`, this pins a green run as a bug artifact.
6. **Why it happens in real use:** the documented bug workflow is
   capture → shrink → save; a capture taken after a partial fix, or a flaky
   live failure, silently becomes a no-op "shrunk bug".
7. **Repro/failing test idea:** call `shrink_replay_case` with
   `still_fails = |_| false` and a non-zero history; assert it returns an
   error / sentinel instead of a `ShrinkReport` with `shrunk_len ==
   original_len` and refreshed constants (it currently returns the latter).
8. **Fix:** run `still_fails(&initial_report)` first; return a typed
   `DidNotReproduce` error (mirroring `ShrinkCapturedReplayError`'s existing
   `phase: "initial replay"` shape) instead of a report.
9. **LLM-pattern?** Yes — happy-path loop correct, boundary precondition
   assumed instead of enforced.

### TG-4 — Saved byte-replay artifact loader can panic on malformed UTF-8 hex
1. **Severity:** Low
2. **Confidence:** High
3. **File/line:** `tina-proof-harness/src/byte_replay.rs:666-687`
   (`decode_hex`: `let pair = &value[index..index + 2];`).
4. **Violated invariant:** artifact loaders fail closed with typed
   `Decode` errors on any malformed input (the function's own error channel).
5. **Concrete bug:** the even-length check is in bytes; a `chunk=` line
   containing a multi-byte UTF-8 character at a position where
   `index..index+2` is not a char boundary (e.g. `chunk=€a`, 4 bytes, slice
   `0..2` lands mid-'€') panics with "byte index 2 is not a char boundary"
   instead of returning `ProtocolByteReplayIoError::Decode`.
6. **Why it happens in real use:** hand-edited or corrupted saved cases; the
   loader is explicitly designed (and tested) to reject bad files with typed
   errors, so a panic is the one failure shape consumers don't handle.
7. **Repro:** `std::fs::write(p, "tina-protocol-byte-replay-v1\n…\nchunk=€a\n")`
   then `ProtocolByteReplayCase::load(p, "x")` → panic, not `Err(Decode)`.
8. **Fix:** iterate `value.as_bytes().chunks_exact(2)` and reject non-ASCII
   up front (`value.bytes().any(|b| !b.is_ascii_hexdigit())` → `Decode`).
9. **LLM-pattern?** Yes — string slicing by byte arithmetic that's correct
   for the ASCII data it writes, wrong for adversarial input it reads.

### TG-5 — Timeline export still assumes global event ids / global slot ids
1. **Severity:** Low
2. **Confidence:** High
3. **File/line:** `tina-tracing/src/timeline.rs:339`
   (`events.sort_by_key(|event| event.id().get())` — id-only sort of the
   merged multishard trace), `:337` (`deferred_starts: BTreeMap<u64, _>`
   keyed by bare `slot_id`), `:791` (`cause_id` arg printed without origin
   shard), `tina-runtime/src/threaded_multi_shard.rs:601` (merged trace also
   `sort_by_key(event.id())` only).
4. **Violated invariant:** consumers of the merged trace must not assume an
   event id names one event / encodes cross-shard order (the premise the
   G1/G2 fixes established).
5. **Concrete bug:** post-G1, live multishard ids are per-shard (both shards
   count from 1). The id-only sort no longer yields a cross-shard order; `ts`
   collides across shards (benign-ish: tid=shard separates tracks). The
   stable sort preserves per-shard order so handler/call span pairing
   survives ((shard,isolate) and global `CallId` keys). But deferred-reply
   spans are paired by bare `slot_id`: `DeferredSlotRegistry` is per-shard in
   the threaded runtime, so two shards reuse slot id 1 — the second
   `DeferredReplyCaptured` evicts the first into a bogus
   `unmatched/replaced_by_event_id` instant and the close event can pair with
   the wrong shard's capture. `cause_id` in args is ambiguous the same way
   (`CauseId` carries no shard — the invariants module fails closed on this;
   the timeline silently prints it).
6. **Why it happens in real use:** any multishard live trace exported via
   `TraceTimeline::from_snapshot(runtime.trace())` with deferred replies on
   more than one shard.
7. **Failing test idea:** build events for shards 11 and 22 each with
   `DeferredReplyCaptured{slot_id:1}` then `DeferredReplySent{slot_id:1}`;
   assert the export contains two `deferred_reply` "X" spans and zero
   `unmatched` instants (currently one capture is evicted).
8. **Fix:** key `deferred_starts` by `(ShardId, slot_id)`; sort by
   `(shard, id)`; add a metadata flag naming the id regime
   (`"event_id_scope": "per_shard"`), and omit/qualify `cause_id` when the
   trace spans >1 shard.
9. **LLM-pattern?** Partially — a consumer of a changed invariant that
   compiled fine and is "only a visual view" per its docs (which is why this
   is Low, not Medium).

### TG-6 — Perf JSON line drops the leak_checked distinction (G3 residue)
1. **Severity:** Low
2. **Confidence:** High
3. **File/line:** `tina-proof-harness/src/perf.rs:124-152` (`json_line` emits
   `"leak_clean": self.load.leak_clean`, no `leak_checked` field; schema
   `tina.perf_report.v1`).
4. **Violated invariant:** G3's "an unchecked run must not be conflated with
   a verdict". The human `summary_line` renders `leak=unchecked` honestly;
   the machine line cannot express it.
5. **Concrete bug:** an unchecked run serializes as `"leak_clean": false` —
   conservative (never falsely clean), but a JSON consumer cannot tell
   "verified leaky" from "never looked", which inverts into noise: tooling
   that alerts on `leak_clean:false` fires on every unchecked perf row, and
   the natural "fix" is to ignore the field.
6. **Why real:** the JSON line is the machine-comparison surface for CI
   trend tooling.
7. **Failing test idea:** `PerfReport::from_load(...)` over an unchecked
   `LoadReport`; assert `json_line()` contains `"leak_checked":false`.
8. **Fix:** add `"leak_checked"` to the JSON (schema bump or additive field).
9. **LLM-pattern?** Mild — the honesty fix landed in one renderer, not its
   sibling.

## Disproven suspicions (with proof)

- **Simulator wall-clock leakage via `virtual_anchor: Instant::now()`**
  (`tina-sim/src/lib.rs:205`): disproven. The anchor is captured once;
  handlers see `anchor + virtual_now` (`sim_impl.rs:316`) and restart-budget
  windows (`sim_impl.rs:1394`) compare those Instants relative to each other,
  so wall time never changes a decision. Replay determinism is pinned by
  `tina-sim`'s own `assert_replays` double-run tests.
- **Timeline output nondeterminism from `HashMap` iteration**
  (`timeline.rs:455`, unmatched-begin flush): disproven. Emission order is
  HashMap-ordered, but the final `out.sort_by_key(|e| (e.ts, e.event_id, e.order))`
  (`:247`) sorts unmatched begins by `(start.event.id(), start.order)` — both
  assigned during the deterministic event walk — so the JSON ordering is
  stable. (`order` only tiebreaks identical `(ts, event_id)`, which distinct
  unmatched begins can't share.)
- **G2 per-shard invariants over- or under-scoped**
  (`tina-sim/src/dst/invariants.rs:135-225`): verified honest. Monotonicity
  is per-shard strictly-increasing (accepts both per-shard-contiguous live
  ids and global-interleaved sim ids); cross-shard causes are accepted only
  when the cited id is unique in the merged trace, else fail closed; the
  send/call settlement checks guard the attempt id's uniqueness before
  trusting `cause ==` matches. Tests cover each branch. Residual non-bug:
  `causes_point_backward` / `event_id_is_unique` are O(n²); at the 16k-event
  capture cap that's ~10⁸ comparisons per suite run — test-side slowness
  only.
- **G5 sweep bypass**: fixed as claimed. `sweep_seeds`
  (`tina-sim/src/dst/sweep.rs:113-146`) asserts `case.seed == swept seed` and
  `history.seed() == seed`, then runs through `observe_replay_case`, which
  unconditionally panics on case/history drift and runner report-identity
  drift (`replay_case.rs:2059-2075`). `discover_constants` goes through the
  same guard (`discovery.rs:66`).
- **G3 leak_clean default**: fixed as claimed. `LoadObservation` derives
  `Default` with `leak_checked:false, leak_clean:false` (`load.rs:238-247`);
  `no_leaked_capacity_at_shutdown` fails closed when `!leak_checked`
  (`load.rs:444-451`); summary renders `leak=unchecked`. (Residue: TG-6.)
- **G1 front-door fix and its test**: verified. `snapshot_complete` /
  `compare_live_shape_complete` reject `shards().len() > 1` and
  `upstream_dropped_events != 0`; `RunCapture::finish` routes through
  `snapshot_complete`. `tina-proof-harness/tests/multishard_trace_determinism.rs`
  is honest: 20 iterations of fail-closed on a real 2-shard cross-traffic
  workload plus a 20-run single-shard stability contrast; capture uses a
  growth-stable drain before shutdown rather than a fixed sleep.
- **`check_captured_replay` Ok-path `report.expect(...)` panic**
  (`replay_case.rs:1899`): unreachable — the `Err(projection)` arm always
  pushes `CapturedReplayChange::Projection`, so `changes` is non-empty
  whenever `report` is `None`.
- **Nondeterministic shard merge order in
  `ThreadedMultiShardRuntime::trace()`**: `commands` is a
  `BTreeMap<ShardId, _>` (`threaded_multi_shard.rs:65`), so shard iteration
  and the stable id sort give a deterministic merge for a given event set.
  (The interleave is still semantically fake post-G1 — covered in TG-5.)
- **`write_saved_replay_case` torn-write risk**: `std::fs::write` is not
  atomic, but a torn file fails closed at load (header check + required-field
  checks in `read_saved_replay_case`). Not filed.
- **load.rs worker accounting races**: `ops_attempted` counts only executed
  ops (`ok+err+timeout`); the dispatch counter over-increment on the halt
  path never reaches the op; `first_error_op_index` uses the pre-increment
  dispatch ordinal; per-worker `max_consecutive` is documented as per-worker.
  Sound.

## Ranked fixes

1. TG-1 — gate `LiveReplayCaptureBuilder::finish` (multishard + lossy) like
   `RunCapture::finish`; this closes the last G1 side door.
2. TG-2 — make `complete_trace`/`TraceSnapshot` carry and refuse
   `trace_dropped`; wire `trace_for_proof` into the public wrappers.
3. TG-3 — shrinkers verify the initial failure reproduces; typed
   `DidNotReproduce`.
4. TG-4 — `decode_hex` ASCII guard.
5. TG-5 — timeline `(shard, slot_id)` span key + `(shard, id)` sort +
   id-regime metadata.
6. TG-6 — `leak_checked` in perf JSON.

## Suggested tests

- Builder gate: multishard live events into `capture_live_run(...).finish()`
  → typed error (mirror of `live_multishard_proof_snapshot_fails_closed`).
- Retention: `ThreadedRuntime` + `TraceRetention::Bounded(3)` →
  `complete_trace()` errs; `trace().is_complete()` false; timeline metadata
  carries `trace_dropped`.
- Shrink: `still_fails = |_| false` → `DidNotReproduce`, never a refreshed
  "shrunk bug".
- Fuzz: `ProtocolByteReplayCase::load` and `read_saved_replay_case` over
  arbitrary bytes — must return typed errors, never panic (catches TG-4 and
  guards the whole artifact-load surface).
- Timeline: two shards with colliding `slot_id` → two paired deferred spans,
  zero unmatched instants.

## Track coverage map

| Area | Result |
|---|---|
| live_replay.rs (LiveTrace, RunCapture, wrappers) | TG-1 (via re-exports); G1 fix verified |
| dst/replay_case.rs (capture builder, saved cases, check_captured_replay) | TG-1; saved-case format verified fail-closed |
| dst/projection.rs | TG-1 contributing (no sort/no gate by design for sim); fail-closed kind alphabet verified |
| dst/invariants.rs | G2 verified; O(n²) noted |
| dst/sweep.rs, dst/discovery.rs | G5 verified |
| dst/shrink.rs | TG-3 |
| dst/overload.rs | TG-1 (capture_overload_run); assertion helpers verified |
| dst/mod.rs (observe/check/assert, ReplayConfig) | verified; identity guards sound |
| load.rs | G3 verified; accounting verified |
| perf.rs | TG-6 |
| byte_replay.rs | TG-4; save/load/replay/shrink otherwise fail-closed |
| tina-runtime trace accessors | TG-2 |
| tina-tracing timeline.rs / observer.rs / events.rs | TG-5 (install.rs/live.rs flush excluded per brief) |
| tina-sim lib.rs/sim_impl.rs (clock, ids) | wall-clock leakage disproven |
