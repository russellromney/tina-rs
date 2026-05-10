# 069 — DST As A First-Class Dev Mode

## Status

- Done:
  - Rock 0 audit (below).
  - Rock 1: `ReplayCase<Op>`, `ReplayReport<Output>`, `ReplayConfig`,
    `ReplayReport::from_case_and_events` in `tina_sim::dst`.
    `ReplayConfig` carries the full `SimulatorConfig` plus declared
    per-isolate mailbox capacities (`mailbox(role)` panics on missing
    role) so a saved case is self-contained on every knob the runner
    needs. Hash from `stable_trace_hash`. Visible seed/config/
    mailboxes/history. Unit tests in `dst::replay_case_tests`.
  - Rock 2: `check_replay_case`, `assert_replay_case`,
    `ReplayMismatch<Op>` with actionable Display message that names
    the next decision and includes the case history. Debug-asserts
    that `case.name`/`case.seed` match `case.history`. Unit tests pin
    failure-message shape and the drift assertions. Runner bound
    relaxed to `FnMut` so stateful runners work.
  - Rock 3: `sweep_seeds` + `SweepFailure` / `SweepSuccess`. Failing
    case has refreshed expected count/hash and is replayable directly.
    Pure `make_case`. Display form is pasteable.
  - Rock 4: `shrink_replay_case` + `ShrinkReport`. Preserves
    name/seed/config/scenario/invariant; refreshes expected count/hash
    on the smaller case; honors `max_attempts`; pasteable Display
    output ends with a review step.
  - Rock 5: rewrote `docs/tina-user-guide/08-simulation-and-dst.md`
    around the build/sweep/save/shrink workflow with copyable test
    skeleton and bug-report shape.
  - Rock 6: upgraded `examples/specimen_replay_dst` to one saved
    `ReplayCase` with `Tick`/`Drain` ops where every op is
    load-bearing (deleting one changes the trace hash), runner,
    sweep demo, shrink demo, and same-seed/different-seed regression
    tests. README is pasteable. Migrated
    `remote_full_burst_is_known_edge_contract_and_replays` in
    `timmerhus_dst.rs` to the `ReplayCase` shape so the new helpers
    are the way for new DST tests, not just a parallel API.
  - Rock 7: `tina-sim/tests/saved_replay_cases.rs` pins one
    service-shaped case (`burst overflow under local-send delay`) that
    proves a real `SendRejected{ reason: Full }` mailbox-pressure fact
    with exact `full_rejections` / `accepted_sends` counts plus a
    stable event count + `stable_trace_hash`.
  - Rock 8: docs-only. The user-guide chapter now states the
    projection convention (full hash is sim-only truth; live-vs-sim
    needs an explicit projection; no projection DSL).
  - Rock 9: CHANGELOG entry under "DST As A First-Class Dev Mode";
    moved phase row from near-term roadmap to the completed phase
    index. No `examples/FINDINGS.md` changes (no new product findings
    surfaced — the missing primitive was helper shape, not simulator
    semantics).
- In progress: none.
- Open: none.
- Deferred:
  - Cancellation/deadline/pending-call cleanup — owned by 066.

### Rock 0 — Audit Result

What already exists in `tina_sim::dst`:

- `History<Op>` (name, seed, operations) with `with_operations`, `len`,
  `is_empty`. Plain data; visible to callers.
- `DstRun<Output, Artifact = ReplayArtifact>` — pair of semantic
  output + replay artifact.
- `run_twice_same_history` and `assert_replays` — run-twice replay
  check. Failure prints name, seed, history length, ops.
- `ShrinkConfig` + `delete_shrink` + `ShrunkFailure` — deletion-only
  shrinker; preserves name, seed, original.
- `InvariantSuite::standard()` plus standalone invariant checks
  (monotonic events, causes_point_backward, send/call settlement,
  no_handler_after_stop, no_untraced_abandonment).
- `persistence_image_replays`, `contains_visible_pressure`,
  `assert_projection_eq`.

What already exists outside `dst`:

- `tina_runtime::stable_trace_hash` — canonical trace fingerprint.
- `tina_sim::ReplayArtifact` / `MultiShardReplayArtifact` exposing
  `config`, `final_time`, `event_record`, `checker_failure`,
  `observed_peer_output`, `durable_image`.
- Seeded faults: `FaultConfig`, `LocalSendFaultMode::DelayByRounds`,
  `FaultMode::DelayBy`, `TcpCompletionFaultMode`,
  `ScriptedStorageFaultConfig`.
- `examples/specimen_replay_dst` already proves
  same-seed/same-hash and different-seed/different-hash with
  `stable_trace_hash`. Uses a hand-rolled `Report` struct.
- DST tests: `tina-sim/tests/dst_harness.rs`,
  `timmerhus_dst.rs`, `portable_service_dst.rs`,
  `tina-tokio-bridge/tests/bridge_model_dst.rs`.

What is duplicated:

- Every DST test recomputes a trace fingerprint with
  `stable_trace_hash(sim.trace().iter())` and asserts equality between
  two runs by hand or via `assert_replays`. There is no "bug in a box"
  case type, so saved-seed tests reinvent shape per file.
- Examples and tests reimplement "did the property still hold"
  predicates for shrinking by re-running the workload.
- `specimen_replay_dst` rolls its own `Report` instead of speaking the
  shared `ReplayCase`/`ReplayReport` shape.

What must stay raw:

- `SimulatorConfig`, `FaultConfig`, mailbox capacities, `History`
  operations, and seeds. The "no helper hides seed/config/history"
  rule applies to anything new. `ReplayConfig` (this phase) is a
  visible struct, not a builder.
- `stable_trace_hash` is the only fingerprint. No debug-string hashing.
- Live-vs-sim comparison stays an explicit projection (already what
  `timmerhus_dst.rs` and `portable_service_dst.rs` do). No projection
  DSL.

Implications for rocks 1+:

- Rock 1 introduces `ReplayCase`, `ReplayReport`, and `ReplayConfig`.
  `ReplayConfig` carries the simulator-replay knobs needed to redo a
  story when a bare `History` is not enough. Today the existing tests
  encode all knobs inside their runner closure; for the saved-case
  workflow the knobs need to be visible data on the case.
- Rock 2 adds `check_replay_case` / `assert_replay_case`. These must
  print actionable failures, not just numbers.
- Rock 3 (sweep) and Rock 4 (shrink) operate on `ReplayCase` so a
  failing sweep yields a pasteable case and a shrunk case carries
  refreshed expected count/hash.

## Goal

Make DST foundational for Tina users who need it.

Normal serious-service workflow:

```text
write service logic once
run it live
run it in sim
sweep seeds
save bad seed
shrink history
commit replay case
```

Grug truth:

```text
same seed, same story
saved seed, saved bug
seed alone is not replay
seed + config + history + expected trace shape is replay
```

Sim proves state-machine interleavings. Live proves physics.

User-experience rule: the user should not feel like they are using a
simulator API. They should feel like they are putting a bug in a box.

Coding-agent rule: everything needed to replay must be visible as boring
Rust data. Agents copy explicit structs and loud helper names better than
hidden builders.

## Boundaries

- Home: shared DST helpers live in `tina_sim::dst`.
- No new crate.
- No generic property-test framework.
- No hidden randomness.
- No claim that live runtime traces replay byte-for-byte.
- No cancellation/deadline semantics. 066 owns that.
- If a replay case wants cancellation, use a domain `Stop` message or
  defer it until 066 lands.

## Rock 0 — Audit First

Read current DST surfaces:

- `tina-sim/src/dst.rs`
- `Simulator::run_until_quiescent`
- `ReplayArtifact`
- `stable_trace_hash`
- `docs/tina-user-guide/08-simulation-and-dst.md`
- `examples/specimen_replay_dst`
- saved-seed tests in `tina-sim/tests`

Update this status block with:

- what already exists;
- what is duplicated;
- what must stay raw.

No code before this audit.

## Rock 1 — ReplayCase And ReplayReport

Add first-class test-support types in `tina_sim::dst`:

```rust
pub struct ReplayCase<Op> {
    pub name: &'static str,
    pub seed: u64,
    pub config: ReplayConfig,
    pub scenario: &'static str,
    pub history: History<Op>,
    pub expected_event_count: usize,
    pub expected_trace_hash: u64,
    pub invariant: &'static str,
}

pub struct ReplayReport<O> {
    pub name: &'static str,
    pub seed: u64,
    pub config: ReplayConfig,
    pub scenario: &'static str,
    pub event_count: usize,
    pub trace_hash: u64,
    pub output: O,
}
```

Shape may change. Meaning must not.

Rules:

- `ReplayCase` is "bug in a box."
- `ReplayReport` is what the runner observed.
- `ReplayConfig` is visible, boring data. It contains simulator/runtime
  knobs needed to replay the story. If the history type already carries
  all config, say so explicitly and prove it.
- `scenario` and `invariant` are human-facing. They should read well in
  a test failure and a bug report.
- the case must be inspectable without running code: seed, scenario,
  config, history, expected count/hash, and invariant are visible data.
- trace hash uses `stable_trace_hash`, never debug strings.
- hash is a conscious test pin, not a semver-stable external format.

Proof:

- same case repeats;
- different seeded fault case can differ;
- changing visible config changes the report or invalidates the case
  deliberately;
- failure output names case, seed, scenario, count, hash.

## Rock 2 — Assert Case

Add the lower-level check helper and the blessed regression wrapper:

```rust
check_replay_case(&CASE, run_case) -> Result<ReplayReport<Output>, ReplayMismatch>
assert_replay_case(&CASE, run_case);
```

Where `run_case` is a normal function:

```rust
fn run_case(case: &ReplayCase<Op>) -> ReplayReport<Output>
```

`check_replay_case` checks:

- expected event count;
- expected trace hash;
- optionally semantic output if caller asserts it.

`assert_replay_case` is thin test sugar over `check_replay_case`.

Failure must say:

- case name;
- seed;
- config summary;
- scenario;
- invariant;
- actual vs expected count/hash;
- "bump constants only after deciding whether behavior changed or only
  trace vocabulary/order changed."

The failure message should be useful to a coding agent with no extra
context. It should name the next decision, not just print numbers.

## Rock 3 — Seed Sweep

Add a deterministic seed-sweep helper:

```rust
sweep_seeds(name, seeds, make_case, run_case, check_report)
```

This is not QuickCheck.

Rules:

- operation history is explicit;
- scenario/config is explicit;
- `make_case(seed)` is pure and deterministic;
- every generated operation is materialized into `ReplayCase.history`
  before the simulator runs;
- first failing case is returned as a `ReplayCase`;
- helper can print or format the failing case in a pasteable form;
- no hidden random generator;
- suitable for ignored/local tests, not default CI unless tiny.

Proof:

- all-good sweep returns success;
- failing sweep returns the first failing seed;
- two calls to `make_case(seed)` produce the same visible case before
  running the simulator;
- returned case can be replayed by `assert_replay_case`.

## Rock 4 — Shrink ReplayCase

Make shrinking operate on `ReplayCase`, not bare `History`.

Workflow:

```text
sweep finds failing ReplayCase
shrink ReplayCase.history
save smaller ReplayCase
assert smaller ReplayCase forever
```

Rules:

- operation list stays visible;
- seed, name, and scenario are preserved;
- shrunk history is ordinary Rust data, not opaque bytes;
- output records original len, shrunk len, attempts, reason;
- shrink returns a new `ReplayCase` or a `ShrinkReport` containing
  freshly observed event count and trace hash for the smaller case;
- output includes a review step: paste the new constants only after
  deciding the smaller case preserves the intended bug/invariant;
- no QuickCheck clone.

Proof:

- failing case shrinks;
- shrunk case still fails;
- shrunk case constants are refreshed, not inherited from the larger
  case;
- max-attempt cap is honored.

## Rock 5 — User Guide

Rewrite `docs/tina-user-guide/08-simulation-and-dst.md` around the real
workflow:

1. build explicit history;
2. run same case twice;
3. sweep seeds locally;
4. save bad seed as `ReplayCase`;
5. shrink history;
6. commit saved case;
7. use live tests for physics.

Keep it grug. Include one copyable test skeleton:

```rust
#[test]
fn saved_seed_replays_bug() {
    assert_replay_case(&BUG_CASE, run_case);
}

#[test]
#[ignore]
fn seed_sweep() {
    // local search, not every PR
}
```

Also include a copyable bug-report shape:

```text
Replay case:
- name:
- seed:
- config:
- scenario:
- history len:
- expected events:
- expected hash:
- invariant:
- command:
```

## Rock 6 — Specimen Replay Tutorial

Upgrade `examples/specimen_replay_dst` from "same seed same hash" to the
full workflow:

- define one `ReplayCase`;
- run and assert it;
- show a tiny seed sweep;
- show a shrink step if small enough;
- README section: "copy this into your bug report."
- README should show the `ReplayCase` as readable data, not hide it
  behind a builder.

Do not turn the example into a shared harness.

## Rock 7 — One Service-Shaped Saved Case

Add one production-shaped replay proof.

Preferred shapes:

- timer retry/backoff interleaving;
- sharded fanout partial aggregate;
- mailbox full under seeded local-send delay.

Avoid cancellation/deadline semantics while 066 is active.

Required:

- saved `ReplayCase`;
- pinned event count/hash;
- test includes a real Tina pressure/lifecycle fact such as `Full`,
  `Closed`, bounded mailbox admission, retry timer ordering, or partial
  aggregate outcome;
- test explains that pressure/lifecycle fact through `invariant`;
- same case replays;
- different seed is non-trivial if the chosen scenario supports it.

## Rock 8 — Projection Convention

Docs only unless two tests need identical code.

Rules:

- full trace hash is simulator-only truth;
- live-vs-sim comparison needs explicit projection;
- projection keeps event kind and terminal outcome visible;
- do not build a projection DSL.

## Rock 9 — Paperwork

Update:

- `examples/FINDINGS.md` only for new product findings;
- roadmap/changelog only for user-facing helpers or precise missing sim
  primitive.

No artifact noise.

## Done Means

- `ReplayCase` exists and is used.
- Saved-seed assert helper exists.
- Seed sweep can return a replayable failing case.
- Shrink workflow works on a `ReplayCase`.
- `specimen_replay_dst` teaches the full loop.
- One service-shaped saved case is pinned.
- Docs say sim proves interleavings; live proves physics.

## Hostile Review

- Did helper code hide seed/config/history?
- Is the config visible enough for the case to move between machines?
- Is the replay case pasteable into a bug report?
- Would a coding agent know what to update after a hash mismatch?
- Did any test hash debug strings?
- Did seed sweep invent hidden randomness?
- Does `make_case(seed)` produce the same visible case twice?
- Did any docs claim live replay?
- Did saved constants pin real trace shape?
- Did shrinking keep operations visible?
- Did shrinking refresh constants for the smaller case?
- Does the service-shaped proof include real Tina pressure/lifecycle?
- Did this make DST foundational, or merely prettier?
