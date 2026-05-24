# system_copied_service_path_companion

Companion proof for the canonical copied service path.

It keeps compile/API checks and proof-harness assertions out of the skeleton so
the skeleton stays readable.

What it proves:

- the skeleton imports the task-shaped helpers directly;
- `WebSocketSessionMsg::AppControl(Drain)` is a normal app message;
- proof-harness load assertions can be copied without parsing report strings;
- `CallJoinSet` and `CallSelectSet` compile from the public prelude surface.

Smoke:

```sh
cargo test --manifest-path examples/systems/system_copied_service_path_companion/Cargo.toml
```
