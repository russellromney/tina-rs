# Phase 092: AWS Bridge Follow-Ups

## Status

- Ready to implement.
- One PR if it stays narrow.
- Can run beside HTTP/2 or timer work. Owns `tina-aws-bridge`.
- Implementation note: this slice ships SQS first, with explicit
  `SqsRequest` / `SqsResponse` / `SqsError` shapes, queue-url based
  requests, message body and receive-count caps, named visibility timeout,
  empty receive as a typed response, and the same timeout/late-result/
  close-drain metrics truth as S3. Fake-local tests use a tiny localhost
  HTTP endpoint with static dummy credentials and no real AWS account.

## Grug Truth

AWS SDK is external work.

Tina bounds admission.

AWS may still finish late.

Retry belongs to caller.

Idempotency belongs to caller.

Big bodies/items/messages need caps.

Fake-local CI or it did not happen.

## Goal

Extend `tina-aws-bridge` after S3 first form.

Preferred order:

1. SQS send/receive/delete;
2. DynamoDB get/put/query;
3. SNS publish;
4. Secrets Manager get-secret.

Ship the largest prefix that stays boring and well tested.

Do not build a generic AWS framework. Add service surfaces only when the shape
is explicit and bounded.

## Non-Goals

- no real AWS credentials in CI;
- no SigV4 rewrite;
- no hidden retry;
- no automatic idempotency;
- no unbounded message/body/item buffers;
- no broad AWS service coverage;
- no "cancel means AWS rolled back";
- no shared bridge framework unless three exact repeated shapes demand it.

## Rock 0: Read First

Read:

- `tina-aws-bridge/src/*`;
- `.intent/phases/088-aws-bridge-first-form/plan.md`;
- AWS bridge tests;
- bridge docs;
- this plan.

Before coding, add a status note here:

- services chosen;
- fake/local test strategy;
- request/response/error shape;
- caps added.

## Rock 1: SQS First

Add SQS if only one service fits.

Operations:

- `SendMessage`;
- `ReceiveMessage`;
- `DeleteMessage`.

Required truth:

- queue URL is explicit;
- message body cap;
- max receive count cap;
- visibility timeout is named;
- empty receive is a typed response, not error;
- receipt handle is required for delete;
- timeout/late-result follows bridge truth.

No automatic delete after receive.

No automatic retry.

## Rock 2: DynamoDB

Add DynamoDB only if SQS is stable or skipped deliberately.

Operations:

- `GetItem`;
- `PutItem`;
- `Query` first page only.

Required truth:

- table name explicit;
- item size cap;
- response item count/bytes cap;
- condition failure is typed;
- throttling is typed transient, but retry is caller-owned;
- pagination token is visible if query has more.

No automatic pagination.

No expression-builder framework.

## Rock 3: SNS And Secrets

SNS:

- `Publish`;
- message cap;
- topic ARN explicit;
- typed message id response.

Secrets Manager:

- `GetSecretValue`;
- response size cap;
- binary vs string secret truth;
- not-found/access-denied typed errors.

Only ship if fake/local testing is solid.

## Rock 4: Shared AWS Vocabulary

Add only boring shared pieces:

- operation kind;
- request id metadata if SDK exposes it;
- typed transient/fatal classifier if repeated across services;
- metrics counters per service/operation.

Do not add a trait hierarchy or macro.

If a helper is copied three times exactly, factor it. Two is coincidence.

## Rock 5: Tests

Required for each shipped service:

- happy path;
- cap exceeded before SDK call where possible;
- full/in-flight pressure;
- closed bridge;
- timeout and late-result truth;
- typed SDK/service error;
- close/drain report includes operation kind;
- supplied-client ownership if the path exists.

Fake/local endpoint is required. Tests must not need real AWS credentials.

## Docs

Update:

- `tina-aws-bridge` crate docs;
- bridge user guide;
- one small example if a new service is shipped.

Docs must say:

- Tina bounds and observes AWS work;
- AWS is not simulator-replayable unless facts are recorded;
- cancellation after SDK acceptance is best-effort/late-result truth.

## Required Checks

- `cargo fmt --all --check`
- `cargo test -p tina-aws-bridge`
- `cargo clippy -p tina-aws-bridge --tests -- -D warnings`
- `RUSTDOCFLAGS="-D warnings" cargo doc -p tina-aws-bridge --no-deps`

## Hostile Review Notes

- Risk: many AWS services make one giant enum soup.
  Fix: ship one or two services first; keep operation kinds explicit.
- Risk: receive/delete hides SQS lifecycle.
  Fix: receive returns receipt handle; caller chooses delete.
- Risk: DynamoDB query hides pagination.
  Fix: first page only, token visible.
- Risk: retry fog.
  Fix: classifier okay, retry loop caller-owned.
- Risk: fake-local test lies.
  Fix: tests assert request path, caps, typed errors, and close/drain truth.
