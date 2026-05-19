# Phase 127: Unified Session And Stream Lifecycle

## Status

- Future implementation plan for the second post-122 core wave.
- Should run after Phase 116 and after enough of Phase 119 exists to reuse
  resource lifecycle words. Can overlap with protocol completion if ownership is
  split carefully.

## Purpose

Make long-lived sessions boring across protocols.

User story:

```text
my WebSocket, HTTP/2, gRPC, and TCP sessions all open, drain, close, cancel,
and report in the same way
```

## Includes

- shared lifecycle vocabulary: open, active, idle, draining, closed, failed
- close/cancel/drain/report behavior across TCP sessions, HTTP/2 streams, gRPC
  streams, and WebSocket sessions
- slow-peer policy
- half-close/reset/idle-timeout truth
- per-session pressure report
- graceful server shutdown draining sessions with deadline
- session registry/table helper if repeated code proves it

## Does Not Include

- no remoting/clustering
- no global session manager
- no fake cancellation of external work
- no per-protocol lifecycle dialect unless the protocol truly needs it
- no hidden retry or reconnect

## Must Not Change

- Existing HTTP/1, HTTP/2, gRPC, and WebSocket success paths keep their current
  wire behavior.
- Existing protocol error mappings stay stable unless this phase explicitly
  adds a more precise typed outcome and updates tests.
- Existing request-scoped cancellation truth remains: Tina stops waiting and
  Tina-owned rails close only where the rail contract supports it.

## Implementation Shape

Use common nouns:

```text
SessionState
SessionCloseReason
SessionDrainReport
SessionPressureReport
SlowPeerPolicy
IdlePolicy
```

Rules:

- Stop accepting new work before drain.
- Drain waits for accepted in-flight work until a deadline.
- Force close marks still-owned work closed/stale and reports it.
- Slow-peer action is explicit: shed message, close session, or backpressure.
- Half-close and reset are different outcomes.
- Idle timeout closes or reports why close could not run.
- Reports include accepted, completed, cancelled, reset, timed out, full,
  high-water, final-current.
- Request-scoped cancellation feeds session close/cancel through the same report
  vocabulary. No orphan body/source/session work after caller disconnect.

## User Proof Specimens

- chat/room service: many sessions, one slow peer, active peer continues
- gRPC bidi stream: shutdown drains accepted messages and rejects late sends
- HTTP/2 streaming response reset: body source receives cancel/close truth
- TCP half-close service: peer close does not leak rail ownership

## Required Proof

- WebSocket slow reader evicted or backpressured with report
- HTTP/2 stream reset while body source is active settles body source visibly
- gRPC bidi shutdown drains accepted messages and rejects late messages
- TCP half-close and reset do not leak resource ownership
- graceful server shutdown stops ingress, drains sessions, force-closes
  leftovers, emits report
- idle timeout does not leave a live owned stream with no owner
- simulator or protocol-fact replay records supported lifecycle facts; unsupported
  byte-level replay is declared explicitly
- blast-radius proof: existing HTTP/1 keepalive, HTTP/2 strictness, gRPC
  streaming, and WebSocket browser tests still pass

## Hostile Review Notes

- Do not let each protocol invent names for the same lifecycle.
- Do not call a session closed if the rail is still owned and live.
- Do not hide slow-peer buffering.
- Do not make graceful shutdown docs stronger than the code.
