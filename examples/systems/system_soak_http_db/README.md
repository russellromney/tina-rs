# system_soak_http_db

One-process soak that emits the discovery lines CI is supposed to
grep. Pretends to be an HTTP + DB service: each request charges
`soak.http.in_flight`, sleeps a fake HTTP delay, then charges
`soak.db.in_flight`, sleeps a fake DB delay, then replies. Slow
end-to-end requests push a `SlowEvent` into a bounded event sink.

The soak does not open sockets. The point is observability output,
not the network stack — the same discovery shape is what a real
HTTP+DB service would print.

## Run

```bash
cargo test --manifest-path examples/systems/system_soak_http_db/Cargo.toml
```

## Output shape

The smoke test asserts these lines appear and are CI-greppable:

```text
scope name=soak.http.in_flight unit=requests max=4 cur=0 high=4 full=46 admitted=128 released=128
scope name=soak.db.in_flight unit=queries max=2 cur=0 high=2 full=12 admitted=82 released=82
events sink=soak.slow_requests cap=8 policy=drop_oldest len=8 high=8 dropped=63 dropped_oldest=63 dropped_newest=0 accepted=8
capacity surface=soak.http.in_flight mode=fixed max=- cur=0 high=0 full=0 util_bp=10000 suggest="weighted fixed cap is tight; consider raising" weight_unit=requests max_weight=4 cur_weight=0 high_weight=4 weight_full=46
capacity surface=soak.db.in_flight mode=fixed max=- cur=0 high=0 full=0 util_bp=10000 suggest="weighted fixed cap is tight; consider raising" weight_unit=queries max_weight=2 cur_weight=0 high_weight=2 weight_full=12
capacity surface=soak.slow_requests mode=fixed max=8 cur=8 high=8 full=63 util_bp=10000 suggest="saw Full — raise cap or shed earlier"
capacity surface=soak.outbound.pool kind=pool_waiters state=unavailable reason="no outbound pool installed in this soak"
service=soak_http_db surfaces=4 measured=3 unavailable=1 full=121 unavailable_surfaces=soak.outbound.pool soak.outbound.pool=unavailable
FAIL surface=soak.http.in_flight filled=weight observed=46 — see capacity discovery for cap and high water
```

- `scope name=…` — `SharedCapacityScope::discovery_line`. One line per
  scope.
- `events sink=…` — `BoundedEventSink::discovery_line`. One line per
  event sink.
- `capacity surface=…` — universal `format_discovery_line` output.
  `util_bp` is high-water utilization in basis points. `state=unavailable`
  surfaces are explicit, never silently omitted.
- `service=…` — one-line summary. `unavailable_surfaces=` names the
  missing observers so on-call can find them.
- `FAIL surface=…` — `format_assertion_failure` output for each
  filled surface. Copyable into a test assertion.

## CI grep recipes

| Want | Recipe |
|---|---|
| Any fills? | `grep -E 'full=[1-9]' soak.out` |
| Unobserved surfaces | `grep state=unavailable soak.out` |
| Over 80% utilization | `awk 'match($0, /util_bp=([0-9]+)/, m) { if (m[1]+0 > 8000) print }' soak.out` |
| Copyable test failure | `grep '^FAIL surface=' soak.out` |

## Findings

What felt good:

- One `ServicePressureReport` ties every surface together, including
  surfaces the service does not (yet) measure. The discovery output
  always names a missing observer instead of leaving it implicit.
- Event sink drops are first-class — `dropped_oldest` vs
  `dropped_newest` lets a reader see policy effects without running
  the system.
- `assert_no_full()` returns every offender at once, so one CI run
  surfaces all caps that need tuning instead of one-at-a-time
  whack-a-mole.
- `flow!` carries the original request and move-only HTTP/DB leases
  through both raw timer outcomes. There is no qid, pending-reply map,
  take/reinsert cycle, or service-envelope construction in the service.
- Caller timeout and shutdown both release the parked lease exactly;
  the smoke suite asserts a timed-out request leaves both shared scopes
  at zero after clean terminal shutdown.

What remains policy-specific:

- The DB cap can lose a request after HTTP admit if DB is full. The
  specimen accepts that as "DbFull" but a real service might want a
  shared scope policy that holds the upstream lease until DB admits.

Tina capability pulled:

- `SharedCapacityScope`, `BoundedEventSink`, `ServicePressureReport`.
- `flow!` raw request steps and `then_service_event_with_request`.
- `CapacitySummary::assert_no_full` + `format_assertion_failure`.
- `format_discovery_line` (with the new `util_bp` field).

Verdict:

- keep
