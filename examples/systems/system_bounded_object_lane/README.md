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
`RunReport::work_failures` retains those values instead of reducing them to a
failure count.

`RunConfig::validate` rejects zero callers/in-flight capacity and oversized
mailbox/duration values before runtime or barrier construction. Zero mailbox
remains available for intentional host-`Full` proof (`run_put_terminals`).

## Run

```bash
cargo run --manifest-path examples/systems/system_bounded_object_lane/Cargo.toml
cargo test --manifest-path examples/systems/system_bounded_object_lane/Cargo.toml
cargo test --manifest-path examples/systems/system_bounded_object_lane/Cargo.toml --test public_smoke public_smoke -- --exact
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

What stays application-specific:
- `ConcurrencyPendingReplies::report()` covers the authority-sensitive state;
  `accepted`, `busy`, and backend completion remain application counters because
  they describe lane policy rather than generic runtime pressure.
- The real S3 temptation was useful: completion delivery must be observed, not
  best-effort `try_send`, or in-flight capacity can leak.

Tina capability pulled:
- Multi-turn request/reply.
- Runtime-owned time.
- Bounded in-flight admission.
- Host-side concurrent calls.
- Typed AWS bridge outcome shape.

The real S3 path:
- `run_against_s3` accepts `S3Config`, installs the bridge through the same
  `LocalSystem` as the lane, and never accepts a foreign runtime address.
- Typed install, application, bridge-drain, combined application-plus-drain,
  startup, and facade-shutdown failures remain distinct.
- The bridge is closed and drained while its facade is still alive;
  `S3RunReport` makes both `workload` and successful `drain` mandatory.
- A hermetic HTTP endpoint test exercises this full path without AWS access.

Verdict:
- keep
