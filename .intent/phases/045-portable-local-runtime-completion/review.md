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

# Second Porting-Readiness Hostile Review

Verdict: now on target. The plan is finally about building the portable runtime
surface that Baobab needs, not just proving rails around it.

What got fixed:

- The target workload must use ordinary Tina effects only. Good. That protects
  the no-async-handler story.
- Resource-budget manifest completeness is now explicit. Good. Porting cannot
  require spelunking into `ThreadedRuntimeConfig`.
- Backpressure patterns are now part of the harness: reject/busy and
  retry/backoff. Good. Baobab can compare app behavior instead of inventing it.
- Canonical local service runner is now a rock. Good. This is the shape a
  porter needs.
- Listener/session lifecycle is now mandatory. Good. That is the heart of
  Tokio-shaped services.
- Supervision plus I/O, request/reply boundary, sibling progress under failure,
  and service-harness CI are all now named. Good.

Remaining issues to pin before or during implementation:

1. **Canonical runner must not stay test-only if it reveals missing public API.**
   The plan says tiny public helper only if needed. During implementation, be
   strict: if every realistic service test repeats the same shutdown/signal/run
   ceremony, that is a product smell. Either add a small public helper or
   deliberately record why the explicit ceremony is the intended Tina surface.

2. **Resource budget manifest must include mailbox capacities.**
   `LocalSystemConfig` covers runtime/lane/shard capacities, but isolate
   mailbox capacities are still passed at registration/spawn. That may be
   correct, but the manifest proof must name it: app-level isolate capacities
   remain per-registration/per-child, while runtime resource budgets live in
   `LocalSystemConfig`.

3. **Bridge is not a portable runtime rail.**
   It is an adapter boundary. The plan correctly includes bridge timeout/cancel
   because porting from Tokio needs it, but implementation should avoid pulling
   Tokio bridge concerns into `tina-runtime` capability truth. Runtime table and
   bridge table can cross-check, but should stay distinct.

4. **Session lifecycle must prove ownership transfer.**
   The harness should assert not just "session reads/writes" but that after a
   stream is handed to a session, stale/competing owner behavior is rejected or
   impossible by construction. This is the kind of bug a port would hit fast.

5. **Supervision plus pending I/O needs exact restart outcome.**
   If a supervised session panics while a read/write/process/file call is
   pending, the plan should prove whether restart happens after cancellation,
   whether late completion is rejected, and whether new generation receives no
   old completion. Do not settle for "trace contains restart."

6. **Failure-domain proof should distinguish isolate failure from shard failure.**
   A session panic is not the same as a worker-thread/shard failure. Both matter
   for porting confidence. If both cannot fit, make one `fixed` and the other
   `deferred-nonclaim` with capability/Baobab update.

7. **Cost report rows should include configured capacities.**
   A bounded framework's cost without capacity context is weak. The cost report
   should print the capacities/preallocation used for each row, not just timing
   and allocation counts.

No blocker beyond these pins. With them, 045 is a real pre-Baobab build phase.

# Plan Review 3: IDD Hostile Review

Verdict: close, but not ready. The plan points at the right thing: build the
portable local runtime surface a real app will use. The remaining problems are
IDD problems: changed behavior is not always tied to direct public-path proof,
old behavior is not always re-proved, and one rock can still turn into a table
instead of code.

I read `/Users/russellromney/Documents/Github/idd`: `review.md` is append-only,
plans must say what changes, what must not change, how new behavior is proved,
and how blast radius is proved. Direct proof beats surrogate proof. User-shaped
e2e is preferred for changed behavior.

## Strong Proof

- Public runner is now required. That closes the "test-only app shape" smell.
- The plan requires ordinary Tina effects only: no async handlers, raw backend
  handles, or Tokio tasks inside isolates.
- The service harness is user-shaped: listener, session, I/O, bounded queues,
  shutdown, report.
- Scary-edge tests are named, not hand-waved.
- DST is included as weird-combo pressure, not a replacement for normal tests.
- `make verify-portable-runtime` must run the actual service harness, not just
  tables.

## Weak Or Missing Proof

1. **Rock 1 still risks becoming the phase.**
   "Coverage Map, Then Fix" is useful, but IDD says build and prove. Rename it
   to an implementation ledger and state it cannot be the main deliverable.
   The ledger records discovered gaps and proof status while building.

2. **Public runner API is not pinned enough.**
   The plan allows "explicit builder/run path first" and an attribute macro
   later. Pin the 045 target: explicit public `LocalSystem` runner/builder now,
   no attribute macro in this phase. Direct proof: outside integration test
   uses the public runner and receives a terminal report.

3. **Blast radius for existing low-level runtime APIs is vague.**
   Adding a public runner must not break direct `LocalSystem`,
   `LocalMultiShardSystem`, or `ThreadedRuntime` construction. Required proof
   should include existing low-level tests plus one new test that bypasses the
   runner and still works.

4. **Resource-budget manifest must name mailbox split.**
   Runtime budgets can live in runner/config, but isolate mailbox capacities
   are per registration/spawn/child. Pin that as intentional. Direct proof:
   one public-runner test configures runtime budgets and separately configures
   small mailbox capacity, then observes `Full`.

5. **One mega-harness can hide missing direct proof.**
   Rock 10 asks a lot from one harness. IDD wants changed paths hit directly.
   Keep one canonical service harness, but require focused direct tests for
   each changed rail: runner, budget, lifecycle, lane full, cancel/tombstone,
   shutdown report, trace partial, supervision plus I/O.

6. **Blocking cancellation needs proof vocabulary.**
   Queued blocking work can cancel. Started blocking work may only tombstone
   and report. Direct tests must prove both separately. Do not let one
   "cancel" test close both claims.

7. **Trace/report survival needs public negative tests.**
   Add direct outside tests: after shard failure, `trace()` returns partial
   with missing shard names, `complete_trace()` fails cleanly, and terminal
   report still carries topology/resource/error truth.

8. **Bridge capability must not pollute runtime capability.**
   Bridge matters for porting, but it is adapter truth. Keep runtime capability
   table and bridge capability table distinct. Direct proof: one bridge test
   exercises timeout/cancel/late response without changing runtime capability
   claims.

9. **Live DST must not depend on OS timing.**
   For live runtime, DST/projection should compare stable semantic facts only:
   accepted/full/closed/timeout/cancel/report shape. Simulator/model DST owns
   randomness and shrinking. Say this in the plan.

10. **Non-toy example needs CI-safe side effects.**
    Require ephemeral ports, temp directories, no external network, and cleanup.
    Otherwise the example will pass locally and rot in CI.

11. **Cost report needs stable mode.**
    Pin profile and modes: small smoke mode for CI, larger manual mode for
    humans. Include configured capacities/preallocation in every row.

12. **Tiny-helper rule needs a hard stop.**
    045 may add helpers only when repeated public runner/service ceremony proves
    the need. No second semantic path, no hidden retry, no hidden capacity, no
    `flow!`.

## Old Behavior At Risk

- Existing explicit runtime construction could regress behind the new runner.
- Existing capability reports could become bridge-contaminated.
- Existing cancel/shutdown semantics could be overclaimed as preemption.
- Existing trace APIs could become prettier but less honest under failure.
- Existing platform-gated rails could become silent skips in CI.

## Human Decisions Needed

None. The obvious choices are:

- explicit public runner now;
- no attribute macro in 045;
- runtime capability and bridge capability remain separate;
- one canonical harness plus focused direct tests;
- started blocking work is tombstoned/reported unless truly cancellable.

## Required Plan Edits

- Rename Rock 1 to **Implementation Ledger And Gap Closure**.
- Pin explicit public `LocalSystem` runner/builder as 045 target.
- Add blast-radius proof for low-level runtime construction.
- Pin mailbox-capacity split in resource manifest.
- Split mega-harness proof into canonical harness plus focused direct tests.
- Add queued-cancel versus started-tombstone proof language.
- Add public negative trace/complete-trace tests.
- Separate runtime and bridge capability truth.
- Pin live DST as semantic/scripted, not wall-clock random.
- Require ephemeral ports/temp dirs/no external network for the example.
- Pin cost report smoke/manual modes and capacity context.
- Add tiny-helper stop rule.

After those edits, 045 is ready to execute.

# Plan Review 4: Final Hostile Pass

Verdict: almost ready. The Plan Review 3 edits landed. The phase is now
implementation-shaped and mostly IDD-shaped. I would still tighten a few
things before execution so the implementer cannot satisfy the words while
leaving a squishy public surface or weak blast-radius proof.

## What Is Strong Now

- Rock 1 is now a ledger, not the main work.
- The public runner is pinned as real public API.
- No attribute macro in 045. Good.
- Runtime capability truth and bridge truth are separated.
- Mailbox capacity is explicitly separate from runner/runtime budgets.
- Queued cancel and started-work tombstone are separated.
- Trace/`complete_trace()` negative public tests are required.
- Live DST is semantic/scripted, not wall-clock random.
- The example is CI-safe by design.
- Cost report has smoke/manual modes and capacity/profile context.

## Remaining Issues

1. **Plan lacks a plain "What Will Not Change" section.**
   Non-goals are good, but IDD wants old intent named. Add a small section:
   handlers remain sync/effect-returning, mailbox semantics remain bounded
   `Full`/`Closed`, existing low-level runtime constructors remain supported,
   no async/raw backend/Tokio leakage into isolates, no hidden queues, no
   preemption claim, no bridge semantics in runtime capability truth.

2. **Public runner target still lacks a minimum API shape.**
   The plan says explicit public `LocalSystem` runner/builder, but not the
   minimum shape. This can cause bikeshedding or halfwork. Add a pin like:
   builder/configure/register/run/drain/terminal-report. Names may follow code
   style, but those operations must be present and public.

3. **Runner must not become a hidden scheduler shortcut.**
   The runner should compose existing `LocalSystem` behavior. It must not add a
   second delivery engine, hidden worker pool, hidden queue, or special service
   path that tests pass but normal Tina semantics do not use.

4. **Blast-radius proof should name exact old paths.**
   "Direct construction without the public runner still works" is right but
   broad. Name at least these: single-shard `LocalSystem`, multi-shard
   `LocalMultiShardSystem`, and low-level `ThreadedRuntime`/current runtime
   construction. Also keep existing bridge tests green.

5. **No-async-leakage proof could be stronger.**
   The harness says ordinary Tina effects only. Add proof shape: the non-toy
   example and public-runner e2e compile and run without `tokio::spawn`,
   `async fn` handlers, or raw backend handles in isolate code. This can be
   code review plus targeted `rg`/compile proof, not necessarily a fancy test.

6. **Bridge timeout/cancel remains in scope but not central.**
   Plan includes bridge metrics and bridge DST. Good. Make clear bridge work is
   only to keep existing adapter truth from regressing while the runtime
   surface changes. 045 should not become another bridge phase.

7. **"Every rail" proof can still over-expand.**
   Required Proof says every rail has many proof modes. Some rails may be
   `not-applicable(reason)` or already covered. Execution should close by
   direct proof for changed behavior, blast-radius proof for old behavior, or
   named non-claim. Do not require reauthoring every old rail test if the rail
   did not change.

8. **Service harness needs deterministic external process choice.**
   Process rail in a portable CI harness can be flaky if it shells out to
   platform-specific commands. Pin a tiny deterministic command or a
   platform-gated process scenario.

9. **SYSTEM.md update timing should be explicit.**
   Update `SYSTEM.md` only after direct proof, not during planning or halfway
   through execution. The plan should say closeout updates SYSTEM/roadmap/
   changelog only with landed truth.

## Required Plan Edits

- Add **What Will Not Change** section.
- Add minimum public runner operation shape.
- State runner composes existing `LocalSystem`; no second delivery engine.
- Name old-path blast-radius proof exactly.
- Strengthen no-async-leakage proof for example/harness.
- Keep bridge work scoped as adapter regression proof, not central runtime
  work.
- Clarify "every rail" closure: changed rails direct proof, old rails
  blast-radius or existing proof, nonclaims explicit.
- Pin deterministic/platform-gated process command in harness.
- State `SYSTEM.md` updates only after proof.

After those edits, I would hand 045 to implementation.
