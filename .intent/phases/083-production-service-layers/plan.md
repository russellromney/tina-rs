# 083 Production Service Layers

## Status

- Implemented in `examples/systems/mini_saas_api`.
- Chosen DB bridge/pool: `tina-sqlite-bridge::SqliteWorker`, using its
  documented single-connection pool-shaped pressure report
  (`max_in_flight = 1`, no hidden waiters).
- Chosen outbound HTTP path: native `tina-http` keepalive pool from
  `build_keepalive_pool`, with explicit acquire/request/release and
  `shutdown_keepalive_pool` for close/drain.
- System path: `examples/systems/mini_saas_api`.
- Routes:
  - `GET /health`;
  - `GET /ready`;
  - `POST /items`;
  - `GET /items/{id}`;
  - `POST /items/{id}/notify`;
  - `GET /debug/capacity`.
- Helper APIs expected: none outside the system. Repeated route/body/status,
  capacity formatting, and smoke-script glue stay specimen-local unless the
  implementation proves repeated ugliness across crate boundaries.
- Helper homes if that changes: protocol-only helpers in `tina-http`,
  generic request/reply helpers in `tina`, runtime/capacity helpers in
  `tina-runtime`, DB helpers in the DB bridge crate, otherwise
  specimen-local.
- Actual helper APIs added outside specimen: none.
- Docs updated:
  - `README.md`;
  - `docs/tina-user-guide/00-agent-quickstart.md`;
  - `docs/tina-user-guide/10-service-patterns.md`;
  - `docs/tina-user-guide/14-lifecycle-and-shutdown.md`;
  - `docs/tina-user-guide/15-service-client-worked-example.md`
    (`origin/main` name for the service-client page);
  - `docs/tina-user-guide/18-bridge-crates.md`;
  - `examples/systems/README.md`;
  - `examples/systems/mini_saas_api/README.md`.
- Checks run:
  - `cargo fmt --all --check`;
  - `cargo fmt --all --check --manifest-path examples/systems/mini_saas_api/Cargo.toml`;
  - `cargo test --manifest-path examples/systems/mini_saas_api/Cargo.toml`;
  - `cargo run --manifest-path examples/systems/mini_saas_api/Cargo.toml -- smoke`;
  - `cargo run --manifest-path examples/systems/mini_saas_api/Cargo.toml -- pressure`;
  - `cargo clippy --manifest-path examples/systems/mini_saas_api/Cargo.toml --all-targets -- -D warnings`;
  - `cargo test -p tina-sqlite-bridge`;
  - `cargo test -p tina-http --test keepalive_pool`;
  - `cargo test -p tina-sim request_context`;
  - `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`;
  - `cargo clippy --workspace --all-targets -- -D warnings`;
  - `make verify`.
- Hostile review result: fixed the only implementation finding found during
  review, where the pressure script initially failed to hold the outbound
  keepalive lease long enough to prove pool `Full`. Rechecked no web framework
  was added, no `Arc<Mutex<AppState>>` domain state exists, `Full`/`Closed`/
  `Timeout` route bodies stay distinct, the multi-turn route carries
  `RequestContext`, shutdown uses `shutdown_keepalive_pool`, and the documented
  system smoke command passes.
- One PR.
- Can run beside 094 WebSocket usable server if it does not edit WebSocket
  internals.
- Can run beside 057 gRPC if both avoid broad `tina-http` rewrites.
- Owns docs, one production-shaped specimen/system, and tiny helper APIs only
  where repeated service code proves the need.

## Grug Truth

Pieces are not a service.

Users need one copied shape.

LLMs need one copied shape even more.

The shape must be real enough to hurt:

- inbound HTTP;
- DB pool;
- outbound HTTP pool;
- state isolate;
- graceful shutdown;
- health/readiness;
- tracing;
- capacity reports;
- DST capture.

No framework fog.

No hidden queues.

No "just read these ten specimens".

## Goal

Build the canonical Tina local-service skeleton.

After this phase, a user or coding agent should be able to say:

```text
I can build a moderate Tokio-style HTTP service in Tina by copying this
service skeleton and changing the domain messages.
```

This phase does not chase web-framework comfort. It assembles existing Tina
capabilities into one blessed service pattern and fills only the tiny gaps that
make the copied path awkward.

## Non-Goals

- no Axum clone;
- no middleware framework;
- no broad router framework if the current router helpers are enough;
- no new database bridge semantics;
- no new HTTP protocol semantics unless a skeleton bug proves one;
- no hidden async runtime;
- no automatic retry;
- no unbounded logs, metrics, task queues, request queues, or background jobs;
- no production auth/session framework;
- no "all possible services" claim.

## The Service We Build

Build one production-shaped specimen/system. Prefer:

```text
examples/systems/mini_saas_api
```

If that exact folder already exists, upgrade it. If not, create it.

It must be tested. Either add it to the workspace intentionally or document and
test it by exact `--manifest-path` command. No prose-only system.

Required service shape:

- native `tina-http` HTTP/1.1 listener, HTTPS optional if cheap;
- routes:
  - `GET /health`;
  - `GET /ready`;
  - `POST /items`;
  - `GET /items/{id}`;
  - `POST /items/{id}/notify`;
  - `GET /metrics` or `GET /debug/capacity`;
- state/controller isolate owns request workflow;
- DB pool consumer path uses shipped SQLite or SQLx pool helpers;
- outbound keepalive pool path sends one webhook/notification;
- readiness depends on DB pool and outbound pool state;
- graceful shutdown stops ingress, drains/cancels in-flight work, closes pools,
  closes bridges/resources, and returns a terminal report;
- capacity report covers at least HTTP body bytes, DB pool waiters/in-flight,
  outbound pool waiters/in-flight, and the controller mailbox;
- live smoke proves happy path and at least one pressure path;
- DST/replay case captures at least one service-shaped pressure/lifecycle fact.

Pick SQLite if Postgres makes CI heavy. Pick SQLx only if an existing test
database path is already reliable in `make verify`.

## Rock 0: Read First, Pin Scope

Read:

- `.intent/SYSTEM.md`;
- `ROADMAP.md` near-term and capability layers;
- `docs/tina-user-guide/00-agent-quickstart.md`;
- `docs/tina-user-guide/10-service-patterns.md`;
- `docs/tina-user-guide/13-lifecycle-and-shutdown.md`;
- `docs/tina-user-guide/14-service-client-worked-example.md`;
- `docs/tina-user-guide/18-bridge-crates.md`;
- `examples/systems/README.md`;
- existing HTTP, DB, keepalive, capacity, RequestContext, shutdown specimens.

At start of implementation, edit this status with:

- chosen DB bridge/pool (`sqlite` or `sqlx`);
- chosen outbound HTTP path;
- exact specimen/system path;
- exact routes;
- helper APIs you expect to add, if any;
- where each helper lives: `tina`, `tina-runtime`, `tina-http`, a bridge crate,
  docs-only, or specimen-local.

Do not start by adding helpers. Build the service first. Let ugly repeated code
earn helpers.

Helper home rule:

- protocol helpers live in `tina-http`;
- generic request/reply helpers live in `tina` only if not HTTP-specific;
- runtime/topology/capacity helpers live in `tina-runtime`;
- DB helpers stay in the DB bridge crate;
- specimen-only glue stays specimen-only.

If unsure, keep it specimen-local and document the pain.

## Rock 1: Multi-Turn Request Pattern

The service must use the post-086 request pattern correctly.

Rules:

- every inbound request that waits on DB/outbound work captures caller
  authority with `CallContext` / `RequestContext`;
- no handler relies on "plain continuation keeps caller context" unless the
  current runtime guarantee explicitly covers that path;
- docs show the expanded form once;
- if `reply_with_current_request(call, f)` or equivalent exists, use it only
  after the expanded form is tested and documented.

Required proof:

- one route does DB work before replying;
- one route does DB + outbound HTTP before replying;
- neither route times out because caller context was lost;
- unsupported or abandoned request paths return typed rejection/failure, not a
  silent timeout.
- test includes at least one route whose final reply happens two or more turns
  after the original HTTP/controller call.

## Rock 2: Routing And Domain State

Use boring route code.

Acceptable:

- existing Tina HTTP router helpers;
- small route table helper if current call sites repeat heavily;
- direct `match (method, path)` if that is clearer.

Not acceptable:

- new web framework;
- macro router;
- hidden global state;
- `Arc<Mutex<AppState>>` for domain state.

The controller isolate owns domain workflow state. DB owns durable-ish data.
HTTP handlers translate requests into controller messages and replies.

Do not let the HTTP handler do all DB/outbound work directly from host-side
helpers. The point is to prove a Tina service coordinator, not only bridge
calls from tests.

Required proof:

- route not found;
- method not allowed or bad request;
- create item;
- read item;
- request body cap or malformed body returns typed HTTP error;
- controller mailbox pressure is visible.

## Rock 3: DB Pool Consumer

Use the shipped pool consumer API, not raw bridge calls everywhere.

Required path:

- acquire lease;
- execute or query;
- release `Reuse` on success;
- retire on known-bad resource or close;
- handle `Full`, `Timeout`, `Closed`, and DB error distinctly;
- expose a pressure report for waiters/in-flight/high-water/full.

If SQLite serial first form is chosen, be honest: it may be pool-shaped around
one worker/connection. Do not claim parallel DB queries unless the bridge
actually supports them.

Required proof:

- successful insert/query;
- DB constraint or decode/error path;
- pool full or acquire timeout;
- close/drain path during shutdown;
- pressure report high-water/full count.

## Rock 4: Outbound Keepalive Consumer

Use Tina's native HTTP keepalive pool for a webhook/notification path.

Required path:

- acquire or call through keepalive pool;
- bounded request/response body;
- classify success/failure enough for the route report;
- release/reuse/retire honestly;
- close/drain pool during shutdown.

No automatic retry unless the route explicitly decides retry safety and max
attempts. If retry is included, every timer/backoff is visible.

Required proof:

- notification success;
- upstream failure/closed path;
- pool full or timeout;
- shutdown closes/drains pool.

## Rock 5: Health And Readiness

Add real health/readiness, not just "process alive".

Shape:

- `/health` answers if the service process/runtime is alive enough to answer;
- `/ready` answers only if ingress is open and required pools/bridges/resources
  can accept useful work;
- readiness names typed reasons when false.

Candidate vocabulary:

- `ServiceHealth`;
- `ServiceReadiness`;
- `ReadinessReason`;
- `ServiceStatusReport`.

Keep this tiny. If a struct is easier than an enum, use a struct.

Required proof:

- healthy at start;
- not ready after DB pool/bridge close;
- not ready during shutdown after ingress stop;
- response body or report includes the reason.

## Rock 6: Graceful Shutdown Program

Write shutdown as a Tina program, not scattered sleeps.

Order:

1. stop ingress;
2. reject or drain new HTTP requests visibly;
3. cancel/settle in-flight controller requests;
4. close/drain DB pool;
5. close/drain outbound pool;
6. stop controller;
7. shutdown runtime;
8. assert terminal report.

Use existing lifecycle helpers where they fit. Add a tiny helper only if the
same close/drain pattern is repeated across DB/outbound pools.

Required proof:

- shutdown with no work is clean;
- shutdown with in-flight DB/outbound work settles callers visibly;
- no hanging caller;
- terminal report says clean or names the exact unclean reason;
- no resource/pool/bridge ghost remains in topology/report.

## Rock 7: Capacity And Tracing

The skeleton must print or return a capacity/tracing summary a user can copy.

Required surfaces:

- HTTP body bytes;
- listener/connection mailbox if exposed;
- controller mailbox;
- DB pool waiters/in-flight;
- outbound pool waiters/in-flight;
- bridge in-flight if relevant;
- shutdown/lifecycle terminal summary.

Add one concise formatter if needed. Prefer existing capacity discovery line
format over new prose.

Required proof:

- capacity report has stable names;
- full/high-water counts change under a pressure test;
- trace query helper or event count proves one request path and one shutdown
  path;
- no report relies on stale caller-supplied config.

## Rock 8: DST / Live Replay Hook

This skeleton should teach Tina's superpower.

Required:

- one saved or generated replay case for a service-shaped story;
- at least one pressure/lifecycle fact in the capture;
- config/history/fact mismatch fails closed;
- docs show how to run/discover constants if applicable.
- if `tina_sim::dst` already has live-fact helpers from 093, use them instead
  of inventing a parallel saved-case shape.

Good target:

- request body full on `POST /items`;
- DB pool full/timeout under burst;
- shutdown while notification in flight;
- outbound pool full under burst.

Do not infer app operations from raw trace text. Materialize ops.

## Rock 9: Docs And Copy Path

Update docs so a new user knows where to start.

At minimum:

- `docs/tina-user-guide/00-agent-quickstart.md`;
- `docs/tina-user-guide/10-service-patterns.md`;
- `docs/tina-user-guide/13-lifecycle-and-shutdown.md`;
- `docs/tina-user-guide/14-service-client-worked-example.md` or a new
  service skeleton page;
- `examples/systems/README.md`;
- specimen/system README.

Docs must include:

- architecture diagram in words or simple Mermaid;
- route table;
- isolate list and who owns state;
- capacity table;
- shutdown order;
- readiness meanings;
- "what is still out of scope";
- exact commands to run tests/smoke.

Docs must not say "framework" unless the code actually provides one.

## Rock 10: Helper Cut Line

Allowed tiny helpers if earned:

- `reply_with_current_request(...)`;
- route/body parse helper;
- service readiness report struct;
- close/drain helper for pools;
- capacity summary helper;
- test scenario runner if it only lives in tests/specimen.

Every helper added outside the specimen needs:

- a copied call site in the skeleton;
- a focused unit/integration test;
- docs or rustdoc if public;
- no weaker trace/capacity truth than the expanded form.

Not allowed:

- new app framework;
- macro DSL;
- global service container;
- hidden retry engine;
- unbounded scenario runner;
- helper that hides `Full`/`Closed`/`Timeout`.

Every helper must delete more confusing code than it adds.

## Required Checks

- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets -- -D warnings` or explain why a
  narrower command is appropriate
- service specimen/system smoke test
- exact documented command from the specimen/system README
- DB bridge/pool tests touched by the service
- HTTP keepalive tests touched by the service
- relevant capacity/DST tests
- `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` if docs/rustdoc
  changed
- `make verify` before final PR unless runtime makes this impractical; if not
  run, state exactly what was run and why

## Done Means

- One production-shaped Tina service exists and works.
- It uses native HTTP, DB pool, outbound pool, state isolate, readiness,
  graceful shutdown, tracing/capacity, and DST capture.
- A new user can copy the skeleton without reading ten specimens first.
- Pressure and shutdown are typed, bounded, and tested.
- No new framework fog was added.
