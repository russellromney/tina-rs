# Phase 137: Optional Shard Pinning

## Status

- Planned (2026-05-24). Not started.
- Authored after a thread-per-core review found **no CPU affinity anywhere** in
  the workspace: shard worker threads float across cores at the OS scheduler's
  whim, eroding the cache/NUMA locality that is the point of thread-per-core.
- Low effort, high leverage. Pairs naturally after Phase 136 (TLS off its lane),
  which lowers the per-shard thread count and makes pinning cleaner.

## Starting Facts

- `grep` for `affinity` / `sched_setaffinity` / `core_affinity` across the
  workspace (excluding the vendored substrate) returns **nothing**.
- Shard worker threads are spawned plainly at `threaded.rs:311` and
  `threaded_multi_shard.rs:264` via `thread::Builder`, with no pinning.
- SYSTEM.md already states hard OS thread pinning is **not claimed**. This phase
  makes it an opt-in capability, not a silent default.
- Per shard today there is one shard thread plus several mostly-idle helper-lane
  threads (DNS, storage, process, unix, and — until Phase 136 — TLS). Pinning the
  shard thread while leaving the helper threads free is the intended shape.

## Purpose

Let an operator pin shard worker threads to cores, so the hot per-shard work
keeps its caches warm and stays NUMA-local, without forcing it on anyone or
lying under container CPU limits.

```text
on a box where I own the cores, I can pin shard N to core N and keep the cold
helper lanes floating, and on a box where I don't (k8s CPU quota), pinning
degrades to off instead of pinning to cores I've been throttled off of
```

## Includes

- A `PinPolicy` config on the threaded multi-shard runtime:
  - `Off` (default — current behavior, byte-for-byte unchanged),
  - `Sequential` (shard *i* → core *i*),
  - `Cores(Vec<usize>)` (shard *i* → the *i*-th core in the provided list).
- Pinning applied at shard-thread spawn (`threaded.rs:311`,
  `threaded_multi_shard.rs:264`) **only on platforms where the OS performs a real
  hard pin** (Linux `sched_setaffinity`). We offer "hard pinning" only where it is
  hard. Prefer a vetted dependency (`core_affinity`) restricted to platforms with
  real support; a thin `libc` wrapper is acceptable if the crate's guarantees are
  fuzzy.
- **Helper-lane threads are explicitly NOT pinned.** They float onto spare cores
  so they soak idle capacity rather than fighting a shard for its core. This is a
  stated rule, not an accident.
- Available-core detection so `Sequential` over-provisioned past the core count,
  or `Cores([...])` naming an absent core, **fails loudly at setup** rather than
  pinning wrong or silently.
- Capability/topology report: the live topology snapshot names whether shards are
  pinned and to which core (advisory ownership already exists; extend it to carry
  real pin state, distinct from advisory).

## Does Not Include

- Pinning helper lanes (intentional — see above).
- **Best-effort macOS affinity hints.** macOS only offers thread-affinity
  *hints*, not a hard pin. We do **not** ship a hint path dressed up as pinning.
  On macOS (and any platform without a real hard pin) the capability reports
  `NotClaimed` and `PinPolicy` other than `Off` returns a typed unsupported
  result at setup. "Hard pinning" means the OS actually pins, or we don't claim
  it.
- NUMA-aware memory placement / first-touch allocation tuning. Out of scope; a
  later phase if a workload proves it matters.
- Any change to the explicit-step or simulator runtimes — pinning is a live-
  substrate concern only and has no semantic effect.
- A claim that pinning improves throughput. This phase ships the *mechanism* and
  honest reporting; a benchmark phase owns any performance claim.

## How We Prove The New Behavior (direct proof)

- `PinPolicy::Off` produces byte-identical behavior to today (no affinity call
  made) — proved by a test asserting the spawn path takes the no-pin branch.
- `Sequential` / `Cores` actually pin: a live test reads back the calling
  thread's affinity mask inside the shard worker and asserts it matches the
  requested core.
- Over-provision / absent-core configs return a typed setup error, not a panic
  and not a silent mis-pin.
- Topology report reflects the pin state.

## How We Prove We Did Not Break Old Intent (blast-radius proof)

- Full existing threaded-runtime and multi-shard suites pass with
  `PinPolicy::Off` (default), confirming zero behavior change when the feature is
  unused.
- A run with pinning enabled passes the same multi-shard service e2e as the
  unpinned run (pinning changes scheduling, not semantics).
- On a platform without affinity support (or under a restrictive cgroup), setup
  degrades to a typed unsupported/off result — proved by a guarded test or an
  honest `NotClaimed`-style capability.

## Risks / Open Decisions

- **macOS is decided: `NotClaimed`.** No hint path. Hard pinning ships only where
  the OS hard-pins (Linux). This is settled, not an open question.
- **Crate vs raw syscall (open).** `core_affinity` (restricted to real-pin
  platforms) vs a thin `sched_setaffinity` wrapper. Decide in implementation;
  prefer the vetted crate if its Linux guarantee is solid.
- **Container reality.** Under k8s CPU quotas "one thread per core" is fuzzy.
  Default `Off` and loud-on-misconfig keep us honest; do not auto-pin.
- **Interaction with helper lanes.** Keep them unpinned. If a future workload
  shows a helper lane starving, that is a separate decision, not a reason to pin
  everything.

## IDD Next Step

Plan only (Session A). Next: `Plan Review 1` in
`.intent/phases/137-optional-shard-pinning/review.md` before any code.
Open scope questions for that review are flagged inline above — resolve the
crate-vs-syscall and macOS-support questions there.
