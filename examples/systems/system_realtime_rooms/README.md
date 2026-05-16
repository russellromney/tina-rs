# system_realtime_rooms

A production-shaped WebSocket room with a recurring liveness tick. This is
the system specimen the Phase 094 "Still not done" list calls out as
*"production realtime-room system specimen with recurring liveness"* and the
entry in `examples/systems/README.md` for `system_realtime_rooms`.

## What this pulls on

- Native server-side WebSocket from `tina-http`: `WebSocketSessionHandle`,
  `WebSocketSessionMsg`, `websocket_upgrade`, `HttpResponse::websocket`.
- A bounded member table (one room, fixed capacity, fill-close-refill safe).
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
cargo test --manifest-path examples/systems/system_realtime_rooms/Cargo.toml
cargo clippy --manifest-path examples/systems/system_realtime_rooms/Cargo.toml --tests -- -D warnings
```

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

Verdict:

- keep. The room ships, the recurring tick is bounded, the slow-peer
  policy is explicit, and the rough bits are now written down.
