# eiffel-tracing-demo

Smallest end-to-end demo of `tina-tracing`.

Single-shard runtime, one caller isolate, one-slot mailbox. The
caller fans six zero-duration `sleep` calls. First reply fits; the
rest hit `MailboxFull` until the slot drains.

`TracingObserver` is wired before any event records, so each event
becomes a structured `tracing::Event` as it happens. Fields
preserved: `kind`, `event_id`, `cause_id`, `shard`, `isolate`,
`call_id`, `call_kind`, typed `reason`. No end-of-run dump.

```text
cargo run --manifest-path examples/eiffel_tracing_demo/Cargo.toml
RUST_LOG=tina_runtime=trace cargo run --manifest-path examples/eiffel_tracing_demo/Cargo.toml
```

`RUST_LOG` controls the level filter. Use `tina_runtime=trace` for
every event, `tina_runtime=warn` for only `Full`/`Closed`/`Timeout`
rejections.

Tiny by design. Not a benchmark, not a service, not a metrics
adapter — just shows runtime truth flowing into `tracing_subscriber::fmt`
without flattening.
