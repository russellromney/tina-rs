# Rock 10 Decision Note — Reqwest Flat Reply Mapper

## Status

Decision. **No helper.** `flatten_outcome` stays opt-in.

## The Pain

`eiffel_webhook_publisher` shows three call shapes side by side.
Shape 3 is denser:

```rust
.reply(DriverMsg::PostedViaSendRequest)                           // bare ctor
.reply(DriverMsg::PostedViaRawCall)                               // bare ctor
.reply(|outcome| DriverMsg::PostedFlattened(flatten_outcome(outcome))) // closure
```

A first-time reader looks at shape 3 twice. Possible helper:

```rust
send_request(...).reply_flat(DriverMsg::PostedFlattened)
```

Shape 3 then reads like 1 and 2.

## Why No Helper

`eiffel_webhook_publisher` is the only specimen that mixes all
three shapes. After the Round-4 cleanup, every other caller
picks one shape per call-site cluster and is consistent inside
a single isolate. The mixed-mode density is pedagogical only.

Plan rule: ship a helper only if a non-pedagogical caller wants
flat errors repeatedly. That caller does not exist in-tree.

## Documentation Rule

`tina-reqwest-bridge`: pick layered or flat per call-site
cluster, not per-isolate-mixed. `flatten_outcome` is opt-in.
Mixed mode is intentionally awkward; the awkwardness is
documentation that the cluster boundary is wrong.

If a future non-pedagogical site asks for the helper, ship it
under these constraints:

- opt-in only;
- preserves bridge-vs-worker layer naming in the error type;
- no retry policy;
- the raw layered path remains the documented first form.

## Decision

`flatten_outcome` stays as is. `eiffel_webhook_publisher`
keeps the mixed-mode `reply` site. No `reply_flat` helper.

Finding 7 stays open with this note: the answer is opt-in
`flatten_outcome` plus discipline at call-site clusters.
