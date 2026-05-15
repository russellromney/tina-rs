# 099 Production Service Skeleton Refresh

## Status

- IDD phase.
- One PR.
- Runs after Phase 095 call-context defer ergonomics.
- Builds on completed Phase 083 `examples/systems/mini_saas_api`.
- Owns the system specimen refresh, copied docs, and small helper extraction
  only if repeated service code proves it.

## Grug Truth

The skeleton exists.

But Tina changed under it:

- `CallContext::defer(...)` is now the blessed multi-turn path;
- ordinary continuations are `then(...)`;
- cancelable multi-turn work has a sharp admission rule;
- service layers have more pressure/capacity/lifecycle vocabulary.

The copied service must teach the current shape, not old ceremony.

One real service is better than ten fragments.

No framework fog.

## Goal

Refresh the production-shaped service skeleton so an LLM can copy it today and
build a moderate HTTP service safely.

The refreshed skeleton should show:

- inbound HTTP route;
- controller/state isolate;
- multi-turn DB reply using `call_ctx.defer(...)`;
- outbound keepalive call;
- health/readiness;
- capacity/debug report;
- graceful shutdown;
- one pressure path;
- one cancel/deadline path if cheap;
- docs that name what is service-local and what is reusable Tina API.

## Non-Goals

- no Axum clone;
- no router framework unless repeated code screams for a tiny helper;
- no auth/session framework;
- no SQLx/Postgres requirement if SQLite keeps CI simpler;
- no broad WebSocket/gRPC service merge;
- no hidden background queue;
- no automatic retry;
- no new bridge semantics;
- no large public API unless at least two service call sites need the same
  exact helper.

## Rock 0: Read First, Freeze Scope

Read:

- `.intent/phases/083-production-service-layers/plan.md`;
- `.intent/phases/095-call-context-defer-ergonomics/plan.md`;
- `.intent/phases/097-cancelable-deferred-admission/plan.md`;
- `examples/systems/mini_saas_api`;
- `docs/tina-user-guide/00-agent-quickstart.md`;
- `docs/tina-user-guide/04-request-reply.md`;
- `docs/tina-user-guide/10-service-patterns.md`;
- `docs/tina-user-guide/14-lifecycle-and-shutdown.md`;
- `docs/tina-user-guide/15-service-client-worked-example.md`;
- `docs/tina-user-guide/18-bridge-crates.md`.

Before coding, update status with:

- exact system path;
- routes kept/changed;
- old patterns found;
- helper candidates, if any;
- exact smoke/pressure commands.

Default path: update `examples/systems/mini_saas_api`, not a new system.

## Rock 1: Multi-Turn Request Refresh

Migrate service/controller code to the current pattern.

Rules:

- use `call_ctx.defer(work).reply(Msg::Done)` for ordinary multi-turn replies;
- use `then(...)` for ordinary non-caller continuations;
- do not use old `reply(...)` continuation spelling except compatibility tests;
- do not manually carry `RequestContext` where `defer(...)` is the clearer
  copied path;
- keep one docs/example snippet of expanded `into_request_context()` only if it
  teaches why `defer(...)` exists;
- if cancelable deferred work exists, either use Phase 097 helper if landed or
  keep explicit admission-before-dispatch code.

Proof:

- readiness route waits on DB/outbound work and replies;
- create/read route waits on DB and replies;
- notify route waits on DB plus outbound HTTP and replies;
- no route times out because caller context was lost;
- unsupported/abandoned path returns typed rejection/failure.

## Rock 2: Pressure And Capacity

Make pressure easy to see.

Required report includes, as available:

- HTTP body bytes/high-water;
- controller mailbox pressure;
- DB bridge/pool in-flight/full/closed/timeout;
- outbound keepalive pool waiters/leased/full/closed;
- request/call full/timeout counts if already exposed;
- shutdown state.

Keep it boring:

- `GET /debug/capacity` or existing route;
- key=value text or small JSON;
- no unbounded event log.

Proof:

- pressure command forces at least one visible `Full`, `Closed`, or `Timeout`;
- test asserts the pressure response, not just prints it;
- docs say how to run the pressure command.

## Rock 3: Lifecycle And Shutdown

Refresh shutdown to the current lifecycle vocabulary.

Required order:

1. stop accepting new work;
2. reject or drain outstanding controller work;
3. close DB bridge/pool;
4. close outbound keepalive pool with `shutdown_keepalive_pool`;
5. stop listener/runtime;
6. emit terminal report.

Proof:

- smoke path shuts down cleanly;
- shutdown while one request is in flight returns typed terminal truth;
- after shutdown, new work is rejected/closed visibly;
- no `Arc<Mutex<Option<Report>>>` completion mailbox if `stop_with` /
  `observe_result` works.

## Rock 4: Helper Extraction Only If Earned

Look for repeated local code.

Possible tiny helpers:

- route response builder;
- capacity formatting;
- request body decode helper;
- shutdown report formatting.

Rules:

- protocol helper goes in `tina-http`;
- generic request/reply helper goes in `tina`;
- runtime/capacity helper goes in `tina-runtime`;
- DB helper stays in DB crate;
- one-system-only glue stays local.

Do not ship a service framework. If unsure, document the pain and leave local.

## Rock 5: Docs

Update:

- `examples/systems/mini_saas_api/README.md`;
- `examples/systems/README.md`;
- `docs/tina-user-guide/00-agent-quickstart.md` if copied commands changed;
- `docs/tina-user-guide/04-request-reply.md` if the skeleton is the clearest
  `defer(...)` example;
- `docs/tina-user-guide/10-service-patterns.md`;
- `docs/tina-user-guide/15-service-client-worked-example.md`.

Docs should say:

- this is a service skeleton, not a framework;
- where caller authority is preserved;
- where pressure is reported;
- how shutdown works;
- what remains intentionally local.

## Required Checks

Run at least:

```text
cargo fmt --all --check
cargo test --manifest-path examples/systems/mini_saas_api/Cargo.toml
cargo run --manifest-path examples/systems/mini_saas_api/Cargo.toml -- smoke
cargo run --manifest-path examples/systems/mini_saas_api/Cargo.toml -- pressure
cargo clippy --manifest-path examples/systems/mini_saas_api/Cargo.toml --all-targets -- -D warnings
cargo test -p tina-http --test multi_turn_service -- --nocapture
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

If `make verify` is too heavy locally, run targeted checks and let CI finish
the rest. Do not ignore a repeated failure as a flake.

## Success

The system specimen is the copied shape for new Tina services.

It uses current `CallContext::defer(...)`.

It shows pressure and shutdown.

It does not invent a framework.

It teaches cheap models the boring safe path.
