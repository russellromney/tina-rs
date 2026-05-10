# 072 — Deadline and PendingCallSet

## Status

- Done:
  - Rock 1 `Deadline` ships in `tina` with explicit-`now` constructor
    `Deadline::from_instant(now, after)` plus
    `Context::now()` / `Context::deadline_after(after)` runtime/sim
    sugar. The runtime stamps `Context::now()` from its `Clock` per
    handler invocation; the simulator stamps it from
    `virtual_anchor + virtual_now`. No `Deadline::after` shortcut. Live
    + sim parity proven by `tina-runtime/tests/deadline.rs` and
    `tina-sim/tests/deadline.rs` (exact 150ms virtual delta under sim,
    ≥ 140ms wall delta live).
  - Rock 3 `PendingCallSet<K, R>` ships in `tina`. Fixed-capacity
    `Vec`-backed slab; typed `Full` / `DuplicateKey` insert errors;
    explicit `remove(&key)` on completion; `drain()` for cancel-all
    (no helper hides per-handle `CancelOutcome` truth). Unit invariants
    in `tina/src/pending_call_set.rs`; runtime fill -> cancel/timeout
    -> refill proof in `tina-runtime/tests/pending_call_set.rs`.
  - Rock 2 cleanup truth: caller timeout and explicit cancel already
    share the same machinery (066). 072 made the cleanup pattern
    visible at the user-state layer (`PendingCallSet`) rather than
    rolled inline at every call site.
  - Rock 6 specimen upgrades: `specimen_cancellation_chain` +
    `specimen_pool_cancel_reclaim` use `PendingCallSet` for bounded
    handle storage; `specimen_backpressure_chain` propagates a real
    `Deadline` through hops (each hop reads
    `remaining_or_zero(ctx.now())` against its own `now`).
  - Docs: ergonomics-checklist gains a "Bounded pending call handles"
    section and a "Deadlines" section; FINDINGS 8 + 15 moved to
    resolved.
- In progress: none.
- Deferred:
  - Rock 4 `Deadline::split_for_next_call` — only ship if a consumer
    asks for it; raw `remaining_or_zero(now)` is enough in current
    specimens.
  - Rock 5 bridge late-reply specimen proof — covered today by 063/064
    bridge work and the existing late-reply rejection classifier;
    revisit when an external-work cancel specimen asks for fresh
    evidence.
- Deferred (separate phase): global idle observation, pool consumers
  (073), flow/pipeline sugar.

## Goal

Make "how much time is left?" and "which calls are still mine?" boring.

This is core runtime work. It is not a nicer retry helper. It is the
base that pools, fanout, bridges, and service pipelines need before they
can be honest under pressure.

Grug truth:

> Caller owns a deadline. Owner owns pending calls. Runtime tells truth
> when cancel or timeout happens.

## Non-Goals

- No hidden retry.
- No helper that hides a call, timeout, capacity, or cancellation point.
- No magic "app is idle" observer.
- No pipeline helper that hides stage names.
- No pool consumer implementation. That is 073.
- No claim that external work stopped if Tina only stopped waiting.

## Rock 0 — Audit Before Code

Read the current specimens and tests that hand-roll deadline math or
pending-call maps.

Likely targets:

- retrying outbound HTTP;
- pool cancel/reclaim;
- dynamic worker pool;
- two-stage pipeline, only if the helper keeps stage truth visible;
- HTTPS / bridge tests that use host-thread calls and explicit timeouts.

Record which repeated code is real and which code is intentionally
explicit.

## Rock 1 — Clock-True Deadline

Do not freeze a live-only `Instant` API unless it clearly says
"live-only, no simulator/replay claim."

Preferred first form:

```rust
let deadline = Deadline::after(Duration::from_secs(2));
let left = deadline.remaining_or_zero();
```

But the hard part is the clock:

- live runtime and simulator must agree on what "now" means;
- replay must not depend on wall-clock time;
- DST cases must be able to materialize deadline decisions;
- docs must say whether a deadline is live-only or sim-backed.

If the clock abstraction is not ready, this rock may land as a design
note and no public `Deadline`.

## Rock 2 — Timeout and Cancel Share Cleanup Truth

Existing call timeout already exists. Explicit `cancel_call(handle)`
also exists.

This phase must say whether timeout is implemented through the same
internal cancellation path or only emits matching trace/capacity facts.
Do not let the two paths drift.

Required states:

- queued but not delivered to callee;
- delivered to handler, normal reply still pending;
- deferred reply slot captured;
- backend/bridge work already accepted;
- caller isolate stopped;
- callee isolate stopped.

Each state needs:

- terminal caller outcome;
- trace reason;
- capacity reclamation point;
- late-reply behavior.

## Rock 3 — PendingCallSet

Small helper for isolates that own many outstanding calls.

Shape:

```rust
let mut pending = PendingCallSet::<RequestId, Reply>::with_capacity(64);
pending.insert(id, handle)?;
pending.complete(&id);
pending.cancel_all(ctx)?;
```

Rules:

- fixed-capacity table/slab/ring; no growing `HashMap`;
- insert returns `Full`;
- completion removes the entry;
- explicit cancel removes the entry;
- timeout cleanup is explicit and blessed;
- owner stop drains or cancels all owned calls;
- fill, cancel/timeout, refill proof required.

Timeout cleanup should stay Tina-shaped: every call has a visible
completion, cancel, or timeout continuation that removes the key. Do not
hide a background sweeper unless the runtime already owns the timeout
event.

## Rock 4 — Deadline Propagation Helper

Make the copied pattern short but still visible:

```rust
call(addr, msg, deadline.remaining_or_zero()).reply(Msg::Returned)
```

Useful helper methods are fine:

- `Deadline::remaining_or_zero()`;
- `Deadline::expired()`;
- `Deadline::split_for_next_call(min_remaining)` only if it does not
  hide policy.

No helper should retry or extend time. Deadline is budget, not a wish.

## Rock 5 — Bridge and External Work Rule

Prove one bridge-shaped case.

Rule:

- if Tina cancels before the bridge accepts work, no external work starts;
- if Tina cancels or times out after the bridge accepts work, Tina stops
  waiting but the external operation may still finish;
- late reply is rejected visibly;
- bridge metrics record worker-terminal outcome;
- docs say "not cancelled" when not cancelled.

Good candidates:

- reqwest accepted request completes late;
- sqlite accepted query completes late;
- TLS/client operation completes after caller timeout.

Trace-only is not enough. The test must assert the caller outcome and
the bridge/runtime late-result fact.

## Rock 6 — Specimen Upgrades

After the primitives land, update specimens where the new shape removes
bookkeeping without hiding truth.

Targets:

- retrying outbound HTTP: deadline flows through retry attempts;
- pool cancel/reclaim: pending acquire handles live in
  `PendingCallSet`;
- dynamic worker pool: owned child calls are tracked and drained;
- two-stage pipeline: only update if stage variants remain explicit.

README rule:

- say what got shorter;
- say what stayed explicit;
- do not teach hidden retry or hidden pipeline magic.

## Done Means

- Deadline story is live/sim honest, or deliberately not shipped.
- Caller timeout and explicit cancel reclaim capacity consistently.
- `PendingCallSet` is bounded and proves fill-cancel-refill.
- Accepted external work does not pretend to be cancelled.
- At least two specimens use the new helpers.
- 073 can start without timeout/cancel ambiguity.
