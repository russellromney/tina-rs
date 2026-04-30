# 014 Reviews And Decisions

Session:

- A (artifacts)
- B (this round)

## Review Log

---

## Round 1: Spec Diff Review

Artifact reviewed:

- `.intent/phases/014-mariner-tcp-completeness/spec-diff.md`

Read against:

- `.intent/SYSTEM.md` (current shipped surface, especially the
  TCP-call-family + listener-spawn-with-bootstrap paragraph)
- `ROADMAP.md` (Mariner section, "Mariner I/O" delivered, "Done when"
  for Mariner)
- `.intent/phases/012-mariner-io-current-runtime-and-echo/spec-diff.md`
  and the listener flow shipped under it
- `.intent/phases/013-mariner-socket-address-introspection/spec-diff.md`
  (vendored Betelgeuse, honest `local_addr` / `peer_addr`)
- `tina-runtime-current/tests/tcp_echo.rs` and
  `tina-runtime-current/examples/tcp_echo.rs` (current one-shot
  topology)
- `tina-runtime-current/tests/task_dispatcher.rs` (slice-011 reference
  shape this spec invokes by name)

### Positive conformance review

Judgment: passes

- **P1.** Honest framing of what is missing. Today's Listener handles
  `Bound` → one `TcpAccept`, `Accepted` → `Effect::Spawn(...)`, and
  then **never re-arms**. Calling that "shaped like a one-shot demo"
  matches the code, not just the headline.
- **P2.** Substrate-neutrality preserved. The "What Does Not Change"
  list explicitly keeps `tina` boundary, sync handlers, and sync
  `step()`. Re-arming an already-existing `Effect::Call(TcpAccept)` is
  expressible without new public effects, in principle.
- **P3.** Negative space is sharp: no timer, no benchmark numbers, no
  multi-shard, no production claims. "100k connections" stays a
  Mariner Done-When item, not a 014 deliverable.
- **P4.** Continuity with slice 011's user-shape is named: connection
  children remain *restartable* and supervised. The spec is not
  smuggling in a one-off bypass around restartable workers just to
  make multi-client cleaner.
- **P5.** "The accepted connection lifecycle remains runtime-owned"
  in Acceptance pins exactly the right invariant: the listener
  spawns/supervises, the child boots through the runtime workflow
  (i.e. `with_bootstrap`, not harness ingress kicks). This protects
  the 012 fix from regressing.
- **P6.** The acceptance criterion "proof still asserts on runtime
  events, not just payload round-trips" is the right defense against
  multi-client tests becoming "we read the right bytes" with no
  trace-level proof.

### Negative conformance review

Judgment: passes with one human-escalation and several
agent-resolvable tightenings.

- **N1. Re-arm shape is underspecified, and at least one plausible
  candidate would force a `tina` boundary change.**
  *Class: human-escalation.*
  Three materially different shapes can all be called "re-arm
  through normal isolate control flow":

  (a) **Self-targeted `ReArmAccept` message.** The listener handles
      `Accepted { stream }` by emitting `Effect::Send(self_addr,
      ReArmAccept)`; on the next turn, `ReArmAccept` triggers
      `Effect::Call(TcpAccept)`. But this leaves the spawn
      out — the listener can only return one effect per handler
      invocation. So the spawn has to come from a different arm
      (e.g. a `SpawnPending { stream }` self-message), which means
      per-listener state to remember the pending stream id between
      arms.

  (b) **Listener stashes the stream and posts itself a follow-up.**
      `Accepted { stream }` stores `stream` in self-state and emits
      `Effect::Send(self_addr, SpawnAndRearm)`. Next turn fires the
      `Spawn(...)`, then a third arm posts `ReArmAccept`. Three
      message turns per accepted connection. Honest but heavy.

  (c) **Allow multiple effects per handler invocation.** Either
      `Effect::Batch(Vec<Effect>)` or a richer post-spawn callback
      that the listener can use to chain a follow-up. This would be
      a real `tina` boundary change and is exactly the
      pause-gate-named "different public effect shape."

  Slice 012 already lets the runtime deliver a bootstrap message
  to the spawned child via `with_bootstrap`. There is no symmetric
  "post a message back to the parent after a successful spawn"
  mechanism. The cleanest re-arm probably wants either (a) with
  per-listener pending state, or a (d) variant: a small public
  shape change that lets the parent learn its new child's address
  AND chain another effect — but anything in (c)/(d) crosses
  boundaries.

  **Recommendation.** Pick shape (a) or (b) before implementation,
  pin the per-listener state shape in the plan, and explicitly
  name (c)/(d) as out-of-scope unless evidence forces otherwise.
  Shape (a) with per-listener `pending_stream: Option<StreamId>`
  is the smallest move and reuses existing primitives; it is also
  what slice 011's task-dispatcher would do if it had to emit
  multiple effects from one client message.

- **N2. Self-addressing for the listener should be pinned, not
  discovered.**
  *Class: agent-resolvable.*
  `Context::current_address::<M>()` already exists in `tina/src/lib.rs`
  and gives any handler its own typed `Address`. The plan should
  state that the listener captures `ctx.current_address::<ListenerMsg>()`
  during `Bootstrap` (or stashes it in self-state on first handler
  call) and uses that for self-targeted `Effect::Send`. Without that
  pin, an implementer might thread the address in through state at
  registration time, forcing a placeholder/re-register dance like
  the one we already cleaned up in 012.

- **N3. "Small bounded amount of overlap or back-to-back work" is
  vague.**
  *Class: agent-resolvable.*
  On a single-shard single-thread runtime, "overlap" can mean only
  one concrete thing: two TCP streams holding pending Betelgeuse
  completions in the IoBackend's `pending` list at the same time.
  The spec should pin that specifically: "at least one test forces
  ≥ 2 connection isolates in flight (recv pending) simultaneously,
  exercised by two client threads connecting and writing without
  the first one closing first." Without that pin, "back-to-back"
  could devolve to "client A connects, finishes, disconnects, then
  client B connects" — which proves only that the listener accepted
  twice, not that the runtime can hold two stream completion slots
  concurrently.

- **N4. No assertion budget for multiplicity.**
  *Class: agent-resolvable.*
  The current `assert_call_path_completed` helper just checks "≥ 1
  event of this kind in the trace." For N clients, the proof should
  count: exactly 1 `CallCompleted{TcpBind}`, ≥ N `CallCompleted{TcpAccept}`,
  ≥ N `Spawned`, ≥ N `CallCompleted{TcpStreamClose}`. Without
  multiplicity assertions, a regression that quietly restarts the
  listener between clients (the very thing the spec's "without
  rebuilding the listener" criterion guards against) could pass.

- **N5. "Without rebuilding the listener" needs a concrete trace
  assertion.**
  *Class: agent-resolvable.*
  The natural check: exactly one `CallCompleted{TcpBind}` and no
  `Spawned { child_isolate: <listener_id> }` event after the initial
  registration. This is the trace-level shape of the acceptance
  criterion and the spec should commit to it.

- **N6. Listener close / graceful shutdown is not addressed.**
  *Class: human-escalation.*
  A "credible first TCP server shape" plausibly includes "the
  server can be told to stop and stop cleanly." Today
  `TcpListenerClose` exists at the call boundary but is unused
  outside the focused call-dispatch tests. Two reasonable answers:
  (i) the proof drives N clients then sends the listener a `Stop`
  control message that emits `Effect::Call(TcpListenerClose)` and
  `Effect::Stop`, asserted in the trace; or (ii) graceful shutdown
  is explicitly deferred to a later slice. Either is fine; silence
  is not, because it leaves "credible server shape" reading as
  "server shape that never cleans up."

- **N7. The runnable example termination semantics are
  undefined.**
  *Class: agent-resolvable.*
  The 012 example connects one client, asserts, exits. A "small
  but honest server-shaped example" with N clients needs to either
  (i) accept exactly N clients then exit (matches the test) or
  (ii) loop until SIGINT. The spec should pin (i) for the same
  smoke-vs-daemon reasons the 012 example was assertion-backed —
  otherwise CI runs the example and never returns.

### Adversarial review

Judgment: passes, with a few angles to pin.

- **A1. Procedural: this is genuinely 012's deferred deliverable
  named separately.**
  *Class: process-meta, not blocking.*
  The original 012 spec called for "one or more connections" but
  the proof shipped one. 014 is honest re-scoping, not new
  product. This is fine; worth naming so the closeout commit
  message and SYSTEM.md update don't give the impression that
  multi-client TCP is a fresh capability beyond what 012 promised.
  The roadmap's `Mariner I/O` row should add a brief "(multi-
  connection accept re-arm landed in 014)" pointer when 014 closes.

- **A2. Implicit-concurrency claims through test interleaving.**
  *Class: agent-resolvable.*
  When two client threads connect at once, the OS may deliver
  bytes in unexpected orders. The single-shard runtime serializes
  handler invocations, so there is no race inside the runtime, but
  the test could accidentally encode an assertion shaped like
  "both clients' bytes echo back in send order regardless of
  scheduling." That is *not* what the runtime promises. The Traps
  list should add: "Do not assert on cross-stream ordering; assert
  per-stream payload integrity only."

- **A3. CI flakiness budget.**
  *Class: agent-resolvable.*
  Current echo test has a 5-second pump deadline. Multi-client
  TCP tests in CI are flakier than single-client ones (port pools,
  loopback latency variance, GC pauses on busy CI). The plan
  should pin either a generous deadline (e.g. 10 s) or a
  deterministic step-count cap before timing out — and call out
  what "flaky" looks like, so the next reviewer knows whether a
  CI fail is a real bug or test-infra noise.

- **A4. The re-arm shape decision (N1) might leak.**
  *Class: human-escalation, partial duplicate of N1.*
  The pause gate "re-arming `TcpAccept` reveals a need for a
  different public effect shape" is exactly the right backstop,
  but it would be cheaper to pre-declare in the plan that the
  package commits to shape (a) (self-message + per-listener
  pending state) and that any move toward (c)/(d) trips the
  gate. As-is the plan describes step 1 as "rework the listener
  workflow so `TcpAccept` is re-armed through normal isolate
  control flow" without naming which workflow.

- **A5. Test-replacement vs test-addition is unclear.**
  *Class: agent-resolvable.*
  The plan says "expand the TCP proof surface" and lists
  multi-client / re-arm / continued-supervised-child-boot. It
  doesn't say whether the existing
  `tcp_echo_round_trips_one_client_payload` test is replaced by a
  multi-client variant or kept as a smaller smoke. Almost
  certainly we want both (the smaller test is faster on CI and
  pins the no-overlap baseline), but the plan should commit.

- **A6. The example becomes "almost a server" without saying
  what stops it.**
  *Class: agent-resolvable, partial duplicate of N7.*
  The trap "do not let the example become a logs-only demo"
  catches one failure mode (no assertions). The other failure
  mode is "example loops forever and is now a long-running
  process in CI." The plan should pin termination policy.

What I checked and found defended:

- Spec preserves the supervised-parent + restartable-child shape.
- No new top-level `Effect` variants are spec'd; new behavior is
  expected to live in existing `Effect::Call(TcpAccept)` +
  `Effect::Spawn` paths.
- Trace events shipped in 012 (`CallDispatchAttempted`,
  `CallCompleted`, `CallFailed`, `CallCompletionRejected`,
  `Spawned`) are sufficient to express multi-client lifecycle
  without new event kinds. The plan does not appear to require
  new trace vocabulary.
- The "do not turn the runtime into a general-purpose production
  network server" non-goal in `What Does Not Change` is the right
  ceiling.

## Decisions Taken

- **Re-arm shape:** choose **(a)**. Use listener self-messages plus
  listener-owned pending state, with the listener capturing its own typed
  address through `ctx.current_address::<ListenerMsg>()`. Do not change the
  `tina` boundary for 014.
- **Graceful shutdown:** choose **(i)**. The bounded reference workload should
  drive `N` clients, then have the listener close its listening socket through
  `TcpListenerClose` and stop cleanly, with trace evidence.

The agent-resolvable tightenings from this review are folded into the active
spec and plan.

---

## Round 3: Artifact Correction Before Implementation

Session A implementation analysis found a real blocker against the reviewed
014 shape:

- the current `tina::Effect` surface still allows only one effect per handler
  turn
- a credible listener needs to express at least "spawn accepted child, then
  re-arm accept" without harness tricks or hidden callbacks
- `SpawnSpec` / `RestartableSpawnSpec` do not provide a parent follow-up hook
  after spawn

Decision:

- keep 014 as the TCP-completeness package
- broaden it to include the minimum honest `tina` boundary change needed to
  express that workflow: an ordered batch effect over the existing closed
  effect set

This is not a silent redesign of the runtime model; it is the smallest
boundary change that preserves Tina's explicit state-machine shape while
making the reviewed 014 server workflow actually expressible.

---

## Round 2: Plan Review

Artifact reviewed:

- `.intent/phases/014-mariner-tcp-completeness/plan.md`

### Positive conformance review

Judgment: passes

- **P1.** Step ordering (rework listener → connection lifecycle →
  expand proof → refresh example → close out) matches the spec
  diff and respects the "do not silently keep listener one-shot
  while broadening only the test wrapper" trap.
- **P2.** Pause gates are sharp and named. The "different public
  effect shape" gate is the right backstop for N1/A4.
- **P3.** Traps catch the most likely correctness-theater
  shortcuts: harness-side lifecycle kicks, silent listener-stays-
  one-shot, logs-only example, scope creep to timers/benchmarks.
- **P4.** "Black-box and workload-oriented where possible" matches
  the existing 012 proof discipline (integration tests +
  assertion-backed example).
- **P5.** No `tina` boundary surface changes are listed — the
  trait crate is correctly absent from the implicit "files to
  change" list.

### Negative conformance review

Judgment: passes with the same N1 escalation plus several
agent-resolvable tightenings.

- **N1. Re-arm shape decision missing from the plan.** Same
  finding as Round 1 N1; the plan should commit to a shape (or
  explicitly defer to a small spike) before code lands, because
  shape (a) and (b) require different listener `Message`
  vocabularies.
  *Class: human-escalation.*

- **N2. No "Files Likely To Change" section.** Most prior phase
  plans list files. For 014 the natural list is short (`tests/
  tcp_echo.rs`, `examples/tcp_echo.rs`, possibly a new test
  file, `CHANGELOG.md`, `ROADMAP.md`, `.intent/SYSTEM.md`,
  `.intent/phases/014/*`). Adding it makes the blast radius
  visible and helps the implementation reviewer.
  *Class: agent-resolvable.*

- **N3. No "Areas That Should Not Be Touched" section.** Same
  story; prior plans pin areas (`tina-mailbox-spsc` unsafe,
  cross-shard, simulator crates, etc.) explicitly. For 014 the
  list should include `tina/src/lib.rs` (no boundary changes),
  `tina-runtime-current/src/lib.rs` runtime core (no new public
  surface), `vendor-betelgeuse/*` (vendored upstream, edits
  belong to a different phase or upstream PR), and
  `tina-supervisor` (no policy widening).
  *Class: agent-resolvable.*

- **N4. Verification list is shallower than 012.** 012's plan
  named focused-test surfaces, integration test, runnable
  example, and `make verify`. 014's plan names integration
  tests + example + `make verify`. It should also commit to:
  trace-multiplicity assertions per N4 from Round 1, the
  no-listener-rebuild trace assertion per N5 from Round 1, and
  whether `connection_retries_partial_write_before_reading_again`
  (today in `tcp_echo.rs`) stays as-is or gets folded into the
  new test file structure.
  *Class: agent-resolvable.*

- **N5. "Continued trace assertions per call kind / lifecycle
  event" is hand-waved.** Per Round 1 N4, the plan should pin
  what "continued" means quantitatively (e.g. exact bind count,
  ≥ N accept counts, ≥ N close counts).
  *Class: agent-resolvable.*

- **N6. Plan doesn't say whether `has_in_flight_calls()` (a
  drainage helper added in 012) needs to grow a multi-stream
  variant or stays as-is.** The current echo test uses it to
  drain stragglers before reading the trace. With multi-client
  workloads, "drained" is a more interesting predicate. The
  plan should say either "the existing helper is sufficient"
  with a one-line justification, or "extend the helper" with the
  shape.
  *Class: agent-resolvable.*

### Adversarial review

Judgment: passes, with two notes.

- **A1. Plan is shorter than 012's, which is appropriate scope-
  wise but loses some structural fields (Mapping From Package To
  Code, Areas That Should Not Be Touched, Files Likely To
  Change, Report Back).** 014 is a smaller package than 012, so
  the plan can be smaller; but those fields encode IDD discipline
  that helps the implementation reviewer. Recommend at least
  bringing back "Files Likely To Change" + "Areas That Should
  Not Be Touched" + "Report Back."
  *Class: agent-resolvable.*

- **A2. The plan does not call out the closeout deliverables.**
  In 012, closeout updated `SYSTEM.md` (specifically the "Current
  shipped surface" paragraph that today claims "one client
  connection"), `ROADMAP.md` (the `Reference examples` and
  `Runtime-owned I/O` rows mention "one client connection"
  explicitly), and `CHANGELOG.md`. The plan should commit to
  those edits in step 5; otherwise the closeout PR ends up
  patching three different artifacts ad-hoc.
  *Class: agent-resolvable.*

What I checked and found defended:

- The plan does not propose new `tina` trait surface.
- The plan does not propose new runtime trace event kinds.
- The plan does not propose new supervision semantics.
- The pause gates are well-shaped against the actual risks the
  spec identifies.

---

## Open Decisions

One human-escalation decision before 014 is implementable:

1. **Re-arm shape.** Three materially different "re-arm through
   normal isolate control flow" implementations exist. Concrete
   recommendation:

   - **(a) Self-targeted `ReArmAccept` plus per-listener
     `pending_stream: Option<StreamId>`.** `Accepted { stream }`
     stores the stream in self-state and emits `Effect::Send(self,
     SpawnPending)`. `SpawnPending` arm emits `Effect::Spawn(...)`
     and emits `Effect::Send(self, ReArmAccept)`. `ReArmAccept`
     emits `Effect::Call(TcpAccept)`. Three message turns per
     accepted connection. Smallest move; reuses existing
     primitives; no `tina` boundary change.
   - **(b) Variant of (a) but consolidate to two turns.**
     `Accepted { stream }` emits `Effect::Spawn(...)` and stores
     `(stream, post_spawn=ReArmAccept)` in self-state. The
     bootstrap message is delivered by the runtime; the listener
     never explicitly re-arms via spawn-completion. Re-arm comes
     from a self-`Send`. Two turns. Slightly subtler.
   - **(c) Multi-effect handler return.** Adds either
     `Effect::Batch(Vec<Effect>)` or a post-spawn-callback
     mechanism to `tina`. Cleaner code shape; trips the
     "different public effect shape" pause gate; almost
     certainly the wrong move for 014's scope.

   Recommend **(a)**: smallest move, no boundary change, costs
   one extra message turn per accepted connection — which is
   cheap on a single-shard runtime stepping through accepts.
   Pin the chosen shape in the plan before implementation.

2. **Listener close / graceful shutdown.** Either:
   - **(i) In scope.** The proof drives N clients, then a control
     message tells the listener to issue
     `Effect::Call(TcpListenerClose)` followed by `Effect::Stop`.
     Tests assert the trace shows clean teardown.
   - **(ii) Explicitly deferred.** Spec/plan add one line saying
     graceful shutdown is a future slice; tests rely on process
     exit for cleanup.

   Recommend **(i)**, because it is small (one new control message
   variant in the listener) and it removes a real correctness-
   theater gap from the "credible first TCP server shape" claim.

Agent-resolvable items Session A can fold autonomously without
escalation:

- Pin self-addressing via `ctx.current_address::<ListenerMsg>()`
  in the plan (Round 1 N2).
- Pin "≥ 2 connections simultaneously pending in IoBackend" as the
  concrete meaning of "small bounded overlap" (Round 1 N3).
- Add multiplicity assertions to the verification section
  (Round 1 N4 / Round 2 N5).
- Add the "exactly one `CallCompleted{TcpBind}`, no listener
  rebuild" trace assertion (Round 1 N5).
- Pin example termination policy (Round 1 N7 / Round 2 A2):
  example accepts a fixed N clients then exits, mirroring the
  test.
- Add a Traps line forbidding cross-stream ordering assertions
  (Round 1 A2).
- Pin a CI deadline / step-cap budget for multi-client
  workloads (Round 1 A3).
- Decide test-replacement vs test-addition: keep
  `tcp_echo_round_trips_one_client_payload` as the small smoke;
  add a multi-client test alongside (Round 1 A5).
- Add "Files Likely To Change," "Areas That Should Not Be
  Touched," and "Report Back" sections to the plan (Round 2 N2,
  Round 2 N3, Round 2 A1).
- Commit closeout deliverables (SYSTEM.md, ROADMAP.md, CHANGELOG)
  in step 5 (Round 2 A2).
- Note in plan that `has_in_flight_calls()` is sufficient for
  drainage assuming sequential-after-pumping semantics, or extend
  it with a one-line justification (Round 2 N6).

If you take the two human-escalation decisions and Session A
folds the agent-resolvable items, 014 is ready to execute under
autonomous bucket mode.

---

## Round 4: Implementation and Verification

014 landed with one intentional boundary change and direct proof for the
new behavior:

- `tina::Effect` now includes `Batch(Vec<Effect<I>>)` for ordered
  left-to-right sequencing of existing effects.
- `tina-runtime-current` executes batched effects in order and
  short-circuits later batched effects after `Stop`.
- Direct batch proof lives in
  `tina-runtime-current/tests/batch_effect.rs`.
- The TCP listener now captures `ctx.current_address::<ListenerMsg>()`,
  re-arms `TcpAccept` through ordinary self-messages, batches
  `Spawn` + self-`Send`, closes the listener through
  `TcpListenerClose`, and then stops through the normal runtime path.
- The TCP proof surface now includes:
  - one-client smoke
  - sequential multi-client handling without rebinding the listener
  - bounded overlap with two concurrent client connections
  - graceful listener close / stop
- A crate-local runtime test directly proves that two accepted stream
  reads can be pending in `IoBackend` at once, so the bounded-overlap
  claim is not inferred only from client timing.

Verification that passed before closeout:

- `cargo +nightly test -p tina --test sputnik_api`
- `cargo +nightly test -p tina-runtime-current --lib --test batch_effect --test tcp_echo`
- `cargo +nightly run -p tina-runtime-current --example tcp_echo`
- `make verify`

Implementation note:

- 014 ended up being the right place to make the small boundary extension
  instead of faking server completeness with one-effect-per-turn workarounds.
  That keeps the code aligned with the updated IDD standard: direct proof for
  the claimed server shape, no harness tricks, and no hidden callback path.
