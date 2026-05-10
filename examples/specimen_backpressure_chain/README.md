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
side=tokio successful=3 c_timed_out=3 chain_dropped=0 exit_clean=true
side=tina  successful=3 c_timed_out=3 chain_dropped=0 exit_clean=true
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

The README counts `c_timed_out` only because *the test scripts which
hop is slow* (`c_is_slow(i)`). In real Tokio code, the runtime tells
you nothing more than "the chain timed out."

## Tina shape

Three first-class isolates: `ServiceA`, `ServiceB`, `ServiceC`. Each
is a service in the chapter-10 sense (request → maybe runtime calls
→ reply). The deadline is propagated as a typed `Duration` in the
request message and as the matching `IsolateCall` timeout:

```rust
// A → B
call(self.b_addr, BMsg::Forward { iteration, budget: self.deadline },
     self.deadline + Duration::from_millis(50))   // outer slack
    .reply(AMsg::BDone)

// B → C, with the budget B received from A
call(self.c_addr, CMsg::Compute { iteration }, budget)
    .reply(BMsg::CDone)
```

When C is too slow, B's `IsolateCall` to C resolves as
`CallOutcome::Timeout`; B translates it to a typed `BReply::CTimedOut`
and replies fast. A receives `CallOutcome::Replied(CTimedOut)` and
knows *exactly* which hop ran out. No invisible drops:

| Outcome at A                          | Hop responsible        |
|---------------------------------------|------------------------|
| `Replied(Success)`                    | (chain finished ok)    |
| `Replied(CTimedOut)`                  | C ran past `budget`    |
| `Timeout`                             | A's own outer deadline |
| `Replied(Error)` / `Full` / `Closed`  | B ingress / lifecycle  |

## Discussion

What feels better:

- **Per-hop attribution.** When the deadline fires, the report names
  the hop. A debugger reading the trace sees one
  `CallOutcome::Timeout` event at exactly the call that ran past
  budget, not a chain of dropped futures with no provenance.
- **Deadline is a real value in the message.** The `budget: Duration`
  in `BMsg::Forward` is not magic. A future B could observe its own
  arrival time and shrink the budget further before passing it to C
  — exactly the deadline-propagation contract that distributed
  systems books spend chapters on.
- **The reply translator is tiny.** B's `match outcome { Replied(())
  => reply(Ok), Timeout => reply(CTimedOut), Full | Closed =>
  reply(Error) }` is the entire propagation layer. No second tower
  middleware, no `tracing::Span` plumbing.

What feels worse:

- **A's outer timeout must be slightly larger than B's downstream
  timeout.** Otherwise A's outer call to B fires
  `CallOutcome::Timeout` at the same wall-clock instant as B's call
  to C, and A loses the per-hop attribution. The specimen adds 50 ms
  of slack at A's outer call. In a real chain with N hops, the
  outermost call ends up with N × slack budget — and there's no
  helper that names "outer = innermost + slack" pattern. Roll-your-
  own arithmetic at every hop.
- **No deadline as ambient context.** Each call passes the
  `Duration` explicitly. There is no `ctx.deadline()` API; if a hop
  forgets to forward `budget`, the chain silently uses the default
  (often `forever`). Compare to `context.Context` in Go or
  `tower::Service` request extensions in Rust — both ship a
  conventional spot for the deadline.
- **Three isolates feel like overkill for a three-hop demo.** Each
  hop is its own struct with its own `enum`, its own `Reply`, and
  its own `RuntimeCall` continuation. For a real production service
  this is appropriate — but the ratio of "boilerplate per hop" is
  high enough that a `tina_rpc` wrapper or a derive-based helper
  would shrink it significantly.

## What this suggests

- A small "deadline" type that carries `(start: Instant, total:
  Duration)` and computes `remaining()` would replace the explicit
  `Duration` budget and the +50ms slack pattern. A future Tina
  primitive `propagate_deadline(target, msg, deadline)` could
  compute `remaining()` and pass it through to the underlying
  `call(...)`.
- See FINDINGS finding 15 (deadline as first-class context) for
  the proposed `Deadline` value type.
