# Phase 137: Hard Shard Pinning (finish the affinity layer)

## Status

- Planned (2026-05-24), v2 after deep-dive against `origin/main` (`a6cbaa9`).
  **The v1 premise ("no CPU affinity anywhere") was wrong** — it was written
  against a stale branch. Main already ships the affinity *vocabulary,
  config, and reporting*; what is missing is the syscall that actually pins.
- This phase finishes that layer: make a requested core produce a real OS pin
  on Linux, reported as `AffinityStatus::Applied`, instead of only advisory
  intent.
- Low effort, high leverage. Pairs naturally after Phase 136 (TLS off its
  per-operation threads), which lowers the per-shard thread count.

## Starting Facts (verified on `origin/main` a6cbaa9)

- The affinity layer **already exists** — do not reinvent it:
  - `tina-runtime/src/threaded.rs:73-78`: `configured_core: Option<usize>` —
    "Desired OS core for this shard worker… does not hard-pin the worker
    without a platform-specific affinity [backend]." Default `None`
    (`threaded.rs:107`).
  - `tina-runtime/src/live_report.rs:244-258`: `AffinityStatus { NotRequested,
    Applied, Unsupported, Failed(String), AdvisoryOnly }`. `Applied` is
    documented as "the backend proved hard affinity was applied."
  - `LiveShardReport` carries `configured_core`, `observed_core`, and
    `affinity_status` (`live_report.rs`).
  - Today, setting `configured_core` yields `AffinityStatus::AdvisoryOnly`
    (`live_report.rs:167-170`) — ownership intent, **no OS scheduling control**.
    Nothing ever produces `Applied`.
- Shard worker threads spawn at `threaded.rs:311` and
  `threaded_multi_shard.rs:264` via `thread::Builder`. The pin must be applied
  **inside the spawned thread**, before it runs its loop.

## Purpose

Make `configured_core` real on platforms that can pin, and honest everywhere
else — so a requested core is either an OS-proven `Applied` pin or a typed
`Unsupported`/`Failed`, never `AdvisoryOnly` masquerading as control.

```text
on Linux I set configured_core = N and the shard worker is actually pinned to
core N (observed_core == N, affinity_status == Applied); on macOS the same
request reports Unsupported instead of pretending; helper lanes stay unpinned
```

## Includes

- A hard-pin step inside the shard worker thread body (`threaded.rs:311`,
  `threaded_multi_shard.rs:264`): if `configured_core` is `Some(core)`, call
  the platform pin **from within the new thread** before the loop starts.
  - **Linux:** `sched_setaffinity` (via a vetted `core_affinity`-style dep
    restricted to real-pin platforms, or a thin `libc` wrapper). On success set
    `AffinityStatus::Applied` and record `observed_core`. On failure set
    `Failed(reason)` and keep running unpinned.
  - **macOS / any platform without a hard pin:** set
    `AffinityStatus::Unsupported`. **No best-effort hint path.** Darwin offers
    only affinity *hints*; we do not dress a hint up as a pin.
- `observed_core` populated from a real read-back of the running thread's core
  where the platform supports it (this is the proof hook).
- **Helper-lane threads stay unpinned** (DNS, storage, process, unix, and TLS
  workers until Phase 136 retires them). They float onto spare cores. Stated
  rule, not an accident.
- Available-core validation: a `configured_core` past the core count fails
  loudly at setup (typed error), not a silent mis-pin.

## Reuse, Do Not Reinvent

- **Use the existing `configured_core` config.** Do **not** add a new
  `PinPolicy` enum (the v1 plan's mistake). The knob already exists per shard.
- **Use the existing `AffinityStatus` variants.** `Applied` / `Unsupported` /
  `Failed` were added for exactly this; this phase makes them reachable.
  `AdvisoryOnly` stays for callers who want intent-only reporting without a pin.
- **Use the existing `LiveShardReport` fields** (`configured_core`,
  `observed_core`, `affinity_status`). No new report surface.

## Does Not Include

- A new config enum or a second affinity surface.
- Pinning helper lanes.
- macOS best-effort hints (reports `Unsupported`).
- NUMA memory placement / first-touch tuning.
- Any change to explicit-step or simulator runtimes (live-substrate only; no
  semantic effect).
- A throughput claim (mechanism + honest reporting only; a benchmark phase owns
  performance claims).

## How We Prove The New Behavior (direct proof)

- **Linux pin works:** with `configured_core = N`, the shard worker reports
  `affinity_status == Applied` and `observed_core == N` (read back inside the
  thread). This is the headline proof.
- **`configured_core == None`** is byte-identical to today: `NotRequested`, no
  affinity syscall (assert the no-pin branch).
- **macOS / unsupported:** `configured_core = N` reports `Unsupported`, runs
  unpinned, does not error the runtime.
- **Out-of-range core** fails loudly at setup with a typed error.
- **`Failed(reason)`** path: a forced-failure pin reports `Failed` and the shard
  keeps running unpinned (pinning is best-effort-correctness, not fatal).

## How We Prove We Did Not Break Old Intent (blast-radius proof)

- Existing threaded-runtime / multi-shard suites pass with `configured_core`
  unset (default), confirming zero behavior change when unused.
- Existing `AffinityStatus::AdvisoryOnly` reporting still works for callers that
  set a core but do not want a hard pin (if that distinction is kept) — or the
  advisory path is explicitly migrated and that migration is proven.
- A pinned run passes the same multi-shard service e2e as an unpinned run
  (pinning changes scheduling, not semantics).

## Risks / Open Decisions

- **`AdvisoryOnly` vs `Applied` semantics.** Decide whether `configured_core`
  now *always* attempts a hard pin (→ `Applied`/`Unsupported`/`Failed`), or
  whether a separate opt-in distinguishes "advisory intent" from "please pin."
  Lean: `configured_core` means "pin if you can," and `AdvisoryOnly` is retired
  or kept only for an explicit intent-only mode. Resolve in `Plan Review 1`.
- **macOS is decided: `Unsupported`.** No hint path. Settled.
- **Crate vs raw syscall (open).** `core_affinity` (real-pin platforms only) vs
  a thin `sched_setaffinity` wrapper. Decide in implementation.
- **Container reality.** Under k8s CPU quotas "one thread per core" is fuzzy.
  Out-of-range / throttled cores must surface as `Failed`/`Unsupported`, not a
  silent mis-pin.

## IDD Next Step

Plan v2 (Session A), corrected against main. Next: `Plan Review 1` in
`.intent/phases/137-optional-shard-pinning/review.md` (resolve the
`AdvisoryOnly`-vs-`Applied` semantics and crate-vs-syscall) before any code.
