# specimen_two_stage_pipeline

Three sequential stages — parse → validate → execute — driven per
request. Some inputs fail parse, some fail validate, the rest run
through.

## Run

```sh
cargo test --manifest-path examples/specimen_two_stage_pipeline/Cargo.toml
```

## Tokio shape

```rust
let parsed = parse(i).await?;
let validated = validate(parsed).await?;
let _ = execute(validated).await?;
```

One async fn per request. The `?` operator handles short-circuit.

## Tina shape

The `Pipeline` isolate uses `tina::flow!` to declare the three-step chain.
`Submit` captures caller authority, dispatches the parse call, and hands the
`RequestContext` into the flow; each step matches the prior stage's
`CallOutcome`, dispatches the next call (threading `req` through via
`.then_with_request`), or replies with the terminal outcome:

```rust
tina::flow! {
    flow PipelineFlow for Pipeline {
        reply PipelineReply;
        step Parsed() -> ParseReply { /* dispatch validate, or reply */ }
        step Validated() -> ValidateReply { /* dispatch execute, or reply */ }
        step Executed() -> ExecuteReply { /* reply Completed or exact terminal */ }
    }
}
```

`tina::flow!` generates the `PipelineFlow` continuation enum and its
dispatcher (`handle_pipeline_flow`) — the same shape this specimen used to
hand-roll as `PipelineMsg::{Parsed,Validated,Executed}`. Caller authority now
threads through `req: RequestContext<PipelineReply>` directly, so the
qid-keyed `PendingReplies` table this specimen previously needed is gone too.

## Discussion

What feels good:

- Each stage is its own `Isolate`, with its own message type, reply
  type, and bounded mailbox. Adding a fourth stage adds one isolate
  and one flow step, no other plumbing.
- `req` threading replaces the qid/`PendingReplies` indirection: one fewer
  keyed table, one fewer failure mode (`Full`/`DuplicateKey` on park).
- The generated dispatcher still names each stage as its own variant, and
  every suspension point (`call(...).then_with_request(...)`) stays
  trace-visible — `flow!` only deletes the boilerplate, not the shape.
- The host fanout is built from `BoundedItems` and `bounded_batch`; every
  stage and outer `Full`, `Closed`, `Timeout`, and `Rejected(reason)` remains
  distinct in `PipelineTerminal`.

What still doesn't disappear:

- The deferred reply slot survives across three stages, but caller
  timeout truth is now distributed across three `IsolateCall`
  timeouts. If the *original* caller times out at `STAGE_TIMEOUT`,
  every downstream call also has its own. There is no single
  deadline that propagates. `flow!` does not solve this — it is a
  continuation-boilerplate helper, not a deadline-propagation helper.
- `flow!` is deliberately linear-only: no fan-out, no joins, no loops. A
  pipeline stage that needs to fan out to N children still hand-writes
  its own continuation enum.
