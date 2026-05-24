# system_copied_service_path_smoke

Executable cheap-model proof copied from `../system_copied_service_path`.

Checklist:

- What I copied: the skeleton crate dependency and `smoke_line()` entry.
- What was not obvious: nothing outside the skeleton README was needed; the
  helper names were task-shaped enough to find from the checklist.
- What got fixed in the copied path: session control, load assertions, and
  join/select helper names are task-shaped.

Smoke:

```sh
cargo run --manifest-path examples/systems/system_copied_service_path_smoke/Cargo.toml
cargo test --manifest-path examples/systems/system_copied_service_path_smoke/Cargo.toml
```
