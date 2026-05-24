# Phase 131: Outbound Connect And Session Managers

## Status

- Future implementation phase.
- Runs after the current 120/129/130 wave, or after any rebases it needs.
- One PR. If it gets too large, land `ConnectPolicy` + WebSocket manager first
  and leave HTTP/2/gRPC pool adapters as explicit follow-up commits on the same
  phase branch.

## Grug Truth

Real services are clients too.

One WebSocket/HTTP2/gRPC connection working once is not enough. Production
clients reconnect, shed, report stale sessions, close cleanly, and say which
address path failed.

## Goal

Make native outbound clients production-shaped without hiding queues or retry.

Ship:

- DNS/connect policy
- bounded reconnect policy
- WebSocket client manager
- HTTP/2 and gRPC pooled manager shape
- stale session replacement truth
- pressure/lifecycle reports
- live and sim/replay facts where Tina can model them

## Starting Facts

- `tina-http::websocket_client` is explicitly one session, not a reconnecting
  manager.
- HTTP/1 keepalive pooling exists.
- HTTP/2 and gRPC native clients exist.
- DNS is a bounded lane but has no address-family / Happy-Eyeballs policy.
- Request scopes and resource lifecycle reports exist.

## Does Not Include

- no hidden infinite reconnect loop
- no unbounded client pool
- no remote service discovery
- no load balancer with dynamic membership
- no global session manager
- no fake cancellation of work already accepted by an outside system
- no HTTP/3/QUIC
- no protocol feature expansion except what managers require

## Decisions

- User-facing name: `ConnectPolicy`.
- User-facing manager names:
  - `WebSocketClientManager`
  - `Http2ClientPool`
  - `GrpcClientPool`
- Reconnect is explicit policy:
  - max attempts
  - backoff
  - max live sessions
  - stale-session close behavior
  - total queue cap
- Manager admission returns typed outcomes:
  - `Admitted`
  - `Full(report)`
  - `Closed(report)`
  - `NoHealthyEndpoint(report)`
  - `ConnectFailed(report)`
  - `TimedOut(report)`
- DNS/connect result must name:
  - host
  - port
  - resolved addresses
  - attempted addresses
  - address family
  - timeout / full / closed / refused / TLS failure
- Happy Eyeballs first form:
  - fixed delay between IPv6 and IPv4 attempts
  - bounded max attempts
  - deterministic order in sim
  - visible loser close/cancel truth
- WebSocket manager is first proof because it has long-lived sessions,
  reconnect, ping/pong, close, and slow-peer pressure.
- HTTP/2/gRPC pools use a fixed endpoint list + round-robin +
  no-healthy-endpoint. No discovery.

## Implementation

### Rock 1: Connect Policy

Add a small policy type near outbound protocol code:

- `ConnectPolicy`
- `AddressFamilyPolicy`
- `HappyEyeballsPolicy`
- `ConnectAttemptReport`
- `ConnectReport`

Use runtime DNS and TCP/TLS calls. Do not spawn a background resolver.

### Rock 2: WebSocket Client Manager

Build a bounded manager over `WebSocketClientConnection`:

- owns at most `max_sessions`
- reconnects only when policy says so
- exposes send/receive/report/close calls
- replaces stale sessions visibly
- keeps old session close reports
- reports wrong-lane messages
- drains on shutdown

### Rock 3: HTTP/2 And gRPC Pool Shape

Add first manager/pool shape:

- fixed endpoints
- max connections
- max in-flight streams per connection
- idle close
- stale connection retire
- no-healthy-endpoint outcome
- pressure report

Keep protocol-specific truth. Do not collapse HTTP/2 reset and gRPC status.

### Rock 4: Specimens

Update or add systems:

- realtime room uses WebSocket manager for an outbound client path
- small gRPC client service uses `GrpcClientPool`
- one closed-port/reconnect storm proof

## Required Proof

- WebSocket manager reconnects after peer close and reports old session stale.
- Slow reader fills bounded outbound queue and returns `Full`, not buffering.
- Reconnect storm with closed port has deterministic typed failures.
- DNS full and DNS timeout surface distinctly from TCP connect failure.
- Happy Eyeballs attempts are bounded and reported.
- HTTP/2/gRPC pool returns `NoHealthyEndpoint` when every endpoint is closed.
- Shutdown closes sessions and returns a manager report.
- Sim/replay either reproduces supported facts or records explicit unsupported
  facts. No silent exact replay claim.
- No hidden queue: tests fill every queue/pool/session cap and assert typed
  pressure.
