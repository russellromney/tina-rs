# system_api_gateway_limits

Two routes, two shared weighted budgets. Proves
`SharedCapacityReservation` over two `SharedCapacityScope`s.

A gateway isolate exposes "upload" and "list" requests. Both routes
charge **two** shared, shard-local weighted scopes on every request:

- `gateway.in_flight` (cap `4`) — in-flight-request weight; uploads
  weigh `2`, lists weigh `1`.
- `gateway.body_bytes` (cap `4096`) — request body size; uploads are
  `1024` bytes, lists `128`.

A request is admitted only if **both** budgets have room:
`SharedCapacityReservation::try_reserve([in_flight.charge(...),
body_bytes.charge(...)])` charges both or neither. Callers race;
whichever budget fills first surfaces `Full { filled: "<scope>", … }`
naming the exact shared surface that was exhausted. The smoke tests
drive both the in-flight-bound case (default config) and the
body-bytes-bound case (loose in-flight cap, tight body cap).

## Run

```bash
cargo test --manifest-path examples/systems/system_api_gateway_limits/Cargo.toml
```

## Output shape

The smoke test asserts the *shape* of these lines, not the exact
counts. Counts depend on caller-thread scheduling; the load-bearing
invariants are listed under "What proves what".

```text
scope name=gateway.in_flight unit=weight max=4 cur=0 high=N full=N admitted=A released=A
capacity surface=gateway.in_flight mode=fixed max=- cur=0 high=0 full=0 util_bp=BP suggest="..." weight_unit=weight max_weight=4 cur_weight=0 high_weight=N weight_full=N
system=system_api_gateway_limits upload_admitted=A upload_full=F upload_timeout=T list_admitted=A list_full=F list_timeout=T scope_high_water=N scope_full_count=N scope_current_at_drain=0
```

- `scope ...` — `SharedCapacityScope::discovery_line`. One line per
  scope: cap, current, high water, full count, admitted/released
  totals.
- `capacity surface=...` — `format_discovery_line` for the same scope
  exposed as a `CapacitySurfaceReport`. Same key=value shape used
  everywhere else. `util_bp` is high-water utilization in basis
  points (0..=10000).
- `system=system_api_gateway_limits ...` — one-line summary. The
  smoke test grabs `scope_current_at_drain` after `runtime.shutdown()`
  to prove owner-stop release.

## What proves what

| Claim | Evidence |
|---|---|
| Two routes share one in-flight cap | `upload_admitted * 2 + list_admitted * 1 <= shared_cap` at peak; both routes can see `Full` from the same scope name. |
| Two routes share one body-bytes cap | `body_bytes_budget_fills_independently_of_in_flight`: with a loose in-flight cap and a tight body cap, `body_full_count > 0` while `scope_full_count == 0`, and every `Full` came from `gateway.body_bytes`. |
| Owner stop releases both budgets | `scope_admitted == scope_released`, `scope_current_at_drain == 0`, `body_admitted == body_released`, `body_current_at_drain == 0` after shutdown. |
| Full counter is honest | `scope_full_count == upload_full + list_full` when in-flight is the binding constraint. |
| Discovery line is CI-friendly | smoke test greps `scope `, `capacity surface=`, `util_bp=`, and the `gateway.body_bytes` line. |

## Findings

- `SharedCapacityReservation::try_reserve` makes the two-budget charge
  all-or-nothing. No manual rollback branch.
- `GuardedPendingReplies` parks the caller and owns the reservation.
  Owner-stop release falls out of dropping the parked guard — no
  lifecycle hook needed.
- `discovery_line` and `surface_report` use the same shape as the
  capacity discovery line, so one grepper covers both.

What felt rough:

- This specimen still names each scope at construction and at reporting
  time. A service-level scope registry could make that less repetitive.
- `tina-runtime` does not yet ship a runtime-wide registry of
  scopes, so each isolate carries its own clone. That is fine for a
  shard but a service builder may want a `register_scope("name")`
  one-liner.

Tina capability pulled:

- `SharedCapacityScope`, `SharedCapacityReservation`, `SharedScopeFull`.
- `GuardedPendingReplies`.
- `CapacitySummary::assert_no_full` for one-shot CI.
- `format_assertion_failure` for copyable FAIL lines.

Suggested follow-up:

- Service-level scope registry mirroring `register_with_capacity`.

Verdict:

- keep
