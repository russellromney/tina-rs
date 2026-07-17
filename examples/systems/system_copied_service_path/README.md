# system_copied_service_path

Canonical copied Tina service skeleton. Copy this shape when starting a
normal service.

One `Gateway` isolate runs behind the `LocalSystem` application facade:

- **Request entry** — `GatewayRequest::Submit` is a real caller-authority
  call through `LocalSystem::call_blocking_request`, not a canned
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
- **Linear held work** — request-aware `flow!` carries the caller and
  move-only `SharedLease` through the typed `SleepReply`; there is no qid,
  pending map, take/reinsert cycle, or service envelope.
- **Typed timer pressure** — the bounded timer lane is part of `RunConfig`,
  and refusal is reported as `WorkFailed(TimerFull)` rather than folded into
  application `Full` or a timeout.
- **Graceful shutdown** — `run()` uses `run_to_shutdown_reported`, shuts the app
  down even when registration or reporting fails, and re-reads the
  scope: `scope_current_at_drain` must be `0`. Owner stop drops every
  captured flow authority, which drops its `SharedLease` and releases the
  charge.

Concurrent callers are driven through `tina_proof_harness::load`'s real
load runner against the real runtime, and the run asserts
`assert_cold_work_made_progress` and `assert_no_leaked_capacity_at_shutdown`
against the terminal scope snapshot recorded after `LocalSystem` shutdown,
not a pre-shutdown or default observation.

The load harness scopes and joins its workers, so the operation closure borrows
the runner-owned `LocalSystem`; no application-wide `Arc` or separate shutdown
owner is required.

## What this specimen leaves out

Native protocol clients, session control, run capture/replay, and
join/select call sets are real Tina capabilities, but they do not belong
in the first thing a user copies. See `mini_saas_api` for a larger,
HTTP-fronted shape that uses several of them for real.

## Run

```sh
cargo run --manifest-path examples/systems/system_copied_service_path/Cargo.toml
cargo test --manifest-path examples/systems/system_copied_service_path/Cargo.toml
cargo test --manifest-path examples/systems/system_copied_service_path/Cargo.toml --test public_smoke public_smoke -- --exact
```

`RunConfig::validate` rejects zero capacity/callers/timeout and oversized
counts before the runtime, shared scope, or load runner are constructed.
Zero mailbox and zero timer capacity remain representable for intentional
pressure proofs and fail at the facade/runtime with typed outcomes.

## What proves what

| Claim | Evidence |
|---|---|
| Admission is really bounded | `RunConfig::default()` races 6 callers against capacity 2; `report.full > 0` and `report.admitted + report.full == callers`. |
| The durable-state step really runs | `report.ledger_final_len == report.ledger_seed_len + report.admitted` — one ledger append per admitted request, read back from the isolate via a `Stats` request before shutdown. |
| Owner stop really releases every charge | `report.scope_current_at_drain == 0` and `report.scope_admitted == report.scope_released`, both read from the scope after terminal `LocalSystem` shutdown. The request-aware flow owns each move-only lease until completion or owner stop, so there is no parallel cleanup path to keep in sync. |
| The leak check actually ran | `report.load.leak_checked` is set only after terminal shutdown and `leak_clean` is derived from both exact lease settlement and the post-shutdown surface plateau; an unchecked run renders `leak=unchecked` and `assert_no_leaked_capacity_at_shutdown` fails closed. |

## Findings

- `SharedCapacityScope` is the right tool for a post-shutdown leak
  check: it lives outside the isolate, so the caller can read
  `scope.snapshot()` after the runtime — and the isolate holding every
  `SharedLease` — is gone.
- The request-aware flow is the right shape for one linear hold: the shared
  scope is the honest bound on parked work, and caller loss or owner stop
  settles the lease by ownership rather than a second correlation table.

Verdict:

- keep
