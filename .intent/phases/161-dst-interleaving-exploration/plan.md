# DST interleaving exploration

Phase 160 (sim maturation, sub-part a) gave the simulator a seeded scheduler
dimension: `SchedulerFaultMode::PermuteReadyOrder` makes the run seed permute the
within-round dispatch order of ready isolates. It shipped default-off and is
exercised only in `scheduler_perturbation.rs`. PR #273 then proved the axis is
useful: two isolates doing non-commutative updates to one cell reach a state
(`8`) under some interleavings that registration order (`7`) never reaches.

This phase makes that axis a first-class part of DST. Today the
invariant-checking DST cases check safety on a *single* interleaving
(registration order). This phase runs them across the interleaving space.

## Current state (already exists — do NOT rebuild)

- Real DST harness in `tina-sim/src/dst/`: authored `ReplayCase<Op>` (operations
  + seed) → runner → events. Golden pinning via `.expecting(count, trace_hash)`;
  `assert_replays` for byte-identical determinism; `shrink.rs` for failure
  minimization; `sweep_seeds` (run a case across N seeds, emit the first failure
  as a pasteable regression) in `sweep.rs`.
- `InvariantSuite` (`dst/invariants.rs`): real safety checks over the event
  stream — `events_are_monotonic`, `causes_point_backward`, `send_attempts_settle`,
  `call_attempts_settle`, `no_handler_after_stop`, `no_untraced_abandonment`,
  `persistence_image_replays`. Already used across `dst_randomized`,
  `portable_service_dst`, `timmerhus_dst`, `io_simulation`,
  `persistence_simulation`, `dst_harness`.
- The seeded axis: `SchedulerFaultMode::PermuteReadyOrder` on `FaultConfig`
  (`config.rs`).

The gap: the invariant-checking sweeps all run at `mode = None` (one interleaving
per history). `PermuteReadyOrder` is never combined with the invariant suite, and
`sweep_seeds` is barely used.

## What we build

An interleaving-exploration layer over the existing DST cases.

- A helper `explore_interleavings(case, seeds, invariant_suite, runner)` (in
  `dst/`, alongside `sweep_seeds` in `sweep.rs` or a sibling) that:
  1. For each seed in a bounded set, runs `case` with
     `FaultConfig { scheduler: PermuteReadyOrder, ..case.faults }`.
  2. Asserts the `InvariantSuite` holds on the resulting events — **NOT** a
     golden trace hash. Hashes legitimately differ per interleaving; that is the
     point.
  3. Asserts each seed is internally deterministic (same seed → byte-identical
     events across two runs), so a discovered failure is reproducible.
  4. On the first invariant violation, shrinks the history via `shrink.rs` and
     emits a pasteable regression (seed + minimized ops + violated invariant),
     mirroring `SweepFailure`.
- Wire it over the existing invariant-checked case sets (`dst_randomized`,
  `portable_service_dst`, `timmerhus_dst`, and any suite that already builds an
  `InvariantSuite`). Each gets an additional exploration test running its cases
  across the perturbation axis.

## What must NOT change

- Every pinned golden/replay trace and `.expecting(...)` hash stays byte-
  identical. Golden/replay tests keep running at `mode = None`. The exploration
  layer is additive and asserts invariants, never a hash. A changed golden hash
  is a bug.
- Per-seed determinism under `PermuteReadyOrder`: same seed → byte-identical
  across runs (this is what makes a discovered violation reproducible and
  shrinkable). No `HashMap` iteration / `Instant::now` / unseeded rng in the
  explored path (already true from phase 160 — do not regress).
- Model preservation: the axis only reorders handling of already-
  popped messages; causal delivery within a round is unchanged. Exploration walks
  existing legal interleavings; it must not invent new causal edges.
- The axis stays default-off. Only the new exploration tests opt into
  `PermuteReadyOrder`; `FaultConfig::default().scheduler` stays `None`.

## How we prove both

- **The layer FINDS real ordering bugs (the whole point).** Add a deliberately
  order-sensitive test service whose invariant holds under registration order but
  is violated under some interleaving. The sweep must FIND the violating seed and
  the shrinker must minimize it to a pasteable regression. Then disable the
  shuffle (`mode = None`) → the sweep finds nothing and the "must find a
  violation" assertion fails. That is the prove-the-test-catches-the-bug gate:
  mechanism on → catches the injected bug; mechanism off → does not.
- **Existing golden traces unchanged.** Full `cargo test -p tina-sim` green; every
  `.expecting(...)` / `stable_trace_hash` assertion byte-identical. Zero rebless.
- **Reproducibility.** A discovered violation's seed reproduces byte-identically
  on re-run, and the shrunk case still violates.
- **Real services stay clean.** Running the existing case sets across the axis
  produces NO invariant violation on current code. If one DOES fire, that is a
  real bug — surface it and record the regression seed; do not suppress.

## Traps (greppable — wrong vs right)

- Golden hash under perturbation. Wrong: `.expecting(count, hash)` with
  `PermuteReadyOrder` → false failures as hashes differ per interleaving. Right:
  assert the `InvariantSuite`.
- Flipping the default. Wrong: changing `FaultConfig::default().scheduler`. Right:
  the new exploration tests set the mode explicitly; everything else stays `None`.
- Inventing randomness. Wrong: any `HashMap` iteration / `Instant::now` / unseeded
  rng in the sweep or runner. Right: the order stays a pure function of the seed
  (phase 160's `splitmix64` over `(seed, shard, step_ordinal, swap_position)`).
- Silent truncation. Bound the sweep to a fixed, documented seed count (e.g.
  64–256) and report how many seeds were examined, so a "0 violations" result is
  honest coverage, not a hidden cap.

## Files to read first

- `tina-sim/src/dst/{mod.rs,sweep.rs,invariants.rs,shrink.rs,discovery.rs}`
- `tina-sim/tests/{dst_randomized.rs,portable_service_dst.rs,timmerhus_dst.rs}`
  and `scheduler_perturbation.rs` — especially #273's
  `permute_axis_discovers_ordering_sensitive_outcome_and_reproduces_it`, the
  proven axis-usefulness pattern to generalize.
- `tina-sim/src/config.rs` (`SchedulerFaultMode`, `FaultConfig`)
- `.intent/phases/160-sim-maturation/plan.md` (the axis this builds on)
- `docs/tina-user-guide/08-simulation-and-dst.md` (the current determinism,
  replay, and model-preservation contract)

## What not to touch

- The runtime dispatch path (`tina-runtime`) — this is sim-only.
- The golden/replay tests' expected hashes.
- Phase 160 sub-parts (b) generation threading and (c) sim-as-runtime-config.

## Out of scope / register

- Sim-as-runtime-configuration (phase 160 sub-part c): the ~2k-line dispatch dedup
  between tina-sim and tina-runtime. Separate phase.
- Generation threading into the sim handler ctx (phase 160 sub-part b): separate.
- If exploration finds a real invariant violation in an existing service, that
  fix is its own follow-up: record the regression seed, open a finding, do not
  bundle the fix into this phase.

## Commands (worktree, shared cache)

```sh
export RUSTC_WRAPPER=sccache
export CARGO_TARGET_DIR=<main-checkout>/target
cargo test -p tina-sim --offline --locked
cargo fmt -p tina-sim --check
cargo clippy -p tina-sim --all-targets --offline --locked -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc -p tina-sim --no-deps --offline --locked
```

Destructive actions: work on a branch in its own worktree, open a PR, do NOT
merge. Self-review against this plan before the PR: confirm the injected
order-sensitive bug is caught with the mechanism on and missed with it off, and
that zero golden traces reblessed.
