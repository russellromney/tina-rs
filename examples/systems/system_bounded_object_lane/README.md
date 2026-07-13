# Bounded Object Lane

This system is a tiny stand-in for an S3-style worker lane.

Many host callers concurrently submit `Put` calls to one Tina isolate. The
isolate admits only `lane_in_flight` operations at once. Admitted work is
represented by runtime-owned `sleep(...)`; extra callers receive a typed
`Busy` reply immediately.

By default the lane uses a `FakeSleep` backend so tests are hermetic and
deterministic. The same lane shape also supports a real `tina-aws-bridge`
S3 backend through [`run_against_s3`]. The bridge's two-layer outcome
(`CallOutcome::Full/Closed/Timeout` outer, `S3Error::*` inner) is mapped
into the lane's typed `WorkFailure` reply without losing the exact bridge
rejection reason, worker error, or unexpected response.

## Run

```bash
cargo run --manifest-path examples/systems/system_bounded_object_lane/Cargo.toml
cargo test --manifest-path examples/systems/system_bounded_object_lane/Cargo.toml
```

## Findings

What felt good:
- `ConcurrencyPendingReplies` owns each parked caller and its concurrency
  charge together. Completion tickets carry neither caller authority nor a
  manually released permit, so caller departure and owner stop retire capacity
  structurally.
- Overload is a typed reply (`Busy`) instead of a hidden wait queue.
- The lane reports live, completed, retired, and caller-gone charges and checks
  both parked/live agreement and full settlement accounting.
- Host callers borrow `&LocalSystem` through scoped threads, and
  `run_to_shutdown_reported` retains workload and shutdown failures separately.

What felt rough:
- The mini service wants a standard pressure-report helper so `accepted`,
  `busy`, `completed`, and `in_flight` do not become per-service vocabulary.
  `ConcurrencyPendingReplies::report()` covers the authority-sensitive portion;
  application counters still name accepted work and backend completion.
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
