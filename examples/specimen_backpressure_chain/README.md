# specimen_backpressure_chain

Tokio-vs-Tina A → B → C chain with one shared deadline. The point is
*deadline propagation* — does the runtime tell you which hop ran out
of time, or does it only tell you that something did?

C's work time alternates fast (20 ms, well under budget) and slow
(200 ms, well over the 80 ms budget). The driver fires 6 requests
through the chain.

## Run

```sh
cargo run --manifest-path examples/specimen_backpressure_chain/Cargo.toml -- both
cargo test --manifest-path examples/specimen_backpressure_chain/Cargo.toml
```

```
side=tokio successful=2 c_timed_out=0 b_timed_out=0 caller_timeout=3 full=0 closed=0 rejected=0 domain_failure=1 runtime_failure=0 exit_clean=true
side=tina  successful=2 c_timed_out=3 b_timed_out=0 caller_timeout=0 full=0 closed=0 rejected=0 domain_failure=1 runtime_failure=0 exit_clean=true
```

## Read

- [`src/tokio_impl.rs`](src/tokio_impl.rs)
- [`src/tina_impl.rs`](src/tina_impl.rs)

## Tokio shape

A wraps the chain in `tokio::time::timeout(TOTAL_DEADLINE, service_b(i))`.
B is `async fn` that calls C. C is `async fn` that sleeps. When the
outer timer fires, the inner futures are dropped from the bottom up;
B and C never observe the deadline:

```rust
match tokio::time::timeout(total, service_b(i)).await {
    Ok(()) => Success,
    Err(_) => Timeout, // we cannot tell which hop was slow
}
```

The Tokio report therefore increments `caller_timeout`; it does not infer a
C timeout from the test script. In real Tokio code, the runtime tells you
nothing more than "the chain timed out."

## Tina shape

Three first-class isolates: `ServiceA`, `ServiceB`, `ServiceC`. Each
is a service in the chapter-10 sense (request → maybe runtime calls
→ reply). The deadline is a typed `Deadline` value anchored at A's
`Context::now()`; downstream hops read the remaining budget against
their own `now`:

```rust
// A → B
let deadline = Deadline::from_instant(call.now(), self.budget);
call.defer(call_request(
    self.b_addr,
    BRequest::Forward { iteration, deadline },
    self.budget + Duration::from_millis(50), // outer slack
))
.reply_service_event(AEvent::BDone)

// B → C, with whatever budget remains at B's now
let timeout = deadline.remaining_or_zero(call.now());
call.defer(call_request(
    self.c_addr,
    CRequest::Compute { iteration },
    timeout,
))
.reply_service_event(BEvent::CDone)
```

When C is too slow, B's `IsolateCall` to C resolves as
`CallOutcome::Timeout`; B translates it to a typed `BReply::CTimedOut`
and replies fast. A receives `CallOutcome::Replied(CTimedOut)` and
knows *exactly* which hop ran out. No invisible drops:

| Outcome at A              | Report bucket / truth             |
|---------------------------|-----------------------------------|
| `Replied(Success)`        | chain finished                    |
| `Replied(CTimedOut)`      | C ran past `budget`               |
| `Replied(BTimedOut)`      | A's wait for B expired            |
| outer `Timeout`           | driver's wait for A expired       |
| `Full`                    | bounded admission was full        |
| `Closed`                  | destination was closed            |
| `Rejected(_)`             | typed runtime rejection           |
| `Replied(DomainFailure)`  | service-domain failure            |
| `Replied(RuntimeFailure)` | runtime-owned continuation failed |

## Discussion

What feels better:

- **Per-hop attribution.** When the deadline fires, the report names
  the hop. A debugger reading the trace sees one
  `CallOutcome::Timeout` event at exactly the call that ran past
  budget, not a chain of dropped futures with no provenance.
- **Deadline is a real value in the message.** `Deadline` carries
  the absolute monotonic time the budget runs out. Each hop reads
  `remaining_or_zero(ctx.now())` against its own runtime/sim-stamped
  `now`, so the budget shrinks honestly across hops — the
  deadline-propagation contract that distributed systems books spend
  chapters on, made concrete in 25 lines of Tina.
- **Runtime/sim honest.** `Deadline` does not call `Instant::now()`
  internally. It carries an `Instant` the runtime stamped via
  `Context::now()`; the simulator stamps the same field from its
  virtual clock anchor, so DST/replay tests see deterministic
  deadline math.
- **The reply translator is exhaustive.** B and A preserve `Full`,
  `Closed`, `Rejected`, B timeout, outer caller timeout, domain failure, and
  runtime-owned continuation failure as distinct
  typed replies. The driver counts those buckets independently instead of
  collapsing them into a generic error.

What feels worse:

- **A's outer timeout must be slightly larger than B's downstream
  timeout.** Otherwise A's outer call to B fires
  `CallOutcome::Timeout` at the same wall-clock instant as B's call
  to C, and A loses the per-hop attribution. The specimen adds 50 ms
  of slack at A's outer call. In a real chain with N hops, the
  outermost call ends up with N × slack budget — there is no helper
  that names the "outer = innermost + slack" pattern, and Tina is
  not going to ship one: each call's timeout is its own typed
  truth.
- **`Deadline` does not retry, does not extend, does not cancel
  work.** It is a budget value. If the budget is gone, you stop
  waiting; whatever already-accepted external work was started runs
  to completion (or its own bridge timeout). Cancellation is its own
  primitive — see `specimen_cancellation_chain`.
- **Three isolates feel like overkill for a three-hop demo.** Each
  hop is its own struct with its own `enum`, its own `Reply`, and
  its own typed request continuation. For a real production service
  this is appropriate — the ratio of "boilerplate per hop" is high
  but each suspension point and each `Full` / `Closed` / `Timeout`
  edge stays named.

## Deadline propagation, not a fixed budget

An earlier form of this specimen passed `budget: Duration` through the
chain. The budget was the *original* budget at every hop: B's call
to C used the full `TOTAL_DEADLINE` even though A's hop had already
consumed some of it. This worked because the work happens to be
short, but it is not honest.

This form propagates a `Deadline` instead. Each hop reads
`deadline.remaining_or_zero(ctx.now())` against its own
runtime/sim-stamped `now`, so the budget actually shrinks across
hops. The same shape works under live and simulator runtimes —
`Context::now()` returns the simulator's virtual time when run
under DST.
