# specimen_webhook_outbox

A webhook outbox that records work before sending it, survives a restart, and
resumes the work it never finished — built two ways so you can compare feel.

## The story

Phase A enqueues three webhooks. Two are sent and durably marked sent; the third
is sent but the process "crashes" before the mark is durable. Phase B is a fresh
process: recover, compact the journal, and resume the one unsent webhook.

Because the first form is **at-least-once**, the third webhook is delivered
again. That is the honest outcome of a crash after the side effect but before
the durable mark — not a bug, and not papered over.

## Two implementations

- [`src/tina_impl.rs`](src/tina_impl.rs) — built on `tina_runtime::DurableOutbox`.
  Record-before-send is a type rule (`apply` only accepts a `RecordedWork`),
  recovery is typed (`TailStatus`), compaction is one call (`recover_compacted`),
  the durable swap is one call (`commit_file_atomic`) guarded by a commit fence,
  and the resume loop is one call (`ResumeQueue::next_apply`).
- [`src/hand_impl.rs`](src/hand_impl.rs) — the same outbox over a flat log,
  written by hand. It works, but read the module header for everything it has to
  invent (append-before-send by convention, manual dedup) and what it still
  skips (no checksum, non-atomic compaction, no commit fence).

Both produce the same `Report`; the smoke tests assert they agree.

## Run

```sh
cargo run            # both sides
cargo run -- tina    # durable outbox only
cargo run -- hand    # hand-rolled only
cargo test           # smoke tests
```

Each side prints one comparison line: three sent, two marked before the crash,
one recovered and resent, three marked in total, and the journal compacted from
five records to one.

## Wiring into a runtime

This specimen composes the outbox with synchronous journal calls for clarity.
Driving it from a Tina isolate — issuing `journal_append` / `journal_replay` as
runtime calls and carrying the staged token through the continuation message —
is shown in `tina-runtime/tests/durable_outbox.rs`.
