# Phase 137: Hard Shard Pinning (finish the affinity layer)

## Status

- Planned v4 (2026-05-25): v2 corrected the premise against `origin/main`
  (`a6cbaa9`); v3 folds in `Plan Review 1` (decides the `configured_core`
  meaning, names the real blast radius, specifies the readback mechanism). v4
  clarifies that `configured_core` is an OS CPU id selected from the process's
  allowed affinity mask, not `0..num_cpus`.
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
on Linux I set configured_core = an OS CPU id in this process's allowed affinity
mask, and the shard worker is actually pinned there (observed_core == that id,
affinity_status == Applied); on macOS the same request reports Unsupported
instead of pretending; helper lanes stay unpinned
```

## Includes

- A hard-pin step inside the shard worker thread body (`threaded.rs:311`,
  `threaded_multi_shard.rs:264`): if `configured_core` is `Some(core)`, call
  the platform pin **from within the new thread** before the loop starts.
  - **Linux:** use the existing Unix `libc` dependency to call
    `sched_getaffinity`, `sched_setaffinity`, and `sched_getcpu` directly. Do
    not add `core_affinity` or another crate for this. Treat `configured_core`
    as an OS CPU id, not "nth available core." On success set
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
- Available-core validation: read the process's allowed affinity mask on Linux
  and reject a `configured_core` not present in that mask. Do not use
  `0..num_cpus` as proof; containers and cpusets can expose sparse allowed CPU
  ids. Unsupported platforms report `Unsupported` rather than validating against
  a fake mask.

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

- **Linux pin works:** choose `N` from the process's allowed affinity mask, set
  `configured_core = N`, and assert the shard worker reports
  `affinity_status == Applied` and `observed_core == N` (read back inside the
  thread). This is the headline proof.
- **`configured_core == None`** is byte-identical to today: `NotRequested`, no
  affinity syscall (assert the no-pin branch).
- **macOS / unsupported:** `configured_core = N` reports `Unsupported`, runs
  unpinned, does not error the runtime.
- **Unavailable core** (not in the allowed affinity mask) fails loudly at setup
  with a typed error.
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
    `Applied` on Linux when the requested OS CPU id is in the process affinity
    mask, `Unsupported` otherwise. Tests must branch on platform/CI capability
    and pick from the allowed mask, not hard-code `AdvisoryOnly` or assume CPU
    `0` is available.
  - `tina-runtime/tests/blue_whale_checklist.rs:26` — evidence string
    "AffinityStatus reports NotRequested or AdvisoryOnly; **no OS pinning claim
    yet**" is a checked invariant asserting the opposite of this phase. Rewrite it
    to the new claim; that rewrite is proof the non-claim was lifted deliberately.
  - `tina-runtime/src/local_system.rs:853` doc ("reports `AdvisoryOnly` when set")
    updated to the new outcomes.
- A pinned run passes the same multi-shard service e2e as an unpinned run
  (pinning changes scheduling, not semantics).

## Risks / Decisions

- **`AdvisoryOnly` vs `Applied` — DECIDED** (see Status): `configured_core` means
  "pin if you can"; `AdvisoryOnly` retired as a `configured_core` outcome. No
  longer open.
- **macOS is decided: `Unsupported`.** No hint path. Settled.
- **Linux backend is decided:** use a tiny `libc` wrapper around
  `sched_getaffinity` / `sched_setaffinity` / `sched_getcpu`. No new dependency.
- **Container reality.** Under k8s CPU quotas and cpusets, "one thread per core"
  is fuzzy and allowed CPU ids may be sparse. Pin only to ids in the allowed
  mask; unavailable / throttled cores must surface as a typed setup error,
  `Failed(reason)`, or `Unsupported`, not a silent mis-pin.

## IDD Next Step

Plan v4 (Session A): Plan Review 1 folded in (semantics decided, blast radius
named, readback specified, Linux backend pinned to `libc`). Begin coding only on
go.
