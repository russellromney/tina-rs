# Phase 133: Request Scope End-To-End

## Status

- Future implementation phase.
- Runs after Phase 120 if it wants the refreshed service skeleton.
- One PR.

## Grug Truth

A request is a tree.

When the client goes away, the tree should stop waiting. Tina-owned rails should
cancel or close as strongly as they can. External work may finish late, but it
must not become a ghost.

## Goal

Make request-scoped cancellation the copied path for real services.

Ship:

- request scope wired through HTTP request handling
- body stream cancel
- timer cancel
- pool acquire / lease cancel
- DB/bridge late-result truth
- WebSocket/gRPC session close integration, adding scoped adapters where needed
- one request-shaped report

## Starting Facts

- `RequestScope`, `RequestScopeSet`, and scoped call helpers exist.
- Scope cancel is single-shard honest; cross-shard cancel reports
  `WrongShard`.
- Protocol/body/pool/bridge resources have their own close/report surfaces.
- Current services still wire much of this by hand.

## Does Not Include

- no fake cancellation of already-accepted external work
- no cross-shard magic beyond visible `WrongShard`
- no global request registry
- no hidden background canceller
- no retry policy
- no broad web framework

## Decisions

- User-facing names:
  - `RequestScope`
  - `RequestScopeReport`
  - `ScopedRequest`
  - `ScopedRequestSet`
- Cancellation causes stay typed:
  - client disconnect
  - caller cancel
  - timeout
  - owner stop
  - shutdown drain
  - user-defined
- A scope report must include:
  - registered children
  - cancel cause
  - cancelled count
  - already-settled count
  - wrong-shard rows
  - late-result count if observed
  - unreleased capacity after drain
- Bridge/external late completion stays worker-terminal plus
  `CallReplyRejected`; do not convert it to success or silence.

## Implementation

### Rock 1: HTTP Scoped Request Path

Add a copied path for native HTTP handlers:

- create scope on request admission
- register body/source/DB/timer child work
- cancel scope on client disconnect or request timeout
- include scope report in service shutdown/report path

### Rock 2: Body And Stream Cancel

Wire cancel through:

- request body stream pull
- response body source
- WebSocket session close where request owns a session operation
- gRPC stream scoped adapter for unary and one streaming mode

If a protocol cannot support a scoped child in this phase, add an explicit
unsupported row in the scope report and a test that proves it fails closed.

### Rock 3: Pool And Bridge Integration

Prove scoped:

- pool acquire waiting
- held pool lease during request
- SQLite or SQLx bridge call
- local reqwest bridge call against a hermetic in-process server

Caller timeout and worker-terminal late completion remain distinct.

### Rock 4: Canonical System

Update one production-shaped system:

- HTTP request
- streaming body or response
- DB/pool operation
- outbound bridge/client call
- timer/deadline
- shutdown while request active

The README should show the copied path and the report.

## Required Proof

- Client disconnect cancels scope and reports child rows.
- Request timeout cancels timer/body/pool/bridge waits.
- Owner stop drains scopes and reports unreleased capacity as zero.
- Bridge accepted work may finish late; trace/report names the late truth.
- Cross-shard child registered into scope reports `WrongShard`, not success.
- A dropped/closed body source receives cancel or an explicit unsupported row.
- Scope cap full returns typed admission failure and returns authority.
- Live test and sim/replay proof agree where facts are supported; unsupported
  facts fail closed.
