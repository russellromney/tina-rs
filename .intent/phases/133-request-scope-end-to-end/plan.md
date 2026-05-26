# Phase 133: Request Scope End-To-End

## Status

- Future implementation phase.
- One PR.
- Can run beside 131/132 if it owns request-scope adapters, one system
  specimen, docs, and tests.
- Phase 134 budget manifests are shipped. Request-scope caps and child caps
  must surface as manifest rows in the copied system.

## Grug Truth

A request is a tree.

When the caller goes away, the tree stops waiting. Tina-owned rails cancel or
close as strongly as Tina can. External work may finish late, but it must not
become a ghost.

## Current Code Facts

- `RequestScope`, `RequestScopeSet`, `ScopeCancelReport`, and
  `ScopeCancelCause` already exist in `tina-runtime::scope`.
- `CallContext::defer_scoped(scope, label, call_cancelable(...))` already
  exists.
- `DeferredScopedCall::try_admit(...)` already does the all-or-nothing path:
  store pending token, register child in scope, roll back pending storage if
  scope registration fails, and only then return the child effect.
- HTTP/1 response sources already have `ResponseChunkMsg::Cancel`.
- HTTP/1 request-body pulls and HTTP/2 body streams already have call-shaped
  pull/cancel paths.
- Plain `sleep(...)` is not currently `CallHandle`-cancelable. A scoped timer
  path must either add a real cancelable timer rail or ship a ticketed
  tombstone helper that reports "timer ignored after scope cancel" honestly.
- `mini_saas_api` is the current copied service skeleton: HTTP + SQLite +
  keepalive pool + shutdown/report.

So this phase should integrate the existing primitive. Do not rebuild request
scope from scratch.

## Goal

Make request-scoped cancellation the copied path for real services:

- one service request owns one `RequestScope`;
- body streams, timers, pool waits, pool leases, DB/bridge calls, outbound
  calls, and session operations register as scoped children where Tina owns a
  cancel handle;
- cancel produces a report with the same typed cause vocabulary everywhere;
- late external completions remain visible as late/rejected truth;
- request-scope caps appear in the service budget manifest;
- one canonical system demonstrates the whole path.

## Does Not Include

- no fake cancellation of already-accepted external work;
- no cross-shard magic beyond visible `WrongShard`;
- no global request registry;
- no hidden background canceller;
- no retry policy;
- no web framework router.

## Names And Homes

- Keep core names:
  - `RequestScope`
  - `RequestScopeSet`
  - `ScopeCancelReport`
  - `ScopeCancelCause`
- Add:
  - `ScopedRequestReport` as the request-level aggregate. It wraps
    `ScopeCancelReport`, `RequestScopeSetCapacityReport`, late-result counts,
    and unsupported rows. It must not replace `ScopeCancelReport`.
  - `ScopedTimer` / `ScopedTimerSet` only if the phase chooses the tombstone
    timer path. Name it for the user action ("timer for this request"), not
    for internal handle mechanics.
- Add adapters near the resources they adapt:
  - HTTP body/request adapters in `tina-http`;
  - pool/bridge examples in systems/specimens;
  - docs in request-reply and lifecycle pages.

## Implementation

### Rock 1: HTTP Request Scope Copied Path

Add a small copied helper/pattern for HTTP services:

- allocate `RequestScope::with_child_cap(RequestScopeId::alloc(), cap)` when
  the request is admitted;
- store it in a bounded `RequestScopeSet` keyed by request id;
- close admission with `Full(report)` if the set is full;
- cancel with `ClientDisconnect`, `Timeout`, `OwnerStopped`, or explicit
  caller cancel;
- drain/remove the scope on final reply.
- install `RequestScopeSet` and per-request child caps from
  `ServiceBudgetManifest`, not scattered constants, in the copied system.

Use existing `defer_scoped(...).try_admit(...)` for cancelable child work.
Do not return a child effect if pending/scope admission fails.

### Rock 2: Body, Timer, Pool, Bridge Adapters

Prove scoped behavior for:

- response body source: cancel calls `ResponseChunkMsg::Cancel`;
- request body pull: client disconnect/request timeout releases the parked
  pull authority;
- timer/deadline child: either a new cancelable timer rail returns a
  `CallHandle`, or the phase ships a bounded `ScopedTimer` tombstone helper.
  In the tombstone path, the runtime sleep may still fire, but the continuation
  observes the cancelled ticket, skips user work, and reports the ignored
  timer. Do not pretend the timer was physically cancelled.
- pool acquire wait: scope cancel releases caller capacity;
- held pool lease: owner-stop/drain retires or releases according to existing
  pool disposition truth;
- SQLite or SQLx bridge call: caller sees cancel/timeout, worker terminal late
  truth remains visible;
- reqwest or HTTP keepalive outbound call: late completion is not success.

Rails that cannot support scoped cancellation in this phase must emit an
explicit unsupported row in `ScopedRequestReport` and have a test proving the
row fails closed. Do not leave a prose-only unsupported path.

### Rock 3: WebSocket/gRPC Session Operations

Add scoped adapters for operations a request owns:

- WebSocket send/report/close through a session handle;
- gRPC unary call;
- one gRPC streaming pull or cancel path.

Do not claim a whole long-lived WebSocket session is a short HTTP request
scope. Scope only the operation owned by the request.

### Rock 4: Canonical System Proof

Update `examples/systems/mini_saas_api` to use request scopes for at least:

- `POST /items/{id}/notify`;
- a DB lookup;
- a keepalive pool acquire/request/release path;
- a timer/deadline;
- shutdown while one request is active.
- budget-manifest consistency for request scopes and scoped children.

Add one tiny new system `examples/systems/system_scoped_request_tree` focused
on streaming body disconnect. Keep it small: one HTTP route, one body stream,
one timer, one cancelable child, one report.

### Rock 5: Docs

Update:

- request-reply guide: multi-turn + scoped child example;
- lifecycle guide: first-cause-wins cancel report;
- service patterns: "one HTTP request = one request tree";
- `mini_saas_api` README with the exact copied shape.

## Required Proof

- Scope set full returns typed admission failure and returns authority.
- Scope registration failure does not dispatch child work.
- Scoped timer cancellation is honest: either `cancel_call` closes a real
  cancelable timer wait, or a tombstoned timer fires later and is ignored with
  a report row.
- Client disconnect cancels scope and reports child rows.
- Request timeout cancels/tombstones timer work and cancels body/pool/bridge
  waits where those rails expose cancellation authority.
- Double cancel keeps first cause and reports later cancel as redundant.
- Owner stop drains scopes and reports unreleased capacity as zero or names the
  unreleased resource.
- Bridge accepted work may finish late; trace/report names late truth.
- Cross-shard child registered into scope reports `WrongShard`, not success.
- Dropped/closed body source receives cancel or explicit unsupported row.
- Cancel after child completion reports already-settled, not cancelled.
- Stale child completion cannot remove or settle a newer request with the same
  key.
- `mini_saas_api` smoke proves user-visible HTTP response for disconnect,
  timeout, full, shutdown, and late bridge completion.
- `mini_saas_api` budget test proves request-scope surfaces are declared and
  joined with live pressure.
- `system_scoped_request_tree` proves streaming body disconnect without
  depending on the larger SaaS flow.
- Sim/replay agrees where facts are supported. Unsupported facts fail closed.
