# eiffel-tracing-demo

Smallest end-to-end demo of `tina-tracing`.

A single-shard runtime runs one caller isolate with a one-slot
mailbox. The caller fans out six zero-duration `sleep` calls. The
first reply fits; the next ones land while the slot is full, so the
runtime emits `RuntimeEventKind::CallCompletionRejected { reason:
MailboxFull }`. Replies that arrive after the slot drains are
delivered. When all six are observed, the caller stops cleanly.

The example wires a `TracingObserver` before any event records, so
every event becomes a structured `tracing::Event` as it happens.
Fields preserved: `kind`, `event_id`, `cause_id`, `shard`, `isolate`,
`call_id`, `call_kind`, and the typed `reason`. No end-of-run dump
needed.

```text
cargo run --manifest-path examples/eiffel_tracing_demo/Cargo.toml
```

`RUST_LOG` controls the level filter. Useful settings:

```text
RUST_LOG=tina_runtime=trace    # every runtime event
RUST_LOG=tina_runtime=warn     # only Full / Closed / Timeout rejections
```

The example is intentionally tiny. It is not a benchmark, not a
service, and not a metrics adapter — it just shows that Tina runtime
truth flows into a normal `tracing_subscriber::fmt` pipeline without
flattening.
