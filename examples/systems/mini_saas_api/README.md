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
current/high-water/full/timeout/io counters, controller mailbox cap, the
controller's typed `drain.stage` (`open` / `draining` / `stopped`), DB
capacity/waiters/in-flight/full/closed/timeout counts, and outbound keepalive
capacity/waiters/leased/full/closed/cancel counts. The stage field comes from
the `tina_runtime::DrainState` helper, so the same typed vocabulary appears
here as in any other Tina service that drains.

## Readiness

`/health` is liveness. `/ready` is useful-work readiness. It returns `503` with
typed reasons such as `db_closed`, `db_full`, `db_timeout`, `outbound_full`,
`outbound_closed`, or `ingress_stopped`.

## Shutdown Order

1. Begin the controller drain (`DrainState::begin`) so the next public
   request reads `drain.is_open() == false` and replies `ingress_stopped`.
2. Let one already-admitted slow notify request finish with a typed reply.
3. Probe readiness over HTTP so `ingress_stopped` is visible.
4. Probe `/debug/capacity` so `drain.stage=draining` is visible in the report.
5. Send one new POST and prove the typed `503 ingress_stopped` reply.
6. Close the SQLite bridge.
7. Probe readiness so `db_closed` is visible.
8. Drain and stop the outbound keepalive pool with `shutdown_keepalive_pool`.
9. Stop the notification listener and public listener.
10. Shutdown the runtime and assert the terminal report/trace facts.

The controller stays in `Draining` for the rest of its life. `Stopped` is the
`DrainState` terminal arm that fires when a service owns its own drain
handshake; here the host owns terminal proof through the runtime trace and the
keepalive pool shutdown report, so `drain.finish()` is never called from
inside the controller. `examples/systems/system_metrics_shipper` is the
worked example for the service-owned drain shape, where `Stopped` is reached.

## Multi-Turn Replies

`POST /items/{id}/notify` proves the current request pattern:

```text
HTTP call -> Controller CallContext
  -> call_ctx.defer(SQLite query).reply(NotifyLoaded)
  -> call(...).then_with_request(req, NotifyAcquired)   // outbound pool acquire
  -> call(...).then_with_request(req, NotifySent)       // keepalive request
  -> call(...).then_with_request(req, NotifyReleased)   // pool release
  -> reply_to_request(req, ...)                          // final HttpResponse
```

The first hop uses `call_ctx.defer(work).reply(...)` to consume caller
authority and carry it into the next continuation as `RequestContext<R>`.
Each follow-on hop calls `then_with_request(req, ...)` to keep that context
moving across messages. The final turn settles the caller with
`reply_to_request`. There is no hidden caller context preservation; `Full`,
`Closed`, and `Timeout` remain distinct in the route bodies at every hop.

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
# Full system smoke (scripted scenarios + capacity assertions):
cargo test --manifest-path examples/systems/mini_saas_api/Cargo.toml --test smoke

# Load/soak proof (Phase 108) with typed capacity contract:
cargo test --manifest-path examples/systems/mini_saas_api/Cargo.toml --test soak -- --nocapture
```

The soak proof drives 4 workers × 200 mixed-read ops via
`tina_proof_harness::load` and prints one summary line shaped like:

```text
soak load label=mini_saas_api_soak workers=4 ops=200 ok=158 err=42 \
  timeout=0 p50_us=643 p99_us=2377 max_us=2957 elapsed_ms=43 \
  leak_clean=true shutdown_clean=true capacity={ ... db.full=42 ... }
```

The contract: every harness 5xx must map to a typed `db.full` event on
the controller's `/debug/capacity` line (`db.full >= ops_err`). If the
two diverge, the test fails closed because pressure is escaping the
typed surface — the central proof Phase 108 exists to make easy.

What this exposes when it fails:

- `ops_timeout > 0` → transport hung; the listener or connection
  drain path stopped accepting work mid-load.
- `leak_clean=false` → the load harness's leak hook returned false
  (currently unused but reserved for future capacity-snapshot probes).
- `db.full < ops_err` → some other pressure path (controller mailbox,
  body parser, outbound pool) is returning 5xx without a matching
  typed event. That is the hidden-pressure regression the soak is
  designed to catch.
- `shutdown_clean=false` → the keepalive pool drained dirty (leaked
  in-flight, timed out, or hit `connection_failures`).

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
- `call_ctx.defer(...).reply(...)` for the first hop plus
  `then_with_request(...)` for follow-on hops keeps the caller-preserving path
  easy to copy without hidden context.
- `DrainState` names the host-driven `Open` → `Draining` transition with
  one typed stage field in `/debug/capacity`, instead of hiding it behind a
  service-local `bool`. The terminal `Stopped` arm exists in the helper but
  is reached by services that own their own drain handshake, not by a
  host-driven HTTP service like this one.
- SQLite and keepalive pool pressure reports already had the right vocabulary.

What felt rough:
- The route body parsing is deliberately local and boring.
- Service-shaped live-replay facts still need a broader reusable saved-case
  story later.

Tina capability pulled:
- Native HTTP, SQLite bridge, keepalive pool, readiness, shutdown, capacity,
  `DrainState`, and trace-derived multi-turn proof.

Suggested follow-up:
- Keep shutdown/report formatting local until another system repeats the same
  exact shape.

Verdict:
- keep
