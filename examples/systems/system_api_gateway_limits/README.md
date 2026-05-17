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

The smoke test asserts these grep-friendly lines:

```text
scope name=gateway.in_flight unit=weight max=4 cur=0 high=4 full=2 admitted=14 released=14
capacity surface=gateway.in_flight mode=fixed max=- cur=0 high=0 full=0 util_bp=10000 suggest="weighted fixed cap fits" weight_unit=weight max_weight=4 cur_weight=0 high_weight=4 weight_full=2
system=system_api_gateway_limits upload_admitted=2 upload_full=2 list_admitted=6 list_full=0 scope_high_water=4 scope_full_count=2
```

- `scope ...` — `SharedCapacityScope::discovery_line`. One line per
  scope: cap, current, high water, full count, admitted/released
  totals.
- `capacity surface=...` — `format_discovery_line` for the same scope
  exposed as a `CapacitySurfaceReport`. Same key=value shape used
  everywhere else.
- `system=system_api_gateway_limits ...` — one-line summary copy of
  what each caller observed.

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
