# system_api_gateway_limits

Two routes, one cap. Proves [`SharedCapacityScope`].

A gateway isolate exposes "upload" and "list" requests. Both routes
charge one shared, shard-local weighted scope (`gateway.in_flight`,
cap `4`). Uploads weigh `2`, lists weigh `1`. Callers race; one
route can drain the scope on its own; the other still gets
`Full { filled: "gateway.in_flight", … }` because the cap is shared.

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
| Two routes share one cap | `upload_admitted * 2 + list_admitted * 1 <= shared_cap` at peak; both routes can see `Full` from the same scope name. |
| Owner stop releases held charges | `scope_admitted == scope_released` and `scope_current_at_drain == 0` after the runtime shuts down. |
| Full counter is honest | `scope_full_count == upload_full + list_full`. |
| Discovery line is CI-friendly | smoke test greps `scope `, `capacity surface=`, and `util_bp=`. |

## Findings

What felt good:

- `SharedCapacityScope::try_admit` returns a lease whose `Drop`
  releases the charge. Owner-stop release falls out of dropping the
  lease — no lifecycle hook needed.
- `discovery_line` and `surface_report` use the same shape as the
  capacity discovery line, so one grepper covers both.

What felt rough:

- Routing dispatch holds a `HashMap<qid, SharedLease>` so the lease
  outlives the timer wake-up. A future ergonomic affordance could
  attach the lease to the deferred reply slot directly.
- `tina-runtime` does not yet ship a runtime-wide registry of
  scopes, so each isolate carries its own clone. That is fine for a
  shard but a service builder may want a `register_scope("name")`
  one-liner.

Tina capability pulled:

- `SharedCapacityScope`, `SharedLease`, `SharedScopeFull`.
- `CapacitySummary::assert_no_full` for one-shot CI.
- `format_assertion_failure` for copyable FAIL lines.

Suggested follow-up:

- Lease handoff into `PendingReplies` slot so isolates do not need a
  parallel `HashMap`.
- Service-level scope registry mirroring `register_with_capacity`.

Verdict:

- keep
