# Mini SaaS API

Copy this when you need a production-shaped Tina service skeleton, not a web
framework.

## Architecture

```mermaid
flowchart LR
    C["HTTP client"] --> L["tina-http listener"]
    L --> A["Controller isolate"]
    A --> D["tina-sqlite-bridge worker"]
    A --> P["tina-http keepalive pool"]
    P --> N["Notification HTTP service"]
```

The controller isolate owns request workflow state (`next_id`, live item cache,
ingress-open flag). SQLite owns durable-ish item rows. The keepalive pool owns
outbound connection leases. No domain state is hidden behind
`Arc<Mutex<AppState>>`.

## Routes

| Route | Meaning |
| --- | --- |
| `GET /health` | Process/runtime can answer. |
| `GET /ready` | DB bridge and outbound pool can accept useful work. |
| `POST /items` | Create `name=<value>` after SQLite insert. |
| `GET /items/{id}` | Query SQLite before replying. |
| `POST /items/{id}/notify` | Query SQLite, acquire keepalive lease, call webhook, release lease, then reply. |
| `GET /debug/capacity` | Live capacity and pressure report. |

## Capacity

| Surface | Cap |
| --- | --- |
| HTTP body bytes | `32` for the public API listener |
| controller mailbox | `2` |
| SQLite bridge mailbox | `2` |
| SQLite pool shape | one in-flight connection, no waiters |
| outbound keepalive pool | one connection, zero waiters |

`GET /debug/capacity` returns a small key=value line with real HTTP body
current/high-water/full/timeout/io counters, controller mailbox cap, shutdown
state, DB capacity/waiters/in-flight/full/closed/timeout counts, and outbound
keepalive capacity/waiters/leased/full/closed/cancel counts.

## Readiness

`/health` is liveness. `/ready` is useful-work readiness. It returns `503` with
typed reasons such as `db_closed`, `db_full`, `db_timeout`, `outbound_full`,
`outbound_closed`, or `ingress_stopped`.

## Shutdown Order

1. Mark public ingress closed in the controller.
2. Let one already-admitted slow notify request finish with a typed reply.
3. Reject new public work with `ingress_stopped`.
4. Probe readiness over HTTP so `ingress_stopped` is visible.
5. Close the SQLite bridge.
6. Probe readiness so `db_closed` is visible.
7. Drain and stop the outbound keepalive pool with `shutdown_keepalive_pool`.
8. Stop the notification listener and public listener.
9. Shutdown the runtime and assert the terminal report/trace facts.

## Multi-Turn Replies

`POST /items/{id}/notify` proves the current request pattern:

```text
HTTP call -> Controller CallContext
  -> call_ctx.defer(SQLite query).reply(...)
  -> outbound pool acquire continuation
  -> keepalive request continuation
  -> release continuation
  -> final HttpResponse
```

The route replies several turns after the original HTTP/controller call.
`Full`, `Closed`, and `Timeout` remain distinct in the route bodies.

## Live-Replay Fact

The smoke run prints a materialized fact:

```text
live_replay_fact case=mini_saas_body_full ops=[post:/items:41bytes] fact=status_413 cap=32
```

This is intentionally small: the operation is explicit, the fact is checked via
`tina_sim::dst` live-replay capture with a typed capacity surface, and a
cap/status mismatch fails the smoke run instead of being inferred from raw log
text.

## Commands

Run the system smoke from the repo root:

```sh
cargo test --manifest-path examples/systems/mini_saas_api/Cargo.toml
```

Run the documented executable smoke:

```sh
cargo run --manifest-path examples/systems/mini_saas_api/Cargo.toml -- smoke
```

The test smoke asserts the exact user-visible status/body sequence for health,
readiness, create, read, notify, notify-after-peer-close, missing item, wrong
method, malformed create, parser body cap, duplicate create, in-flight shutdown
notify, ingress-closed rejection, and DB-closed readiness. The peer-close case
forces the upstream notification server to answer with `Connection: close`, then
proves the one-slot keepalive pool still serves the next notification by
releasing successful requests with `Reuse`. The parser body-cap observation has
an empty body because `tina-http` rejects the oversized request before
dispatching to the controller isolate. The tests parse live capacity and
terminal shutdown reports as key=value fields, reject duplicate keys, and check
pre-shutdown, during-shutdown, and terminal views separately.

Run the pressure variant:

```sh
cargo run --manifest-path examples/systems/mini_saas_api/Cargo.toml -- pressure
```

The pressure variant holds the one outbound keepalive lease with a slow notify
request, sends a second notify, and asserts the user sees
`503 outbound_full` while the first notify still succeeds.

This specimen keeps shutdown host-driven so the whole service shape fits in
one copied system. Larger services can move the same order into a coordinator
isolate and publish the terminal report with `stop_with(report)` /
`observe_result`.

## Out Of Scope

No auth/session framework, no middleware stack, no macro router, no automatic
retry, no hidden DB pool semantics, no HTTPS claim, and no production readiness
claim from this small smoke.

## Findings

What felt good:
- `call_ctx.defer(...).reply(...)` makes the caller-preserving path easy to copy.
- SQLite and keepalive pool pressure reports already had the right vocabulary.

What felt rough:
- The route body parsing is deliberately local and boring.
- Service-shaped live-replay facts still need a broader reusable saved-case
  story later.

Tina capability pulled:
- Native HTTP, SQLite bridge, keepalive pool, readiness, shutdown, capacity,
  and trace-derived multi-turn proof.

Suggested follow-up:
- Keep shutdown/report formatting local until another system repeats the same
  exact shape.

Verdict:
- keep
