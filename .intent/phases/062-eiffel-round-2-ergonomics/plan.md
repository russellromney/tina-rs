# Phase 062: Eiffel Round 2 Ergonomics

## Status

- Done:
  - Round 2 findings 1-7 harvested from Eiffel specimens.
  - Rock 0: re-read of `eiffel_rate_limited_worker`,
    `eiffel_retrying_outbound_http`, `eiffel_sharded_fanout_read`. The
    seven Round 2 findings still match the in-tree specimens; no new
    product-shaped pain has appeared.
  - Rock 1: `ThreadedMultiShardRuntime::observe_result` lifted from
    the single-shard surface, routed to the owning shard. Panics on
    unknown shard (matches `try_send`). Sharded specimens rewritten.
  - Rock 3: `try_send_outcome` + `HostBurstOutcomes` accumulator.
    Rate-limited worker rewritten.
  - Rock 4: `send_observed_until` retry helper. BurstClosed loop
    collapsed to one call.
  - Rock 5: `SingleCallGate`. Rate-limited worker uses it.
  - Rock 6: `ReqwestOutcomeExt::classify` with typed reasons
    (`UpstreamServer { status }`, `BridgeTimeout`, `WorkerTimeout`,
    `WorkerTransport(msg)`, `BridgeFull`, `BridgeClosed`,
    `WorkerFull`, `WorkerClosed`, `RequestTooLarge`,
    `ResponseTooLarge`, `InvalidRequest(msg)`,
    `UpstreamClient { status }`). `eiffel_retrying_outbound_http`
    six-arm match collapsed to three.
  - User guide updated:
    `docs/tina-user-guide/11-ergonomics-checklist.md` carries
    "use this, not that" entries for each shipped primitive;
    `docs/tina-user-guide/18-bridge-crates.md` mentions `classify`
    next to `flatten_outcome`.
- In progress:
  - (none — all in-scope rocks landed)
- Open:
  - Rocks 2 (self-address) and 8 (scatter/gather helper) are blocked
    on a written design note before any code lands.
  - Rock 7 (flat reqwest continuation) waits until a non-pedagogical
    caller mixes layered + flat.
- Deferred:
  - Broad `flow!` / pseudo-async authoring surface.
  - Macro-heavy scatter/gather until a helper has proved the shape.
  - Hidden retry, hidden queues, hidden topology.

### Rock 0 read (2026-05-07)

The new specimens confirmed the seven Round 2 findings without adding
new ones. One sharpening worth recording for Rock 3:

- `try_send_outcome` cannot be made truly *synchronous in the host*
  without violating SPSC and exposing the runtime's address->mailbox
  registry to the host thread. The mailbox is owned by the worker
  shard. The shipped surface stays observer-based; what we remove is
  the *bookkeeping* (per-send closure, Arc-cloned counters, manual
  observed barrier), not the worker roundtrip. The plan's "open design
  question" resolves to: keep the existing observer shape, ship a tiny
  accumulator that holds the truth-typed counts.

This means Rock 3 in this phase ships as
`runtime.try_send_outcome(addr, msg, &HostBurstOutcomes)` plus
`HostBurstOutcomes::wait_complete(...)` / `snapshot()`. No fake
precision: the observer still fires on the worker thread, but the
caller no longer hand-rolls one closure per send.

Rocks 4 and 5 stay in scope. Rock 4 collapses to a thin
`retry_send_observed(addr, msg, deadline)` free function over
`ThreadedRuntime::send_and_observe` plus a documented convention that
"BurstClosed-style control" travels through the same data mailbox.

## Goal

Turn the second Eiffel pain harvest into Tina primitives.

Round 1 fixed the obvious papercuts. Round 2 found deeper but still small
ergonomic gaps: multi-shard host observation, self-address bootstrap, precise
host-send admission, timer gates, reqwest retry classification, and
scatter/gather setup.

Near-grug:

> If example writes same ceremony twice, maybe Tina missing a small rock.
> But if ceremony is truth, keep it visible.

## Baseline

Already shipped or landing nearby:

- `stop_with(value)` + `ThreadedRuntime::observe_result::<T>(addr)`;
- `try_send`, `send_and_observe`, `try_send_and_observe_with`;
- `sleep(...).reply(Tick)` timer continuations;
- `tina_runtime::sharded::{ShardPlacement, ShardServiceTable, ReplyAdapter}`;
- `ScatterGatherConfig` / `ScatterGatherReport`;
- `tina-reqwest-bridge::{ReqwestCallOutcome, flatten_outcome, send_request}`;
- 061 deferred replies / `PendingReplies`, once merged.

Current pain from `examples/FINDINGS.md` Round 2:

- multi-shard result reporting still uses `Arc<Mutex<Option<T>>>`;
- scatter/gather happy path requires coord + adapter + bind + accumulator;
- isolates cannot learn their own typed address during registration;
- host precise send admission needs observer closures and atomics;
- timer-driven workers repeat the same single-in-flight gate;
- reqwest retry classifiers repeat a six-arm match;
- flattened reqwest calls are useful but syntactically lopsided.

## Non-Goals

- No async handlers.
- No `await` cosplay.
- No hidden retry helper that decides idempotency for the user.
- No unbounded host-send queue.
- No second secret control mailbox unless capacity is explicit.
- No scatter/gather macro as the first answer.
- No broad app framework.
- No "make examples short by hiding the thing being taught."

## Rules

- Convenience may remove bookkeeping.
- Convenience may not hide capacity, timeout, topology, retry policy,
  `Full`, `Closed`, `Timeout`, or trace truth.
- Eiffel examples are specimens. If a primitive lands, rewrite the specimen so
  the before/after is visible.
- LLM-copyability matters. Prefer small explicit helpers over magic macros.
- Add docs only after the code shape is real.

## Rocks

### Rock 0: Wait For The Next Eiffel Batch

Before implementation, read the next three Eiffel specimens and their README
notes.

Update this phase with any repeated pain that is:

- product-shaped;
- seen in more than one place, or obviously fundamental;
- not just "this example is intentionally pedagogical."

Do not churn APIs before this read. Eiffel is the scout.

### Rock 1: `observe_result` On `ThreadedMultiShardRuntime`

Lift the single-shard threaded result waiter to the multi-shard threaded shell.

Shape:

```rust
let waiter = runtime.observe_result::<Report, _, _>(addr)?;
runtime.try_send(addr, Msg::Start)?;
let report = waiter.wait(timeout)?;
```

Requirements:

- routes registration to the owning shard from `Address`;
- same error vocabulary as `ThreadedRuntime::observe_result`;
- no replay cache;
- single-claim per `(isolate, generation)`;
- waiter cleanup follows the same rules as single-shard;
- does not require `Arc<Mutex<Option<T>>>` in sharded examples.

Proof:

- live multi-shard success;
- timeout;
- already-stopped / stopped-without-result path if supported by the
  single-shard waiter;
- two waiters contend and one loses visibly;
- update `eiffel_sharded_fanout_read` and `eiffel_sharded_keyspace` to delete
  host polling.

### Rock 2: Self Address At Registration Time

Add a registration form where an isolate constructor can see its own typed
address.

Candidate shape:

```rust
let addr = runtime.register_with_capacity_using::<I, E>(
    capacity,
    |self_addr| I::new(self_addr, other_state),
)?;
```

Multi-shard shape mirrors `register_with_capacity_on`.

Requirements:

- constructor gets the exact address/generation being registered;
- works for threaded and explicit-step runtime surfaces where practical;
- no fake address that later changes;
- no extra message required just to bind self address;
- error behavior matches normal registration.

Proof:

- single-shard explicit-step and threaded examples/tests;
- multi-shard registration on a chosen shard;
- stale address behavior unchanged after restart;
- update a specimen that currently has `Bind` before `Start`.

Notes:

- This may need different names per runtime if return/error shapes differ.
- Do not block Rock 1 on this.

### Rock 3: Precise Nonblocking Host Send Outcome

Add the missing host-send shape:

```rust
match runtime.try_send_outcome(addr, msg) {
    Ok(()) => admitted += 1,
    Err(MailboxFull(_)) | Err(IngressFull(_)) => full += 1,
    Err(Closed(_)) => closed += 1,
}
```

Intent:

- `try_send` is fast but only tells command-ingress truth;
- `send_and_observe` is precise but each call roundtrips through the worker;
- `try_send_and_observe_with` is precise and nonblocking but caller ceremony is
  high.

Requirements:

- no observer closure;
- no hidden queue;
- message is returned on failure where possible;
- distinguishes mailbox full from threaded ingress full and closed/stale
  address;
- works under tight bursts without allowing the worker to drain one message per
  send call.

Proof:

- burst fills mailbox and gets `MailboxFull`;
- command queue saturation gets `IngressFull`;
- closed/stale address returns `Closed`;
- message ownership on failure is tested;
- update `eiffel_rate_limited_worker` to remove observer atomics/barrier.

Open design question:

- Can the threaded host safely inspect the mailbox synchronously, or does the
  command lane own that truth? If not, name the limitation and keep the
  existing observer shape. Do not fake precision.

### Rock 4: Control Message Helper For Saturated Mailboxes

Keep "host is done sending" as a Tina message, but make the common retry loop
boring.

Candidate shape:

```rust
runtime.send_control_retrying(addr, Msg::BurstClosed(n), deadline)?;
```

Or, if Rock 3 makes it enough:

```rust
retry_until(deadline, || runtime.try_send_outcome(addr, Msg::BurstClosed(n)))
```

Requirements:

- bounded wait;
- no hidden queue;
- returns typed full/closed/timeout;
- message ownership clear on failure;
- docs say this is a host-side helper, not a second mailbox.

Proof:

- control message eventually lands after a full data mailbox drains;
- timeout returns visibly if no slot opens;
- closed address returns visibly.

This rock may collapse into Rock 3 if the helper would just be a tiny wrapper.

### Rock 5: `SingleCallGate`

Name the timer/call-in-flight pattern used by rate-limited workers.

Candidate shape:

```rust
if self.gate.start_if_idle() {
    sleep(window).reply(Msg::Tick)
} else {
    noop()
}

// later
self.gate.complete_one();
```

Better if it can also count queued work:

```rust
match self.gate.push() {
    GateAction::Start => sleep(window).reply(Msg::Tick),
    GateAction::AlreadyRunning => noop(),
}
```

Requirements:

- tiny state helper, not runtime magic;
- no hidden timer;
- no hidden queue beyond named counters;
- no macro first;
- readable by an LLM copying the pattern.

Proof:

- rate limit schedules one sleep at a time under many submits;
- cancellation/error path does not underflow;
- update `eiffel_rate_limited_worker` only if the helper is clearer than the
  raw `pending`/`was_idle` code.

### Rock 6: Reqwest Outcome Classifier

Add opt-in classification for `ReqwestCallOutcome`.

Candidate shape:

```rust
match outcome.classify() {
    ReqwestOutcomeClass::Succeeded(response) => finish_ok(response),
    ReqwestOutcomeClass::Transient(reason) => retry(reason),
    ReqwestOutcomeClass::Fatal(reason) => fail(reason),
}
```

Reason examples:

- `UpstreamServer { status }`;
- `BridgeTimeout`;
- `WorkerTimeout`;
- `Reqwest`;
- `BridgeFull`;
- `BridgeClosed`;
- `InvalidRequest`;
- `ResponseTooLarge`;
- `RequestTooLarge`.

Requirements:

- classify only; do not retry;
- caller still owns idempotency, budget, and backoff;
- layered raw `ReqwestCallOutcome` remains available;
- `flatten_outcome` remains opt-in and separate;
- docs say when to use layered, flat, or classified.

Proof:

- status 503 is transient;
- status 404 is fatal unless explicitly configured otherwise (first form can be
  fixed policy);
- bridge timeout and worker timeout classify distinctly;
- full/closed are fatal by default;
- update `eiffel_retrying_outbound_http` to delete the six-arm local match.

Open design question:

- Should server 5xx always be transient? First form can say yes, but leave
  room for user policy later.

### Rock 7: Flattened Reqwest Continuation Helper

Do not flatten by default, but reduce the call-site mismatch when a user
chooses the flat shape.

Current dense shape:

```rust
.reply(|outcome| Msg::Done(flatten_outcome(outcome)))
```

Possible helper:

```rust
send_request(...).reply_flat(Msg::Done)
```

Or:

```rust
send_request_flat(...).reply(Msg::Done)
```

Requirements:

- opt-in at the call site;
- visible name includes `flat`;
- flat error still says bridge vs worker;
- raw layered call remains the default docs path;
- no derive/macro until a real non-pedagogical caller needs it.

Proof:

- update `eiffel_webhook_publisher` only if this makes the example clearer;
- raw path still tested.

### Rock 8: Scatter/Gather Happy-Path Helper

After 061 and after self-address work, build the smallest helper that removes
coord/adapter setup ceremony for the boring "fan out to these targets, collect
all replies" case.

Prefer a normal builder/helper before a macro.

Candidate shape:

```rust
let coord = ScatterCoord::register(
    &runtime,
    shard,
    table,
    config,
    |report| Msg::Done(report),
)?;
```

Requirements:

- keeps `ScatterGatherReport` typed;
- keeps `Full` / `Closed` / `Timeout` / `AggregateTimeout` distinct;
- no hidden unbounded collector;
- helper owns adapter wiring if adapter is needed;
- capacity must be explicit in `ScatterGatherConfig`;
- no retry inside helper.

Proof:

- happy-path sharded fanout read;
- one full target;
- one closed target;
- aggregate timeout;
- update `eiffel_sharded_fanout_read`.

Dependencies:

- likely after Rock 1 and Rock 2;
- may depend on 061 if it uses deferred replies.

## Suggested Order

1. Rock 0: wait for next Eiffel batch.
2. Rock 1: multi-shard `observe_result`.
3. Rock 3: precise host send outcome.
4. Rock 6: reqwest classifier.
5. Rock 2: self-address registration.
6. Rock 4 / Rock 5: control retry and single-call gate if still painful.
7. Rock 7: flat reqwest continuation helper if still painful.
8. Rock 8: scatter/gather helper after the lower rocks land.

## Required Proof

- Every new helper has capacity/timeout/closed behavior tested.
- No helper introduces an unbounded queue.
- Every affected Eiffel specimen gets rewritten after the primitive lands.
- `examples/FINDINGS.md` marks closed items as closed or moves them to
  history.
- `docs/tina-user-guide/11-ergonomics-checklist.md` gets "use this, not
  that" updates only for shipped primitives.
- `cargo test --workspace` and workspace clippy pass, or a blocker is named.

## Done Means

- Round 2 current findings are either implemented, explicitly deferred, or
  superseded by findings from the next Eiffel batch.
- At least three Eiffel examples delete visible ceremony because of this
  phase.
- The deleted ceremony is product ceremony, not truth.
- Docs teach the new shapes and do not preserve stale examples.
- No new helper hides retry, overload, timeout, topology, or trace facts.

