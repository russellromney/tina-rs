# Specimen WebSocket Room

Paired Tokio-vs-Tina implementation of a tiny chat-room broadcast over real
WebSocket connections. Two clients (`alpha`, `bravo`) connect to `/ws`, each
publishes one text frame, and each is asserted to receive both broadcasts.

Run both sides:

```bash
cargo run --manifest-path examples/specimen_ws_room/Cargo.toml -- compare
```

Run one side:

```bash
cargo run --manifest-path examples/specimen_ws_room/Cargo.toml -- tokio
cargo run --manifest-path examples/specimen_ws_room/Cargo.toml -- tina
```

## What this comparison taught us

### Tokio side

- `tokio::sync::broadcast::channel(64)` does almost the entire job. Each
  connection clones the sender and `subscribe()`s for a receiver. About 40
  lines of handler code total.
- Pressure on slow readers becomes `RecvError::Lagged`, which the Tokio
  recipe is to silently swallow. There is no obvious affordance for
  *visible* slow-reader behavior — the broadcast channel decides for you.

### Tina side

- The `Room` isolate genuinely owns `Vec<UnboundedSender<String>>`. Adding
  / removing subscribers and fan-out are three lines each, and the
  `retain(|tx| tx.send(...).is_ok())` form prunes dead subscribers in the
  same step as the publish. That is satisfying — no separate "garbage
  collect dead subscribers" pass.
- Same `TinaTowerService` + `BridgeRequest<RoomRequest, RoomReply>` shape
  as the axum counter. The composition story is now visibly consistent
  across two HTTP-shaped comparisons. Each per-connection task clones
  the service, which clones the underlying bridge handle — cheap
  Arc-clone, no admission ceremony.
- Pressure on slow readers (if we used a bounded channel) would surface as
  a `try_send` failure inside the handler, which is exactly the point.
  This example uses `UnboundedSender` to keep parity with the Tokio side
  for now; visible slow-reader semantics are a deliberate next step.

### What was awkward

- ~~Bridge mailbox + factory boilerplate again. Fourth copy.~~
  **Resolved in phase 047:** the room uses `DefaultThreadedMailboxFactory`.
- ~~The macro would not let me omit `shard = RoomShard` even for a
  one-isolate service.~~ **Resolved in phase 047:** omitted `shard = ...`
  defaults to `SingleShard`, so this comparison no longer declares a
  throwaway shard.
- Connection plumbing still happens in tokio code (axum WS upgrade), and
  the Tina runtime only owns the room-state isolate. That is the honest
  shape of using the bridge — but it means liveness/ping-pong concerns
  end up entirely on the Tokio side. Tina has no visible role in
  per-connection liveness in this example.
- The bridge `call(...).await` cost shows up twice per published frame
  (once for `Subscribe`, once per `Publish`). For a real chat workload
  with many small frames, the Tokio direct-broadcast path is going to
  win on bytes-per-broadcast latency, and the Tina story has to argue
  visibility instead of speed.
- Two `svc.clone()`s inside `handle_socket` — one for the Subscribe
  call before splitting, one for the per-frame Publish loop. They're
  needed because Tower's `&mut self` is per-clone, not shared, and
  the reader/writer halves run on different async tasks. The pattern
  is standard Tower but it took a real moment to realise the obvious
  "share via `&mut`" path doesn't compile. Now documented in
  `tina-tower-bridge`'s "Cloning and `&mut self`" section, with a
  fan-out snippet specifically for this WebSocket shape.
- The `RoomService` type used to inherit the same six-generic
  brutality as the counter; the bridge polish slice replaced it
  with `TinaService<RoomRequest, RoomReply>`.
