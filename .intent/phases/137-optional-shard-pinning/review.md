# Phase 137 Review (append-only)

## Plan Review 1 — hostile (2026-05-24)

Verdict: correctly rebuilt on the existing affinity layer (good — v1's
"invent PinPolicy" was wrong). But the central intent question is parked in
Open Decisions, and the blast radius is concrete and under-named.

### Finding 1 (blocking) — the load-bearing intent decision is deferred

IDD: a plan that cannot answer "what are we building" is not ready. The plan
leaves open whether `configured_core` now *always* attempts a hard pin
(→ `Applied`/`Unsupported`/`Failed`) or whether a separate opt-in distinguishes
"advisory intent" from "please pin." That single choice **is** the phase. Decide
it here. Recommendation: `configured_core` means "pin if the platform can," and
`AdvisoryOnly` is retired as a status that `configured_core` can produce (kept
only if an explicit intent-only mode is added — and no caller has asked for one).

### Finding 2 (blocking) — the blast radius is real and named in code; the plan hand-waves it

Changing what `configured_core` produces **breaks existing asserted intent**:

- `tina-runtime/tests/local_system.rs:1908,1935,1936`:
  `assert_eq!(shard.affinity_status(), &AffinityStatus::AdvisoryOnly)`.
- `tina-runtime/tests/blue_whale_checklist.rs:26`: evidence string
  "AffinityStatus reports NotRequested or AdvisoryOnly; **no OS pinning claim
  yet**" — this is a *checked invariant asserting the opposite of this phase*.
- `tina-runtime/src/local_system.rs:853` doc: "reports show `AdvisoryOnly` when
  this is set."

**Required plan change:** the blast-radius section must list these sites and
state the migration: on Linux these become `Applied` (or `Unsupported` in CI
without a pinnable core); the blue_whale checklist line must be rewritten from
"no OS pinning claim yet" to the new claim, and that rewrite is itself proof the
intent changed on purpose. A blanket "if the distinction is kept… or migration is
proven" is not enough — the distinction is *not* currently kept by any opt-in, so
the meaning change is unavoidable and must be owned.

### Finding 3 — `observed_core` proof mechanism is unspecified

"reads back the calling thread's affinity and asserts it matches" — `sched_getcpu`
(current core, can drift) vs `sched_getaffinity` (the mask). For a single-core
pin both should agree; name which call the test uses and assert against a
single-core mask so the test is not flaky on a multi-core CI box.

### Finding 4 — the cgroup/quota claim has no proof, only a risk note

"Out-of-range / throttled cores surface as Failed/Unsupported." There is no test
for the real cgroup-restricted case (hard in CI). Mark it honestly:
`surrogate proof` (unit-test the error-mapping for an out-of-range core) +
`missing proof` (real cgroup env). Do not let "degrades cleanly under quotas"
read as a proven claim.

### Keep

macOS = `Unsupported` (no hint path), helper lanes stay unpinned, reuse of the
existing config/enum/report. All correct.

## Plan Review 2 — second reviewer (2026-05-25)

Verdict: still right, but core numbering needed to be made OS-real.

### Finding 1 — `configured_core` cannot mean `0..num_cpus`

Containers and cpusets can expose sparse allowed CPU ids. Pinning to CPU 0 may
be invalid even when the process has several cores available. Fixed in plan v4:
`configured_core` is an OS CPU id and Linux validation reads the process's
allowed affinity mask. Tests choose from that mask rather than assuming CPU 0.

## Plan Review 3 — implementation-choice cleanup (2026-05-25)

Verdict: one needless implementation fork remained.

### Finding 1 — do not leave crate-vs-syscall as planning work

`tina-runtime` already has a Unix `libc` dependency for process handling, and
the needed calls are exactly Linux syscalls (`sched_getaffinity`,
`sched_setaffinity`, `sched_getcpu`). Fixed in plan v4: use a tiny local `libc`
wrapper, no new affinity crate.

## Implementation (2026-05-25)

Built exactly to plan v4. No new crate, no new config enum, no new report
surface.

- New `tina-runtime/src/affinity.rs`: `apply(Option<usize>)` runs inside the
  worker thread. Linux reads the allowed mask (`sched_getaffinity`), validates
  the requested OS CPU id against it (pure `validate_core`, unit-tested as the
  surrogate proof), pins to a single-core mask (`sched_setaffinity`), and proves
  it by reading the running core back (`sched_getcpu`). Non-Linux returns
  `Unsupported`. `None` returns `NotRequested` with no syscall.
- `LiveShardMetrics` now publishes thread id + affinity status + observed core
  under one lock from inside the worker (`publish_worker_start`), before
  recording the thread id, so a report that names the worker also carries its
  proven pin outcome. Both worker loops (`threaded.rs`,
  `threaded_multi_shard.rs`) call `affinity::apply`; helper lanes are untouched.

Blast radius migrated, not patched over:

- `local_system.rs` topology tests now pick a core from the process's allowed
  mask and assert `Applied` + `observed_core == configured` on Linux,
  `Unsupported` off Linux, and `Failed` for a core outside the mask. They no
  longer hard-code `AdvisoryOnly` or assume CPU 0/contiguous ids.
- `blue_whale_checklist.rs` "thread pinning" line rewritten from "no OS pinning
  claim yet" to the real Linux claim; the `Advisory` status was removed. That
  rewrite is the proof the non-claim was lifted on purpose.
- `AffinityStatus::AdvisoryOnly` retired as a `configured_core` outcome; kept as
  a reserved variant (tracing string mapping unchanged).

Proof status:

- Direct: Linux pin `Applied` + `observed == configured`; `None` →
  `NotRequested`; macOS → `Unsupported`; `Failed` for an out-of-mask core; a
  pinned shard still serves; an unavailable core keeps the shard serving; the
  helper-lane float restores the original mask for a child spawned under a pin.
- **Executed on real Linux** (aarch64 nightly container, real `sched_setaffinity`
  / `sched_getcpu` and io_uring substrate): all 6 affinity unit tests pass and
  all 3 affinity integration tests pass (io_uring needs `seccomp=unconfined` in
  Docker — that EPERM is a substrate/seccomp limit, not an affinity defect). The
  x86_64 Linux path is additionally type- and clippy-checked, and runs in CI on
  `ubuntu-latest`.
- Surrogate: `validate_core` reject path unit-tested for an out-of-range core.
- Missing (honest): a real cgroup/cpuset-restricted environment — hard in CI.
- Blast radius: full `local_system` (46) and `sharded_threaded` (9) suites green
  with pinning enabled and with default `None`; `tina-tracing` (24) green;
  `tina-http` TLS suites green (the TLS-driver float touch); whole workspace
  builds; `fmt` + `clippy -D warnings` + `cargo doc -D warnings` clean.
