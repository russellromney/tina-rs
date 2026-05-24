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
- Phase 131 adds `tina-http::connect`, unresolved endpoint types, connect
  policy, reconnect/session managers, and typed DNS/connect reports. Native AWS
  must use that path. It must not grow a second DNS/connect vocabulary.
- The workspace does not yet have AWS signing/XML/form dependencies.
- `tina-http::HttpRequest` is intentionally small: method, origin-form path,
  headers, and buffered/streaming body. It does not parse URLs or build query
  strings for users.
- `tina-http::HttpTarget` owns Host/SNI policy. If the request already has
  `Host`, the client rejects with `DuplicateHostHeader`.
- `tina-http::build_keepalive_pool` returns both the pool address and the
  connection isolate addresses. Pool close and connection stop are separate
  truths.
- `PendingCancelableCallSet` exists for bounded cancelable multi-turn work.
  Use it for in-flight AWS operations; do not hand-roll a growing map.

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

- `http`
- `hmac`
- `sha2`
- `hex`
- `time`
- `percent-encoding`
- `quick-xml`

If implementation needs another dependency, it must be sync, small, and named
in the PR rationale.

## Pinned Shape

This phase builds service isolates, not a loose helper bag.

Installers:

- `install_s3_native(&runtime, S3Config) -> InstalledS3Native<S>`
- `install_sqs_native(&runtime, SqsConfig) -> InstalledSqsNative<S>`

Installed handles contain:

- service address;
- keepalive pool handles;
- closer;
- metrics/report handle.

User call shape:

```rust
send_s3_native(self.s3, S3Request::PutObject(req), timeout)
    .then(AppMsg::S3Done)
```

Raw call shape:

```rust
call(self.s3.address(), S3Msg::Send(S3Request::GetObject(req)), timeout)
    .then(AppMsg::S3Done)
```

The helper is just the copied path. The raw call remains visible.

Continuation reply shape is two-layer, like the bridge crates:

```rust
CallOutcome<Result<S3Response, S3Error>>
CallOutcome<Result<SqsResponse, SqsError>>
```

Outer `CallOutcome::Full/Closed/Timeout/Rejected` means the Tina call itself
did not reach or complete against the AWS isolate. Inner `S3Error`/`SqsError`
means the isolate admitted the operation and then hit AWS-shaped truth such as
service error, body cap, auth failure, operation timeout, or HTTP transport.
Do not collapse the two layers.

Each native AWS service isolate owns:

- explicit config;
- one native HTTP keepalive pool for its endpoint;
- bounded in-flight operation table;
- bounded cancelable pending set;
- counters and pressure report;
- close/drain state.

Config validation rejects:

- empty region, host, bucket, queue URL, access key id, or secret key;
- zero pool capacity, zero in-flight capacity, zero mailbox capacity;
- zero request/response body limits;
- SQS `ReceiveMessage.max_messages == 0`;
- SQS `wait_time_seconds` greater than the operation timeout budget unless the
  caller explicitly chooses a larger operation timeout.

No user callback runs inside hidden storage. Continuations still return ordinary
messages to the AWS isolate.

## Signing Rules

`AwsSigner` is a pure sync value. It does not read the clock, env vars, files,
network, or global config.

Inputs:

- `AwsStaticCredentials`;
- `AwsRegion`;
- `AwsService`;
- explicit `AwsSigningTime`;
- structured method/path/query/header/body facts.

Outputs:

- `AwsSignedRequest`;
- canonical request string for tests/reports with secrets removed;
- signed header list;
- payload SHA-256 hex;
- `x-amz-date`;
- `x-amz-content-sha256`;
- `Authorization`;
- optional `x-amz-security-token`.

`AwsSigningTime` is explicit because SigV4 is wall-clock shaped:

```rust
AwsSigningTime::from_utc(...)
AwsSigningClock::System
AwsSigningClock::Fixed(AwsSigningTime)
```

The signer itself takes `AwsSigningTime`. The live client may use
`AwsSigningClock::System`, but tests and replay-shaped examples use `Fixed`.
Reports name the signing timestamp, not the secret.

Do not support `UNSIGNED-PAYLOAD` in first form. Always sign the actual payload
hash.

Credential debug/display:

- access key id may show only a short redacted prefix/suffix;
- secret access key never appears;
- session token never appears;
- panic/report text must not include raw credentials.

## URI And Query Rules

Do not accept raw AWS URLs for signing.

S3:

- `S3BucketName` validates bucket names.
- `S3ObjectKey` is arbitrary bytes/string except empty key is rejected where the
  operation needs an object key.
- Path-style canonical URI is `/{bucket}/{encoded-key-segments}`.
- Virtual-hosted canonical URI is `/{encoded-key-segments}` and endpoint host is
  `{bucket}.{base_host}`.
- Virtual-hosted bucket names must be DNS-compatible.
- The implementation owns percent-encoding. Callers never pass a pre-encoded
  canonical path.

SQS:

- `SqsQueueUrl` parses with `http::Uri`.
- It rejects missing scheme, authority, host, or path.
- The queue URL supplies scheme/host/port/path. Region still comes from config.
- Query API parameters are a sorted encoded form body, not a raw caller string.

Canonical query/form encoding:

- sort by encoded name, then encoded value;
- encode space as `%20` for SigV4 canonical query;
- encode form bodies with AWS Query API compatible `application/x-www-form-urlencoded`;
- duplicate parameter names are allowed only when the operation type creates
  them deliberately.

## HTTP Execution Rules

Use native `tina-http`:

- Phase 131 endpoint/connect policy;
- `build_keepalive_pool` for the fixed endpoint pool underneath the AWS client;
- never `tokio`, `hyper`, `reqwest`, or the AWS SDK in `tina-aws-native`.

The AWS isolate sends `HttpRequest`s with origin-form paths. It does not set
`Host`; `HttpTarget` / endpoint policy sets Host and catches conflicts.

For every operation:

1. validate config and body caps;
2. admit into bounded in-flight storage;
3. compute `AwsSigningTime`;
4. sign;
5. acquire a keepalive pool lease;
6. call `KeepaliveConnectionMsg::request`;
7. release the lease as `Reuse` unless the connection isolate itself is suspect;
8. parse service response;
9. reply with typed AWS outcome.

If step 2 fails, do not sign and do not send bytes.

The `HttpClientConfig.limits.max_body_bytes` used by the keepalive pool must be
the AWS response cap. Do not let `tina-http` buffer a larger response and then
have AWS reject it after the fact.

If a caller timeout/cancel fires after HTTP admission, Tina stops waiting. AWS
may still have acted. Late completion is counted and cannot later settle the
caller as success.

Close:

- `close()` stops new admission.
- `close_and_drain(timeout)` waits for in-flight AWS operations and reports
  operation kinds still running.
- keepalive pool close and connection-isolate stop are both reported.

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

Tests must include known SigV4 vectors checked at each stage:

- canonical request;
- string to sign;
- signature;
- final Authorization header.

### Rock 2: Native HTTP Execution

Build one bounded AWS HTTP executor over native `tina-http`:

- use Phase 131 endpoint/connect policy;
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

User-callable messages:

- `S3Msg::Send(S3Request)`;
- `S3Msg::Close`;
- `S3Msg::CloseAndDrain { timeout }`;
- `S3Msg::PressureReport`;
- same shape for `SqsMsg`.

Internal continuation messages must not be accepted through `handle_call`; use
the public/internal message split already used elsewhere.

Timeout/cancel truth:

- before HTTP admission: caller sees typed timeout/cancel, no bytes sent;
- after HTTP request admitted/sent: Tina stops waiting, but remote service may
  have acted;
- late HTTP completion is counted and cannot become success for the caller.

In-flight storage:

- fixed capacity;
- keyed by operation id/generation;
- stores operation kind, request context, cancel handle, signing timestamp, and
  admitted byte counts;
- duplicate/stale completions cannot remove a newer operation.

Pressure report:

- capacity;
- current in-flight;
- high water;
- full count;
- timeout count;
- cancel count;
- late result count;
- per-operation in-flight counts;
- keepalive pool pressure summary.

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

S3 response parsing:

- success request id from `x-amz-request-id` when present;
- `ETag`, `Content-Length`, `Content-Type`, `x-amz-version-id`, and
  `x-amz-delete-marker` from headers;
- service errors from S3 XML `<Error><Code>...`;
- missing/malformed required XML fields map to `Protocol`.

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

Body cap rule:

- `PutObject` checks request bytes before HTTP admission.
- `GetObject` stops reading/fails if the response would cross the cap.
- `HeadObject` and `DeleteObject` never allocate a response body.

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

SQS response parsing:

- Query API over `POST`;
- content type `application/x-www-form-urlencoded`;
- explicit `Action` and `Version=2012-11-05`;
- parse `SendMessageResponse`, `ReceiveMessageResponse`, and
  `DeleteMessageResponse`;
- parse AWS error XML into typed service errors;
- preserve `MessageId`, `ReceiptHandle`, `MD5OfBody`, `SequenceNumber`,
  attributes supported by the first form, and request id.

First form attributes:

- receive supports message body, id, receipt handle, MD5, and basic string
  attributes only;
- no FIFO batching;
- no message attribute binary values;
- no long-polling magic beyond the explicit `wait_time_seconds` field.

### Rock 5: Fake AWS Servers

Add hermetic fake servers in tests:

- fake S3 verifies signature headers, canonical path, signed payload hash, and
  Host/S3 style;
- fake S3 returns success, not found, access denied, throttled, malformed XML,
  body too large;
- fake SQS verifies form parameters, canonical signing facts, signed payload
  hash, and queue URL path;
- fake SQS returns send success, empty receive, one-message receive, delete
  success, queue-not-found, malformed XML.

Use native Tina HTTP server for the fake services so the tests prove native Tina
on both sides of the wire.

The fake servers are not public API. Keep them in integration test support
unless a later system specimen proves a shared fake crate is useful.

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

- Crate has no dependency on `aws-*`, `tokio`, `hyper`, `reqwest`, or async
  runtime crates.
- SigV4 golden vector tests cover canonical request, string-to-sign, signature,
  signed headers, query ordering, header normalization, and session token.
- Credentials never appear in `Debug`, `Display`, reports, or panic text.
- Fixed signing time produces byte-for-byte stable signed requests.
- System signing clock is named in reports as live-only wall-clock input.
- S3 path-style and virtual-hosted signing produce different expected Host/path
  facts.
- Caller-supplied raw canonical paths/URLs are not accepted by the public S3
  API.
- SQS queue URL parser rejects missing scheme/host/path and preserves the queue
  path used for signing.
- Config validation rejects zero caps and impossible long-poll timeout budgets
  before registration.
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
- Runtime `CallOutcome::Full` is distinct from inner `S3Error::Full` /
  `SqsError::Full`.
- Keepalive connection `must_retire` is reported but normal consumers release
  `Reuse`; no slot drains to zero under ordinary errors.
- Operation timeout before send and after send are tested separately.
- Late completion after caller timeout increments late-result/report counters
  and cannot settle the caller as success.
- Close stops new admission; drain reports in-flight operations by kind.
- `close_and_drain` reports both AWS in-flight truth and keepalive
  pool/connection shutdown truth.
- Request-scoped cancellation cancels Tina-owned waits and preserves external
  late-result truth.
- Live replay capture records operation kind, status/error class, and capacity
  facts; unsupported facts fail closed.
- Duplicate/stale operation completions cannot remove a newer operation.
- Bounded in-flight storage full returns typed `Full` before signing or HTTP
  admission.
- `cargo test -p tina-aws-native`, crate docs, and the native AWS system
  specimen pass.

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
