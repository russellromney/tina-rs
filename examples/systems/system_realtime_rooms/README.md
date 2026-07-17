# system_realtime_rooms

A production-shaped WebSocket room with a recurring liveness tick. This is
the system specimen for bounded realtime-room behavior: recurring liveness,
slow-peer handling, explicit shutdown, and member-table pressure.

## What this pulls on

- Native server-side WebSocket from `tina-http`: `WebSocketSessionHandle`,
  `WebSocketSessionMsg`, `websocket_upgrade`, `HttpResponse::websocket`.
- The bounded member table helper `tina_http::WebSocketMemberTable` for
  admit / fanout / shutdown / slow-peer eviction. The room isolate keeps
  the idle-eviction policy, the recurring liveness tick, and the bootstrap
  message; the table owns the `BTreeMap<WebSocketSessionId, ...>` and the
  per-outcome counters via [`SendOutcomeAction`].
- A recurring liveness tick — one `sleep_then` self-reschedules the room
  every `presence_tick`, broadcasts a heartbeat to every live member, and
  evicts members whose last activity was older than `idle_evict`.
- An explicit app-control bootstrap installed atomically through
  `register_split_service_with_bootstrap`, so the recurring tick cannot be
  omitted after registration.
- A bounded fan-out path that calls each connection owner with a fixed timeout.
  Every `Full`, `Closed`, `Timeout`, foreign-system, stale-session, or accepted
  outcome returns as a typed `SendOutcome` on the room's event lane.
- Graceful shutdown via the public `WebSocketSessionMsg::Shutdown` variant.
  The room iterates its bounded member table and emits a `Close` frame per
  handle through the ordinary owner — no second writer, no hidden queue.

## What it deliberately does NOT do

- No simulator/replay coverage of raw WebSocket bytes; replay currently uses
  higher-level protocol facts.
- No browser CI; the smoke uses `tungstenite` clients over `ws://`.
- No `permessage-deflate`, no pooled/reconnecting native client manager, no
  HTTP/2 WebSocket — those remain explicit follow-up edges.
- No multi-room sharding; one bounded room is enough to surface the
  recurring-liveness pain.

## Run

Public runner:

```sh
cargo test --manifest-path examples/systems/system_realtime_rooms/Cargo.toml --test public_smoke public_smoke -- --exact
```


```
# Smoke (join + tick, overflow, shutdown):
cargo test --manifest-path examples/systems/system_realtime_rooms/Cargo.toml --test smoke

# Bad-peer + slow-reader proof (the proof):
cargo test --manifest-path examples/systems/system_realtime_rooms/Cargo.toml --test bad_peer -- --nocapture

# Lint:
cargo clippy --manifest-path examples/systems/system_realtime_rooms/Cargo.toml --tests -- -D warnings
```

The `bad_peer` proof drives four typed HTTP bad-peer scenarios from
`tina_proof_harness` (`ResetImmediately`, `HalfClose`, `MalformedFrame`,
`StalledReader`) against the listener — each with its own typed
assertion on what the server did (`bytes_read>0`, `server_closed=true`,
etc.) — then runs one real WebSocket slow-reader scenario that proves
the room evicts the silent peer via the typed `left_idle` counter, then
proves a fresh good client can still join + see ticks. Sample output:

```text
bad_peer label=reset       connected=true connects_ok=1 bytes_sent=0  ...
bad_peer label=half_close  connected=true connects_ok=1 bytes_sent=48 bytes_read=66 server_closed=true ...
bad_peer label=malformed   connected=true connects_ok=1 bytes_sent=27 bytes_read=66 server_closed=true ...
bad_peer label=stalled_reader_no_upgrade connected=true connects_ok=1 bytes_sent=52 bytes_read=64 server_closed=true ...
room.summary live=1 joined=2 left_idle=1 left_slow=0 left_peer=0 \
  presence_ticks=13 shutdown_started=false
```

What this exposes when it fails:

- The harness lines tell you which transport story broke. `connected=false`
  → listener stopped accepting. `server_closed=false` on the half-close /
  malformed cases → the server is treating bad input as an open session.
- `left_idle=0` after the slow-reader window → the recurring tick is
  not aging out silent peers, which means the bounded member table
  leaks under realistic slow-client load.
- `live=N` with N > 0 → the room kept the slow peer alive in its
  member table, which would cap-block subsequent joiners.

The smoke covers three scenarios end-to-end:

1. **Join + tick.** Two real `tungstenite` clients connect and drain. The
   room's recurring `sleep_then` fires at least twice; at least one client
   observes a `tick:N:M` frame on the wire.
2. **Overflow.** The member cap is pinned to 2 and four clients connect.
   Two are admitted; the other two are rejected at `SessionOpen` with a
   server-sent close frame instead of a `join:N` reply.
3. **Shutdown.** Two clients join, the host sends
   `WebSocketSessionMsg::Shutdown`, and at least one client observes the
   close frame on the wire while the room counts every requested,
   completed, and failed close.

## Findings

What felt good:

- The native WebSocket server surface (`WebSocketSessionHandle`,
  `WebSocketSessionId`, `WebSocketSessionMsg`) is enough to build a
  bounded, fan-out room without touching `HttpConnection` internals.
- `register_split_service_with_bootstrap` keeps bounded registration and the
  one startup event in a single host operation.
- The `sleep_then(d, msg)` self-reschedule pattern reads like ordinary
  state-machine code; no hidden cancellation or callbacks were needed.

What felt rough:

- ~~Internal control messages are encoded as `WebSocketSessionMsg::Text`
  with magic prefixes (`__bootstrap__`, `__tick:N`).~~ the copied-service-path pass replaced
  this with `WebSocketSessionMsg::AppControl(WebSocketSessionControl::...)`.
  Control remains an ordinary bounded app message, but it is no longer peer
  text.
- Fanout must choose an explicit owner-call timeout. The table takes that
  timeout at the broadcast call site so the bound remains visible; it does not
  hide a retry queue or silently turn timeout into success.
- The room mailbox orders recipient snapshots, but connections settle their
  sends independently. Counters therefore describe exact per-recipient
  outcomes rather than implying a global wire-completion order.

Tina capability pulled:

- `tina-http` WebSocket (`WebSocketSessionHandle`, `WebSocketSessionMsg`,
  `websocket_upgrade`, `HttpResponse::websocket`).
- `tina_runtime::sleep_then` for the recurring liveness tick.
- `WebSocketMemberTable::broadcast_text` for bounded, outcome-observed owner
  calls whose continuations enter the event lane.
- `LocalSystem::register_split_service_with_bootstrap` for atomic bounded
  registration and startup.

Suggested follow-up:

- Promote the bootstrap-on-register pattern (already on the capacity-aware registration
  list as `register_and_bootstrap`).
- Consider an app-injectable typed variant on `WebSocketSessionMsg` so
  recurring app-level messages do not have to ride on `Text("__...")`
  prefixes.
- The bad-peer proof tried to reach `OutboundQueueFull` via a real WS
  slow reader and could not within the deterministic window — kernel
  TCP send buffers are large on loopback. The proof falls back on the
  room's own typed `left_idle` counter, which is deterministic. If a
  later change makes the connection report `OutboundQueueFull` for
  real slow peers, the proof should be widened to assert `left_slow`
  too.

Verdict:

- keep. The room ships, the recurring tick is bounded, and every fanout offer
  has an exact actor-owned terminal disposition.
