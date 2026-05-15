# Bounded Object Lane

This system is a tiny stand-in for an S3-style worker lane.

Many host callers concurrently submit `Put` calls to one Tina isolate. The
isolate admits only `lane_in_flight` operations at once. Admitted work is
represented by runtime-owned `sleep(...)`; extra callers receive a typed
`Busy` reply immediately.

With the optional `real-s3` feature, the same lane can send admitted work to a
bounded S3 bridge worker and PUT small objects to a real S3-compatible bucket.
The Tina isolate still owns admission and pressure; the bridge is only the
side-effect adapter.

## Run

```bash
cargo run --manifest-path examples/systems/system_bounded_object_lane/Cargo.toml
cargo test --manifest-path examples/systems/system_bounded_object_lane/Cargo.toml
```

Real S3 mode is opt-in and env-configured:

```bash
OBJECT_LANE_S3_BUCKET=my-bucket \
OBJECT_LANE_S3_PREFIX=tina-object-lane/ \
OBJECT_LANE_CALLERS=10 \
OBJECT_LANE_IN_FLIGHT=2 \
OBJECT_LANE_CALL_TIMEOUT_MS=10000 \
cargo run \
  --manifest-path examples/systems/system_bounded_object_lane/Cargo.toml \
  --features real-s3
```

This writes objects named like `tina-object-lane/object-0`; clean up the prefix
after manual runs if you point it at a real bucket.

Useful optional env vars:
- `OBJECT_LANE_S3_REGION`
- `OBJECT_LANE_S3_ENDPOINT_URL` for MinIO/localstack/S3-compatible services
- `OBJECT_LANE_S3_FORCE_PATH_STYLE=true`
- `OBJECT_LANE_S3_BODY_BYTES=16`
- `OBJECT_LANE_S3_OPERATION_TIMEOUT_MS=10000`

## Findings

What felt good:
- The in-flight cap is ordinary isolate state, not a pool mutex or scheduler
  side effect.
- Overload is a typed reply (`Busy`) instead of a hidden wait queue.
- `RequestContext` plus `reply_with_request` makes multi-turn replies explicit.
- The real S3 path can be swapped in without changing the pressure contract:
  fake work and real PUTs both go through the same admit/reply shape.

What felt rough:
- The request-context shape is correct, but still visually heavy for a common
  "accept now, reply after runtime call" path.
- The mini service wants a standard pressure-report helper so `accepted`,
  `busy`, `completed`, and `in_flight` do not become per-service vocabulary.
- Real S3 currently uses a specimen-local bridge worker. That is fine for a
  probe, but a reusable AWS bridge should eventually own completion delivery,
  cancellation, and terminal reporting as first-class Tina vocabulary.

Tina capability pulled:
- Multi-turn request/reply.
- Runtime-owned time.
- Bounded in-flight admission.
- Host-side concurrent calls.
- Optional bounded bridge to real S3-compatible storage.

Suggested follow-up:
- A small generic "lane pressure report" shape would help S3/DB/HTTP pool
  examples converge on one vocabulary.
- A real `tina-aws-bridge` should reuse this admission shape but provide typed
  AWS outcomes, cancellation, shutdown, and pressure reports.

Verdict:
- keep
