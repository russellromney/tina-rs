# Sim maturation

Last workability item from the core review: the simulator seed does not yet
drive the scheduler's logical interleaving. Today `seed` only feeds the narrow
fault knobs (local-send delay, timer-wake delay, TCP-completion reorder). The
within-round dispatch order of ready isolates is fixed to registration order,
so the seed cannot explore the one interleaving that matters most for finding
ordering bugs: which ready isolate handles its message first.

This phase has three sub-parts. Only (a) ships here; (b) and (c) are planned
follow-ups in the register.

## What we build (a: seeded scheduler perturbation)

A new seeded scheduler dimension: the run seed deterministically permutes the
order in which ready isolates are dispatched within one `step()` round.

- New `SchedulerFaultMode` in `config.rs`, a field `scheduler` on `FaultConfig`.
  Default `None` (registration order — today's behavior). `PermuteReadyOrder`
  turns on the seeded permutation.
- `step_with_remote` computes a dispatch order over the fixed base enumeration
  `0..round_messages.len()` (registration order) once per round, then visits
  ready slots in that order. `None`/seed-0/len<2 => identity `0..len`. Non-zero
  seed + `PermuteReadyOrder` => a Fisher-Yates shuffle over a counter-based
  `splitmix64` stream keyed by `(seed, shard_id, step_ordinal, swap_position)`.
- The permutation only reorders the handling of messages already popped for
  this round (visibility was decided before the loop, using `visible_at_step`).
  A send this round is `visible_at_step = step+1`, so reordering never changes
  causal delivery inside a round — only trace event-id interleaving and the
  order of side-effecting resource ops (call ids, peer output). This is exactly
  the model-preserving "local-send delivery order can shift in seeded ways"
  allowed by the simulator's established model-preservation contract.

Why gated behind a mode instead of always-on: many existing tests run non-zero
seeds and pin exact traces/behavior under the current fault-only perturbation.
Making the seed always permute dispatch order would silently rebless dozens of
golden traces — a non-negotiable violation. The mode is the seed's scheduler
dimension; it is off by default so the seed's meaning for existing runs is
unchanged, and on-demand for tests and DST that want interleaving exploration.

## What must NOT change

- Determinism: same `(seed, config, history)` => byte-identical trace. The
  permutation is a pure function of replay-stable inputs only.
- No non-determinism leak: no `HashMap` iteration in the dispatch path, no
  `Instant::now`, no unseeded rng. Only `config.seed`, `shard_id`,
  `step_ordinal`, `swap_position` feed the choice.
- Every pinned golden/replay trace and DST case: unchanged. They never set the
  new field, so they stay on `SchedulerFaultMode::None` => identity order =>
  byte-identical. No rebless. (Verified: all `.expecting(...)` cases and the
  `stable_trace_hash` assertions run default `faults`.)
- The loom surface: physical memory ordering stays loom-checked, not replayed.
  This change adds no atomics/ordering; it reorders a single-threaded loop.
- Public `tina` / `tina-runtime` API: untouched. Only `tina-sim` config grows a
  defaulted field (non-breaking, per the `SimulatorConfig` functional-update
  contract).

## How we prove both

- `scheduler_perturbation` tests:
  - same seed + `PermuteReadyOrder` reproduces a byte-identical trace across
    repeated runs (multi-isolate round).
  - seed X vs seed Y under `PermuteReadyOrder` produce different trace hashes
    (perturbation is real).
  - `SchedulerFaultMode::None` reproduces the exact registration-order trace
    (default path unchanged); an all-seeds sweep with mode off matches the
    seed-0 trace.
  - a replay artifact captured with `PermuteReadyOrder` replays byte-for-byte
    (replay still works with perturbation on).
- Break-determinism proof (required): temporarily mix wall-clock nanos into
  `scheduler_draw`, run the same-seed-same-trace test, watch it FAIL, restore.
- Full `cargo test -p tina-sim`, DST/replay/shrink suites, plus the three static
  gates (`fmt`, `clippy -D warnings`, `doc -D warnings`).

## Reblessed golden traces

None. Every pinned case stays on `SchedulerFaultMode::None`.

## Register (cut/deferred work)

| Item | Risk | When it bites | Suggested fix |
| --- | --- | --- | --- |
| (b) thread generation into the sim handler ctx | Sim handlers cannot observe their incarnation generation; a generation-sensitive bug in user code is invisible to sim. | When a workload branches on `ctx` generation and the live runtime diverges from sim. | Plumb the registered generation into the `Context` built in `step_with_remote` (the live runtime already exposes it); add a differential test that reads generation across a restart. |
| (c) sim-as-runtime-configuration | ~2k duplicated dispatch lines between `tina-sim` and `tina-runtime` drift independently; a fix landed in one can be missed in the other (the RPC dispatch bug pattern). | Every time runtime dispatch semantics change and the sim mirror is edited by hand. | Extract the shared effect-execution/dispatch core into one seam both crates configure (sim = virtual-time + scripted rails; runtime = live rails), behind a differential trace-parity gate. Large; own phase. |
| Wire `PermuteReadyOrder` into DST discovery/shrink sweeps | Perturbation exists but DST does not yet explore it by default, so scheduler-order bugs are only found when a test opts in. | When a real ordering bug lives only under a non-identity dispatch permutation. | Add a seed/scheduler axis to the DST op/seed generators and pin regressions; keep default sweeps on `None` to avoid mass rebless. |
