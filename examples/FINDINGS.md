# Specimen Findings — Public Corpus Closure Ledger

This file is the certification closure ledger for the public example
corpus: every specimen, system, extension, README, guide page, and
runnable entry point under `examples/` and `docs/`. The active list is
closed — no known example-local workaround remains, and every active
finding from prior rounds is closed into this ledger or named below as a
blocker. Earlier rounds that have closed are summarized at the bottom so
external references stay valid; the long-form history lives in
[`FINDINGS_HISTORY.md`](FINDINGS_HISTORY.md).

## Closure

As of the final certification (final commit and post-merge workflow
recorded in [Remaining blockers](#remaining-blockers) when the last gate
lands), the public corpus is certified: 71 crate rows and 44 document
rows, each recording its owning pull request or allowlist disposition,
focused command, direct proof, and merge SHA.

A certified corpus teaches one coherent Tina model:

- actors own mutable state and terminal reports;
- caller, child, permit, lease, and resource authority stay typed and
  linear;
- producers are bounded before effects, allocations, threads, or sockets
  exist;
- `Full`, `Closed`, `Timeout`, `Rejected`, cancellation, failure, and
  shutdown remain distinct until application policy deliberately combines
  them;
- `LocalSystem` or `LocalMultiShardSystem` is the normal live host
  facade;
- live and simulator authoring use the same vocabulary when they share a
  contract;
- application code does not construct `ServiceMessage` envelopes or
  publish results through mutexes, condvars, polling loops, or
  default-data sidecars.

### Certification mechanics

1. **Focused proof target.** Every crate row carries
   `tests/public_smoke.rs` with exact `public_smoke` and
   `public_characterization` test functions. The focused command per row
   is

   ```
   cargo test --manifest-path <row>/Cargo.toml --test public_smoke public_smoke -- --exact
   ```

   with `public_characterization` substituted to run the characterization
   pin. The test drives the same public runner or binary path the row's
   README documents; binary-only crates use Cargo's `CARGO_BIN_EXE_*`
   integration-test executable rather than copying the implementation. No
   smoke test is ignored, sleeps for race ordering, or contacts the
   public internet. `specimen_postgres_counter` is the sole
   external-service row: CI starts PostgreSQL 16, sets a unique schema
   through `DATABASE_URL`, runs both the Tina and control paths, and
   drops the schema; a skip when the variable is absent is not
   certification evidence.
2. **Three guards** with deliberately different mechanisms:
   - **Structural** — `cargo test -p tina-runtime --test
     public_corpus_guard` parses corpus Rust sources with `syn` and
     rejects manual envelope construction and public envelope aliases,
     raw production runtime hosts, manual drain where a guaranteed
     terminal runner exists, wildcard collapse of distinct terminal
     outcomes, and process-artifact names baked into identifiers.
     `#[cfg(test)]` items are skipped; pass, fail, and evasion fixtures
     (including paths with spaces) are driven by the guard's own tests.
   - **Lexical** — `./scripts/public_corpus_lexical_guard.sh` (self-test
     via `--self-test`; also `make public-corpus-lexical-guard`) rejects
     result-sidecar signatures, sleep-polling result loops, obsolete
     raw-runtime vocabulary in corpus markdown, and exact
     process-artifact phrases. Portable across GNU/BSD `rg` + `perl`.
   - **Inventory** — `cargo test -p tina-runtime --test
     public_corpus_inventory` requires filesystem discovery to match the
     71 crate rows and 44 documents exactly. A new public crate, a
     missing crate-local README, or a missing proof target fails closed.
3. **Allowlist** — `examples/public-corpus-allowlist.toml`. Each entry
   names a path, one narrow guard rule it exempts, a behavior reason, the
   focused test that proves the exempted behavior, the reviewer, and the
   commit at which the form was reviewed. Unknown fields, stale paths,
   and entries that no longer match a live guard hit fail closed.
   Nothing is added without a recorded human decision and reviewer
   agreement.
4. **Behavioral proof, not searches**, pins validation-before-allocation
   (zero, maximum, maximum-plus-one, and checked-overflow inputs are
   rejected before any runtime, thread, socket, barrier, mailbox, map, or
   batch is constructed) and exact terminal settlement (one terminal
   disposition per capability and resource, asserted through explicit
   acquire/transfer/release ledgers).

### Owning pull requests

- **#364** — typed restart-aware service continuations; owns
  `specimen_supervised_worker`.
- **#366** — actor-backed typed gRPC routes; owns `specimen_grpc_counter`.
- **#367** — direct bounds and terminal outcomes cohort; owns six
  systems rows and the perf-native deployment README.
- **#369** — SQLite observed-result migration; owns
  `specimen_sqlite_counter`.
- **#370** — observed-result cohort; owns five specimen/systems rows.
- **#372** — multi-shard session host; owns `system_session_auth`.
- **#374** — typed child lifecycle migrations; owns
  `specimen_real_io_chat` and `system_job_queue`.
- **#376** — network systems; owns `mini_saas_api`,
  `system_scoped_request_tree`, `system_realtime_rooms`, and
  `specimen_websocket_room`.
- **#387** — residual host migrations; owns 21 specimen rows and the
  `docs/tcp-loops.md` audit.
- **#390** — public runner proof: 30 `public_smoke` targets (the other
  41 rows already had them), the two missing crate READMEs
  (`specimen_tcp_echo`, `specimen_multi_turn_request_context`), and the
  `specimen_tcp_echo` facade migration.
- **#391** — public docs and README reconciliation.

Framework prerequisites that own no corpus rows landed separately:
typed child lifecycle observation (#368, merge `0f1d9c2d`), keepalive-pool
installation with consuming `close_and_drain` (#371, merge `38c2ebf6`),
typed HTTP service delivery (#373, merge `7dd60291`), and typed WebSocket
delivery with the cross-client delivery correction (#375, merge
`30416eb4`).

## Consolidated crate ledger

Proof codes: **B** bounded-input rejection before allocation; **T**
exhaustive terminal outcomes; **A** authority/settlement ledger; **N**
real loopback network path; **S** live/simulator parity; **P**
characterization pins protocol, persistence, replay, or workload facts.
Merge SHAs are merge-commit prefixes. The focused command for every row
is the standard one from the mechanics section with `<row>` set to the
row path.

| Row | Owner (PR) | Disposition | Focused command | Direct proof | Merge SHA |
| --- | --- | --- | --- | --- | --- |
| `examples/specimen_axum_counter` | #390 | already-canonical | `cargo test --manifest-path examples/specimen_axum_counter/Cargo.toml --test public_smoke public_smoke -- --exact` | T,N — Axum request/count behavior; bridge `Full`/`Closed`/`Timeout` stay distinct HTTP statuses | `35b90d37` |
| `examples/specimen_backpressure_chain` | #390 | already-canonical | `cargo test --manifest-path examples/specimen_backpressure_chain/Cargo.toml --test public_smoke public_smoke -- --exact` | T,A — chained pressure settlement; every caller settles exactly once | `35b90d37` |
| `examples/specimen_bounded_batcher` | #390 | already-canonical | `cargo test --manifest-path examples/specimen_bounded_batcher/Cargo.toml --test public_smoke public_smoke -- --exact` | B,T — batch ceiling and refill; no failure bucket moves on the happy path | `35b90d37` |
| `examples/specimen_cancellation_chain` | #390 | already-canonical | `cargo test --manifest-path examples/specimen_cancellation_chain/Cargo.toml --test public_smoke public_smoke -- --exact` | T,A — cancellation propagation; the report flows through typed observation | `35b90d37` |
| `examples/specimen_cpu_run` | #390 | allowlisted — benchmark-control runner wrapping a comparison binary under CPU contention; allowlist: `shared-state` (spinner stop flag) | `cargo test --manifest-path examples/specimen_cpu_run/Cargo.toml --test public_smoke public_smoke -- --exact` | P — baseline vs. contended workload facts | `35b90d37` |
| `examples/specimen_cross_shard_child_ownership` | #390 | allowlisted — low-level cross-shard ownership stepping demonstration; allowlist: `raw-runtime-host` | `cargo test --manifest-path examples/specimen_cross_shard_child_ownership/Cargo.toml --test public_smoke public_smoke -- --exact` | A,S — cross-shard child ownership report | `35b90d37` |
| `examples/specimen_dynamic_worker_pool` | #390 | already-canonical | `cargo test --manifest-path examples/specimen_dynamic_worker_pool/Cargo.toml --test public_smoke public_smoke -- --exact` | T,A — resize and worker settlement | `35b90d37` |
| `examples/specimen_graceful_drain_server` | #387 | migrated | `cargo test --manifest-path examples/specimen_graceful_drain_server/Cargo.toml --test public_smoke public_smoke -- --exact` | T,A,N — admitted-request drain before the terminal report | `127510d3` |
| `examples/specimen_graceful_pool_shutdown` | #387 | migrated | `cargo test --manifest-path examples/specimen_graceful_pool_shutdown/Cargo.toml --test public_smoke public_smoke -- --exact` | T,A — exact pool/worker/release/close terminals (prior characterization retained) | `127510d3` |
| `examples/specimen_graceful_shutdown` | #370 | migrated | `cargo test --manifest-path examples/specimen_graceful_shutdown/Cargo.toml --test public_smoke public_smoke -- --exact` | T,A — typed signal result and cleanup through the terminal runner; dependency failure hardened by #384 | `30500ff9` |
| `examples/specimen_grpc_counter` | #366 | migrated | `cargo test --manifest-path examples/specimen_grpc_counter/Cargo.toml --test public_smoke public_smoke -- --exact` | B,T,A,N — actor-backed unary/stream routes; distinct wire statuses; caller-gone maintenance corrected by #380 | `69ff461a` |
| `examples/specimen_hot_key_fairness` | #390 | already-canonical | `cargo test --manifest-path examples/specimen_hot_key_fairness/Cargo.toml --test public_smoke public_smoke -- --exact` | B,T — fairness and capacity accounting | `35b90d37` |
| `examples/specimen_http_body_streaming` | #387 | migrated | `cargo test --manifest-path examples/specimen_http_body_streaming/Cargo.toml --test public_smoke public_smoke -- --exact` | B,T,A,N — streaming body protocol; iterator-body registration moved off host escapes by #389 | `127510d3` |
| `examples/specimen_idempotent_retry` | #387 | migrated | `cargo test --manifest-path examples/specimen_idempotent_retry/Cargo.toml --test public_smoke public_smoke -- --exact` | T,A — idempotency and exact retry count | `127510d3` |
| `examples/specimen_local_io_codec_ipc` | #390 | already-canonical | `cargo test --manifest-path examples/specimen_local_io_codec_ipc/Cargo.toml --test public_smoke public_smoke -- --exact` | T,A — codec and IPC settlement; README commands corrected to the manifest-path form | `35b90d37` |
| `examples/specimen_mem_run` | #390 | allowlisted — benchmark-control runner under `RLIMIT_AS` tiers; owns no raw runtime forms, so no allowlist entry is required | `cargo test --manifest-path examples/specimen_mem_run/Cargo.toml --test public_smoke public_smoke -- --exact` | P — per-tier workload facts; platform truth declared | `35b90d37` |
| `examples/specimen_mini_keyspace` | #387 | migrated | `cargo test --manifest-path examples/specimen_mini_keyspace/Cargo.toml --test public_smoke public_smoke -- --exact` | T,A — keyspace results through the typed terminal report | `127510d3` |
| `examples/specimen_multi_turn_request_context` | #390 | already-canonical — gained its crate README here | `cargo test --manifest-path examples/specimen_multi_turn_request_context/Cargo.toml --test public_smoke public_smoke -- --exact` | T,A — caller authority across turns | `35b90d37` |
| `examples/specimen_mux_client` | #387 | migrated | `cargo test --manifest-path examples/specimen_mux_client/Cargo.toml --test public_smoke public_smoke -- --exact` | T,A,N — multiplexed client protocol | `127510d3` |
| `examples/specimen_native_http` | #387 | migrated | `cargo test --manifest-path examples/specimen_native_http/Cargo.toml --test public_smoke public_smoke -- --exact` | B,T,A,N — HTTP/1.1 behavior on the typed facade | `127510d3` |
| `examples/specimen_native_https` | #387 | migrated | `cargo test --manifest-path examples/specimen_native_https/Cargo.toml --test public_smoke public_smoke -- --exact` | B,T,A,N — TLS and HTTP behavior with checked-in test certificates | `127510d3` |
| `examples/specimen_outbound_fetch` | #387 | migrated | `cargo test --manifest-path examples/specimen_outbound_fetch/Cargo.toml --test public_smoke public_smoke -- --exact` | T,A,N — outbound fetch results | `127510d3` |
| `examples/specimen_outbound_http` | #387 | migrated | `cargo test --manifest-path examples/specimen_outbound_http/Cargo.toml --test public_smoke public_smoke -- --exact` | T,A,N — outbound HTTP behavior through an owned keepalive pool | `127510d3` |
| `examples/specimen_owned_state_leak` | #390 | allowlisted — labeled adversarial anti-pattern; allowlist: `raw-runtime-host`, `manual-drain`, `shared-state`, README vocabulary | `cargo test --manifest-path examples/specimen_owned_state_leak/Cargo.toml --test public_smoke public_smoke -- --exact` | A — the escaped state is the demonstration; contained and labeled | `35b90d37` |
| `examples/specimen_periodic_batcher` | #387 | migrated | `cargo test --manifest-path examples/specimen_periodic_batcher/Cargo.toml --test public_smoke public_smoke -- --exact` | B,T,A — timer batching and shutdown | `127510d3` |
| `examples/specimen_persistent_counter` | #370 | migrated | `cargo test --manifest-path examples/specimen_persistent_counter/Cargo.toml --test public_smoke public_smoke -- --exact` | P,T,A — persisted value and terminal report; transactional persistence hardened by #384 | `30500ff9` |
| `examples/specimen_pool_cancel_reclaim` | #387 | migrated | `cargo test --manifest-path examples/specimen_pool_cancel_reclaim/Cargo.toml --test public_smoke public_smoke -- --exact` | T,A — cancel/reclaim settlement (prior characterization retained) | `127510d3` |
| `examples/specimen_postgres_counter` | #387 | migrated | `cargo test --manifest-path examples/specimen_postgres_counter/Cargo.toml --test public_smoke public_smoke -- --exact` (requires `DATABASE_URL`; CI provides PostgreSQL 16) | P,T,A — sole external-service row; both paths asserted against PostgreSQL 16; facade escapes closed by #389 | `127510d3` |
| `examples/specimen_rate_limited_worker` | #390 | already-canonical | `cargo test --manifest-path examples/specimen_rate_limited_worker/Cargo.toml --test public_smoke public_smoke -- --exact` | B,T,A — admission/refill accounting with distinct `KeyCapacityFull`/`Closed` | `35b90d37` |
| `examples/specimen_real_io_chat` | #374 | migrated | `cargo test --manifest-path examples/specimen_real_io_chat/Cargo.toml --test public_smoke public_smoke -- --exact` | T,A,N,S — typed connection terminal observed by listener and host; reservation settlement hardened by #382 | `9f05cb62` |
| `examples/specimen_replay_dst` | #390 | already-canonical | `cargo test --manifest-path examples/specimen_replay_dst/Cargo.toml --test public_smoke public_smoke -- --exact` | P,T,S — replay facts and deterministic simulation behavior | `35b90d37` |
| `examples/specimen_request_scope_fanout` | #387 | migrated | `cargo test --manifest-path examples/specimen_request_scope_fanout/Cargo.toml --test public_smoke public_smoke -- --exact` | T,A — scoped fanout settlement (prior characterization retained) | `127510d3` |
| `examples/specimen_retrying_outbound_http` | #387 | migrated | `cargo test --manifest-path examples/specimen_retrying_outbound_http/Cargo.toml --test public_smoke public_smoke -- --exact` | T,A,N — retry policy and exact request count | `127510d3` |
| `examples/specimen_rpc` | #390 | already-canonical | `cargo test --manifest-path examples/specimen_rpc/Cargo.toml --test public_smoke public_smoke -- --exact` | T,A — RPC outcomes (prior characterization retained) | `35b90d37` |
| `examples/specimen_scatter_gather` | #387 | migrated | `cargo test --manifest-path examples/specimen_scatter_gather/Cargo.toml --test public_smoke public_smoke -- --exact` | T,A — aggregate settlement (prior characterization retained) | `127510d3` |
| `examples/specimen_sharded_fanout_read` | #387 | migrated | `cargo test --manifest-path examples/specimen_sharded_fanout_read/Cargo.toml --test public_smoke public_smoke -- --exact` | T,A,S — shard fanout and result aggregation | `127510d3` |
| `examples/specimen_sharded_keyspace` | #387 | migrated | `cargo test --manifest-path examples/specimen_sharded_keyspace/Cargo.toml --test public_smoke public_smoke -- --exact` | T,A,S — sharded keyspace behavior | `127510d3` |
| `examples/specimen_sqlite_counter` | #369 | migrated | `cargo test --manifest-path examples/specimen_sqlite_counter/Cargo.toml --test public_smoke public_smoke -- --exact` | P,T,A — SQLite query/update and metrics; exact bridge failures and retryable close corrected by #386 | `bf2e915a` |
| `examples/specimen_supervised_worker` | #364 | migrated | `cargo test --manifest-path examples/specimen_supervised_worker/Cargo.toml --test public_smoke public_smoke -- --exact` | T,A,S — typed initial/replacement lifecycle; `FactoryPanicked` public proof added by #378 | `96383353` |
| `examples/specimen_tcp_echo` | #390 | migrated — gained its crate README and moved the standing server to the typed facade here | `cargo test --manifest-path examples/specimen_tcp_echo/Cargo.toml --test public_smoke public_smoke -- --exact` | T,A,N — echo protocol (prior characterization retained) | `35b90d37` |
| `examples/specimen_tower_timeout_counter` | #390 | already-canonical | `cargo test --manifest-path examples/specimen_tower_timeout_counter/Cargo.toml --test public_smoke public_smoke -- --exact` | T,A — Tower timeout distinction | `35b90d37` |
| `examples/specimen_tracing_demo` | #390 | allowlisted — explicit stepping/trace demonstration; allowlist: `raw-runtime-host` | `cargo test --manifest-path examples/specimen_tracing_demo/Cargo.toml --test public_smoke public_smoke -- --exact` | P,S — stepping/trace vocabulary | `35b90d37` |
| `examples/specimen_two_stage_pipeline` | #387 | migrated | `cargo test --manifest-path examples/specimen_two_stage_pipeline/Cargo.toml --test public_smoke public_smoke -- --exact` | T,A — pipeline settlement (prior characterization retained) | `127510d3` |
| `examples/specimen_webhook_fanout` | #387 | migrated | `cargo test --manifest-path examples/specimen_webhook_fanout/Cargo.toml --test public_smoke public_smoke -- --exact` | T,A,N — fanout settlement (prior characterization retained) | `127510d3` |
| `examples/specimen_webhook_outbox` | #390 | already-canonical | `cargo test --manifest-path examples/specimen_webhook_outbox/Cargo.toml --test public_smoke public_smoke -- --exact` | P,T,A — durable outbox behavior | `35b90d37` |
| `examples/specimen_webhook_publisher` | #387 | migrated | `cargo test --manifest-path examples/specimen_webhook_publisher/Cargo.toml --test public_smoke public_smoke -- --exact` | T,A,N — publisher protocol and retry; timing workaround removed and error precedence restored by #389 | `127510d3` |
| `examples/specimen_websocket_room` | #376 | migrated | `cargo test --manifest-path examples/specimen_websocket_room/Cargo.toml --test public_smoke public_smoke -- --exact` | T,A,N — exact cross-client delivery correction; Full/stale/removal outcomes and counters corrected by #379 | `532f6591` |
| `examples/specimen_worker_pool` | #390 | already-canonical | `cargo test --manifest-path examples/specimen_worker_pool/Cargo.toml --test public_smoke public_smoke -- --exact` | T,A — worker settlement (prior characterization retained) | `35b90d37` |
| `examples/specimen_ws_room` | #390 | already-canonical | `cargo test --manifest-path examples/specimen_ws_room/Cargo.toml --test public_smoke public_smoke -- --exact` | T,A,N — WebSocket room protocol | `35b90d37` |
| `examples/systems/ergonomics_playground` | #370 | migrated | `cargo test --manifest-path examples/systems/ergonomics_playground/Cargo.toml --test public_smoke public_smoke -- --exact` | T,A,S — public call shape and result | `30500ff9` |
| `examples/systems/mini_saas_api` | #376 | migrated | `cargo test --manifest-path examples/systems/mini_saas_api/Cargo.toml --test public_smoke public_smoke -- --exact` | B,T,A,N — HTTP protocol, report, guaranteed drain; retained keepalive authority corrected by #385; typed HTTP caller settlement by #388 | `532f6591` |
| `examples/systems/perf_native` | #367 | migrated — raw benchmark control rows allowlisted (`raw-runtime-host`, `envelope-construction`, `manual-drain`, `shared-state`, README vocabulary) | `cargo test --manifest-path examples/systems/perf_native/Cargo.toml --test public_smoke public_smoke -- --exact` | P,B,T — accepted workload and counts; raw rows remain comparison controls only | `83700b3f` |
| `examples/systems/system_api_gateway_limits` | #390 | already-canonical | `cargo test --manifest-path examples/systems/system_api_gateway_limits/Cargo.toml --test public_smoke public_smoke -- --exact` | B,T,A,N — gateway limits | `35b90d37` |
| `examples/systems/system_bounded_object_lane` | #367 | migrated | `cargo test --manifest-path examples/systems/system_bounded_object_lane/Cargo.toml --test public_smoke public_smoke -- --exact` | B,T,A — object-lane admission plus direct failure-path proof | `83700b3f` |
| `examples/systems/system_cache_with_fill` | #367 | migrated | `cargo test --manifest-path examples/systems/system_cache_with_fill/Cargo.toml --test public_smoke public_smoke -- --exact` | B,T,A — fill deduplication and pressure | `83700b3f` |
| `examples/systems/system_copied_service_path` | #367 | migrated | `cargo test --manifest-path examples/systems/system_copied_service_path/Cargo.toml --test public_smoke public_smoke -- --exact` | B,T,A — copied-service bounds and results | `83700b3f` |
| `examples/systems/system_job_queue` | #374 | migrated | `cargo test --manifest-path examples/systems/system_job_queue/Cargo.toml --test public_smoke public_smoke -- --exact` | B,T,A,S — readiness, restart, and exact startup failure without polling | `9f05cb62` |
| `examples/systems/system_live_replay_bugbox` | #370 | migrated | `cargo test --manifest-path examples/systems/system_live_replay_bugbox/Cargo.toml --test public_smoke public_smoke -- --exact` | P,T,A,S — equivalent live/replay facts; replay parity hardened by #384 | `30500ff9` |
| `examples/systems/system_lock_manager` | #367 | migrated | `cargo test --manifest-path examples/systems/system_lock_manager/Cargo.toml --test public_smoke public_smoke -- --exact` | B,T,A — lock admission and release | `83700b3f` |
| `examples/systems/system_metrics_shipper` | #370 | migrated | `cargo test --manifest-path examples/systems/system_metrics_shipper/Cargo.toml --test public_smoke public_smoke -- --exact` | T,A — actor-owned metrics report; bounded waiters by #384 | `30500ff9` |
| `examples/systems/system_realtime_rooms` | #376 | migrated | `cargo test --manifest-path examples/systems/system_realtime_rooms/Cargo.toml --test public_smoke public_smoke -- --exact` | T,A,N — exact cross-client delivery and stats; corrected by #379 | `532f6591` |
| `examples/systems/system_scoped_request_tree` | #376 | migrated | `cargo test --manifest-path examples/systems/system_scoped_request_tree/Cargo.toml --test public_smoke public_smoke -- --exact` | T,A,N — typed HTTP request tree and exhaustive enrich outcomes; caller settlement by #388 | `532f6591` |
| `examples/systems/system_session_auth` | #372 | migrated | `cargo test --manifest-path examples/systems/system_session_auth/Cargo.toml --test public_smoke public_smoke -- --exact` | B,T,A,S — shard/time/session semantics; pre-allocated bounded session identities by #383 | `c18ec119` |
| `examples/systems/system_soak_http_db` | #367 | migrated | `cargo test --manifest-path examples/systems/system_soak_http_db/Cargo.toml --test public_smoke public_smoke -- --exact` | P,B,T,A,N — accepted soak workload and database behavior | `83700b3f` |
| `examples/systems/system_tenant_rate_limiter` | #390 | already-canonical | `cargo test --manifest-path examples/systems/system_tenant_rate_limiter/Cargo.toml --test public_smoke public_smoke -- --exact` | B,T,A,S — tenant admission and refill | `35b90d37` |
| `examples/systems/system_webhook_relay` | #390 | already-canonical | `cargo test --manifest-path examples/systems/system_webhook_relay/Cargo.toml --test public_smoke public_smoke -- --exact` | B,T,A,N — relay protocol and delivery settlement | `35b90d37` |
| `examples/extensions/tina-extension-capacity-surface` | #390 | already-canonical | `cargo test --manifest-path examples/extensions/tina-extension-capacity-surface/Cargo.toml --test public_smoke public_smoke -- --exact` | B,T — public capacity surface | `35b90d37` |
| `examples/extensions/tina-extension-compile-fail` | #390 | already-canonical | `cargo test --manifest-path examples/extensions/tina-extension-compile-fail/Cargo.toml --test public_smoke public_smoke -- --exact` | compile-fail capability and API-shape fixtures; probe count pinned against the literal | `35b90d37` |
| `examples/extensions/tina-extension-custom-codec` | #390 | already-canonical | `cargo test --manifest-path examples/extensions/tina-extension-custom-codec/Cargo.toml --test public_smoke public_smoke -- --exact` | T,A — codec extension contract | `35b90d37` |
| `examples/extensions/tina-extension-fake-bridge` | #390 | already-canonical | `cargo test --manifest-path examples/extensions/tina-extension-fake-bridge/Cargo.toml --test public_smoke public_smoke -- --exact` | T,A,S — bridge ownership and parity | `35b90d37` |
| `examples/extensions/tina-extension-service-policy` | #390 | already-canonical | `cargo test --manifest-path examples/extensions/tina-extension-service-policy/Cargo.toml --test public_smoke public_smoke -- --exact` | B,T — policy outcomes | `35b90d37` |

Every crate-local README and smoke test is owned by its crate row's owner
and proved by the same focused command; the inventory guard requires each
crate row's `README.md` on disk and fails closed on a missing or
unlisted crate.

## Document ledger

The focused command for every document row is the lexical guard plus the
inventory guard —

```
./scripts/public_corpus_lexical_guard.sh
cargo test -p tina-runtime --test public_corpus_inventory
```

— backed by a by-hand audit that every command and API claim matches the
merged code. `reconciled` means the docs pull request edited the file;
`already-canonical` means the audit found no drift.

| Row | Owner (PR) | Disposition | Focused command | Direct proof | Merge SHA |
| --- | --- | --- | --- | --- | --- |
| `README.md` | #391 | reconciled | lexical + inventory guards, by-hand audit | public crate boundary; the pinned bounded-mailbox block is an intentional low-level demonstration (allowlisted) | `5b353b50` |
| `docs/README.md` | #391 | already-canonical | lexical + inventory guards, by-hand audit | documentation index | `5b353b50` |
| `docs/bridge-composition.md` | #391 | reconciled | lexical + inventory guards, by-hand audit | bridge ownership/composition vocabulary; the raw bridge twin is deliberately documented as the low-level form (allowlisted) | `5b353b50` |
| `docs/mailbox-capacity.md` | #391 | already-canonical | lexical + inventory guards, by-hand audit | capacity and terminal outcomes | `5b353b50` |
| `docs/resource-owner-matrix.md` | #391 | reconciled | lexical + inventory guards, by-hand audit | resource ownership and settlement; raw-runtime column documented next to the facade forms (allowlisted) | `5b353b50` |
| `docs/tcp-loops.md` | #387 | already-canonical — audited; loop-helper guidance only, no stale host vocabulary, no change needed | lexical + inventory guards, by-hand audit | live host and TCP loop guidance | `127510d3` |
| `docs/tina-user-guide/README.md` | #391 | already-canonical | lexical + inventory guards, by-hand audit | guide navigation and canonical vocabulary | `5b353b50` |
| `docs/tina-user-guide/00-agent-quickstart.md` | #391 | already-canonical | lexical + inventory guards, by-hand audit | canonical quickstart | `5b353b50` |
| `docs/tina-user-guide/01-mental-model.md` | #391 | already-canonical | lexical + inventory guards, by-hand audit | ownership mental model | `5b353b50` |
| `docs/tina-user-guide/02-first-isolate.md` | #391 | reconciled | lexical + inventory guards, by-hand audit | canonical isolate form; raw runtime named only as the low-level form after the facade (allowlisted) | `5b353b50` |
| `docs/tina-user-guide/03-effects-and-runtime-calls.md` | #391 | reconciled | lexical + inventory guards, by-hand audit | bounded typed effects | `5b353b50` |
| `docs/tina-user-guide/04-request-reply.md` | #391 | already-canonical | lexical + inventory guards, by-hand audit | request/reply authority | `5b353b50` |
| `docs/tina-user-guide/05-tcp-services.md` | #391 | already-canonical | lexical + inventory guards, by-hand audit | typed TCP service host | `5b353b50` |
| `docs/tina-user-guide/06-boundedness-and-overload.md` | #391 | reconciled | lexical + inventory guards, by-hand audit | bounds and exhaustive overload; lower-level pressure twin named deliberately (allowlisted) | `5b353b50` |
| `docs/tina-user-guide/07-supervision.md` | #391 | reconciled | lexical + inventory guards, by-hand audit | restart-aware lifecycle form | `5b353b50` |
| `docs/tina-user-guide/08-simulation-and-dst.md` | #391 | already-canonical | lexical + inventory guards, by-hand audit | live/simulator parity | `5b353b50` |
| `docs/tina-user-guide/09-tokio-to-tina-porting.md` | #391 | reconciled | lexical + inventory guards, by-hand audit | public facade migration | `5b353b50` |
| `docs/tina-user-guide/10-service-patterns.md` | #391 | reconciled | lexical + inventory guards, by-hand audit | split typed services; low-level forms documented next to facade rows (allowlisted) | `5b353b50` |
| `docs/tina-user-guide/11-ergonomics-checklist.md` | #391 | reconciled | lexical + inventory guards, by-hand audit | checklist matches the guards; comparison rows name low-level forms deliberately (allowlisted) | `5b353b50` |
| `docs/tina-user-guide/12-io-model.md` | #391 | already-canonical | lexical + inventory guards, by-hand audit | resource/transport authority | `5b353b50` |
| `docs/tina-user-guide/13-outcome-glossary.md` | #391 | already-canonical | lexical + inventory guards, by-hand audit | exhaustive outcome names | `5b353b50` |
| `docs/tina-user-guide/14-lifecycle-and-shutdown.md` | #391 | reconciled | lexical + inventory guards, by-hand audit | observed results and drain; low-level shutdown-handle section deliberate (allowlisted) | `5b353b50` |
| `docs/tina-user-guide/15-service-client-worked-example.md` | #391 | already-canonical | lexical + inventory guards, by-hand audit | canonical client calls | `5b353b50` |
| `docs/tina-user-guide/16-continuation-and-pipeline-patterns.md` | #391 | already-canonical | lexical + inventory guards, by-hand audit | typed continuations | `5b353b50` |
| `docs/tina-user-guide/17-pressure-report-convention.md` | #391 | already-canonical | lexical + inventory guards, by-hand audit | pressure reports | `5b353b50` |
| `docs/tina-user-guide/18-bridge-crates.md` | #391 | reconciled | lexical + inventory guards, by-hand audit | bridge ownership; raw keepalive free functions named as the non-facade form (allowlisted) | `5b353b50` |
| `docs/tina-user-guide/19-tracing.md` | #391 | reconciled | lexical + inventory guards, by-hand audit | typed lifecycle traces; raw observer forms named as the low-level twin (allowlisted) | `5b353b50` |
| `docs/tina-user-guide/20-native-websocket-server.md` | #391 | reconciled | lexical + inventory guards, by-hand audit | corrected WebSocket contract | `5b353b50` |
| `docs/tina-user-guide/21-compile-time-safety-rails.md` | #391 | already-canonical | lexical + inventory guards, by-hand audit | capability compile-fail proof; anti-pattern snippet labeled (allowlisted) | `5b353b50` |
| `docs/tina-user-guide/22-http-http2-grpc.md` | #391 | reconciled | lexical + inventory guards, by-hand audit | typed protocol delivery | `5b353b50` |
| `docs/tina-user-guide/23-core-and-batteries.md` | #391 | reconciled | lexical + inventory guards, by-hand audit | public crate boundary | `5b353b50` |
| `docs/tina-user-guide/24-battery-authoring.md` | #391 | already-canonical | lexical + inventory guards, by-hand audit | battery authoring form | `5b353b50` |
| `docs/tina-user-guide/25-extension-hooks.md` | #391 | already-canonical | lexical + inventory guards, by-hand audit | extension API form | `5b353b50` |
| `docs/tina-user-guide/26-async-boundary.md` | #391 | already-canonical | lexical + inventory guards, by-hand audit | authority across the async boundary | `5b353b50` |
| `docs/tina-user-guide/27-which-noun-do-i-use.md` | #391 | already-canonical | lexical + inventory guards, by-hand audit | simplified public nouns | `5b353b50` |
| `docs/tina-user-guide/28-outbound-clients.md` | #391 | reconciled | lexical + inventory guards, by-hand audit | typed outbound clients; WebSocket client manager's lower handle documented (allowlisted) | `5b353b50` |
| `docs/tina-user-guide/29-continuation-flows.md` | #391 | already-canonical | lexical + inventory guards, by-hand audit | continuation ownership | `5b353b50` |
| `docs/tina-user-guide/30-bridge-author-kit.md` | #391 | reconciled | lexical + inventory guards, by-hand audit | typed bridge authoring; two-form install convention documented (allowlisted) | `5b353b50` |
| `examples/README.md` | #391 | already-canonical | lexical + inventory guards, by-hand audit | corpus index and specimen rules | `5b353b50` |
| `examples/systems/README.md` | #391 | reconciled | lexical + inventory guards, by-hand audit | systems index | `5b353b50` |
| `examples/extensions/README.md` | #391 | already-canonical | lexical + inventory guards, by-hand audit | extensions index | `5b353b50` |
| `examples/FINDINGS_HISTORY.md` | #391 | already-canonical — historical journal; preserved claims are marked historical | lexical + inventory guards, by-hand audit | closure-compatible archaeology (allowlisted vocabulary) | `5b353b50` |
| `examples/FINDINGS.md` | final certification pull request | this ledger | lexical + inventory guards | complete manifest and closure record | pending |
| `examples/systems/perf_native/fly/README.md` | #367 | already-canonical | lexical + inventory guards, by-hand audit | deployment workload and count claims | `83700b3f` |

## Corrections after merge

The independent post-merge adversarial pass over the first migration
sweep falsified several contract and proof claims. Each correction landed
as its own reviewed pull request with a green replacement workflow. Two
falsifications are worth naming plainly: the original typed-HTTP proof
had closed the caller with a short service timeout rather than peer EOF
(replaced by one bounded observed child with reserved terminal delivery,
#388), and the residual-host sweep's zero-escape claim was falsified by
two `host_control()` facades (closed with capability-typed parity, #389).

| PR | Correction | Merge SHA |
| --- | --- | --- |
| #377 | Keepalive install, rollback, deadline, and retained authority settlement | `17802b02` |
| #378 | Initial and replacement supervised factory-panic public proof | `fb7c0b6c` |
| #379 | Exact WebSocket Full/stale/removal outcomes and counters | `76c8677c` |
| #380 | Bounded gRPC actor-route caller-gone maintenance without a timer-full hot-loop | `25d91209` |
| #381 | Closed AwaitReady/AwaitQuiescent waiter reclamation | `109f877e` |
| #382 | Physical terminal mailbox reservation and mapper-panic settlement | `eeaa7f9f` |
| #383 | Pre-allocated bounded session identities and duplicate accounting | `77be6c1f` |
| #384 | LocalSystem hosts, transactional persistence, graceful dependency failure, replay parity, bounded metrics waiters | `da0c9050` |
| #385 | Mini-SaaS retained keepalive authority and post-owner typed settlement | `0f28b65c` |
| #386 | Exact SQLite bridge failures, envelope-free host calls, and retryable close settlement | `325157cb` |
| #388 | Typed HTTP caller settlement on buffered keepalive peer disconnect | `ae393323` |
| #389 | Residual host-facade escapes closed; webhook error precedence restored; PostgreSQL dual-failure propagation | `3de11fc4` |

## Remaining blockers

Certification is not final at this writing.

- Corpus rows are merged through **#391** (merge `5b353b50`; post-merge
  workflow run 30188176502 green). The three corpus guards, the reviewed
  allowlist, and the corrections the guards surfaced (wildcard terminal
  settlement, the gRPC and TCP echo facade migrations, the `hello_world`
  guide pin, the RFC 9113 §5.1 late-HEADERS connection-teardown fix, and
  lockfile policy cleanup) land in **#392** (public corpus guards). This
  ledger's guard and allowlist references describe that landed state.
- This ledger, the archival reconciliation of `FINDINGS_HISTORY.md`, the
  removal of the orphaned
  `examples/systems/system_mini_saas_api/Cargo.lock`, the perf_native
  tokio-control teardown race fix, and the now-stale FINDINGS.md
  allowlist entry's removal land in the final certification pull
  request, after #392.
- After that merge: a fresh independent by-hand review of the complete
  fetched `main` corpus rather than accumulated branch diffs, then a
  fully green post-merge workflow. The final commit and workflow run are
  recorded here when they exist.
- Hygiene items that gate a clean final CI claim but are not corpus
  findings: a pre-existing shutdown remaining-budget timing test failed
  once on macOS CI and passed on the failed-job rerun;
  `examples/systems/perf_native/tests/floor.rs`'s raw-TCP wall-clock
  floor is load-sensitive (passes alone, can exceed its bound under
  heavy concurrent load); the scheduled weekly dependency-resolution and
  audit workflow is red on its latest run and needs triage.

Until these close, the 0.1.0 public corpus is not certified.

## Earlier rounds

The dated finding rounds that produced this ledger are closed; their
substance is the table above. Long-form archaeology lives in
[`FINDINGS_HISTORY.md`](FINDINGS_HISTORY.md).

Closed dated rounds, superseded by the ledger:

- 2026-07-14 actor-backed typed gRPC routes → `specimen_grpc_counter` row.
- 2026-07-14 bounded producers and exhaustive specimen terminals → the
  eleven bounded-terminal specimens, re-proved by their ledger owners.
- 2026-07-13 local I/O terminal observation and framed output closure →
  `specimen_local_io_codec_ipc` row.
- 2026-07-13 classified select-race continuation routing →
  `ergonomics_playground` row.
- 2026-07-13 typed multi-shard host routing → framework; consumed by the
  sharded rows.
- 2026-07-13 job-queue LocalSystem migration → `system_job_queue` row.
- 2026-07-12 rate-limit decision ergonomics → `specimen_rate_limited_worker`
  and `system_tenant_rate_limiter` rows.
- 2026-07-12 report-preserving LocalSystem terminal runner, atomic root
  bootstrap parity, guaranteed terminal runner, address-aware root
  construction, split-service outbound facade, request-aware raw flow,
  Unix write-all continuation, typed sharded request-service table,
  runtime address provenance → framework prerequisites; consumed by the
  facade rows.
- 2026-07-12 pure bounded-workload, default-host, debounced batch, copied
  service path, soak HTTP/DB, lock-manager, scatter/gather, and extension
  corpus migrations → the named specimen/systems/extension rows.
- 2026-07-11 bounded shutdown truth, fallible production startup,
  raw-isolate-to-macro cohorts, and the envelope-free continuation cohort
  → corpus-wide; superseded by the guards.
- 2026-07-09 examples canonicalization pass and by-hand follow-up →
  corpus-wide; superseded by this ledger.
- 2026-05-23 status pass and numbered findings 1–37 (including the
  `-historical` duplicates) → closed; archaeology in
  [`FINDINGS_HISTORY.md`](FINDINGS_HISTORY.md).

Round 1 closed in the earliest specimen rounds. Those nine items are
archived verbatim in [`FINDINGS_HISTORY.md`](FINDINGS_HISTORY.md).
Short summary of patterns no new code should copy:

- hand-rolled mailbox factories: use `DefaultMailboxFactory` /
  `DefaultThreadedMailboxFactory`;
- `Arc<Mutex<Option<SocketAddr>>>` for listener bind address: use
  `observe_next_bound()`;
- trace fingerprinting via `Debug`: use `RuntimeEvent::stable_hash()` /
  `stable_trace_hash(...)`;
- one-off shard types for single-shard programs: use `SingleShard` or omit
  `shard = ...`;
- `Arc::try_unwrap` bridge shutdown dances: use the bridge host lifecycle;
- `Arc::try_unwrap(runtime)` host shutdown dances on threaded runtimes:
  use the facade's guaranteed terminal runner
  (`run_to_shutdown_reported` and its multi-shard parity; see
  [docs/tina-user-guide/14-lifecycle-and-shutdown.md](../docs/tina-user-guide/14-lifecycle-and-shutdown.md));
- old shared comparison harnesses: examples are specimens, tests are proof;
- `Arc<Outcome>` / `Arc<Mutex<Vec<_>>>` for an isolate's *final* app
  value: use `stop_with(value)` + typed result observation (works on
  single-shard and multi-shard hosts);
- per-comparison shard types: use `SingleShard` for one-shard programs and
  `tina_runtime::sharded::ShardPlacement` / `ShardServiceTable` for
  multi-shard placement.

## How To Add A Finding

This ledger is closed. New cross-specimen pain opens a new findings
round: add it here under a new dated section only when the finding
implies Tina product work.

```md
### N. Short product-shaped title

**Surfaced by:** `example_name`, `other_example`.

What repeated pain we saw.

**Build:** concrete primitive, API, doc, or test work.
```

Per-example flavor belongs in the example README. Resolved
archaeology belongs in [`FINDINGS_HISTORY.md`](FINDINGS_HISTORY.md).

Numbers are stable: when a finding closes, keep its number so external
references (README links, commit messages, prior PRs) stay valid.
