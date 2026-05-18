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

- install result
- closer
- close mode / close admission
- drain report
- late result count/report
- worker-terminal outcome
- caller-observed outcome warning
- pressure report adapter
- supplied-client ownership note
- classifier category

Rules:

- worker-terminal is not caller-observed
- caller timeout does not imply external work stopped
- supplied-client docs say who owns config and who owns Tina deadlines
- all bridge pressure surfaces have validated names

Tests:

- vocabulary Debug/display tokens are stable
- classifier categories are exhaustive for migrated bridge errors
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
- classifier tests cover `Succeeded`, `Retryable`, `Throttled`, `Auth`,
  `NotFound`, `InvalidRequest`, and `Fatal`

### Rock 3: Bridge Pressure Adapter

Ship one copied adapter path from bridge metrics to `CapacitySurfaceReport` /
`ServicePressureReport`.

Required:

- surface name
- capacity
- current/in-flight
- high-water
- full count
- timeout count
- closed count where available
- late-result count where available
- unavailable when a bridge cannot measure a field

Rules:

- no caller-supplied config can lie about installed capacity
- metrics handle stores effective installed capacity where needed
- pressure report says worker-terminal when that is what it measures

Tests:

- report uses installed capacity, not arbitrary caller config
- late-result count appears when supported
- unavailable field is explicit when unsupported
- service pressure builder can consume the report

### Rock 4: One Non-AWS Bridge Alignment

Update `tina-sqlx-bridge` to the shared vocabulary.

Required:

- docs use shared worker-terminal vs caller-observed wording
- pressure adapter uses shared names
- closer/drain docs use shared lifecycle words
- classifier maps to shared category names where it already has categories

Do not rewrite the whole bridge.

Tests:

- existing bridge tests still pass
- one pressure-report test proves installed capacity truth
- docs no longer overclaim caller-observed outcome

### Rock 5: Bridge Author Docs

Add a bridge author page or section.

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

Docs should be written for someone adding the next SDK bridge.

## Required Proof

Run at least:

```text
cargo fmt --all --check
cargo test -p tina-aws-bridge --tests
cargo test -p tina-sqlx-bridge --tests
cargo test -p tina-reqwest-bridge --tests
cargo clippy -p tina-aws-bridge -p tina-sqlx-bridge -p tina-reqwest-bridge --tests -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

The non-AWS bridge for this phase is `tina-sqlx-bridge`.

## Hostile Review Checklist

Before merge, prove:

- no bridge claims caller saw an outcome it cannot observe
- installed capacity cannot be faked by passing a fresh config to metrics
- close/drain/shutdown words match the lifecycle docs
- late external work is visible or explicitly unsupported
- AWS extraction did not hide service-specific request/response truth
- shared vocabulary did not become a framework blob
- one non-AWS bridge uses the vocabulary

## Done Means

The next bridge author has a copied shape.

AWS bridge code is less repetitive.

At least one non-AWS bridge speaks the same lifecycle/pressure language.

External work remains honestly external.
