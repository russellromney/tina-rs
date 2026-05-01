# 018 Voyager Single-Shard Supervision Parity Plan

Session:

- A

## What We Are Building

Build the next Voyager slice: extend `tina-sim` so it can replay the core
single-shard supervision story that Mariner already proved live.

Concretely, this package adds:

- simulator execution of the real public spawn surface already shipped in
  `tina`:
  - `Effect::Spawn(SpawnSpec<_>)`
  - `Effect::Spawn(RestartableSpawnSpec<_>)`
- simulator support for runtime-owned direct parent-child lineage
- simulator support for restartable child records
- simulator support for direct-child `RestartChildren`
- simulator support for supervised panic restart using the current supervision
  configuration shape and restart-budget meaning
- replayable proof workloads that exercise spawn, panic, stale identity, and
  replacement-child continuity
- replayable proof workloads that also cover bootstrap re-delivery,
  restart-budget exhaustion, and repeated restart reproducibility

The point of 018 is not merely “spawn bookkeeping exists in the simulator.”
The point is that Voyager should be able to replay the same single-shard child
failure and restart story that already matters in Mariner.

## What Will Not Change

- `tina` remains substrate-neutral and does not gain simulator-only public
  API.
- This slice stays single-shard.
- This slice does not simulate TCP yet.
- This slice does not add cross-shard routing or placement.
- This slice does not add a broad checker DSL.
- This slice does not change the already-shipped meaning of timers, seeded
  timer/local-send perturbation, or replay artifacts from 016/017 except as
  needed to incorporate spawn/restart events honestly.
- This slice does not newly broaden seeded perturbation into spawn selection,
  restart policy choice, or bootstrap generation. 017's seeded fault surfaces
  remain additive and scoped to local-send plus timer-wake behavior unless 018
  explicitly exercises their interaction with restart workloads.

## How We Will Build It

- Reuse the live runtime event vocabulary; do not invent simulator-only spawn
  or supervision semantics.
- Execute the same already-shipped public inputs users already write at the
  `tina` boundary and in runtime configuration:
  - `SpawnSpec`
  - `RestartableSpawnSpec`
  - `RestartChildren`
  - current supervision configuration / policy / budget shape, covering the
    shipped `RestartPolicy` variants:
    - `OneForOne`
    - `OneForAll`
    - `RestForOne`
    and restart-budget acceptance / exhaustion via `RestartBudget`
  - restartable bootstrap payload semantics, including bootstrap re-delivery on
    every replacement incarnation
- Keep the simulator faithful to the already-shipped single-shard runtime
  meaning:
  - spawn is still later-turn child execution
  - parent-child lineage is runtime-owned
  - restartable child records survive stop/panic
  - `RestartChildren` stays direct-child only
  - supervised panic restart stays policy/budget driven
  - same-step multiple spawns are recorded and delivered in deterministic
    effect order
  - stale pre-restart child identity fails via the same observable rejected
    send shape the runtime already uses:
    `RuntimeEventKind::SendRejected { reason: SendRejectedReason::Closed, .. }`
  - non-restartable children are skipped via the same observable restart-skip
    shape the runtime already uses:
    `RuntimeEventKind::RestartChildSkipped { reason: RestartSkippedReason::NotRestartable, .. }`
  - budget exhaustion is visible through the same observable supervision
    rejection shape the runtime already uses:
    `RuntimeEventKind::SupervisorRestartRejected { reason: SupervisionRejectedReason::BudgetExceeded { .. }, .. }`
- Be explicit about replay honesty:
  - replay artifacts must be sufficient to reproduce the same run when loaded
    back into the same workload binary and simulator version
  - replay does not serialize arbitrary isolate values or restart/bootstrap
    closures; the workload code remains part of the replay boundary
- Prefer one coherent dispatcher-style workload plus one smaller
  panic/restart-specific harness over many tiny helper tests.

## Build Steps

1. Extend `tina-sim` so it can execute the real public spawn payloads already
   shipped at the `tina` boundary, including restartable specs and bootstrap
   messages where relevant.
2. Record runtime-owned direct parent-child lineage and restartable child
   records in the simulator using the same meaning already shipped in
   `tina-runtime-current`.
3. Implement direct-child `RestartChildren` execution in the simulator.
4. Implement supervised panic restart in the simulator using the current
   supervision configuration shape and restart-budget meaning.
5. Extend replay artifacts only as needed for exact reproduction of spawned and
   restarted-child runs.
6. Add direct proofs for:
   - spawned child runs only on a later step
   - same-step multiple spawns preserve deterministic effect-order delivery
   - bootstrap is re-delivered on every replacement incarnation
   - child lineage survives stop, panic, and restart
   - restarted child gets a fresh incarnation
   - repeated restart (`N >= 2`) is reproducible and advances incarnation
     counters monotonically
   - non-restartable child is skipped via
     `RestartChildSkipped { reason: RestartSkippedReason::NotRestartable, .. }`
   - stale pre-restart child identity fails via
     `SendRejected { reason: SendRejectedReason::Closed, .. }`
   - `RestartChildren` preserves direct-child-only scope
   - supervised panic restart is reproducible from the saved artifact plus the
     same workload binary
   - restart-budget exhaustion is visible and reproducible via
     `SupervisorRestartRejected { reason: SupervisionRejectedReason::BudgetExceeded { .. }, .. }`
7. Add user-shaped workloads:
   - one dispatcher-style preserved-success workload under simulation that
     proves:
     - replacement child receives bootstrap again after restart
     - later work reaches a replacement child after restart
     - stale pre-restart identity fails safely via
       `SendRejected { reason: SendRejectedReason::Closed, .. }`
   - one panic/restart workload that a checker can observe and replay
     - this workload must exercise at least one restart-sensitive invariant,
       not just "a panic happened"
8. Close out docs/evidence:
   - `.intent/SYSTEM.md`
   - `ROADMAP.md`
   - `CHANGELOG.md`
   - phase `commits.txt`
   - review receipt

## How We Will Prove It

Direct proof for the changed behavior:

- at least one public-path workload must execute the real public
  spawn/restart/supervision surface, not simulator-only helpers
- focused simulator tests for:
  - spawned child runs only on a later step
  - same-step multiple spawns preserve deterministic effect-order delivery
  - bootstrap re-delivery occurs on every replacement incarnation
  - restarted child gets a fresh incarnation
  - repeated restart (`N >= 2`) is reproducible
  - non-restartable child is skipped via
    `RestartChildSkipped { reason: RestartSkippedReason::NotRestartable, .. }`
  - restart-budget exhaustion is visible via
    `SupervisorRestartRejected { reason: SupervisionRejectedReason::BudgetExceeded { .. }, .. }`
  - stale identity closes/fails via
    `SendRejected { reason: SendRejectedReason::Closed, .. }`
  - direct-child-only restart scope is preserved
  - same artifact plus the same workload binary reproduces the same
    spawned/restarted event record
- checker/replay tests for:
  - a checker can observe a restart-sensitive invariant such as
    "post-restart work must target only the newest incarnation"
  - replay reproduces the same panic/restart failure exactly
- dispatcher-style preserved-success tests for:
  - replacement child receives bootstrap again after restart
  - work routed through a replacement child after restart
  - stale identity closes/fails safely after restart via
    `SendRejected { reason: SendRejectedReason::Closed, .. }`

Proof modes expected in this package:

- unit proof:
  lineage / child-record bookkeeping
- integration proof:
  spawn and restart semantics on the simulator path
- e2e proof:
  user-shaped dispatcher/supervision workload through `tina-sim`
- adversarial proof:
  checker-triggered failure around panic/restart behavior
- replay proof:
  saved artifact reproduces the same panic/restart run
- blast-radius proof:
  all prior Mariner and Voyager suites still pass

Surrogate proof that helps but does not close the claim:

- proving spawn bookkeeping without a public-path spawned-child workload
- proving checker plumbing on a toy failure that does not involve restart
- proving restart mechanics without later work reaching a replacement child
- proving stale-identity safety only through helper state rather than event
  meaning
- proving one successful restart without budget exhaustion or repeated-restart
  coverage
- relying only on final state without the replayed event record

## How We Will Prove We Did Not Break Earlier Intent

Blast-radius proof for already-shipped behavior:

- existing `tina-sim` timer/replay/fault/checker tests still pass
- existing `Spawn = Infallible` simulator workloads still pass unchanged;
  restart support is additive over the already-shipped spawn-only surface
- existing `tina-runtime-current` tests still pass
- `make verify` still passes
- no `tina` boundary changes are required

## Pause Gates

Stop and ask for human input only if one of these happens:

- faithful simulator spawn/restart support appears to require a new public
  `tina` boundary shape
- faithful restart simulation appears to require TCP simulation after all
- the checker surface wants a much broader API than this package should own
- honest replay appears to require serializing arbitrary isolate or closure
  state rather than depending on the same workload binary
- the only honest preserved-success workload is substantially larger than this
  slice should carry

## Traps

- Do not invent simulator-only child semantics.
- Do not broaden restart beyond direct-child scope.
- Do not silently narrow covered supervisor policy variants; if only a subset
  is implemented in 018, name the subset explicitly in code and docs.
- Do not make replay depend on logs instead of structured artifact data.
- Do not regress the already-shipped timer/fault/checker slice while adding
  spawn/supervision support.
- Do not declare simulator supervision “done” unless a public-path restart
  workload is actually replayed and proves replacement-child continuity plus
  stale-identity failure.

## Files Likely To Change

- `tina-sim/src/lib.rs`
- `tina-sim/tests/`
- `.intent/SYSTEM.md`
- `ROADMAP.md`
- `CHANGELOG.md`

## Areas That Should Not Be Touched

- mailbox crates
- Betelgeuse backend
- live TCP semantics
- multi-shard runtime code
- Tokio bridge code

## Report Back

- exact simulator spawn/restart surface
- exact supervisor policy / budget variants covered in 018
- exact lineage/restart state added
- exact checker/replay additions, if any
- which workloads proved preserved success vs replayed failure
- how bootstrap re-delivery, replacement-child continuity, and stale-identity
  failure were proved
- what is directly proved vs surrogate-supported
- whether 017 seeded local-send/timer perturbation was exercised in any 018
  restart workload, or explicitly left orthogonal
- exact verification results
