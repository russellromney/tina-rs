# Phase 137: Hard Shard Pinning (finish the affinity layer)

## Status

- Planned v3 (2026-05-24): v2 corrected the premise against `origin/main`
  (`a6cbaa9`); v3 folds in `Plan Review 1` (decides the `configured_core`
  meaning, names the real blast radius, specifies the readback mechanism).
- **v1's premise ("no CPU affinity anywhere") was wrong.** Main already ships the
  affinity *vocabulary, config, and reporting*; what is missing is the syscall
  that actually pins.
- **Decided (was the open question):** `configured_core = Some(n)` now **means
  "pin to core n if the platform can."** It produces `Applied` (Linux pin
  succeeded), `Unsupported` (no hard pin on this platform), or `Failed(reason)`.
  `AffinityStatus::AdvisoryOnly` is **no longer a value `configured_core`
  produces** — it is retired unless a future explicit intent-only mode is added,
  and none has been requested.
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
- `observed_core` populated from a real read-back **inside the worker thread**,
  after the pin, via `sched_getcpu()` (the running core) — pin to a *single-core*
  mask so `getcpu()` deterministically returns `n`. (`sched_getaffinity` returns
  the mask, not the running core; use `getcpu` so the test is not flaky on a
  multi-core CI box.)
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
  `AdvisoryOnly` is retired as a `configured_core` outcome (see Decided, above).
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
- **Container/quota behavior — honest proof terms:** `surrogate proof` =
  unit-test the error-mapping for an out-of-range / unavailable core →
  `Failed`/typed setup error. `missing proof` = a real cgroup-restricted env
  (hard in CI). Do not let "degrades cleanly under quotas" read as direct proof.

## How We Prove We Did Not Break Old Intent (blast-radius proof)

- Existing threaded-runtime / multi-shard suites pass with `configured_core`
  unset (default `NotRequested`), confirming zero behavior change when unused.
- **Named blast radius (the meaning change has teeth):** these sites assert the
  *old* outcome and must be migrated, with the migration itself proving the
  intent changed on purpose:
  - `tina-runtime/tests/local_system.rs:1908,1935,1936` —
    `assert_eq!(shard.affinity_status(), &AffinityStatus::AdvisoryOnly)` → become
    `Applied` on a Linux box with a pinnable core, `Unsupported` otherwise.
    Tests must branch on platform/CI capability, not hard-code `AdvisoryOnly`.
  - `tina-runtime/tests/blue_whale_checklist.rs:26` — evidence string
    "AffinityStatus reports NotRequested or AdvisoryOnly; **no OS pinning claim
    yet**" is a checked invariant asserting the opposite of this phase. Rewrite it
    to the new claim; that rewrite is proof the non-claim was lifted deliberately.
  - `tina-runtime/src/local_system.rs:853` doc ("reports `AdvisoryOnly` when set")
    updated to the new outcomes.
- A pinned run passes the same multi-shard service e2e as an unpinned run
  (pinning changes scheduling, not semantics).

## Risks / Open Decisions

- **`AdvisoryOnly` vs `Applied` — DECIDED** (see Status): `configured_core` means
  "pin if you can"; `AdvisoryOnly` retired as a `configured_core` outcome. No
  longer open.
- **macOS is decided: `Unsupported`.** No hint path. Settled.
- **Crate vs raw syscall (open).** `core_affinity` (real-pin platforms only) vs
  a thin `sched_setaffinity` wrapper. Decide in implementation.
- **Container reality.** Under k8s CPU quotas "one thread per core" is fuzzy.
  Out-of-range / throttled cores must surface as `Failed`/`Unsupported`, not a
  silent mis-pin.

## IDD Next Step

Plan v3 (Session A): Plan Review 1 folded in (semantics decided, blast radius
named, readback specified). Remaining open: crate-vs-syscall, resolved at
`Implementation Review 1`. Begin coding only on go.
