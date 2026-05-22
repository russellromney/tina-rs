# Tina extension smoke crates

Small, workspace-excluded crates that prove Tina's **public extension hooks**
work using public APIs only — no private runtime access, no weakened
bounded/DST truth. They are the evidence behind
`docs/tina-user-guide/25-extension-hooks.md`.

Rust crates are Tina's plugin system: traits + feature-gated crates + examples.
There is no dynamic plugin ABI and no generic `Future`/`Stream` bridge.

| Crate | Hook | Proves |
|---|---|---|
| `tina-extension-capacity-surface` | `CapacitySurfaceReport` + `CapacitySummary::push` | A custom pressure surface joins a normal capacity summary. |
| `tina-extension-custom-codec` | `tina_codec::SyncCodec` | A custom codec stays sync/bounded/replayable and drives a service; Tina owns the I/O. |
| `tina-extension-service-policy` | `tina_runtime::ServicePolicy` | A custom per-key policy returns typed decisions, never sends/retries, and replays. |
| `tina-extension-fake-bridge` | `tina_runtime::bridge` vocabulary | A bounded worker bridge proves setup/closer/metrics/pressure/shutdown and caller-timeout honesty. |
| `tina-extension-compile-fail` | (negative proof) | An extension cannot mint runtime-owned tokens or forge private report/capability state. |

Each crate has a `README.md` with its run command and a smoke test. None imports
a private runtime module.

## Run them all

```sh
for m in examples/extensions/*/Cargo.toml; do
  cargo test --manifest-path "$m"
done
```

(`make verify-examples` also walks these crates.)
