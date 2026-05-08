# 064 Service Bootstrap And Fanout Ergonomics

## Status

- Done: plan created from Eiffel Round 4 findings.
- Done in `round4-trailing-followup` / #25 work: `examples/FINDINGS.md`
  was cleaned into Active/Closed lists; stale README references were fixed;
  `eiffel_webhook_publisher` replaced its host `Done` condvar with
  `observe_isolate_complete`; `eiffel_outbound_http` replaced the
  per-request `mpsc` bridge with one scripting Driver isolate that ends via
  `stop_with(report)` + `observe_result`.
- In progress: none.
- Open: implement low-risk helpers, design the model-changing helpers, update Eiffel examples.
- Deferred: fake-async pipeline syntax, hidden retries, hidden queues, broad workflow macros.

## Goal

Round 4 changed the pain.

Tina can now model bigger service shapes. The remaining cost is repeated
bootstrap and fanout ceremony:

```text
Bind before Start.
Drain every pending reply by hand.
Pass deadlines by hand.
Write one message variant per pipeline stage.
Invent cancellation messages per isolate.
```

064 turns the proven repeated pain into small Tina primitives, but only where
the helper keeps the truth visible.

The working rule from Eiffel:

```text
ergonomics may remove bookkeeping.
ergonomics may not remove truth.
```

This phase is allowed to end with fewer shipped helpers than rocks. Some rocks
are design rocks. That is success if the design prevents a bad helper from
landing.

## Execution Shape

Do this as one full 064 sweep if the session has momentum. The split below is
for commit/PR shape and review sanity, not a reason to stop after the first
slice.

- **064A cleanup + small helpers:** Rock 0, Rock 1, likely Rock 4, docs/example
  rewrites.
- **064B runtime-model designs:** Rocks 2, 3, 6, 7, 8, 9. Write design notes
  before code. Implement only if the note makes the semantics boring.
- **064C fanout polish:** Rock 5 after self-address/adapter shape is clear.
- **064D bridge/proof docs:** Rocks 10, 11, 12 as small follow-ups.

You may work through all of 064 in one worktree. Keep the history readable:
land cleanup/small helpers, design notes, model-changing code, and example
migrations as separate commits or clearly separated PR chunks.

Important: "full sweep" does **not** mean every proposed helper must ship.
For model-changing rocks, a design note that rejects or defers the helper is a
valid result.

## Rule

Long explicit code is okay.

Short dishonest code is bad.

Helpers may delete repeated plumbing. Helpers may not hide:

- state ownership;
- capacity;
- `Full`;
- `Closed`;
- `Timeout`;
- cancellation;
- partial progress;
- trace-visible suspension points.

If a helper makes the code shorter but makes the next message unclear, reject
the helper.

This matters even more with LLMs writing Tina code. Named states, named
messages, visible `Full`/`Closed`/`Timeout`, and visible suspension points are
copyable rails. Clever helpers that hide the rails are not wins.

## Non-Goals

- No `async` cosplay.
- No `?`-style hidden failure policy.
- No hidden retry.
- No hidden queue.
- No "just make pipelines look like Tokio".
- No scatter/gather framework before the narrow happy path is proven.
- No external cancellation API that pretends work already accepted by a worker
  vanished.
- No hidden usable address after failed registration. If a user explicitly
  smuggles a constructor-time address through shared state and construction
  fails, later sends must fail loudly; do not pretend the address was live.

## Rock 0: Clean The Findings First

Make `examples/FINDINGS.md` the current product list, not archaeology.

Status: mostly handled by `round4-trailing-followup`. After that branch/PR is
merged, verify instead of redoing the cleanup.

Do:

- rename the file heading away from "Round 2" if it now spans Round 4;
- move 062-closed items to resolved;
- fix stale names (`SingleSleepGate` -> shipped `SingleCallGate`, or remove
  when the shipped helper does not solve that case);
- fix stale example names (`eiffel_graceful_pool_shutdown`, not old sketches);
- fix stale README references and finding numbers;
- keep active findings only for product work still needed.

Proof:

- `examples/FINDINGS.md` has one active list and one resolved list;
- no active finding says "closed";
- every `Surfaced by:` example exists;
- README references point at the stable finding numbers.

## Rock 1: Pending Reply Drain Helpers

This is the low-risk first win.

Today repeated service-stop code looks like:

```rust
let mut effects: Vec<_> =
    self.pending.drain().into_iter().map(|(_, slot)| reply_to(slot, R::Closed)).collect();
effects.push(stop());
Effect::Batch(effects)
```

Add helpers on `PendingReplies<K, R>`:

- `drain_replies_for<I>(value: R) -> Vec<Effect<I>>` or equivalent where
  `I: Isolate<Reply = R>`;
- `drain_with_for<I>(f: impl FnMut(K) -> R) -> Vec<Effect<I>>`;
- `drain_into_effect_for<I>(...) -> Effect<I>`;
- `drain_into_stop_for<I>(...) -> Effect<I>`.

Name can change during implementation, but the shape must stay boring.

Rules:

- helper must be typed so `DeferredReply<R>` can only become
  `Effect<I>` when `I::Reply = R`;
- same-value helper requires `R: Clone`; non-`Clone` cases use a
  `FnMut(K) -> R` closure;
- no hidden stop unless the method name says stop;
- preserves `reply_to` trace facts;
- drains only currently held slots;
- does not sweep/claim/retry behind the user's back.

Apply to:

- `eiffel_graceful_pool_shutdown`;
- `eiffel_bounded_batcher` if natural;
- any Round 4 example with the exact drain pattern.

Proof:

- unit test drains N slots and replies to all;
- unit test empty drain is no-op;
- compile-time shape prevents replying with the wrong isolate reply type;
- example diff deletes manual drain boilerplate.

## Rock 2: Initial Child-Spawn Observation

`observe_child_restarted(parent)` exists. Initial child address discovery still
uses side channels in supervised examples.

Design first. Ship a narrow observation handle for first child start only if the
runtime already emits enough truth or the new event is obviously the missing
truth.

Candidate:

```rust
let waiter = runtime.observe_child_started(parent);
runtime.try_send(parent, ParentMsg::Spawn)?;
let child = waiter.wait(timeout)?;
```

Rules:

- this is observation, not registration-time self-address;
- one event per actual child start;
- must include child address and generation;
- define whether initial children spawned by `with_initial_message` count as
  started at registration, first delivery, or first handler completion;
- must not require the child to publish its own address through `Boot`;
- bounded observation registry rules apply;
- dropped/timed-out waiters must not leak cap.

Apply to:

- `eiffel_supervised_worker`.

Proof:

- initial child start observed;
- supervised restart still uses `observe_child_restarted`;
- timeout/dropped waiter cleanup;
- stale parent/closed runtime behavior.

If current runtime events cannot prove this cleanly, stop and record the needed
runtime event instead of faking it.

## Rock 3: Self-Address At Registration Time Design

Do design first. Implement only after the design survives review. This is not a
small helper.

Candidate shape:

```rust
let addr = runtime.register_with_capacity_using(capacity, |self_addr| {
    MyIso::new(self_addr, other_state)
})?;
```

Questions to settle:

- when is the address generation allocated?
- can messages deliver before the constructor returns? Answer should be no.
- what happens if constructor panics?
- what happens if constructor returns error, if fallible form exists?
- can `self_addr` escape if registration fails?
- can the constructor send `self_addr` to another isolate before registration
  commits? If yes, what does that other isolate observe?
- what trace events exist for allocate/construct/register/fail?
- explicit-step and threaded runtime parity;
- multi-shard `register_with_capacity_using_on(shard, ...)`;
- macro interaction.

Rules:

- no hidden usable address after failed registration; explicit
  user-shared escape fails loudly;
- no hidden first message;
- no delivery until registration is complete;
- address generation semantics match normal registration;
- panic/failure cleanup is tested.

Apply only after implementation:

- remove `Bind { self_addr }` / `Begin { self_addr }` bootstraps in examples
  where that is the only job of the variant.

Proof:

- constructor receives its own typed address;
- constructor panic does not leave live address/mailbox;
- constructor failure cannot leak a usable address through a side effect;
- no message delivers before constructor returns;
- stale address behavior matches normal registration;
- multi-shard path works or is explicitly deferred.

## Rock 4: ReplyAdapter Registration Ergonomics

`ReplyAdapter` is good. Registering it is not.

Add the smallest helper that removes the type-noise without hiding the adapter.

Pin the API home before coding. Likely home is the module that owns
`ReplyAdapter`, not examples.

Candidate:

```rust
let bridge = ReplyAdapter::register_with_capacity_on(
    &runtime,
    shard,
    target,
    capacity,
)?;
```

Rules:

- explicit adapter address remains visible;
- no hidden mailbox capacity;
- no hidden target clone/refresh;
- no scatter/gather policy inside this helper.
- helper names whether it registers on the target shard, coord shard, or caller
  supplied shard.

Proof:

- existing sharded/scatter tests can use the helper;
- helper works on threaded and explicit-step forms, or the unsupported form is
  explicitly not implemented.

## Rock 5: Scatter/Gather Happy Path Design

Do not build a framework.

Design one narrow happy path after Rocks 3 and 4. Implementation is optional in
064; do not force it. If the helper takes longer to explain than the explicit
coord, reject it and keep the explicit coord as the blessed first form.

```rust
scatter_gather_all(table, targets, config, make_msg, fold)
```

or:

```rust
ScatterCoord::register(runtime, table, config, on_complete)
```

Required visible inputs:

- ordered targets;
- collector mailbox capacity;
- max targets;
- per-target timeout;
- aggregate timeout;
- partial outcome policy;
- result cap.
- address table / service table generation policy.

Required visible outputs:

- per-target `Replied`;
- `Full`;
- `Closed`;
- `Timeout`;
- `AggregateTimeout`;
- `MissingShard`;
- partial result.

Rules:

- no unbounded result vec;
- no hidden retry;
- no collapse of target outcomes;
- owner-side wrong-shard validation stays in owner services;
- if the helper needs self-address, it waits for Rock 3.
- if the helper is longer to explain than the explicit coord, reject it.

Proof:

- one happy-path Eiffel example gets shorter;
- one pressure test still shows `Full` or `Timeout`;
- saved simulator seed if simulator shape is practical.

## Rock 6: Deadline Propagation Design

Backpressure chains need one budget, not copied timeout math.

Design a tiny deadline value if it deletes repeated code:

```rust
let deadline = Deadline::after(Duration::from_millis(100));
call(worker, msg, deadline.remaining_or_zero()).reply(...)
sleep(deadline.remaining_or_zero()).reply(...)
```

Rules:

- absolute deadline, not retry policy;
- no hidden cancellation;
- no hidden retry;
- easy conversion to `Duration` for existing APIs;
- expired deadline is visible and testable;
- clock source must be clear for live vs sim;
- do not use `std::time::Instant` in simulator-facing semantics unless the
  design explicitly says "live-only helper";
- decide whether APIs take `Duration`, `Deadline`, or both before coding.

Proof:

- chain A -> B -> C uses one deadline;
- expired deadline returns timeout before dispatch where appropriate;
- examples no longer manually recompute timeout budget.

This rock may land as design only.

## Rock 7: External Cancellation Design

Design before code. Implementation is optional and probably not first.

There are at least three different things called cancellation:

1. stop an isolate;
2. cancel one caller-owned pending isolate call;
3. cancel all calls owned by an isolate without stopping the isolate.

Do not blur them.

Candidate APIs:

```rust
runtime.cancel_isolate(addr) -> CancelOutcome
runtime.cancel_call(handle) -> CancelOutcome
ctx.call_with_handle(...) -> (Effect<Self>, CallHandle)
```

Rules:

- accepted worker work may still finish;
- late replies must become visible rejected facts;
- cancellation must reclaim pending capacity;
- cancellation cannot pretend resource/driver operations are cancelled unless
  the driver actually cancels them;
- simulator parity required for any shipped runtime behavior;
- never call a late worker reply "success" after cancellation;
- trace must distinguish caller timeout, explicit cancel, isolate stop, and
  resource close.

Proof:

- `eiffel_cancellation_chain` can use the new shape, or the design explains
  why domain `Stop` remains the right first form;
- caller timeout and explicit cancel are distinguishable in trace.

## Rock 8: Lifecycle / Work-Settled Observation Design

Some host code wants "the app has settled" rather than "this one isolate
stopped".

Examples:

- producer stopped and consumer caught up;
- all admitted work drained;
- signal observed and no in-flight timer remains.

Design a small observation or convention. Implementation is optional.
Do not ship a vague global
`runtime.wait_idle()` unless it has exact semantics.

This rock is risky because "settled" can become a lie fast. The default answer
should stay boring: a driver isolate owns the app workflow and finishes with
`stop_with(report)`. Ship a runtime/helper shape only if it has a small exact
meaning.

Candidate directions:

- typed app-level `stop_with(report)` driver remains the blessed pattern;
- `observe_isolate_quiescent(addr)` if the runtime can define it honestly;
- a tiny `HostBarrier` for examples/tests only if this is just test plumbing.

Rules:

- no polling atomics as the blessed app shape;
- no global idle lie;
- must say which mailboxes/calls/timers are included;
- if the honest answer is "write a driver isolate and `stop_with(report)`",
  document that and do not ship a helper.

Proof:

- `eiffel_graceful_shutdown` either migrates or documents why app-level driver
  is still the honest shape;
- Do not use `eiffel_webhook_publisher` as evidence for a new `HostBarrier`;
  `round4-trailing-followup` already removed that specimen's condvar by using
  `observe_isolate_complete`.

## Rock 9: Pipeline Ergonomics Design

Be careful.

Long explicit code is okay if it is honest. Short code is bad if it hides the
program.

Pipeline variants are real cost, but they are also real documentation. A Tina
pipeline helper must help readers see the stages faster. It must not make Tina
look like fake `async`/`.then(...)` where important suspension and error edges
disappear.

Pipeline helpers may remove repeated plumbing. They may not hide stages.

Possible acceptable shape, if any:

```rust
enum PipelineMsg {
    Submit(Input),
    StageDone(StageId, StageOutcome),
}
```

or a helper that owns only:

- stage ids;
- next-stage routing;
- timeout conversion;
- result accumulation.

Rules:

- every stage remains named;
- every suspension point remains trace-visible;
- per-stage timeout/full/closed remains visible;
- partial progress remains visible;
- no hidden `?`;
- no hidden retry;
- raw match-state-machine form stays the semantic truth;
- this rock may end with "do not build a helper yet".

Proof:

- `eiffel_two_stage_pipeline` gets clearer, not merely shorter;
- README says what is hidden and what is not;
- if the helper fails the honesty test, leave the example explicit and record
  why.

## Rock 10: Reqwest Flat Reply Mapper Decision

Low priority.

`flatten_outcome` is useful, but the closure is noisy:

```rust
.reply(|outcome| Msg::Done(flatten_outcome(outcome)))
```

Do not make flat errors default.

Current evidence: after `round4-trailing-followup`, the remaining mixed
layered/flat call site is pedagogical. It intentionally shows all three
reqwest shapes side by side. That is not enough evidence to add a public
helper.

Only ship a helper if a non-pedagogical call site wants flat errors repeatedly.

Possible shape:

```rust
send_request(...).reply_flat(Msg::Done)
```

Rules:

- opt-in only;
- error still says bridge vs worker;
- no retry policy;
- raw layered path remains documented.
- if no non-pedagogical caller exists, do only docs/FINDINGS cleanup.

## Rock 11: Bridge Setup Unification Audit

Do not extract a common bridge framework yet.

Audit `tina-tokio-bridge`, `tina-tower-bridge`, `tina-rpc-tokio`,
`tina-reqwest-bridge`, and the upcoming database bridge plan.

Record:

- install shape;
- config validation;
- metrics;
- close/drain;
- error layering;
- cancellation truth;
- bounded admission;
- late reply / dropped caller behavior;
- supplied external client ownership: which knobs belong to Tina config and
  which belong to the supplied client;
- setup return shape: address, closer, metrics, runtime/host handle.

Shared vocabulary to look for:

- `install`;
- `close`;
- `drain`;
- `metrics`;
- `config validation`;
- `bounded admission`;
- `late reply`;
- `dropped caller`;
- `supplied client owns X`.

Rule:

```text
two crates is coincidence.
three repeated shapes is evidence.
```

If three shapes match, write the tiny shared convention down. Do not implement a
bridge-common crate in this phase unless the duplication is exact and boring.

This audit is load-bearing for 063. A database bridge should not invent a sixth
dialect for install/close/drain/metrics if the existing bridges have already
proved a common shape.

## Rock 12: Owned-State Proof Story

Round 4's owned-state leak probe is positive evidence.

Make it part of the proof story, not just an example README.

Do:

- keep compile-fail probes in the example;
- link them from the user guide or README proof section;
- consider moving durable compile-fail checks into crate-level tests if the
  pattern belongs to `tina` itself.

Do not claim Tina prevents `Arc<Mutex<_>>` if the user intentionally puts one
in the message. The claim is:

```text
Tina's normal typed paths do not require shared mutable state.
Rust still lets users opt out.
```

## Order

1. Rock 0: verify findings/docs cleanup from `round4-trailing-followup` after
   merge; fix only new drift.
2. Rock 1: pending drain helper.
3. Rock 2: initial child-spawn observation if runtime events support it.
4. Rock 4: ReplyAdapter registration helper.
5. Rock 3: self-address design, then implementation only if design is clean.
6. Rock 6: deadline propagation design.
7. Rock 7: external cancellation design.
8. Rock 8: lifecycle/work-settled design.
9. Rock 9: pipeline design with permission to reject helper.
10. Rock 5: scatter/gather happy path after self-address.
11. Rock 10/11/12: reqwest mapper decision, bridge audit, owned-state proof docs.

Within one sweep, keep moving down the list. Stop only when the next rock needs
a design decision that is not yet boring, or when the diff becomes too large to
review safely.

## Done Means

- superseded example-local helpers are gone or explicitly left as pedagogical;
- stale docs and finding numbers are fixed;
- at least two low-risk helpers land and are used by examples;
- Rock 11 leaves behind a concrete bridge convention note, even if no shared
  bridge crate is built;
- every design-only rock either has a design note or is explicitly left for a
  named later phase;
- every model-changing helper touched by the phase has a design note before
  code;
- Eiffel examples are updated only where the new shape is clearer or more
  honest;
- `examples/FINDINGS.md` ends with a sharper active list than it started with.
