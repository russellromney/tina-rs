# specimen-tracing-demo

Smallest end-to-end demo of `tina-tracing`.

Single-shard runtime, one caller isolate, one-slot mailbox. The
caller fans out a producer-bounded set of zero-duration `sleep`
calls, retains exact timer errors, and reads the runtime pressure
summary after every completion has settled. The mailbox is deliberately
small, but the demo reports observed pressure rather than assuming a
specific scheduler interleaving.

`TracingObserver` is wired before any event records, so each event
becomes a structured `tracing::Event` as it happens. Fields
preserved: `kind`, `event_id`, `cause_id`, `shard`, `isolate`,
`call_id`, `call_kind`, typed `reason`. No end-of-run dump.

```text
cargo run --manifest-path examples/specimen_tracing_demo/Cargo.toml
RUST_LOG=tina_runtime=trace cargo run --manifest-path examples/specimen_tracing_demo/Cargo.toml
```

`RUST_LOG` controls the level filter. Use `tina_runtime=trace` for
every event, `tina_runtime=warn` for only `Full`/`Closed`/`Timeout`
rejections.

Tiny by design. Not a benchmark, not a service, not a metrics
adapter — just shows runtime truth flowing into `tracing_subscriber::fmt`
without flattening.
