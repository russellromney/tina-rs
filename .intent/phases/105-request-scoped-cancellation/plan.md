# Phase 105: Request-Scoped Cancellation

## Status

- IDD implementation phase.
- Builds on owner cancellation and `PendingCancelableCallSet`.

## Grug Truth

A request is a tree.

When the request dies, its children should stop waiting. Tina-owned rails should
release promptly. External work may still finish late, and Tina must say that.

## Goal

Add request-scoped cancellation without making every service invent a registry.

## Non-Goals

- No fake kill for SQL, AWS, reqwest, or other external systems.
- No cancellation on drop unless the type name says it.
- No hidden retries.
- No unbounded scope child table.
- No "context bag" for arbitrary user values.

## API Shape

Build this explicit first form:

- `RequestScopeId`
- `RequestScope`
- `ScopedCallHandle`
- `ScopeCancelReport`
- bounded `RequestScopeSet`

The service creates/adopts a scope, admits cancelable children into bounded
storage, and cancels the scope on:

- client disconnect
- caller cancel
- timeout
- owner stop
- explicit user action

Failed admission returns the caller authority or pending token so the caller can
be answered. No stranded authority.

## Rocks

### Rock 1: Scope Storage

Build bounded request scope storage:

- fixed cap
- duplicate id rejection
- child cap
- cleanup on complete/cancel/timeout/owner stop
- capacity report

No growing map pretending to be bounded.

### Rock 2: Tina-Owned Rail Cancellation

Wire scope cancellation into Tina-owned rails where the runtime owns the wait:

- sleep/deadline
- TCP read/write/accept
- TLS read/write/accept
- body stream source
- pool acquire

Late completions become tombstoned/rejected trace facts, not mystery events.

### Rock 3: External Work Honesty

For bridges:

- scope cancel stops waiting and reclaims caller capacity
- bridge worker may still finish
- late result is counted/traced as worker-terminal after abandonment where the
  bridge can observe it

Docs must say "cancel request" is not "database rolled back" unless a bridge
operation proves that.

### Rock 4: HTTP/WebSocket/gRPC Integration

Hook scopes into connection/session lifetimes:

- HTTP client disconnect cancels request scope.
- WebSocket close cancels session-owned in-flight work.
- gRPC reset/cancel maps to scope cancel.
- Server shutdown cancels scopes after drain deadline.

### Rock 5: RequestContext Helper

Make the common pattern easy:

```rust
call_ctx
    .defer_scoped(scope, work)
    .reply(key, Msg::Done)
```

The helper first admits storage, then returns the child effect. Failed admission
returns the authority/effect pair so the service can answer deliberately.

## User Proof

Update these proof surfaces:

- `mini_saas_api`: client disconnect or request timeout cancels DB/outbound/timer
  child work and returns visible late-result facts.
- `system_media_ingest_pipeline`: streaming upload cancel stops Tina-owned
  body/process work and cleans caps.
- `system_job_queue`: request-scope cancel cancels queued/running waits and
  proves fill-cancel-refill.

Each README must show: request went away, these children were cancelled, these
external tasks may still finish late.

## Required Proof

- Fill-cancel-refill proves capacity is reclaimed.
- Cancel before delivery.
- Cancel after delivery before reply.
- Cancel after deferred context capture.
- Cancel after bridge accepts work and completes late.
- HTTP disconnect cancels DB/outbound/timer children.
- Owner stop cancels scopes and emits final report.
- Cross-shard caller/callee keeps cancel cause.
- System smoke proof for `mini_saas_api`, `system_media_ingest_pipeline`, and
  `system_job_queue` cancel paths.
- DST case for a scope with at least two child rails.

## Done Means

A user can model "this web request is gone; stop its work" without a custom
registry, and the answer is honest for both Tina-owned work and external work.
