# system_realtime_rooms

A production-shaped WebSocket room with a recurring liveness tick. This is
the system specimen the Phase 094 "Still not done" list calls out as
*"production realtime-room system specimen with recurring liveness"* and the
entry in `examples/systems/README.md` for `system_realtime_rooms`.

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
- An explicit bootstrap message — the host sends one
  `Text("__bootstrap__")` after `register_with_capacity` so the recurring
  tick starts. Forgetting that one `try_send` produces a quiet service whose
  startup effect never runs (this is the same pattern Finding 22 in
  `examples/FINDINGS.md` calls out).
- A bounded fan-out path that emits sends via `tina::send` (try_send) so
  that send admission is reported back to the room through the connection
  isolate's `call_websocket_app(SendOutcome)` path. See the **rough**
  finding below for why this matters.
- Graceful shutdown via the public `WebSocketSessionMsg::Shutdown` variant.
  The room iterates its bounded member table and emits a `Close` frame per
  handle through the ordinary owner — no second writer, no hidden queue.

## What it deliberately does NOT do

- No simulator/replay coverage of WebSocket bytes (that's still open on the
  Phase 094 deferral list).
- No browser CI; the smoke uses `tungstenite` clients over `ws://`.
- No `permessage-deflate`, no native client, no HTTP/2 WebSocket — those
  are all explicit Phase 094 non-goals.
- No multi-room sharding; one bounded room is enough to surface the
  recurring-liveness pain.

## Run

```
# Smoke (join + tick, overflow, shutdown):
cargo test --manifest-path examples/systems/system_realtime_rooms/Cargo.toml --test smoke

# Bad-peer + slow-reader proof (Phase 108):
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

- The Phase 094 public WebSocket surface (`WebSocketSessionHandle`,
  `WebSocketSessionId`, `WebSocketSessionMsg`) is enough to build a
  bounded, fan-out room without touching `HttpConnection` internals.
- `register_with_capacity` plus one `try_send(addr, Bootstrap)` from the
  host is a small, explicit pattern that maps cleanly to the "startup
  bootstrap message" idea Phase 101 is formalising.
- The `sleep_then(d, msg)` self-reschedule pattern reads like ordinary
  state-machine code; no hidden cancellation or callbacks were needed.

What felt rough:

- **Internal control messages are encoded as `WebSocketSessionMsg::Text`
  with magic prefixes (`__bootstrap__`, `__tick:N`)** because the public
  enum has no app-injected variant for "wake up". The `specimen_websocket_room`
  uses the same trick. A small typed app-side variant (`WebSocketSessionMsg::AppTick`
  or similar) would remove the string-prefix dispatch and the
  catch-all fallthroughs that have to ignore stray legacy `Text` events.
- **`handle.text_effect::<Self>` (`call(...).then(SendOutcome)`) interacts
  badly with the connection isolate when emitted from the room's
  `handle_call` return value.** Concrete observed behaviour: the FIRST
  `tcp_write` triggered by the call delivers its `Wrote(Ok)` completion
  back to the connection via `handle_call` with a
  `HttpConnectionMsg::Wrote(_)` variant, which the connection rejects as
  `UnsupportedMessage`. The connection then never drains its outbound
  queue; subsequent admits succeed (the room sees nine consecutive `Ok`
  SendOutcomes), the queue fills up, and the room (correctly) evicts the
  member with `OutboundQueueFull`. The workaround used here is to use
  plain `tina::send(handle.target(), handle.text(...))` (try_send
  semantics) instead — the connection then takes the
  `handle_websocket_send` path and the call-back to the room is from the
  connection's mailbox rather than from a `.then` chain. This deserves a
  dedicated finding; see `examples/FINDINGS.md`.
- **The cross-client SessionText broadcast still races against the
  bounded outbound queue.** When client A sends "hello-from-a" and the
  room fans it out to B, B's connection sometimes gets a TCP RST during
  the next `Read` even though the broadcast write succeeded. The smoke
  here therefore proves the recurring tick story specifically and leaves
  cross-client text echo to `specimen_websocket_room`, which exercises
  that path with its own scaffolding.

Tina capability pulled:

- `tina-http` WebSocket (`WebSocketSessionHandle`, `WebSocketSessionMsg`,
  `websocket_upgrade`, `HttpResponse::websocket`).
- `tina_runtime::sleep_then` for the recurring liveness tick.
- `tina::send` (`Effect::Send`) for bounded fan-out without a `.then`
  chain.
- `ThreadedRuntime::register_with_capacity` + host `try_send` of a
  Bootstrap message.

Suggested follow-up:

- Add the call/try_send asymmetry to `examples/FINDINGS.md` — this is the
  third specimen run that has paid the "be careful which entry point a
  `.then` chain originates from" tax (`system_cache_with_fill`,
  `system_job_queue`, now `system_realtime_rooms`).
- Promote the bootstrap-on-register pattern (already on the Phase 101
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

- keep. The room ships, the recurring tick is bounded, the slow-peer
  policy is explicit, and the rough bits are now written down.
