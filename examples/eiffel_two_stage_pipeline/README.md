# eiffel_two_stage_pipeline

Three sequential stages — parse → validate → execute — driven per
request. Some inputs fail parse, some fail validate, the rest run
through.

## Run

```sh
cargo test --manifest-path examples/eiffel_two_stage_pipeline/Cargo.toml
```

## Tokio shape

```rust
let parsed = parse(i).await?;
let validated = validate(parsed).await?;
let _ = execute(validated).await?;
```

One async fn per request. The `?` operator handles short-circuit.

## Tina shape

The `Pipeline` isolate captures a `DeferredReply` per request, walks
through `Parse`, `Validate`, `Execute` calls (each is an
`IsolateCall` continuation), and replies through the slot at the
end:

```rust
PipelineMsg::Submit(input)         // capture slot, dispatch parse
PipelineMsg::Parsed(qid, outcome)  // dispatch validate or bail
PipelineMsg::Validated(qid, outcome) // dispatch execute or bail
PipelineMsg::Executed(qid, outcome)  // reply_to(slot, Completed)
```

## Discussion

What feels good:

- Each stage is its own `Isolate`, with its own message type, reply
  type, and bounded mailbox. Adding a fourth stage adds one isolate
  and one continuation arm, no other plumbing.
- Out-of-order completion across requests is invisible at the call
  sites. The `qid` on each continuation correlates the reply.

What feels worse:

- The `PipelineMsg` enum has one variant per stage. Three stages =
  four variants (Submit + 3 continuations). For an N-stage pipeline
  the variant count grows linearly. The `parsed → validated →
  executed` async-fn version reads nicer.
- The `bail` helper (closes the slot with the right error variant)
  is small but shows up at the bottom of every match arm; an
  `Effect::Result` style helper that maps `Outcome -> next` could
  shrink the boilerplate.
- The deferred reply slot survives across three stages, but caller
  timeout truth is now distributed across three `IsolateCall`
  timeouts. If the *original* caller times out at `STAGE_TIMEOUT`,
  every downstream call also has its own. There is no single
  deadline that propagates.
