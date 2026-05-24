# Phase 134: Config And Budget Manifest

## Status

- Future implementation phase.
- One PR.
- Can run beside 131/132/133 if it owns manifest data, adapters, docs, and
  system/specimen updates.

## Grug Truth

Bounded services have many knobs.

If users cannot see the knobs in one place, they guess. Guesses become
stupid-high caps or invisible production failures.

## Current Code Facts

- `LocalSystemConfig` already names runtime ingress, shard-pair, storage, DNS,
  TLS, process, signal, idle, and shutdown-lane budgets.
- `ThreadedRuntimeConfig` is the lower-level worker config.
- HTTP/1 has `HttpLimits`, `HttpServerConfig`, `HttpClientConfig`, and
  `PoolConfig`.
- HTTP/2 has `Http2Limits`, `Http2ServerConfig`, and `Http2ClientLimits`.
- Capacity reports and discovery lines exist.
- `ServicePressureReport` and `CapacitySummary` exist.
- Systems still scatter caps through constants and local structs.

So this phase should create a manifest over real configs and reports, not a
new runtime registry.

## Goal

Make boundedness copyable:

- one manifest lists service budgets;
- adapters build manifest rows from existing configs;
- validation catches bad caps before runtime;
- consistency checks compare manifest vs live reports;
- replay/capture records replay-affecting budget truth.

## Does Not Include

- no automatic tuning;
- no memory magic;
- no global cross-shard budget;
- no config-file format war;
- no hidden singleton registry;
- no secret values;
- no silent unbounded mode.

## Names And Homes

- Add `tina-runtime::budget`.
- Add:
  - `ServiceBudgetManifest`
  - `BudgetSurface`
  - `BudgetKind`
  - `BudgetUnit`
  - `BudgetCap`
  - `BudgetManifestReport`
  - `BudgetConsistencyReport`
  - `BudgetValidationError`
- `tina-http` may add adapter methods that produce `BudgetSurface` rows.
- Bridge crates may add adapters only for their installed configs/metrics.

## Budget Surface Shape

Every surface row must carry:

- stable name;
- kind;
- unit;
- cap or explicit unbounded policy;
- mode using existing `CapacityMode`;
- owner label and shard label when known;
- replay impact: `ReplayAffecting` or `DisplayOnly`;
- source: config/report/derived;
- schema version.

`BudgetKind` variants for this phase:

- `RuntimeIngress`
- `ShardPair`
- `RuntimeLane`
- `Mailbox`
- `PendingReply`
- `PendingCall`
- `RequestScope`
- `Pool`
- `BodyBytes`
- `ProtocolSession`
- `ConnectAttempt`
- `BridgeInFlight`
- `EventSink`

`BudgetUnit` variants for this phase:

- `Messages`
- `Calls`
- `Bytes`
- `Connections`
- `Sessions`
- `Attempts`
- `Weight { label: String }` for user-defined weights.

## Implementation

### Rock 1: Manifest Data And Validation

Implement manifest data in `tina-runtime`.

Validation rejects:

- duplicate names;
- invalid capacity surface names;
- zero caps where zero means broken service;
- expired explicit unbounded mode;
- missing required row in copied service skeleton;
- secret-looking values in printable fields.

Secrets do not live in the manifest. Env var names and file paths may; secret
values may not.

### Rock 2: Adapters For Existing Configs

Add adapters for concrete current configs:

- `LocalSystemConfig`;
- `ThreadedRuntimeConfig`;
- `MultiShardRuntimeConfig`;
- HTTP/1 `HttpServerConfig`, `HttpClientConfig`, `PoolConfig`;
- HTTP/2 `Http2ServerConfig`, `Http2ClientLimits`;
- `WebSocketLimits`;
- `WebSocketMemberTableReport`;
- SQLite/SQLx/reqwest/AWS bridge installed configs or metrics handles where
  capacity is known at install time.

Do not require old constructors to accept a manifest. Adapters may build
existing configs from manifest rows only when the mapping is exact.

### Rock 3: Consistency Checker

Implement:

- `manifest.validate()`;
- `manifest.compare_capacity_summary(&CapacitySummary)`;
- `manifest.compare_service_pressure(&ServicePressureReport)`;
- typed rows: missing, extra, cap mismatch, unit mismatch, mode mismatch,
  replay-impact mismatch.

Manifest names must match live capacity/report names exactly. "Close enough"
is a service bug.

### Rock 4: Replay And Capture

Add manifest metadata to live replay capture/saved replay:

- schema version;
- replay-affecting surface hash;
- display-only surface summary;
- consistency report summary.

Changing a replay-affecting budget must change or invalidate replay. Changing
display-only metadata must not change replay hash, and the report must say it
was ignored for replay.

### Rock 5: Systems And Docs

Update:

- `examples/systems/mini_saas_api`: no scattered capacity constants without a
  manifest row;
- one smaller system/specimen with a tiny manifest;
- user guide service patterns with "start here: manifest, then configs";
- README output showing manifest + pressure report side by side.

## Required Proof

- Duplicate surface names fail validation.
- Invalid names fail validation.
- Zero/broken caps fail validation.
- Explicit unbounded-with-expiry validates before expiry and fails after.
- Secret-looking values are rejected or redacted.
- Manifest rows can be generated from `LocalSystemConfig`.
- Manifest rows can be generated from HTTP/1 and HTTP/2 configs.
- Manifest rows can be generated from at least one bridge installed config.
- Existing configs can be built from manifest only for exact mappings.
- Capacity summary compare catches missing/extra/mismatched surfaces.
- Service pressure compare catches missing/extra/mismatched surfaces.
- Replay capture includes manifest metadata.
- Replay-affecting budget change invalidates or changes replay.
- Display-only metadata change does not change replay hash and is reported as
  ignored.
- `mini_saas_api` test proves every cap printed in docs exists in the
  manifest and every live report surface has a manifest row.
- Cheap-model copied service can find all caps in one manifest file/object, not
  by grepping handlers.
