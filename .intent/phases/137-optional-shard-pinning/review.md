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
