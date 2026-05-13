# 088 AWS Bridge First Form

## Status

- Ready to implement.
- One PR. S3 only.
- Can run beside 087/089. This owns a new bridge crate and bridge docs.

## Grug Truth

AWS SDK is async ecosystem.

Tina should not rebuild AWS.

Tina should bound AWS work.

Retries are policy, not fog.

Timeout does not mean AWS undid side effects.

Late result must be visible.

No real AWS credentials in CI.

## Goal

Add `tina-aws-bridge` first form.

First form should make the production shape real:

- install returns `{ address, closer, metrics }`;
- explicit config;
- bounded `max_in_flight`;
- bounded mailbox/admission;
- typed request/response/error;
- typed metrics/pressure;
- close stops Tina-side admission;
- accepted AWS SDK work has honest timeout/cancel/late-result truth.

Start with **S3 only**. S3 pulls on body caps and network pressure. SQS can be
the next slice after this lands.

## Non-Goals

- no SigV4 rewrite;
- no custom AWS protocol implementation;
- no broad `aws-sdk-*` wrapper;
- no DynamoDB/SNS/Secrets in first PR;
- no SQS in first PR;
- no hidden retry loop;
- no real AWS account in CI;
- no fake cancellation claim after SDK accepts work;
- no bridge-common framework unless an existing bridge helper can be reused
  directly.

## Crate

Add:

```text
tina-aws-bridge
```

Feature/dependency choices:

- depend on AWS SDK crates only in this crate;
- S3 SDK first;
- Tokio runtime owned by bridge or supplied by caller, following bridge docs;
- fake/local S3 endpoint support required for tests;
- no real AWS credentials, no ambient credential chain in tests.

Workspace docs must make this opt-in.

## API Shape

Names can change, but first form should be S3-shaped, not fake-generic.

```rust
let installed = tina_aws_bridge::install_s3(&runtime, config)?;

call(
    installed.address,
    S3Request::PutObject { bucket, key, body },
    timeout,
)
.reply(AppMsg::AwsReturned)
```

Types:

- `S3Request`;
- `S3Response`;
- `S3Error`;
- `S3Config`;
- `S3Metrics`;
- `S3Closer`;
- `S3PressureReport` if useful.

Reserve `AwsRequest::S3(...)` / broad `AwsResponse` until a second AWS service
proves the wrapper is worth it.

S3 first-form operations:

- `PutObject { bucket, key, body }`;
- `GetObject { bucket, key, max_bytes }`;
- `HeadObject { bucket, key }`;
- optional `DeleteObject` if small.

Caps:

- max in flight;
- request body max bytes;
- response body max bytes;
- mailbox capacity;
- optional waiter/pending capacity if the worker has one.

## Retry Truth

Default should be:

- SDK retry disabled, or
- SDK retry config explicitly set and counted.

Do not add Tina-owned automatic retry in first form.

Expose:

- attempt count if available;
- retry count if SDK retries are enabled;
- timeout reason;
- SDK error kind/string without pretending it is stable protocol.

Docs must say idempotency is caller-owned.

## Fake S3

Tests must not use a real AWS account.

Use a tiny local HTTP server or AWS SDK-compatible fake endpoint:

- static dummy credentials;
- explicit `endpoint_url`;
- path-style addressing if needed;
- deterministic responses;
- no ambient env credentials;
- no network outside localhost.

## Cancellation Truth

Caller timeout/cancel means Tina stops waiting and reclaims caller capacity.

It does not mean S3 undid `PutObject`.

If the spawned SDK future can be aborted, say what that does and does not prove.
If it cannot prove remote cancellation, count abandoned work visibly.

Required late-result vocabulary:

- worker accepted;
- caller timed out/cancelled;
- SDK later completed if the future reports completion;
- SDK task was aborted/abandoned if no terminal result can be observed;
- metrics/trace record that truth.

Do not promise `late_results` when abort prevents the SDK future from returning.
Use a separate abandoned/aborted counter or docs wording.

## Close / Drain

Closer behavior:

- close stops new Tina-side admission;
- accepted work may drain for bounded time;
- report names in-flight work left at deadline;
- bridge-owned Tokio runtime shuts down only after drain/close policy is done;
- supplied runtime is not shut down by the bridge.

No fire-and-forget close that claims clean shutdown.

## Tests

Use fake/local endpoint. Prefer a tiny HTTP server that behaves like enough S3
for the operations. LocalStack/MinIO is okay only if CI setup is not painful.

Required tests:

- config validation rejects zero caps;
- happy `PutObject`;
- happy `GetObject`;
- happy `HeadObject`;
- `GetObject` reads streaming chunks and rejects too-large object before
  unbounded buffering;
- request cap rejects too-large body;
- max in-flight/full is visible;
- close rejects new work;
- drain waits for accepted work or reports remaining work;
- caller timeout after admission records late/abandoned truth;
- SDK/HTTP error maps to typed `AwsError`;
- supplied-runtime/owned-runtime ownership docs match behavior if both ship.

If supplied runtime/client support ships, guard the Tokio-context trap like the
SQLx bridge: construction/drop must not surprise users outside a runtime, or the
docs must loudly say what is allowed.

## Specimen

Add or update one specimen only if it teaches something:

- `examples/specimen_aws_s3_cache` or similar;
- write object, read object, handle missing key, handle timeout/full;
- no real AWS credentials;
- README compares Tokio AWS SDK direct call vs Tina bounded bridge.

## Docs

Update:

- `docs/tina-user-guide/18-bridge-crates.md`;
- top-level README crate list if this repo tracks crates there;
- crate rustdoc with copied call shape;
- roadmap/changelog.

Docs must say:

- bridge weakens replay boundary;
- no hidden retry;
- timeout is not remote undo;
- CI uses fake/local S3.

## Required Checks

- `cargo fmt --all --check`
- `cargo test -p tina-aws-bridge`
- `cargo clippy -p tina-aws-bridge --tests -- -D warnings`
- fake S3 integration test in CI-safe form
- specimen smoke if added
- `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`

## Done Means

- Tina service can call bounded S3 work;
- Full/Closed/Timeout/Abandoned are visible;
- S3 worker outcome is typed;
- close/drain/late-result truth is tested;
- no real AWS credentials are needed.
