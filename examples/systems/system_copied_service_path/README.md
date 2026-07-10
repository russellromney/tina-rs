# system_copied_service_path

Canonical copied Tina service skeleton. Copy this shape when starting a
normal service.

One `Gateway` isolate runs on a real `ThreadedRuntime`:

- **Request entry** — `GatewayRequest::Submit` is a real caller-authority
  call through `ThreadedRuntime::call_blocking_request`, not a canned
  reply.
- **Bounded admission** — `SharedCapacityScope` charges one weight unit
  per in-flight request. Over-capacity callers get a typed `Full { current,
  max }`, not a silently growing queue.
- **Durable-state step** — the isolate is constructed with a "recovered"
  ledger (state restored before it accepts traffic) and every admitted
  request commits one more record to that ledger before it is held for
  work. The ledger is an in-process `Vec<u64>` standing in for a real
  WAL/database write; swap `Gateway::ledger` for your real store.
- **Reply** — once the (simulated) work finishes, the isolate releases the
  admission charge and replies `Accepted { id, ledger_len }`.
- **Graceful shutdown** — `run()` shuts the runtime down and re-reads the
  scope: `scope_current_at_drain` must be `0`. Owner stop drops every
  parked guarded reply, which drops its `SharedLease`, which releases the
  charge — no separate cleanup path to forget.

Concurrent callers are driven through `tina_proof_harness::load`'s real
load runner against the real runtime, and the run asserts
`assert_cold_work_made_progress` and `assert_no_leaked_capacity_at_shutdown`
against a leak-check closure that reads the scope's real post-run state
— not a `LoadObservation::default()` placeholder.

## What this specimen leaves out

Native protocol clients, session control, run capture/replay, and
join/select call sets are real Tina capabilities, but they do not belong
in the first thing a user copies. See `mini_saas_api` for a larger,
HTTP-fronted shape that uses several of them for real.

## Run

```sh
cargo run --manifest-path examples/systems/system_copied_service_path/Cargo.toml
cargo test --manifest-path examples/systems/system_copied_service_path/Cargo.toml
```

## What proves what

| Claim | Evidence |
|---|---|
| Admission is really bounded | `RunConfig::default()` races 6 callers against capacity 2; `report.full > 0` and `report.admitted + report.full == callers`. |
| The durable-state step really runs | `report.ledger_final_len == report.ledger_seed_len + report.admitted` — one ledger append per admitted request, read back from the isolate via a `Stats` request before shutdown. |
| Owner stop really releases every charge | `report.scope_current_at_drain == 0` and `report.scope_admitted == report.scope_released`, both read from the scope *after* `runtime.shutdown()`. Comment out the `drop(lease)` in `Gateway::hold_done` and `assert_no_leaked_capacity_at_shutdown` fails with a real leak, not a placeholder. |
| The leak check actually ran | `report.load.leak_checked` is `true` because the run supplies a real observation closure; an unchecked run renders `leak=unchecked` and `assert_no_leaked_capacity_at_shutdown` fails closed. |

## Findings

- `SharedCapacityScope` is the right tool for a post-shutdown leak
  check: it lives outside the isolate, so the caller can read
  `scope.snapshot()` after the runtime — and the isolate holding every
  `SharedLease` — is gone.
- `GuardedPendingReplies<K, R, SharedLease>` pairs the parked caller
  with the charge it is holding, so there is no separate
  `HashMap<K, SharedLease>` to keep in sync by hand.
- Pending capacity is set equal to scope capacity, so
  `insert_deferred_guarded` can never see `Full`/`DuplicateKey` in
  practice; those arms panic loudly instead of pretending to handle an
  unreachable accounting bug (same reasoning as `system_job_queue`).

Verdict:

- keep
