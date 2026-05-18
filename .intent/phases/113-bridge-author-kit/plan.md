# Phase 113: Bridge Author Kit

## Status

- IDD implementation phase.
- One PR.
- Can run beside phases 110/111/112 if it owns bridge crates and bridge docs.
- Does not own service skeleton reports except through adapter methods.
- Does not own protocol trace facts.

## Grug Truth

Bridges are where Tina meets messy outside systems.

Tina can bound admission.

Tina can observe worker terminal truth.

Tina cannot always stop the outside system.

Every bridge should say the same boring things:

- install
- ready or failed
- admit or full
- close admission
- drain
- shutdown
- late result
- metrics
- pressure

No bridge should invent its own vocabulary for the same shape.

## Goal

Build a small bridge author kit and migrate one real bridge family to prove it.

The kit should make it easier to write and review bridges without hiding
bridge-specific truth.

First target:

- `tina-aws-bridge`, because 104 added multiple services and exposed repeated
  lifecycle/classifier/scaffolding code.

Comparison target:

- `tina-sqlx-bridge` uses/adapts to the vocabulary enough to prove it is not
  AWS-only.
- `tina-sqlite-bridge` uses/adapts pressure and classifier names enough to
  prove the vocabulary fits the serial blocking bridge too.

## Names

Use user-facing bridge names.

Preferred names:

- `BridgeInstall`
- `BridgeHandle`
- `BridgeCloser`
- `BridgeMetrics`
- `BridgePressure`
- `BridgeOutcomeClass`
- `BridgeTerminal`
- `BridgeLateResult`
- `BridgeDrain`

Avoid:

- `WorkerCore`
- `Harness`
- `Manager`
- `AdapterThing`
- SDK-specific names in generic vocabulary

The kit may have internal modules with boring implementation names. The public
copied path should read like bridge lifecycle, not internal plumbing.

## Non-Goals

- no dynamic plugin ABI
- no broad bridge framework
- no moving all bridge crates under a new folder
- no changing external SDK semantics
- no fake cancellation of AWS/SQLx/reqwest work
- no global bridge registry
- no hidden retry
- no automatic supplied-client ownership
- no forcing every bridge into one type signature

## Build

### Rock 1: Shared Bridge Vocabulary

Add a small shared vocabulary in:

```rust
tina_runtime::bridge
```

Do not add a new crate.

AWS-specific helpers stay inside `tina-aws-bridge`.

Required shared concepts:

- `BridgeInstall`
- `BridgeCloser`
- `BridgeCloseMode`
- `BridgeCloseAdmission`
- `BridgeDrainReport`
- `BridgeLateResultReport`
- `BridgeTerminal`
- `BridgeCallerWarning`
- `BridgePressure`
- `BridgeOutcomeClass`
- `BridgeRetryable`
- `BridgeUnavailable`
- `BridgeFatal`

Required exact categories:

```rust
BridgeOutcomeClass::{
    Succeeded,
    Retryable(BridgeRetryable),
    Unavailable(BridgeUnavailable),
    Fatal(BridgeFatal),
}
BridgeRetryable::{
    BridgeFull,
    CallerTimeout,
    BridgeTimeout,
    ServiceThrottled,
    ProvisionedThroughputExceeded,
    TransactionConflict,
    SdkRetryable,
    PoolAcquireTimeout,
}
BridgeUnavailable::{
    BridgeClosed,
    PoolClosed,
}
BridgeFatal::{
    InvalidRequest,
    TooLarge,
    ConditionalCheckFailed,
    NotFound,
    InvalidParameter,
    AccessDenied,
    DecryptionFailed,
    Decode,
    SdkUnknown,
    Internal,
}
```

Vocabulary meaning:

- `Retryable` means the bridge can plausibly accept the same request again
  after backoff, if the caller's idempotency rules permit it.
- `Unavailable` means the bridge, pool, or resource is closed. The caller may
  need a new bridge/pool/service, not a blind retry loop.
- `Fatal` means changing input, permissions, schema, or code is required.

Rules:

- worker-terminal is not caller-observed
- caller timeout does not imply external work stopped
- supplied-client docs say who owns config and who owns Tina deadlines
- all bridge pressure surfaces have validated names
- do not classify broad SDK errors as retryable unless SDK metadata says
  retryable/throttled/conflict. Unknown SDK errors are `Fatal(SdkUnknown)`.

Tests:

- vocabulary Debug/display tokens are stable
- classifier categories are exhaustive for migrated bridge errors
- Closed/PoolClosed classify as `Unavailable`, not `Retryable`
- unknown SDK errors classify as `Fatal(SdkUnknown)`, not retry fog
- pressure adapter rejects bad names

### Rock 2: AWS Bridge Core Extraction

Refactor repeated AWS service-worker scaffolding.

Targets:

- S3
- SQS
- SNS
- DynamoDB
- Secrets Manager

Extract only repeated code:

- common install result shape
- common closer/drain state
- common metrics/pressure counters
- common late-result handling
- common classifier wrapper
- common supplied-client/runtime ownership docs

Replace AWS-local classifier enums with shared vocabulary. Keep service-specific
operation types and extension traits in the AWS crate.

Do not hide per-service operation types.

Rules:

- each AWS operation remains typed
- each service keeps its own request/response enum
- no macro if a plain helper module is enough
- no hidden retry
- no unbounded completion queue
- late completions remain visible

Tests:

- every AWS service still has a happy path hermetic test
- every AWS service has Full/Closed or equivalent admission test
- one AWS service has timeout plus late-result truth
- one AWS service has shutdown/drain while work is in flight
- classifier tests cover `Succeeded`, `Retryable(ServiceThrottled)`,
  `Retryable(BridgeTimeout)`, `Unavailable(BridgeClosed)`,
  `Fatal(AccessDenied)`, `Fatal(NotFound)`, `Fatal(InvalidRequest)`, and
  `Fatal(Internal)`

### Rock 3: Bridge Pressure Adapter

Ship one copied adapter path from bridge metrics to `CapacitySurfaceReport` /
`ServicePressureReport`.

Required shared shape:

```rust
BridgePressure {
    name,
    capacity,
    current,
    high_water,
    full_count,
    timeout_count,
    closed_count,
    late_result_count,
    worker_terminal_count,
}
```

Required methods:

```rust
BridgePressure::capacity_surface(mode) -> CapacitySurfaceReport
BridgePressure::unavailable(name, reason)
impl From<PgPressureReport> for BridgePressure
impl From<SqlitePressureReport> for BridgePressure
impl From<S3PressureReport> for BridgePressure
impl From<SqsPressureReport> for BridgePressure
impl From<SnsPressureReport> for BridgePressure
impl From<DynamoPressureReport> for BridgePressure
impl From<SecretsPressureReport> for BridgePressure
```

Required:

- surface name
- capacity
- current/in-flight
- high-water
- full count
- timeout count
- closed count; use `0` only when the bridge truly has no close path
- late-result count; use `0` only when the bridge cannot observe late terminal
  completion
- unavailable report when a whole bridge pressure surface cannot be measured

Rules:

- no caller-supplied config can lie about installed capacity
- metrics handle stores effective installed capacity where needed
- pressure report says worker-terminal when that is what it measures
- serial bridges such as SQLite still report capacity truth. Their capacity is
  small, not absent.

Tests:

- report uses installed capacity, not arbitrary caller config
- late-result count appears when supported
- unavailable field is explicit when unsupported
- service pressure builder can consume the report
- SQLite serial pressure maps to one bridge surface with capacity `1`

### Rock 4: One Non-AWS Bridge Alignment

Update `tina-sqlx-bridge` and the serial `tina-sqlite-bridge` pressure/classifier
surface to the shared vocabulary.

Required:

- docs use shared worker-terminal vs caller-observed wording
- pressure adapter uses shared names
- closer/drain docs use shared lifecycle words
- classifier maps to shared category names where it already has categories
- existing SQLx/SQLite typed outcome extension traits stay if they carry typed
  success payloads, but classification names must delegate to the shared
  `BridgeOutcomeClass`

Do not rewrite the whole bridge.

Tests:

- existing bridge tests still pass
- SQLx pressure-report test proves installed capacity truth
- SQLite pressure-report test proves serial capacity truth
- docs no longer overclaim caller-observed outcome
- SQLx and SQLite classifiers agree on `Full`, `Closed`, caller timeout,
  worker timeout, and invalid request category names

### Rock 5: Bridge Author Docs

Add a bridge author section to:

```text
docs/tina-user-guide/18-bridge-crates.md
```

Must include:

- bridge lifecycle diagram
- install/ready/fail
- close admission vs drain vs shutdown
- external work cancellation honesty
- worker-terminal vs caller-observed
- late-result truth
- capacity/pressure report shape
- supplied-client ownership rule
- classifier rule
- hermetic test checklist

Docs must be written for someone adding the next SDK bridge.

## Required Proof

Run at least:

```text
cargo fmt --all --check
cargo test -p tina-aws-bridge --tests
cargo test -p tina-sqlx-bridge --tests
cargo test -p tina-sqlite-bridge --tests
cargo test -p tina-reqwest-bridge --tests
cargo clippy -p tina-aws-bridge -p tina-sqlx-bridge -p tina-sqlite-bridge -p tina-reqwest-bridge --tests -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

The non-AWS bridges for this phase are `tina-sqlx-bridge` and
`tina-sqlite-bridge`.

## Hostile Review Checklist

Before merge, prove:

- no bridge claims caller saw an outcome it cannot observe
- installed capacity cannot be faked by passing a fresh config to metrics
- close/drain/shutdown words match the lifecycle docs
- late external work is visible or explicitly unsupported
- AWS extraction did not hide service-specific request/response truth
- shared vocabulary did not become a framework blob
- SQLx and SQLite use the vocabulary without losing typed success payloads

## Done Means

The next bridge author has a copied shape.

AWS bridge code is less repetitive.

SQLx and SQLite speak the same lifecycle/pressure language.

External work remains honestly external.
