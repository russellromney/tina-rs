# Bounded Object Lane

This system is a tiny stand-in for an S3-style worker lane.

Many host callers concurrently submit `Put` calls to one Tina isolate. The
isolate admits only `lane_in_flight` operations at once. Admitted work is
represented by runtime-owned `sleep(...)`; extra callers receive a typed
`Busy` reply immediately.

This is hermetic on purpose. A real S3 bridge needs observed completion
delivery, cancellation, close/drain, terminal reporting, and late-result truth;
that belongs in a real `tina-aws-bridge`, not hidden inside a specimen.

## Run

```bash
cargo run --manifest-path examples/systems/system_bounded_object_lane/Cargo.toml
cargo test --manifest-path examples/systems/system_bounded_object_lane/Cargo.toml
```

## Findings

What felt good:
- The in-flight cap is ordinary isolate state, not a pool mutex or scheduler
  side effect.
- Overload is a typed reply (`Busy`) instead of a hidden wait queue.
- `RequestContext` plus `reply_with_request` makes multi-turn replies explicit.

What felt rough:
- The request-context shape is correct, but still visually heavy for a common
  "accept now, reply after runtime call" path.
- The mini service wants a standard pressure-report helper so `accepted`,
  `busy`, `completed`, and `in_flight` do not become per-service vocabulary.
- The real S3 temptation was useful: completion delivery must be observed, not
  best-effort `try_send`, or in-flight capacity can leak.

Tina capability pulled:
- Multi-turn request/reply.
- Runtime-owned time.
- Bounded in-flight admission.
- Host-side concurrent calls.
- Future AWS bridge pressure shape.

Suggested follow-up:
- A small generic "lane pressure report" shape would help S3/DB/HTTP pool
  examples converge on one vocabulary.
- A real `tina-aws-bridge` should reuse this admission shape but provide typed
  AWS outcomes, cancellation, shutdown, and pressure reports.

Verdict:
- keep
