# Phase 135: Native AWS First Form

## Status

- Future implementation phase.
- One PR.
- Runs after Phase 131 outbound endpoint/connect policy. Native AWS must not
  invent a private DNS/connect path.

## Grug Truth

AWS is just signed HTTP until it is not.

Tina should own the bounded HTTP path, body caps, timeouts, cancellation,
pressure, trace, and replay facts. The AWS SDK bridge remains the answer for
full AWS ecosystem behavior. Native AWS is the Tina-shaped first form for the
operations we can model honestly.

## Current Code Facts

- `tina-aws-bridge` already supports S3, SQS, SNS, DynamoDB, and Secrets
  through the AWS SDK.
- Bridge docs correctly say the SDK owns SigV4, credential providers, HTTP,
  TLS, retry policy, endpoints, and service protocol edges.
- Native `tina-http` has HTTP/1.1, HTTPS, keepalive pool, HTTP/2/gRPC, body
  caps, chunked transfer, and protocol/pressure facts.
- Phase 131 adds host endpoint + connect policy. Native AWS must use that path.
- The workspace does not yet have AWS signing/XML/form dependencies.

## Goal

Ship a native Tina AWS battery for the smallest useful production shape:

- native SigV4 signing with static credentials;
- native S3 `PutObject`, `GetObject`, `HeadObject`, `DeleteObject`;
- native SQS `SendMessage`, `ReceiveMessage`, `DeleteMessage`;
- bounded request/response bodies;
- typed pressure, timeout, service error, auth error, and late-result truth;
- hermetic fake-AWS tests;
- docs that clearly separate native AWS from `tina-aws-bridge`.

## Does Not Include

- no AWS SDK;
- no Tokio/Hyper under the native crate;
- no default credential provider chain;
- no STS, assume-role, SSO, profile files, IMDS, or web identity;
- no SDK retry policy;
- no automatic idempotency;
- no multipart upload;
- no presigned URLs;
- no DynamoDB, SNS, or Secrets in this first native phase;
- no claim that native AWS replaces the AWS SDK for every app.

## Names And Homes

- Add crate: `tina-aws-native`.
- Keep `tina-aws-bridge` unchanged except docs cross-links.
- Public modules:
  - `aws` for shared signing/config/outcome vocabulary;
  - `s3`;
  - `sqs`.
- Fake AWS servers live under crate tests or `examples/systems` test support.
  They are not public API.
- Public shared names:
  - `AwsRegion`
  - `AwsEndpoint`
  - `AwsStaticCredentials`
  - `AwsService`
  - `AwsLimits`
  - `AwsRequestId`
  - `AwsHttpReport`
  - `AwsError`
  - `AwsSigner`
  - `AwsSignedRequest`
- S3 names:
  - `S3Client`
  - `S3Config`
  - `S3Request`
  - `S3Response`
  - `S3Error`
- SQS names:
  - `SqsClient`
  - `SqsConfig`
  - `SqsRequest`
  - `SqsResponse`
  - `SqsError`

`Client` here means a Tina isolate/manager address wrapper, not an async SDK
client.

## Dependencies

Use small sync crates. Do not hand-roll crypto.

Allowed new dependencies:

- `hmac`
- `sha2`
- `hex`
- `time`
- `percent-encoding`
- `quick-xml`

If implementation needs another dependency, it must be sync, small, and named
in the PR rationale.

## Implementation

### Rock 1: SigV4 Core

Implement `tina-aws-native::aws`:

- canonical URI;
- canonical query string;
- canonical headers;
- signed headers list;
- payload SHA-256;
- string-to-sign;
- HMAC signing key;
- Authorization header;
- `x-amz-date`;
- `x-amz-content-sha256`;
- optional `x-amz-security-token`;
- redacted debug/display for credentials.

Static credentials only:

```rust
AwsStaticCredentials::new(access_key_id, secret_access_key)
    .with_session_token(token)
```

`AwsEndpoint` must be explicit:

- region;
- scheme;
- host;
- port;
- path style vs virtual-hosted style for S3;
- local/fake endpoint support.

No ambient AWS config is read.

### Rock 2: Native HTTP Execution

Build one bounded AWS HTTP executor over native `tina-http`:

- use Phase 131 host endpoint/connect policy;
- use native HTTPS/HTTP targets;
- own an explicit bounded keepalive pool with capacity >= 1;
- explicit idle timeout and close/drain report for that pool;
- per-operation timeout;
- max in-flight operations;
- request body cap;
- response body cap;
- pressure report;
- close/drain report;
- request id / operation kind in reports.

Timeout/cancel truth:

- before HTTP admission: caller sees typed timeout/cancel, no bytes sent;
- after HTTP request admitted/sent: Tina stops waiting, but remote service may
  have acted;
- late HTTP completion is counted and cannot become success for the caller.

### Rock 3: S3 First Form

Implement:

- `PutObject { bucket, key, body, content_type }`;
- `GetObject { bucket, key, max_bytes }`;
- `HeadObject { bucket, key }`;
- `DeleteObject { bucket, key }`.

Support both:

- path-style: `/{bucket}/{key}`;
- virtual-hosted style: `{bucket}.{host}/{key}`.

Copied local/fake tests use path-style.

S3 typed outcomes:

- `PutObjectOk { etag, request_id }`;
- `Object { body, content_length, content_type, etag, request_id }`;
- `ObjectHead { content_length, content_type, etag, request_id }`;
- `DeletedObject { version_id, delete_marker, request_id }`;
- errors: `Full`, `Closed`, `Timeout`, `RequestTooLarge`,
  `ResponseTooLarge`, `Auth`, `AccessDenied`, `NotFound`, `Throttled`,
  `Service { status, code, message }`, `Protocol`, `Transport`.

Do not stream arbitrary large objects in this phase. `GetObject` buffers up to
`min(request.max_bytes, config.response_body_limit)` and then fails closed.

### Rock 4: SQS First Form

Implement AWS Query API calls:

- `SendMessage { queue_url, body, message_group_id, message_deduplication_id }`;
- `ReceiveMessage { queue_url, max_messages, visibility_timeout_seconds,
  wait_time_seconds }`;
- `DeleteMessage { queue_url, receipt_handle }`.

SQS typed outcomes:

- `SentMessage { message_id, sequence_number, request_id }`;
- `ReceivedMessages { messages, request_id }`;
- `DeletedMessage { request_id }`;
- errors: `Full`, `Closed`, `Timeout`, `MessageTooLarge`, `Auth`,
  `AccessDenied`, `QueueNotFound`, `Throttled`,
  `Service { status, code, message }`, `Protocol`, `Transport`.

Empty receive is success with `messages = []`.
Receive never auto-deletes. Delete is caller-owned.

### Rock 5: Fake AWS Servers

Add hermetic fake servers in tests:

- fake S3 verifies signature headers and canonical path;
- fake S3 returns success, not found, access denied, throttled, malformed XML,
  body too large;
- fake SQS verifies form/query parameters and signature;
- fake SQS returns send success, empty receive, one-message receive, delete
  success, queue-not-found, malformed XML.

Use native Tina HTTP server for the fake services so the tests prove native Tina
on both sides of the wire.

### Rock 6: System Specimen And Docs

Add one system:

- `examples/systems/system_native_aws_outbox`;
- writes one object to fake S3;
- sends one SQS message;
- receives and deletes one SQS message;
- exposes pressure/report line;
- proves shutdown/close/drain.

Docs:

- update async-boundary page: native AWS vs AWS bridge;
- update bridge-crates page to keep SDK bridge story;
- add native AWS section under service-client worked examples or a new battery
  page;
- README table entry for `tina-aws-native`.

## Required Proof

- SigV4 golden vector tests cover canonical request, string-to-sign, signature,
  signed headers, query ordering, header normalization, and session token.
- Credentials never appear in `Debug`, `Display`, reports, or panic text.
- S3 put/get/head/delete pass against fake S3.
- S3 not-found/access-denied/throttled map to typed errors.
- S3 request body over cap fails before HTTP send.
- S3 response body over cap fails closed and reports bytes/cap.
- SQS send/receive/delete pass against fake SQS.
- SQS empty receive is success, not error.
- SQS oversized message fails before HTTP send.
- Malformed XML maps to `Protocol`, not panic.
- Wrong signature maps to `Auth`.
- HTTP pool full is distinct from AWS service throttling.
- Operation timeout before send and after send are tested separately.
- Late completion after caller timeout increments late-result/report counters
  and cannot settle the caller as success.
- Close stops new admission; drain reports in-flight operations by kind.
- Request-scoped cancellation cancels Tina-owned waits and preserves external
  late-result truth.
- Live replay capture records operation kind, status/error class, and capacity
  facts; unsupported facts fail closed.
- No `aws-sdk-*`, `tokio`, `hyper`, or async runtime dependency appears in
  `tina-aws-native`.

## Native Versus Bridge Contract

Native AWS is for bounded, replay-aware Tina services that can live inside this
explicit feature set.

Use `tina-aws-bridge` when you need:

- AWS default credential provider chain;
- STS/assume-role/SSO/IMDS;
- custom AWS SDK HTTP/TLS/proxy config;
- SDK retries and middleware;
- services not implemented natively;
- full AWS protocol compatibility over Tina-native replay.

The docs must keep this split sharp.
