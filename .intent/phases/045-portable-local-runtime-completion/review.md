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

# Porting-Readiness Hostile Review

Verdict: strong build plan, but still slightly too "prove rails" and not quite
enough "build the portable runtime surface a Tokio port will actually target."

The right question is not "does 045 prepare Baobab paperwork?" It is:

> After 045, can a small Tokio-style local service be rewritten against Tina's
> portable runtime without immediately discovering missing app/runtime body
> parts?

From that lens, the plan needs a few more feature pins.

What is now right:

- Baobab no longer has to discover obvious lifecycle/reporting holes.
- The plan builds real capability truth, report truth, DST truth, and service
  harnesses.
- It refuses `io_uring` and keeps focus on the portable runtime.
- It does not pretend Tina is a general Tokio replacement yet.

Still missing or under-pinned:

1. **Canonical service runner is not explicit.**
   A porter needs to know the standard shape: build `LocalSystem`, register
   roots, start listener/service isolates, wait for signal/shutdown, drain,
   join, read terminal report. The plan has service harness and shutdown
   reports, but not a named canonical runner/path. Add one reusable helper or
   blessed pattern in tests. This is core surface, not docs polish.

2. **Connection/session lifecycle is not pinned as a first-class workload.**
   Tokio services are often listener -> accept -> spawn/session -> read/write
   loop -> timeout -> close. Tina needs a canonical isolate-shaped version:
   listener isolate, per-connection/session isolate, ownership of stream ids,
   bounded session mailbox, timeout, close, abandon on shutdown. The service
   harness should prove this directly, not just "uses TCP/TLS somewhere."

3. **Supervision plus I/O is missing from the build rocks.**
   A real port will need workers/listeners/sessions supervised. Tina's
   supervision exists, but 045 should prove it composes with runtime-owned I/O:
   supervised worker panics while TCP/TLS/file/process work is pending;
   restart does not inherit stale resources; stale addresses reject visibly;
   shutdown still drains/reports.

4. **Resource-budget manifest should be a user-facing shape.**
   Porting needs one place to set capacities: ingress, mailbox, shard-pair,
   remote-drain, DNS/TLS/process/storage lanes, signal capacity, trace
   retention, preallocation, shutdown drain timeout. Pieces exist, but 045
   should prove the manifest is complete and reachable from `LocalSystem`
   builders/config without falling into low-level `ThreadedRuntimeConfig`.

5. **Backpressure policy is too test-only.**
   "Full is visible" is necessary but users also need standard choices:
   reject, retry with timer/backoff, shed, stop, or reply busy. Do not add a
   giant policy framework, but the portable service harness should include
   at least two Tina-shaped patterns: immediate reject and explicit retry via
   timer. Baobab can then compare behavior rather than invent app policy.

6. **Request/reply boundary needs port-shaped proof.**
   Tokio ports often use request/response over channels or oneshot. Tina has
   isolate calls and bridge calls. 045 should prove local call, cross-shard
   call, timeout, requester closed, requester mailbox full, and bridge timeout
   in one user-shaped request path.

7. **Long-running workflow ergonomics are deferred, but helper gaps are not.**
   No `flow!` is right. But if the service harness becomes unreadable because
   tiny helpers are missing, 045 should add those helpers. Rule: add only
   helpers that remove boilerplate without adding a second semantic path.

8. **Runtime-owned external work lacks a "no async leakage" proof.**
   Since this phase is about replacing Tokio-shaped local services, it should
   include compile/run proof that service isolates do not need async handlers,
   Tokio tasks, or raw backend handles for the target workload. The service
   harness should be written in ordinary Tina effects only.

9. **Failure domain story should include sibling progress under service load.**
   Existing phases proved pieces. 045 should prove in the portable harness:
   one shard/session fails, sibling shard/session keeps serving, topology names
   failed shard/session, partial trace survives.

10. **CI gate should prove the actual port target.**
   `verify-portable-runtime` should not only run tables and DST. It must run
   the canonical service harness and its scary-edge tests. Otherwise the gate
   proves runtime internals but not the thing a porting session will use.

Recommended plan additions:

- Add Rock: **Canonical Local Service Runner**.
- Strengthen Rock 8: listener/session lifecycle is mandatory, not optional.
- Add supervision + I/O composition to Rocks 5/8 or as its own rock.
- Add resource-budget manifest completeness to Rocks 2/4.
- Add two standard backpressure patterns to service harness: reject and retry.
- Add user-shaped request/reply path covering local, cross-shard, and bridge
  timeout/cancel outcomes.
- Add explicit "ordinary Tina effects only, no async handler/raw backend" proof.
- Add sibling progress/failure-domain proof under service load.
- Ensure `verify-portable-runtime` runs the canonical service harness.

After these additions, 045 would really be building the core features Baobab
needs before asking "can Tina port Tokio projects yet?"
