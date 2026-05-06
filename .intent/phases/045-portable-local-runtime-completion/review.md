# Plan Review: Portable Local Runtime Completion

Verdict: correct phase, not yet execution-tight.

This is the right rock before Baobab. Baobab should not judge a runtime that we
already know is missing portable-runtime basics. Phase 045 correctly says:
build the non-`io_uring` local runtime into a boring thing first, then let
Baobab compare and gate it.

What is strong:

- The phase is feature/core work, not comparison theater.
- It keeps `io_uring`, remoting, clustering, durable mailbox, and `flow!` out.
- It focuses on Tina's real rules: visible overload, traceable failure,
  replayable races, bounded rails, no hidden queues.
- The 10 rocks are the right families: lifecycle, resource truth, fairness,
  capability truth, blocking-lane honesty, report survival, service e2e, DST,
  cost report, CI.
- It clearly prepares Baobab instead of pretending to be Baobab.

Load-bearing fixes before implementation:

1. **Rock 1 can become an endless audit.**
   It names every rail and every lifecycle verb. Good. But implementation needs
   a bounded output: an executable lifecycle table plus focused fixes for rows
   that are false. Do not refactor every rail just because names differ.

2. **Existing coverage must be counted before adding new tests.**
   Many rocks already have proof from Victor, Sadie's Ward, Blue Whale,
   resource-rail DST, `local_system.rs`, `application_surface.rs`, and
   `betelgeuse_substrate.rs`. Add a first audit step in `review.md`: for each
   rail, mark `covered | weak | missing` for positive, negative, overload,
   cancellation/timeout, shutdown, trace, and DST. Then fill only weak/missing
   cells.

3. **Capability table shape is underspecified.**
   Pin statuses and authority. Recommended statuses:
   `Supported`, `Partial`, `Unsupported`, `NotClaimed`, `PlatformGated`.
   The Rust table should assert against `RuntimeCapabilities`, bridge
   capabilities where relevant, and named non-claims. Markdown is only summary.

4. **"Every public rail has every proof" may be too literal.**
   Some rails do not have every lifecycle verb. Example: timers do not have
   `close`; signals may not have meaningful `late completion`; persistence has
   commit uncertainty instead of ordinary close. The table must allow
   `NotApplicable(reason)` so absence is explicit rather than fake.

5. **Fairness Completion needs exact starvation targets.**
   "Blocking lanes all get turns" can overclaim: a started blocking worker may
   occupy a lane until it returns. The phase should prove bounded admission,
   tombstoning, shutdown drain, and shard progress around lanes. It should not
   imply Tina can preempt OS blocking work.

6. **Resource ownership inventory needs count vocabulary.**
   Require separate rows for table-owned, worker-held, pending-driver-call,
   queued-lane, and remote-queue pressure. Otherwise one "resource count"
   number can hide the exact class of stuck work.

7. **Trace/report survival should define preferred APIs.**
   The plan names `trace()` and `complete_trace()`, but not the user rule.
   Pin it: `trace()` returns partial trace with completeness truth; 
   `complete_trace()` is strict and may fail; terminal report must retain
   trace/topology/error/resource truth even on failed shutdown.

8. **Full Local Service E2E is too broad unless split.**
   One composed happy service should use many rails. Negative paths should be
   focused per edge. Do not build one giant test where DNS, TLS, process,
   persistence, shard failure, slow peer, and shutdown all race together.

9. **DST families need owners.**
   Name which crate owns each family:
   `tina-sim` for simulator resource histories and shrinker proofs;
   `tina-runtime` for live-vs-sim projection or deterministic live e2e;
   `tina-tokio-bridge` for bridge model DST. This avoids dumping all DST into
   one crate.

10. **Cost report may accidentally become a benchmark policy.**
   Pin it as a report command/test that prints numbers and exits. No thresholds
   except "runs." No CI comparison against prior numbers. No Tokio/Glommio
   rows in this phase unless they are trivial and optional; Baobab owns
   comparison.

11. **CI rails should be named exactly.**
   Existing `.github/workflows/verify.yml` already runs `make verify` on Ubuntu
   and macOS. Phase 045 should add one portable readiness command, for example
   `make verify-portable-runtime`, and have CI call it. Keep long DST behind a
   named env var such as `TINA_DST_LONG`.

12. **Baobab update should be mandatory at closeout.**
   If 045 lands new capability truth or non-claims, 046 must be edited in the
   same closeout commit. Otherwise Baobab will compare stale rocks again.

Recommended plan edits:

- Add Rock 0 or prepend Rock 1 with "coverage matrix first."
- Add explicit table statuses, including `NotApplicable(reason)`.
- Add the count vocabulary for resource inventory.
- Add the `trace()` vs `complete_trace()` user rule.
- Split e2e into composed happy service plus focused scary-edge tests.
- Assign DST families to owning crates.
- Rename Cost/Allocation output to "portable runtime cost report"; no external
  baselines here.
- Name the CI command and long-DST gate.
- Require Baobab plan update in closeout.

After those edits, the plan is ready to execute. Without them, the phase is
directionally right but too easy to turn into either a giant audit or a
half-Baobab comparison phase.
