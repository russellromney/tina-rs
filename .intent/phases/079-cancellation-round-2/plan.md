# 079 Cancellation Round 2

## Status

- Done: plan created after 066 cancellation, 072 deadlines, 074 body
  streaming, and DB/HTTP bridge work landed.
- In progress: PR 1 — response body source cancel + cancellation truth
  docs table.
- Open: PR 2 — pool/bridge audit fixes that are clearly pulled by code.
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
- No generic `Cancelable` framework until three surfaces need the same
  code. Two is still naming, not abstraction.

## Shape

Two PRs max:

1. Response body source cancel + docs table.
2. Pool/bridge audit fixes that are clearly pulled by code.

If PR 1 gets large, stop there. The body-source leak is the concrete
user-facing bug; bridge cancellation is allowed to remain documented
truth if no boring fix exists.

## Rock 0 — Cancellation State Table

Write and keep a small table in this plan/docs. Update the table when
code changes:

| Surface | Cancel can stop waiting? | Cancel can stop work? | Late result visible? |
|---|---|---|---|
| isolate call before delivery | yes | yes | no |
| isolate call after delivery | yes | no, unless callee cooperates | yes |
| deferred reply slot | yes | callee owns cleanup | yes |
| HTTP response body source | yes | yes — `ResponseChunkMsg::Cancel` tells source to release state | body metric + trace |
| SQLx bridge | yes | best-effort via `pg_cancel_backend` if enabled | metrics/trace |
| SQLite bridge | yes | no, blocking call runs to completion | metrics/trace |
| reqwest bridge | yes | maybe future abort handle; today be honest | trace/metrics |
| pool acquire waiter | yes | yes, reclaim waiter slot | no late work |

This table prevents fake cancellation.

## Rock 1 — Response Body Source Cancel ✅

074 shipped response streaming. If the client disconnects mid-stream,
the source can be left idle; failure is visible through
`body_io_error_count`, but the source does not get a typed cancel.

Built:

- `ResponseChunkMsg::Cancel` — source receives typed cancel and can
  release files, downstream calls, and pending slots;
- `IterBodySource` handles `Cancel` with `stop()`;
- connection isolate sends `Cancel` via `call` on every wire-death
  path (`Read(Err)`, `Wrote(Err)`, `handle_wrote(0)`,
  `handle_stream_chunk(Timeout|Full|Closed)`, peer EOF,
  header deadline) and defensively in `begin_close()`;
- duplicate cancel is harmless — the source either already stopped or
  drops the late message.

Proof:

- `streaming_v2::known_length_client_disconnect_sends_cancel_to_source`
- `streaming_v2::chunked_client_disconnect_sends_cancel_to_source`
- both assert `received_cancel` is true and `body_io_error_count > 0`.

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

If the audit finds only docs mismatch, fix docs and stop. Do not invent
new bridge machinery to satisfy the phase title.

## Rock 4 — Owner Stop Cleanup

Services with stored `CallHandle`s should have one copied cleanup
pattern:

```rust
let effects = self.pending.drain().map(cancel_call(...)).collect();
Effect::Batch(effects plus stop)
```

Do not hide cancel outcomes. A helper may collect effects only if it
returns/feeds every `CancelOutcome` visibly. If that makes the helper
uglier than the loop, keep the loop.

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
