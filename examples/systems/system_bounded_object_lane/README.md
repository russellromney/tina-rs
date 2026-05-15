# Bounded Object Lane

This system is a tiny stand-in for an S3-style worker lane.

Many host callers concurrently submit `Put` calls to one Tina isolate. The
isolate admits only `lane_in_flight` operations at once. Admitted work is
represented by runtime-owned `sleep(...)`; extra callers receive a typed
`Busy` reply immediately.

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

Tina capability pulled:
- Multi-turn request/reply.
- Runtime-owned time.
- Bounded in-flight admission.
- Host-side concurrent calls.

Suggested follow-up:
- A small generic "lane pressure report" shape would help S3/DB/HTTP pool
  examples converge on one vocabulary.

Verdict:
- keep

