# specimen_webhook_fanout

One event delivers to four upstream webhooks in parallel. Two
return `200`, one returns `503`, one sleeps past the per-call
timeout. The Tina side uses `tina-reqwest-bridge`; the Tokio side
uses `reqwest` directly with `JoinSet`.

## Run

```sh
cargo test --manifest-path examples/specimen_webhook_fanout/Cargo.toml
```

Both sides produce `delivered=2 unavailable=1 timed_out=1 other=0`.

## What feels good (Tina)

- `ReqwestOutcomeExt::classify` (the retry classifier) collapses the
  two-layer outcome into three buckets — `Succeeded(resp)`,
  `Transient(reason)`, `Fatal(reason)` — with typed reasons that
  still name *which* layer failed. The dispatcher's bucketer is
  five short arms.
- One `send_request(http, req, timeout).then(ctor)` per endpoint,
  produced through `BoundedItems` and shipped with `bounded_batch`.
  Endpoint count is rejected before upstream threads or call effects exist.
- The `503` and the timeout produce trace events the Tokio side
  cannot recover (Tokio's `reqwest::Error::is_timeout` is the only
  hook).

## What feels worse

- The dispatcher tracks its own bounded `pending` counter to know when to
  stop. Every transient or fatal classifier reason is retained in the typed
  report rather than disappearing into the display-oriented `other` count.

## Shape note

This specimen does *not* use `PendingReplies` because the
dispatcher is host-driven (`stop_with(report)` + `observe_result`)
rather than service-driven. A fanout that needs to reply through a
client-captured slot (e.g. Axum handler waiting on the dispatch)
would put `PendingReplies::with_capacity(N)` on the dispatcher and
call `try_capture` per inbound event.
