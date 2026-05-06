# Plan Review: Portable Local Runtime Completion

Verdict: much better. This is now a build phase, not a review phase. Still
needs a few pins before implementation so it does not sprawl or quietly become
Baobab.

What is strong:

- The plan now says "build missing runtime surface" directly.
- Coverage map is only Rock 1, not the whole phase.
- Baobab is correctly downstream: 045 builds the portable runtime, 046 judges
  it.
- `io_uring`, remoting, clustering, durable mailbox, `flow!`, and performance
  claims stay out.
- The concrete build rocks are right: capability table, lifecycle surface,
  resource inventory, fairness/progress, blocking lanes, report survival,
  service harness, DST, cost report, CI, Baobab handoff.

Load-bearing issues:

1. **"Complete portable runtime" can overclaim.**
   The goal should mean complete for *local service experiments*, not complete
   as a universal runtime. Keep that wording everywhere. Anything not needed
   for local services can remain a named non-claim.

2. **Rock 1 still lacks a pass/fail rule.**
   It says fix every missing and load-bearing weak cell "needed for the
   portable local runtime claim." Good, but implementation needs an explicit
   rule: a cell can close as `covered`, `fixed`, or `deferred-nonclaim`.
   `deferred-nonclaim` must update capability truth and Baobab.

3. **Capability statuses need ownership.**
   `NotApplicable(reason)` is listed with normal statuses, but it carries a
   string and may not fit a simple enum. Pin the expected shape before coding:
   either enum + reason field, or enum variant with `&'static str`. The table
   should stay easy to assert, not become prose in disguise.

4. **Unified Driver Lifecycle could still trigger broad refactors.**
   "Build common lifecycle helpers/events" is right, but public API changes
   should be gated. First preference: crate-private helpers and tests. Public
   events/types only when user-visible behavior needs them.

5. **Resource inventory needs queued-lane definition.**
   Current reports already expose table-owned, worker-held, pending driver
   calls, lane capacities, and remote queues. "Queued lane work" may need new
   counters. Pin whether 045 must add exact queued-depth counters or whether
   accepted/rejected/capacity plus pending/worker-held is enough. If exact
   depth cannot be reported honestly for `sync_channel`, say so.

6. **Fairness/progress needs concrete scenarios.**
   Add required tests:
   - local ingress under hot self-sender;
   - remote inbound under local ingress pressure;
   - driver completion under hot mailbox pressure;
   - lane completion under unrelated mailbox pressure;
   - shutdown signal under in-flight lane work.
   Without named scenarios, "fairness" is too easy to declare done.

7. **Blocking lanes should list all target lanes exactly.**
   Rock 6 names storage, DNS, TLS, process, persistence. Persistence rides the
   storage lane today; say whether it is separate in the table or a storage-lane
   use case. Signals are poll-backed, not blocking. UDP is poll-backed. Good,
   but the plan should say that explicitly to avoid fake lane work.

8. **Trace/report API hardening needs bridge boundary truth.**
   Bridge metrics are not the same as `TraceSnapshot`. Pin what bridge must
   expose: accepted/full/closed/timeout/cancelled/responded-late counts and
   shutdown retry state. Do not force bridge to pretend it has deterministic
   replay under Tokio.

9. **Portable Service Harness must live somewhere reusable.**
   Decide location. Recommended: `tina-runtime/tests/portable_service.rs` owns
   the harness for runtime e2e; `tina-sim` owns DST models; bridge tests use a
   smaller adapter-facing harness. If the harness is copied across crates,
   future changes will rot.

10. **DST live-vs-sim projection can be expensive and flaky.**
   Pin live DST as deterministic bounded e2e with saved scripted histories, not
   random OS timing. True random histories belong to simulator/model DST. Live
   tests should project stable semantic facts, not wall-clock order.

11. **Cost report should not print from normal tests.**
   Rust tests that print timing numbers are noisy and unstable. Better shape:
   a small example/bin or `make portable-runtime-cost` command. `make
   verify-portable-runtime` can smoke-run it with tiny iteration count.

12. **CI gate may duplicate `make verify`.**
   Pin `verify-portable-runtime` as additive: capability table, portable
   service e2e, selected DST seeds, cost smoke. It should not rerun the whole
   workspace if CI already runs `make verify`, or CI time will balloon.

13. **Baobab handoff must include review cleanup.**
   At closeout, 045 should update 046's plan and review if the earlier review
   became stale. Otherwise Baobab will inherit old objections about North Sea
   or missing portable truth.

Recommended edits:

- In Goal/Done Means, use "complete for local service experiments."
- Add closure statuses for Rock 1: `covered`, `fixed`, `deferred-nonclaim`.
- Pin capability table shape with reason-bearing status rows.
- Add public API change pause gate for lifecycle surface.
- Define queued-lane truth and whether exact depth is required.
- Add the five named fairness/progress tests.
- Clarify persistence/storage lane and poll-backed signal/UDP.
- Pin bridge metrics expected fields.
- Name `tina-runtime/tests/portable_service.rs` as the main harness home.
- Keep live DST deterministic/scripted; simulator/model DST owns randomness.
- Split cost command from normal tests.
- Keep `verify-portable-runtime` additive, not a full duplicate of
  `make verify`.
- Require 046 plan/review refresh at closeout.

After these edits, I would call 045 ready to implement.
