# Mini SaaS API

This system stitches together a small HTTP edge, a Tina API isolate, the SQLite
bridge, and the reqwest bridge for a webhook.

It is intentionally a little awkward: it pulls on health/readiness, database
calls, outbound bridge calls, multi-turn replies, and shutdown in one place.

## Run

```bash
cargo run --manifest-path examples/systems/system_mini_saas_api/Cargo.toml
cargo test --manifest-path examples/systems/system_mini_saas_api/Cargo.toml
```

## Findings

What felt good:
- The API isolate owns application state and names every external operation as
  a message transition.
- SQLite and reqwest bridges make the ecosystem boundary explicit instead of
  letting async work leak into isolate logic.
- Health/readiness/read/write/webhook all fit in one isolate without shared
  mutable app state.

What felt rough:
- The service still uses `take_reply_slot` directly; newer request-context
  helpers are the clearer copied shape for multi-turn replies.
- The readiness probe originally used the row-less execute helper for
  `SELECT 1`; the typed SQLite split caught that as a visible false negative.
- Hyper edge shutdown is host-side and ad hoc here; a real system wants the
  canonical bridge/lifecycle owner path.

Tina capability pulled:
- SQLite bridge.
- Reqwest bridge.
- Multi-turn replies.
- Health/readiness.
- Host HTTP edge calling Tina via `call_blocking`.

Suggested follow-up:
- Convert this system to `RequestContext`/`reply_with_request` once this branch
  catches up with the newer call-context surface on `main`.
- Add a pressure report endpoint instead of only happy-path item CRUD.

Verdict:
- keep, but modernize

