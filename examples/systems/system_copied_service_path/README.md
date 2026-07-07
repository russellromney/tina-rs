# system_copied_service_path

Canonical copied Tina service skeleton for the copied-service-path pass.

Copy this shape when building a normal service. The point is not that this
crate is a complete product; the point is that the normal service path names
every hard edge in one place:

- call another service with a native protocol client;
- limit work with visible `Full` / `Rejected` / `Timeout`;
- recover durable state before readiness;
- control session apps with `WebSocketSessionMsg::AppControl`, not peer text;
- prove progress and clean shutdown with proof-harness assertions;
- capture this run with `RunCapture`, then save/replay/shrink the bug;
- join all calls with `CallJoinSet`;
- select next completed call with `CallSelectSet`.

The heavier proof is in
`../system_copied_service_path_companion`. A cheap-model smoke copy is in
`../system_copied_service_path_smoke`.

## Request Entry

The copied service starts with a bounded request entry and reports the route
in `CopiedServiceReport::request_entry`:

```rust
let report = system_copied_service_path::run_copied_service_path();
assert_eq!(
    report.request_entry,
    "HTTP request -> gateway isolate -> bounded reply",
);
```

## Capture This Run

Install the observer before constructing the runtime or local system:

```rust
let capture = tina_proof_harness::RunCapture::new("my_service_bug");
let observer = capture.observer();
// pass `observer` into the runtime builder before the first event
```

Finishing a capture intentionally requires the replay facts. The helper fills
source metadata and trace pressure, but it does not guess your history,
mailbox roles, invariant, or unsupported live facts.

## Control This Session

Session app control is ordinary bounded message delivery:

```rust
use tina_http::{WebSocketSessionControl, WebSocketSessionMsg};

let msg = WebSocketSessionMsg::AppControl(WebSocketSessionControl::Start);
```

The WebSocket connection owner never emits this from peer text. If the room
mailbox is full or closed, the caller sees the same bounded send truth as any
other message.

## Join All Calls

Use `CallJoinSet` when the service needs every branch to reach a terminal
reply or explicit cancel outcome:

```rust
let join: tina_runtime::CallJoinSet<&'static str, ()> =
    tina_runtime::CallJoinSet::with_capacity(2);
assert_eq!(join.capacity(), 2);
```

On owner stop, call `drain_pending_for_cancel()`, emit one visible
`cancel_call` effect per request, then feed the cancel continuations back with
`record_cancel(...)`. Late replies after the drain are still recorded; they do
not silently disappear.

## Select Next Completed Call

Use `CallSelectSet` when each branch should be handled as soon as it finishes:

```rust
let select: tina_runtime::CallSelectSet<&'static str, ()> =
    tina_runtime::CallSelectSet::with_capacity(2);
assert_eq!(select.capacity(), 2);
```

`partial_report()` is safe to expose while other branches are live: it names
completed branches, explicit cancels, and the remaining pending count.

Smoke:

```sh
cargo run --manifest-path examples/systems/system_copied_service_path/Cargo.toml
cargo test --manifest-path examples/systems/system_copied_service_path/Cargo.toml
```
