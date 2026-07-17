# Public Example Certification

## What we are building

Make every public Tina specimen, system, extension, README, and runnable entry
point suitable for the 0.1.0 public launch.

The public corpus must teach one coherent Tina model:

- actors own mutable state and terminal reports;
- caller, child, permit, lease, and resource authority stay typed and linear;
- producers are bounded before effects, allocations, threads, or sockets exist;
- `Full`, `Closed`, `Timeout`, `Rejected`, cancellation, failure, and shutdown
  remain distinct until application policy deliberately combines them;
- `LocalSystem` or `LocalMultiShardSystem` is the normal live host facade;
- live and simulator authoring use the same vocabulary when they share a
  contract;
- application code does not construct `ServiceMessage` envelopes or publish
  results through mutexes, condvars, polling loops, or default-data sidecars.

This is one certification objective executed through dependency-ordered pull
requests. It is complete only when the final by-hand audit finds no unexplained
example-local workaround.

## Current baseline

- `origin/main` must be fetched before execution; do not trust the root
  worktree, which may contain unrelated in-progress changes.
- PR #365 is already merged and canonicalizes bounded terminal behavior in
  eleven specimens.
- PR #364 contains typed restart-aware service continuations and the supervised
  worker migration.
- PR #366 contains actor-backed typed gRPC routes and the gRPC specimen
  migration.
- Re-check the state and CI of #364 and #366 rather than relying on this text.
- `examples/FINDINGS.md` is the public closure ledger, not the implementation
  source of truth. Cohort PR descriptions and `commits.txt` carry in-flight
  state; C1 reconciles the public ledger after migration branches have merged.

## What will not change

- Tina remains nightly-only for 0.1.0.
- The Tinio rename is outside this phase.
- Tokio implementations remain honest comparison controls.
- Explicit runtime benchmarks may continue to own raw runtimes.
- Low-level stepping, replay, and ownership demonstrations may continue to use
  low-level APIs when that is the behavior being demonstrated.
- The deliberately adversarial owned-state-leak specimen remains an
  anti-pattern demonstration and must be labeled as such.
- Wire protocols, persistence semantics, replay traces, benchmark workloads,
  allocation ceilings, and user-visible business behavior do not change unless
  a current example is incorrect. Any correction must be named and directly
  proved.
- No generic configuration DSL, universal transport trait, generic report
  channel, or arbitrary multi-outbound abstraction will be introduced without
  two concrete consumers and a smaller public call site.
- Phase names, phase numbers, and intent-artifact names must not appear in
  production code, tests, comments, trace vocabulary, or public documentation.

## Simplifying decisions

1. Use existing `stop_with` and `observe_result` for reports. Do not create a
   second report-delivery abstraction.
2. Solve spawned-child awkwardness as typed child lifecycle observation
   (initial, replacement, terminal result), not arbitrary multiple outbound
   channels, unless direct implementation proves the narrower contract cannot
   work.
3. Use local `RunConfig::validate` methods and typed local errors. Do not build a
   validation framework.
4. Keep LocalSystem resource installation separate from transport-to-service
   delivery. They have different authority and failure contracts.
5. Reuse typed split-service handles for HTTP and WebSocket delivery. Add a
   common adapter only after both implementations demonstrate the same shape.
6. Group mechanical migrations by invariant, not directory. Give examples
   separate pull requests only when their failure or ownership contracts differ.
7. Maintain one reviewed allowlist for intentional raw-runtime and shared-state
   examples. Every entry needs a reason; everything else fails the guards.

## Execution protocol

### Frozen start and durable state

The orchestrator begins by fetching `origin/main` and appending the fetched SHA,
UTC time, and `gh pr view --json` state for #364, #365, and #366 to
`commits.txt`. That recorded SHA is the only starting baseline. For each prior
PR:

- merged: verify its merge commit is an ancestor and its required workflow is
  green;
- open: record its head SHA, rebase it, re-run independent review and replacement
  CI, then merge it;
- closed or superseded: verify the promised behavior on the frozen baseline and
  either record the replacement PR or create a new prerequisite branch.

Prior review counts only when the PR or repository records its reviewed head
SHA, findings, fixes or evidence-based rejection, and the replacement workflow.

### Dependency and worktree graph

Use these stable work codes in PR descriptions and `commits.txt`; do not put
them in production identifiers, tests, traces, comments, or public docs.

```text
I0A restart continuation (#364) --> F1 terminal observation --> M1 child-based examples --+
I0B actor-backed gRPC (#366) -----------------------------------+
D1 direct bounds/outcomes --------------------------------------+
F2 keepalive installation ----------> M4 network systems -------+--> M6 residual hosts --> C1 final certification
F3 typed HTTP delivery --------------> M4 network systems -------+
F4 WebSocket parity/correctness -----> M4 network systems -------+
M2 observed results --------------------------------------------+
M3 SQLite observed result --------------------------------------+
M5 multi-shard session host ------------------------------------+
```

After baseline reconciliation, D1, F1, F2, F3, F4, and the already-independent
M2/M3 work may proceed concurrently in dedicated worktrees. F1 waits for I0A
and M1 waits for F1;
M4 waits for all of F2/F3/F4; M6 waits for all preceding migrations; C1 starts
only after everything else has merged. A framework prerequisite never edits its
motivating example. The owning migration PR performs that example's complete
conversion after rebasing on every prerequisite.

Only one branch updates `examples/FINDINGS.md` at a time. Cohort branches keep
their ledger in their PR description; C1 reconciles all merged evidence into
`FINDINGS.md`. After every merge, append the merge SHA, reviewed head SHA,
workflow URL/result, next eligible work codes, and rebased dependent heads to
`commits.txt`. A single orchestrator owns a dedicated pushed tracking branch
for this file. Every checkpoint is committed and pushed before more branches
start. Resume by fetching that branch and reading its last commit, never by
trusting a root-worktree append. The tracking branch is an execution record,
not a required product PR; C1 copies the complete pre-certification ledger into
main, while the tracking branch records the final main SHA and workflow without
creating a self-referential documentation commit.

### Settled framework contracts

**Typed child lifecycle.** The narrow user shape is observed spawning with typed
initial/replacement events plus a typed terminal mapper, conceptually
`spawn_observed(...).then_service_result(ParentEvent::ChildStopped)`. Child-result
authority moves from the child to the runtime and then exactly once to the
parent event. Admission reserves the bounded parent-delivery slot for that
generation; if reservation is `Full` or `Closed`, spawn or restart is not
admitted and returns that typed outcome. There is no hidden retry queue. Parent
cancellation or system shutdown cancels the reservation and disposes the result
exactly once with a typed trace reason. Duplicate and stale-generation delivery
is rejected and disposed exactly once. Live and simulator owners expose the
same result, traces, and settlement behavior.

**Keepalive installation.** The user shape is
`let pool = system.install_keepalive_pool(config)?` followed by the consuming
`pool.close_and_drain(timeout)`. Installation is atomic. Partial failure rolls
back every installed resource and returns a typed rollback report; duplicate
installation returns a typed conflict. Consuming close makes double close
unrepresentable. A drain timeout does not silently abort admitted work: it
returns the owned handle and exact pending counts so the caller can retry. There
is no public force-close path in this work. Owner failure, explicit close, and
system shutdown are distinct typed outcomes; shutdown follows the runtime's
existing typed cancellation settlement rather than pretending to drain.
Successful drain proves every admitted request and connection settled.

**HTTP delivery.** A typed event-only, request-only, or split-service handle is
installed directly. Transport admission consumes only the authority its lane
requires and preserves exact `Full`, `Closed`, timeout, rejection, cancellation,
peer-close, and shutdown outcomes. It does not expose a raw address or public
envelope alias. Event-only HTTP is explicitly admission-oriented: valid input
that is accepted into the service mailbox completes with `202 Accepted` and an
empty body; it does not claim the actor processed the event. `Full` maps to 429,
`Closed` and shutdown to 503, invalid input to 400, and transport failure remains
a transport error. Request-only and split request lanes do not complete until
the typed reply or exact terminal outcome is available.

**WebSocket delivery and correction.** This is separate from HTTP. When client A
submits a room text event, the room mailbox establishes the ordering boundary.
The message is offered exactly once, in that room-event order, to every other
session admitted in the membership snapshot; A is excluded. A closed or stale
recipient produces an exact typed outcome and is removed according to existing
room policy. No connection write completion enters the application's request
lane. A room send consumes event authority; a call consumes request/reply
authority. There is no global ordering promise before concurrent inputs reach
the room mailbox. Deterministic two-client tests must prove no omission,
duplication, or lane confusion.

### Intentional low-level allowlist

Only these purposes may retain raw runtime/shared-state forms, and each still
needs its focused behavior test:

- `specimen_cpu_run` and `specimen_mem_run`: explicit runtime benchmark controls;
- `specimen_cross_shard_child_ownership`: low-level ownership demonstration;
- `specimen_tracing_demo`: explicit stepping/trace demonstration;
- `specimen_owned_state_leak`: clearly labeled adversarial anti-pattern;
- raw-runtime rows in `perf_native`: comparison benchmark controls only.

No other example may be added without a recorded human decision and reviewer
agreement.

### Correction ledger

Record corrections in `commits.txt` before implementing them:

| Correction | Old behavior | New promise | Compatibility and direct proof |
| --- | --- | --- | --- |
| Restart factory panic | Panic-shaped or incomplete restart reporting | Typed `FactoryPanicked` for initial and replacement creation | Error surface becomes explicit; focused initial/replacement panic tests |
| WebSocket room race | Cross-client output can be omitted or delivered through the wrong lane | Snapshot recipients excluding sender, exactly one offer in room mailbox order | Intentional correctness fix; deterministic two-client network and lane tests |

Any new correction to wire behavior, persistence, replay facts, benchmark
workload, or allocation ceilings stops for a human decision. Other corrections
must still add a row with characterization of the old intended behavior.

### Proof mechanisms

- Preallocation rejection tests use test-only resource counters/hooks around
  runtime, thread, socket, barrier, mailbox, map, and batch creation and assert
  all remain zero.
- Authority and settlement tests use explicit acquire/transfer/release ledgers
  and assert one terminal disposition per capability and resource.
- Capability non-forgeability uses compile-fail fixtures, not runtime tests.
- Installation rollback injects failure at every install boundary and compares
  acquired and released resource ledgers exactly.
- Child duplicate/stale tests control generations and delivery barriers; race
  tests use deterministic barriers and bounded queues, never sleeps.
- Benchmark and replay migrations first record existing accepted counts/facts
  in characterization tests, then re-prove them after the change.
- A finding is fixed and committed before replacement CI. Rejection requires
  falsifying evidence in the PR and explicit agreement from the independent
  reviewer.

For every pull request:

1. Fetch current `origin/main`.
2. Create a dedicated branch and worktree from that exact commit.
3. Write the desired user-facing code shape before adding framework API.
4. Implement the smallest contract justified by the motivating example.
5. Add direct, integration, end-to-end, adversarial, and blast-radius proof as
   applicable.
6. Run focused tests and affected crates before broad gates.
7. Commit and open a non-draft pull request with exact proof evidence.
8. Run an independent adversarial implementation review. The reviewer must try
   to falsify authority, boundedness, failure, cancellation, and shutdown
   claims, then fix every finding and commit those fixes.
9. Wait for the complete replacement GitHub matrix. Do not merge with pending
   or failing checks.
10. Merge using the repository convention, update `main`, verify post-merge CI,
    rebase dependents, and record the merge and proof in `commits.txt`.

Do not preserve a legacy form because it currently works. If the current API
cannot express a clean example, stop that example, implement its prerequisite,
merge the prerequisite, rebase, and finish the example.

## Finite corpus manifest

This is the starting scope. C1 must fail if filesystem discovery finds a public
crate, runnable entry point, README, smoke test, or guide page absent from this
manifest. Every crate row includes its `Cargo.toml`, Tina-facing `src`, runnable
`main`, tests, and local README. Its exact focused command is
`cargo test --manifest-path <path>/Cargo.toml --all-targets`; also run
`cargo clippy --manifest-path <path>/Cargo.toml --all-targets -- -D warnings`
and `RUSTDOCFLAGS='-D warnings' cargo doc --manifest-path <path>/Cargo.toml
--no-deps`. `P` means preserve a characterization test for protocol/persistence/
replay facts, `B` means bounded-input counters, `T` means exhaustive terminal
outcomes, `A` means authority/settlement ledger, `N` means a real network path,
and `S` means live/simulator parity. Cases not represented by a target's public
contract are `N/A`; tests must say why rather than synthesize a fake outcome.

Every crate row has one executable proof target named `public_smoke` containing
an exact test function named `public_smoke`. Its command is therefore
`cargo test --manifest-path <path>/Cargo.toml --test public_smoke public_smoke
-- --exact`. If it does not exist, that row's owner adds it. The test invokes the
same public runner or binary path documented in the README; binary-only crates
use Cargo's `CARGO_BIN_EXE_*` integration-test executable rather than copying
the implementation. A changed row also has `public_characterization` in that
target, written before migration and run with the same command replacing the
final test filter. A named correction adds `public_correction`. The PR ledger
records all three exact expanded commands and marks a nonexistent case `N/A`
with the contract reason.

Default features and no external network are the norm. Network rows bind
loopback ephemeral ports and use deterministic in-process peers. TLS uses
checked-in test certificates; persistence uses isolated temporary directories;
SQLite uses a temporary database. `specimen_postgres_counter` is the sole
external-service row: CI starts PostgreSQL 16, sets a unique schema through
`DATABASE_URL`, runs both Tina and control paths, and drops the schema. A skip
when the variable is absent is not certification evidence. No smoke test may be
ignored, sleep for race ordering, or contact the public internet.

### Specimens

| Path (`examples/…`) | Owner | Direct proof and old behavior at risk |
| --- | --- | --- |
| `specimen_axum_counter` | C1 | T,N; Axum request/count behavior |
| `specimen_backpressure_chain` | C1 | T,A; chained pressure settlement |
| `specimen_bounded_batcher` | C1 | B,T; batch ceiling and refill |
| `specimen_cancellation_chain` | C1 | T,A; cancellation propagation |
| `specimen_cpu_run` | C1 (allowlisted) | P; benchmark workload |
| `specimen_cross_shard_child_ownership` | C1 (allowlisted) | A,S; cross-shard ownership |
| `specimen_dynamic_worker_pool` | C1 | T,A; resize and worker settlement |
| `specimen_graceful_drain_server` | M6 | T,A,N; admitted request drain |
| `specimen_graceful_pool_shutdown` | M6 | T,A; pool terminal behavior characterized by #365 |
| `specimen_graceful_shutdown` | M2 | T,A; terminal report and cleanup |
| `specimen_grpc_counter` | I0B | B,T,A,N; unary/stream protocol and HTTP/2 |
| `specimen_hot_key_fairness` | C1 | B,T; fairness and capacity |
| `specimen_http_body_streaming` | M6 | B,T,A,N; streaming body protocol |
| `specimen_idempotent_retry` | M6 | T,A; idempotency and retry count |
| `specimen_local_io_codec_ipc` | C1 | T,A; codec and IPC settlement |
| `specimen_mem_run` | C1 (allowlisted) | P; benchmark workload |
| `specimen_mini_keyspace` | M6 | T,A; keyspace results |
| `specimen_multi_turn_request_context` | C1 | add README/public runner, then T,A smoke for caller authority across turns |
| `specimen_mux_client` | M6 | T,A,N; multiplexed client protocol |
| `specimen_native_http` | M6 | B,T,A,N; HTTP/1.1 behavior |
| `specimen_native_https` | M6 | B,T,A,N; TLS and HTTP behavior |
| `specimen_outbound_fetch` | M6 | T,A,N; outbound fetch results |
| `specimen_outbound_http` | M6 | T,A,N; outbound HTTP behavior |
| `specimen_owned_state_leak` | C1 (allowlisted) | A; anti-pattern remains explicit and contained |
| `specimen_periodic_batcher` | M6 | B,T,A; timer batching and shutdown |
| `specimen_persistent_counter` | M2 | P,T,A; persisted value and terminal report |
| `specimen_pool_cancel_reclaim` | M6 | T,A; cancellation reclaim characterized by #365 |
| `specimen_postgres_counter` | M6 | P,T,A; database update/query behavior |
| `specimen_rate_limited_worker` | C1 | B,T,A; admission/refill accounting |
| `specimen_real_io_chat` | M1 | T,A,N,S; connection result and chat protocol |
| `specimen_replay_dst` | C1 | P,T,S; replay facts and DST behavior |
| `specimen_request_scope_fanout` | M6 | T,A; scoped fanout settlement characterized by #365 |
| `specimen_retrying_outbound_http` | M6 | T,A,N; retry policy and request count |
| `specimen_rpc` | C1 | T,A; RPC outcomes characterized by #365 |
| `specimen_scatter_gather` | M6 | T,A; aggregate settlement characterized by #365 |
| `specimen_sharded_fanout_read` | M6 | T,A,S; shard fanout and result aggregation |
| `specimen_sharded_keyspace` | M6 | T,A,S; sharded keyspace behavior |
| `specimen_sqlite_counter` | M3 | P,T,A; SQLite query/update and metrics |
| `specimen_supervised_worker` | I0A | T,A,S; initial/replacement lifecycle and panic |
| `specimen_tcp_echo` | C1 | add README/public runner, then T,A,N smoke for echo protocol characterized by #365 |
| `specimen_tower_timeout_counter` | C1 | T,A; Tower timeout distinction |
| `specimen_tracing_demo` | C1 (allowlisted) | P,S; stepping/trace vocabulary |
| `specimen_two_stage_pipeline` | M6 | T,A; pipeline settlement characterized by #365 |
| `specimen_webhook_fanout` | M6 | T,A,N; fanout settlement characterized by #365 |
| `specimen_webhook_outbox` | C1 | P,T,A; durable outbox behavior |
| `specimen_webhook_publisher` | M6 | T,A,N; publisher protocol and retry |
| `specimen_websocket_room` | M4 | T,A,N; exact cross-client delivery correction |
| `specimen_worker_pool` | C1 | T,A; worker settlement characterized by #365 |
| `specimen_ws_room` | C1 | T,A,N; existing WebSocket room protocol |

### Systems and playground

| Path (`examples/…`) | Owner | Direct proof and old behavior at risk |
| --- | --- | --- |
| `systems/ergonomics_playground` | M2 | T,A,S; public call shape and result |
| `systems/mini_saas_api` | M4 | B,T,A,N; HTTP protocol, report, drain |
| `systems/perf_native` | D1 | P,B,T; accepted workload/counts; raw benchmark rows are allowlisted and verified by C1 |
| `systems/system_api_gateway_limits` | C1 | B,T,A,N; gateway limits |
| `systems/system_bounded_object_lane` | D1 | B,T,A; object-lane admission |
| `systems/system_cache_with_fill` | D1 | B,T,A; fill deduplication and pressure |
| `systems/system_copied_service_path` | D1 | B,T,A; copied-service bounds/results |
| `systems/system_job_queue` | M1 | B,T,A,S; readiness/restart/exact startup failure |
| `systems/system_live_replay_bugbox` | M2 | P,T,A,S; equivalent live/replay facts |
| `systems/system_lock_manager` | D1 | B,T,A; lock admission/release |
| `systems/system_metrics_shipper` | M2 | T,A; actor-owned metrics report |
| `systems/system_realtime_rooms` | M4 | T,A,N; exact cross-client delivery and stats |
| `systems/system_scoped_request_tree` | M4 | T,A,N; HTTP request tree and enrich outcomes |
| `systems/system_session_auth` | M5 | B,T,A,S; shard/time/session semantics |
| `systems/system_soak_http_db` | D1 | P,B,T,A,N; accepted soak workload and DB behavior |
| `systems/system_tenant_rate_limiter` | C1 | B,T,A,S; tenant admission/refill |
| `systems/system_webhook_relay` | C1 | B,T,A,N; relay protocol and delivery settlement |

### Extensions and documentation

| Path (`examples/…` unless noted) | Owner | Direct proof and old behavior at risk |
| --- | --- | --- |
| `extensions/tina-extension-capacity-surface` | C1 | B,T; public capacity surface |
| `extensions/tina-extension-compile-fail` | C1 | compile-fail capability and API-shape fixtures |
| `extensions/tina-extension-custom-codec` | C1 | T,A; codec extension contract |
| `extensions/tina-extension-fake-bridge` | C1 | T,A,S; bridge ownership/parity |
| `extensions/tina-extension-service-policy` | C1 | B,T; policy outcomes |
| `README.md`, `extensions/README.md`, `systems/README.md` | C1 | every command and API claim matches merged code |
| every crate-local README and smoke test covered above | same owner as crate | runnable user path and terminal claims |
| `FINDINGS_HISTORY.md` | C1 | preserved historical claims are clearly historical |
| `systems/perf_native/fly/README.md` | D1 | deployment workload and count claims |
| `docs/bridge-composition.md` | C1 | bridge ownership/composition vocabulary |
| `docs/mailbox-capacity.md` | C1 | capacity and terminal outcomes |
| `docs/resource-owner-matrix.md` | C1 | resource ownership and settlement |
| `docs/tcp-loops.md` | M6 | live host and TCP loop guidance |
| `docs/tina-user-guide/00-agent-quickstart.md` | C1 | canonical quickstart |
| `docs/tina-user-guide/01-mental-model.md` | C1 | ownership mental model |
| `docs/tina-user-guide/02-first-isolate.md` | C1 | canonical isolate form |
| `docs/tina-user-guide/03-effects-and-runtime-calls.md` | C1 | bounded typed effects |
| `docs/tina-user-guide/04-request-reply.md` | C1 | request/reply authority |
| `docs/tina-user-guide/05-tcp-services.md` | C1 | typed TCP service host |
| `docs/tina-user-guide/06-boundedness-and-overload.md` | C1 | bounds and exhaustive overload |
| `docs/tina-user-guide/07-supervision.md` | C1 | restart-aware lifecycle form |
| `docs/tina-user-guide/08-simulation-and-dst.md` | C1 | live/simulator parity |
| `docs/tina-user-guide/09-tokio-to-tina-porting.md` | C1 | public facade migration |
| `docs/tina-user-guide/10-service-patterns.md` | C1 | split typed services |
| `docs/tina-user-guide/11-ergonomics-checklist.md` | C1 | checklist matches guards |
| `docs/tina-user-guide/12-io-model.md` | C1 | final resource/transport authority |
| `docs/tina-user-guide/13-outcome-glossary.md` | C1 | exhaustive outcome names |
| `docs/tina-user-guide/14-lifecycle-and-shutdown.md` | C1 | observed results and drain |
| `docs/tina-user-guide/15-service-client-worked-example.md` | C1 | canonical client calls |
| `docs/tina-user-guide/16-continuation-and-pipeline-patterns.md` | C1 | typed continuations |
| `docs/tina-user-guide/17-pressure-report-convention.md` | C1 | pressure reports |
| `docs/tina-user-guide/18-bridge-crates.md` | C1 | bridge ownership |
| `docs/tina-user-guide/19-tracing.md` | C1 | typed lifecycle traces |
| `docs/tina-user-guide/20-native-websocket-server.md` | C1 | corrected WebSocket contract |
| `docs/tina-user-guide/21-compile-time-safety-rails.md` | C1 | capability compile-fail proof |
| `docs/tina-user-guide/22-http-http2-grpc.md` | C1 | typed protocol delivery |
| `docs/tina-user-guide/23-core-and-batteries.md` | C1 | public crate boundary |
| `docs/tina-user-guide/24-battery-authoring.md` | C1 | battery authoring form |
| `docs/tina-user-guide/25-extension-hooks.md` | C1 | extension API form |
| `docs/tina-user-guide/26-async-boundary.md` | C1 | authority across async boundary |
| `docs/tina-user-guide/27-which-noun-do-i-use.md` | C1 | simplified public nouns |
| `docs/tina-user-guide/28-outbound-clients.md` | C1 | typed outbound clients |
| `docs/tina-user-guide/29-continuation-flows.md` | C1 | continuation ownership |
| `docs/tina-user-guide/30-bridge-author-kit.md` | C1 | typed bridge authoring |
| `docs/tina-user-guide/README.md` | C1 | guide navigation and canonical vocabulary |
| `docs/README.md` and `examples/FINDINGS.md` | C1 | complete manifest and closure ledger |

The baseline tracks orphaned
`examples/systems/system_mini_saas_api/Cargo.lock` without a crate; it is a
non-corpus artifact and C1 removes it after confirming no workspace or packaging
reference. Separately, preserve the dirty root's genuinely untracked lockfiles
for `specimen_two_stage_pipeline`, `systems/perf_native`, and
`systems/system_metrics_shipper` without modification or deletion and never
copy them into fresh worktrees.

`specimen_multi_turn_request_context` and `specimen_tcp_echo` are the only crate
rows without local READMEs at planning time. C1 first adds each README with its
exact public runner command; its `public_smoke` must invoke that documented path.

An owner modifies its row completely: source, host, sidecar, transport usage,
README, smoke/e2e test, and relevant guide claim. Prior PRs may supply framework
or characterization prerequisites, but no row has two migration owners.

## Ordered implementation

### 1. Reconcile I0A and I0B

Re-check PR #364 and PR #366 against current `main`.

- Rebase in dependency order.
- Fix any platform-specific guard failure rather than rerunning around it.
- Confirm their independent adversarial reviews and replacement matrices.
- Merge them separately and verify `main` after each merge.

Direct proof:

- supervised worker uses typed initial and replacement events without envelope
  construction;
- initial and replacement factory panics are contained as typed outcomes;
- gRPC unary and streaming routes have bounded admission and unforgeable typed
  completion authority;
- gRPC protocol validation occurs before stream authority transfer.

Blast-radius proof:

- same-shard, cross-shard, live, and simulator spawn behavior remains intact;
- existing raw gRPC routes, tonic interoperability, and HTTP/2 behavior remain
  green.

Exit: #364 and #366 are merged and post-merge `main` is green.

### 2. Complete D1 direct bounds and terminal outcomes

One direct-migration cohort, with no speculative framework API:

- validate copied-service-path, soak HTTP/DB, lock-manager, and perf-native
  public counts, capacities, durations, shard conversions, and derived values
  before allocation or startup;
- preserve exact application-chain terminals in perf-native while retaining raw
  runtime ownership for explicitly benchmarked rows;
- add missing direct failure-path proof to otherwise canonical cache and bounded
  object-lane systems;
- remove any remaining effects built from request-sized input before the input
  bound is established.

Direct proof:

- zero, maximum, maximum-plus-one, and checked-overflow cases;
- rejection occurs before runtime, thread, socket, barrier, mailbox, map, or
  batch construction;
- exact `Full`, `Closed`, `Timeout`, `Rejected`, cancellation, and domain failure
  remain observable.

Blast-radius proof:

- accepted configurations retain existing reports, traces, allocation ceilings,
  and benchmark workload counts.

Exit: public inputs cannot create request-sized resources or overflow derived
values.

### 3. Complete F1, then M1 typed child lifecycle

After I0A merges, F1 is a framework-only PR. Extend observed spawning only as
far as needed for a parent to receive:

- initial child creation;
- successful replacement generations;
- exact typed child terminal result.

The child must retain its existing application outbound channel. Lifecycle
delivery is runtime-owned, bounded, traced, and generation-checked. Application
code must not add a second outbound sidecar merely to report termination.

Also remove explicit parent and child `ServiceMessage` types from observed
split-child macro configuration when those types can be synthesized from the
declared event/request/reply forms.

F1 changes no public example. After F1 merges, M1 is a fresh rebased migration
PR owning only:

- `specimen_real_io_chat` connection terminal reporting;
- `system_job_queue` startup, readiness, replacement, and exact child-start
  failure.

F1 owns framework and macro support; M1 owns exactly those two examples. Any
other envelope match belongs to its manifest owner, and a C1 discovery is a
launch blocker rather than permission to expand M1.

Direct proof:

- initial success and factory panic;
- replacement success and factory panic;
- stale generation and duplicate terminal rejection;
- parent mailbox `Full` and `Closed` with exact result settlement;
- child fail, panic, cancellation, owner stop, caller gone, and shutdown;
- real chat connection result reaches listener and host;
- job queue reports readiness or exact startup failure without polling.

Blast-radius proof:

- existing spawn, restart budget, child address refresh, and application
  outbound behavior remain unchanged across live and simulator owners.

Exit: real chat and job queue have no lifecycle sidecar, readiness mutex, host
spin loop, or explicit service envelope.

### 4. Complete M2 and M3 observed-result migrations

Use existing request replies and terminal observation first.

M2 is one migration PR owning:

- graceful shutdown;
- persistent counter;
- ergonomics playground;
- live replay bugbox;
- metrics shipper.

M3 separately owns SQLite because database metrics and bridge settlement have a
different failure contract. The SQLite owner accumulates query/update metrics
and returns them through its terminal report; point-in-time inspection uses an
existing typed request. M3 does not add a waiter or any framework API. If this
shape is falsified by the actual ownership graph, stop and revise this execution
document through Session B review rather than designing a prerequisite inside
the migration.

Required shape:

- actors privately accumulate results;
- hosts claim typed observation before start;
- actors settle acquired files, streams, listeners, bridge calls, timers, and
  deferred callers before `stop_with`;
- no `Arc<Mutex<_>>`, `Mutex<Option<_>>`, condvar, atomic completion flag,
  default-data fallback, or sleep-poll loop publishes application results;
- metrics are returned by a typed request or included in a terminal report.

Direct proof:

- successful report values on the public runner path;
- timer, signal, persistence, database, bridge, protocol, and cleanup failure;
- observation registered too late, observer timeout, type mismatch, and host
  shutdown;
- no terminal report appears while authority or resources remain unsettled;
- live replay and simulator replay produce the intended equivalent facts.

Blast-radius proof:

- persisted counter values, SQLite query/update behavior, replay traces, and
  metrics semantics remain unchanged.

Exit: the migrated examples have no result sidecars or host polling.

### 5. Complete F2 LocalSystem resource installation parity

Add LocalSystem keepalive-pool installation with a typed owned handle and a
bounded `close_and_drain` path.

Direct proof:

- complete installation;
- partial-install rollback;
- duplicate installation/path conflict;
- close success, already closed, timeout, owner failure, and shutdown;
- every admitted request and connection settles before drain completes.

Blast-radius proof:

- raw `ThreadedRuntime` keepalive APIs retain behavior;
- TLS, HTTP/1.1, HTTP/2, pool refill, peer close, and request cancellation
  suites remain green.

Exit: mini-SaaS no longer needs raw runtime ownership for keepalive resources.

### 6. Complete F3 typed HTTP service delivery

Make HTTP listener delivery accept typed event-only, request-only, and
split-service handles. Eliminate public envelope aliases and raw address
extraction. Do not modify a motivating application in F3.

Direct proof:

- event-only, request-only, and split-service delivery;
- duplicate, full, closed, timeout, rejected, cancellation, malformed input,
  peer close, and shutdown;
- compile-fail fixtures prove a handle cannot be forged or unwrapped into the
  wrong lane.

Blast-radius proof preserves raw adapters, HTTP pressure, fragmentation, TLS,
HTTP/1.1, HTTP/2, and replay behavior.

Exit: scoped request tree can use a typed HTTP handle after rebasing on F3.

### 7. Complete F4 WebSocket delivery and correctness

Make WebSocket upgrade/session delivery accept typed split-service handles,
then implement the settled recipient, ordering, and lane contract above. This is
its own framework/correctness PR and does not migrate a public application.

Do not retain raw `tina::send` as a workaround and do not create an HTTP/
WebSocket common adapter unless their completed public implementations are
identical and the diff makes both smaller.

Direct proof:

- route/upgrade duplicate, full, closed, timeout, rejected, cancellation, peer
  close, malformed input, and shutdown;
- deterministic two-client broadcast reaches every snapshot recipient other
  than the sender exactly once, in room-mailbox order, without lane confusion;
- typed capabilities cannot be forged or unwrapped into the wrong service lane.

Blast-radius proof:

- existing raw transport adapters, HTTP pressure tests, WebSocket manager tests,
  fragmentation, TLS, and replay behavior remain green.

Exit: realtime rooms can be written without envelope aliases, raw addresses, or
cross-lane delivery.

### 8. Complete M4 and M5 dependent systems

After their prerequisites merge, migrate:

- mini-SaaS: split services, `LocalSystem`, typed signal result, validated soak
  configuration, bounded HTTP response, and guaranteed resource drain;
- scoped request tree: typed HTTP handle, actor-owned report, exhaustive enrich
  outcomes, and LocalSystem host;
- realtime rooms: typed WebSocket delivery, actor-owned stats, cross-client
  correctness, and LocalSystem host;
- WebSocket room specimen: typed WebSocket delivery, exact cross-client
  behavior, actor-owned terminal report, and LocalSystem host.

M5 is a separate PR owning only session auth: `LocalMultiShardSystem`,
owner-provided time, checked shard conversion, bounded configuration, and
live/simulator parity. It may proceed independently once baseline prerequisites
are green.

Direct proof uses each system's public runner or network entry point and covers
success plus exact overload, cancellation, timeout, dependency failure, and
shutdown outcomes.

Blast-radius proof preserves each system's business report, protocol behavior,
trace contract, and bounded resource accounting.

Exit: every named system uses the canonical public facade and typed service
vocabulary.

### 9. Complete M6 residual host migrations

M6 owns exactly these residual rows and no row already migrated by M1-M5:

- graceful drain server, graceful pool shutdown, HTTP body streaming,
  idempotent retry, mini keyspace, mux client, native HTTP, and native HTTPS;
- outbound fetch, outbound HTTP, periodic batcher, pool cancellation reclaim,
  Postgres counter, request-scope fanout, and retrying outbound HTTP;
- scatter/gather, sharded fanout read, sharded keyspace, two-stage pipeline,
  webhook fanout, and webhook publisher.

- migrate single-shard applications to `LocalSystem`;
- migrate multi-shard applications to `LocalMultiShardSystem`;
- use fallible startup and guaranteed terminal runners;
- preserve typed host request and observed-result outcomes.

The settled allowlist above is exhaustive. If a listed residual proves it needs
raw ownership to demonstrate its named behavior, stop for a human decision
rather than silently expanding the allowlist.

Direct proof:

- each migrated public runner succeeds through the facade;
- startup, registration, workload, terminal report, and shutdown failures remain
  distinct;
- production startup does not panic;
- early workload failure still drives bounded cleanup.

Blast-radius proof:

- low-level runtime APIs and allowlisted examples remain supported and tested;
- live and simulator shared APIs retain vocabulary and behavior parity.

Exit: no unexplained production-shaped raw runtime remains in the corpus.

### 10. Complete C1 guards and final certification

Add or strengthen guards that reject Tina-facing application code containing:

- manual `ServiceMessage::Event` or `ServiceMessage::Request` construction;
- unexplained service-envelope aliases in public examples;
- result mutexes, condvars, atomics, or polling loops;
- dynamic effects constructed before source bounds;
- production-shaped raw runtime hosts outside the reviewed allowlist;
- manual drain/join where a guaranteed terminal runner exists;
- known collapsed terminal wildcard matches;
- unvalidated public count, capacity, duration, or shard inputs;
- stale README claims that recommend superseded APIs.

Use three deliberately different guard mechanisms:

- a Rust repository test parses Rust sources with `syn` for envelope
  construction, public envelope aliases, raw production hosts, wildcard
  terminal collapse, and intent-artifact leakage in identifiers;
- a portable `rg` lexical guard checks result-sidecar signatures, polling,
  obsolete README vocabulary, and exact intent-artifact phrases in source
  comments and public docs;
- behavioral tests, not searches, prove validation-before-allocation and exact
  terminal settlement.

Production scans cover `examples`, Tina-facing `docs`, and public crate sources.
Exclude `.git`, `.intent`, `target`, vendored/generated code, lockfiles, and the
guard's fixture-data root; fixture pass/fail/evasion cases are instead invoked
directly by guard self-tests. The
allowlist is a behavior-named repository file with path, narrow rule, reason,
focused test, reviewer, and reviewed SHA. Missing roots, parse failures, unknown
allowlist fields, stale paths, or traversal failure fail closed. Each structural
and lexical rule has pass, fail, and near-miss/evasion fixtures, including paths
with spaces and GNU/BSD CI. A separate lexical rule rejects the phase title,
`163-public-example-certification`, `execute.md`, `review.md`, and orchestration
vocabulary outside excluded intent artifacts and PR metadata. The forbidden
source-text phrases are the
exact phase title, `.intent/phases/163-public-example-certification`,
`execute.md`, `review.md`, `Execution Review 1`, and `Execution Review 2`.
The number `163` alone is not forbidden, and no open-ended “orchestration
vocabulary” rule is permitted.

Perform a fresh by-hand review of:

- every `examples/specimen_*` crate;
- every `examples/systems/*` crate;
- every extension and ergonomics playground;
- every README, runnable main, smoke test, and relevant user-guide page;
- `examples/FINDINGS.md` and its migration ledger.

Direct proof:

- every public example runs its user-shaped smoke or end-to-end path;
- every guard has positive, negative, and evasion fixtures;
- the final inventory reports no unexplained match.

Blast-radius proof:

- all affected crates pass `--all-targets`;
- strict Clippy uses `-D warnings`;
- rustdoc uses warnings denied;
- formatting and `git diff --check` pass;
- repository guard, packaging, promoted I/O, system example, live/simulator,
  network, replay, and platform matrices pass;
- final post-merge `origin/main` workflow is fully green.

Exit: `examples/FINDINGS.md` records every closure, no known example-local
workaround remains, no required PR is open, and the 0.1.0 public corpus is
certified.

Certification is objective only when every manifest row records its owning PR
or allowlist disposition, focused command, direct proof, blast-radius workflow,
reviewed head SHA, and merged SHA; every allowlist entry has a focused test;
every active finding is closed or named as a launch blocker; and no required or
dependent PR remains open. A new independent reviewer then checks the complete
fetched `origin/main` corpus by hand rather than reviewing accumulated branch
diffs. Record that exact final SHA and the fully green post-merge workflow in
`commits.txt`. Any remaining blocker means 0.1.0 examples are not certified.

## Proof summary

Unit proof:

- local validation, classification, generation, bounds, and typed error logic.

Integration proof:

- runtime/framework joins: spawn lifecycle, observation, transport delivery,
  resource installation, bridge calls, and shutdown settlement.

End-to-end proof:

- public specimen/system runners and real network/database paths exercise the
  changed behavior as users invoke it.

Adversarial proof:

- full, closed, timeout, rejected, duplicate, stale, caller gone, cancellation,
  panic, partial installation, malformed input, peer close, race, and shutdown.

Blast-radius proof:

- existing live/simulator parity, low-level APIs, protocols, traces, allocation
  ceilings, benchmarks, packaging, and platform behavior remain intact.

## Session operating loop

Maintain a live table in each pull-request description with:

- example or system;
- current awkward form;
- desired public form;
- current API sufficient: yes or no;
- prerequisite PR;
- migration PR;
- direct proof;
- blast-radius proof;
- review findings;
- CI and merge status.

Append merged evidence to `commits.txt` after each merge. Only C1 writes the
consolidated table to `examples/FINDINGS.md`, preventing concurrent ledger
branches from conflicting.

Continue through implementation, review, CI, rebasing, merging, and final
certification. Do not stop at inventory, local green tests, an open PR, or a
documented blocker. A blocker is work for the prerequisite step unless it
requires a product decision beyond the intent stated here.

## Required handoff

Report:

- every framework PR and merge commit;
- every migration PR and merge commit;
- examples and systems migrated per PR;
- API findings and the smaller public shapes that resolved them;
- direct, adversarial, and blast-radius proof evidence;
- final `origin/main` commit and post-merge workflow;
- any remaining blocker, without claiming certification while one remains.
