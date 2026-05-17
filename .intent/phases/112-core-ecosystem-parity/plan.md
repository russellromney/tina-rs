# Phase 112: Core Ecosystem Parity

## Status

- IDD implementation phase.
- Runs after 103-109 identify what remains truly missing.

## Grug Truth

Tokio replacement is not one feature.

It is the boring pile: protocols, files, local IPC, codecs, pools, retries,
limits, persistence, client libraries, shutdown, proof, and docs that admit the
edges.

## Goal

Close the next layer of core ecosystem gaps after the current protocol/bridge
push:

- local IPC
- file streaming
- codec helpers
- admission/rate/concurrency limits
- saga/compensation pattern
- pool maturity
- async ecosystem boundary
- humble benchmarks

## Non-Goals

- No broad framework before specimens pull on it.
- No "Tina replaces all Tokio apps" claim.
- No performance claim without equivalent semantics and pressure behavior.
- No hidden async runtime under native Tina APIs.

## Rocks

### Rock 1: Local IPC

Add Unix-domain socket listener/client rails on platforms that support them:

- bind/connect/accept/read/write/close
- cancel/timeout/shutdown truth
- sim support or honest unsupported
- local admin/sidecar specimen

### Rock 2: File Streaming And Codecs

Add bounded helpers:

- file read stream
- file write stream
- line codec
- length-delimited codec
- framed request/reply helper where it removes repeated loops

No unbounded file reads by default.

### Rock 3: Admission And Rate Limits

Build explicit policy helpers:

- shed
- bounded wait
- rate limit
- per-key/per-tenant cap
- concurrency limit
- retry with backoff

Each returns typed outcomes. No hidden retry.

### Rock 4: Saga / Compensation Pattern

Build one multi-resource workflow specimen and extract only repeated helpers:

- DB step
- HTTP/AWS step
- pool step
- compensation
- timeout
- cancel
- partial failure report

The state machine remains visible.

### Rock 5: Pool Maturity

Extend pool consumers:

- idle eviction
- max lifetime
- health check
- retire/reuse policy
- pooled HTTP/2/gRPC client
- DB pool proof under load

### Rock 6: Async Boundary

Name the honest interop story:

- sync codecs Tina adopts
- async crates that stay behind bridges
- Future/Stream boundary documented as bridge-only for now; no native bridge in
  this phase
- docs saying which Tokio app shapes Tina does not replace yet

### Rock 7: Benchmarks With Humility

Add focused benchmarks only after equivalent semantics exist:

- Tokio/hyper/tungstenite/tonic comparison where fair
- boundedness/failure behavior included
- capacity summary included
- local-machine labels

## User Proof

Add or update systems:

- `system_redisish_keyspace` for framed codec/local IPC or TCP path.
- `system_media_ingest_pipeline` for file streaming.
- `system_checkout_saga` for compensation.
- `system_api_gateway_limits` for admission/rate limits.
- `system_soak_http_db` for pool maturity and humble benchmark output.

## Required Proof

- Each new rail has cancel/close/drain tests.
- Each helper has a pressure/full test.
- Each system has smoke proof and README findings.
- Simulator support exists or reports unsupported honestly.
- Benchmarks include failure/pressure behavior, not only throughput.

## Done Means

The remaining "I need Tokio for this ordinary service piece" list gets shorter,
and every closed gap has a user-shaped proof, not just an API.
