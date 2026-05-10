# 079 Cancellation Round 2

## Status

- Done: plan created after 066 cancellation, 072 deadlines, 074 body
  streaming, and DB/HTTP bridge work landed.
- In progress: none.
- Open: apply cancellation to resource-holding surfaces.
- Deferred: universal preemptive cancellation, OS-level kill semantics,
  exact DB cancellation guarantees.

## Goal

066 made caller-owned call cancellation real.

Now fix the user-facing weak spot:

```text
cancel sometimes means stop waiting.
resource may still work.
source may still hold state.
bridge may still finish late.
```

This phase names the surfaces where Tina can do better, and refuses to
lie where it cannot.

Grug truth:

```text
if we can release resource, release it.
if backend can cancel, ask it.
if backend may keep working, say so.
late result is a trace fact, not a ghost.
```

## Non-Goals

- No claim that cancelling SQLx/rusqlite/reqwest always kills remote
  work.
- No hidden retry.
- No global cancel-all hammer without ownership.
- No unbounded cancellation registry.
- No fake "task abortion" story for work Tina does not own.

## Rock 0 — Cancellation State Table

Write and keep a small table in this plan/docs:

| Surface | Cancel can stop waiting? | Cancel can stop work? | Late result visible? |
|---|---|---|---|
| isolate call before delivery | yes | yes | no |
| isolate call after delivery | yes | no, unless callee cooperates | yes |
| deferred reply slot | yes | callee owns cleanup | yes |
| HTTP response body source | yes | should send `Cancel` to source | body metric + trace |
| SQLx bridge | yes | best-effort via `pg_cancel_backend` if enabled | metrics/trace |
| SQLite bridge | yes | no, blocking call runs to completion | metrics/trace |
| reqwest bridge | yes | maybe future abort handle; today be honest | trace/metrics |
| pool acquire waiter | yes | yes, reclaim waiter slot | no late work |

This table prevents fake cancellation.

## Rock 1 — Response Body Source Cancel

074 shipped response streaming. If the client disconnects mid-stream,
the source can be left idle; failure is visible through
`body_io_error_count`, but the source does not get a typed cancel.

Build:

- `ResponseChunkMsg::Cancel` or equivalent;
- connection isolate sends cancel when wire dies or response is
  abandoned;
- source can release files, downstream calls, and pending slots;
- duplicate cancel is harmless/typed.

Proof:

- source receives cancel on client disconnect;
- source stops and releases its owned state;
- body I/O error metric still increments;
- known-length and chunked paths both covered.

## Rock 2 — Pool Acquire Cancellation

When a caller cancels or owner stops while parked on a pool acquire,
waiter capacity must be reclaimed.

Audit `WorkerPool` and keepalive pool use:

- explicit cancel;
- caller timeout;
- owner stop;
- pool close drain;
- pool close force.

Add missing tests or helpers. Do not add background sweep unless a
bounded explicit sweep is the only honest path.

## Rock 3 — Bridge Cancellation Audit

Audit:

- `tina-sqlx-bridge`;
- `tina-sqlite-bridge`;
- `tina-reqwest-bridge`;
- `tina-tokio-bridge`;
- `tina-tower-bridge`.

For each bridge, document:

- what caller `CallOutcome::Timeout` means;
- what bridge internal timeout means;
- whether backend work continues;
- whether any real cancel rail exists;
- how late result is counted/traced.

Implement only the boring wins:

- SQLx already has opt-in DB cancel; make docs/tests line up.
- SQLite cannot cancel blocking work; document and prove late truth.
- Reqwest may need a future abort-handle design; do not improvise if
  it changes bridge shape.

## Rock 4 — Owner Stop Cleanup

Services with stored `CallHandle`s should have one copied cleanup
pattern:

```rust
let effects = self.pending.drain().map(cancel_call(...)).collect();
Effect::Batch(effects plus stop)
```

Do not hide cancel outcomes. A helper may collect effects, but user code
must still decide what `CancelOutcome` means if it matters.

## Rock 5 — Docs

Add one loud user-guide section:

```text
cancel means Tina stops waiting unless the surface says it can stop work.
```

Include examples:

- isolate call cancel;
- response streaming cancel;
- SQLx best-effort DB cancel;
- SQLite no-cancel late result.

## Done Means

- Response body sources can be cancelled on abandoned wire.
- Pool acquire waiter cancellation/reclaim is proven.
- Bridge cancellation truth is documented and tested where possible.
- No public doc claims "cancel killed work" unless the specific surface
  really can.
- Roadmap/changelog updated.
