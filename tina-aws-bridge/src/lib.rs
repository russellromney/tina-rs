#![forbid(unsafe_code)]
#![deny(missing_docs)]

//! Bounded AWS SDK bridge for Tina services.
//!
//! Ships small, bounded workers around the AWS Rust SDK for S3, SQS,
//! DynamoDB, SNS, and Secrets Manager. The AWS SDK owns SigV4,
//! credentials, endpoints, HTTP, TLS, and service protocol details.
//! Tina owns bounded admission, per-operation deadline truth, visible
//! pressure, typed request/response enums, and lifecycle handles.
//!
//! For the bridge-author copy path — install, close, drain, metrics,
//! pressure, classifier, late-result truth — see
//! `docs/tina-user-guide/30-bridge-author-kit.md`. Each of the five
//! services in this crate is one specimen of that path: the
//! per-service `install_xxx`/`with_supplied_client` returns a
//! `BridgeInstall` handle, the closer flips admission, `close_and_drain`
//! reports remaining in-flight kinds, the metrics handle exposes a
//! typed `BridgePressure` and worker-terminal counts, and
//! `XxxOutcomeExt::classify` projects each outcome onto the shared
//! `BridgeOutcomeClass` vocabulary.
//!
//! Each service worker has the same shape:
//!
//! - `XxxConfig` + `XxxConfigError` validated up front;
//! - `XxxWorker::install(&runtime, cfg)` builds an owned Tokio runtime
//!   and SDK client and registers the worker;
//! - `XxxWorker::with_supplied_client(cfg, client, runtime)` accepts a
//!   caller-built SDK client and Tokio runtime handle;
//! - `send_xxx(addr, request, timeout)` builds the `call(...)` effect;
//! - `XxxCloser::close()` flags admission closed; `close_and_drain`
//!   is a blocking host-thread shutdown helper that waits a bounded
//!   time for already admitted SDK work to leave the in-flight set;
//! - `XxxMetrics` + `XxxPressureReport` expose the configured capacity
//!   and live in-flight pressure;
//! - `XxxOutcomeExt::classify(...)` sorts each outcome into the small
//!   bridge classifier vocabulary (succeeded / transient / fatal /
//!   late-after-abandon) so caller-owned retry loops can decide.
//!
//! # Use
//!
//! ```no_run
//! use std::convert::Infallible;
//! use std::time::Duration;
//!
//! use tina::prelude::*;
//! use tina_aws_bridge::{S3Address, S3CallOutcome, S3Request, S3PutObject};
//! use tina_runtime::{RuntimeCall, call};
//!
//! enum AppMsg {
//!     Start,
//!     S3PutDone(S3CallOutcome),
//! }
//!
//! struct App {
//!     aws: S3Address,
//! }
//!
//! impl Isolate for App {
//!     tina::isolate_types! {
//!         message: AppMsg,
//!         reply: (),
//!         send: tina::Outbound<Infallible>,
//!         spawn: Infallible,
//!         io: RuntimeCall<AppMsg>,
//!         shard: SingleShard,
//!     }
//!
//!     fn handle(&mut self, msg: AppMsg, _ctx: &mut Context<'_, SingleShard, Self::Reply>) -> Effect<Self> {
//!         match msg {
//!             AppMsg::Start => call(
//!                 self.aws,
//!                 tina_aws_bridge::S3Msg::Send(S3Request::PutObject(S3PutObject {
//!                     bucket: "bucket".into(),
//!                     key: "key".into(),
//!                     body: b"hello".to_vec(),
//!                     content_type: Some("text/plain".into()),
//!                 })),
//!                 Duration::from_secs(2),
//!             )
//!             .then(AppMsg::S3PutDone),
//!             AppMsg::S3PutDone(_) => stop(),
//!         }
//!     }
//! }
//! ```
//!
//! The reply shape preserves both layers:
//!
//! - outer `CallOutcome::Full` / `Closed` / `Timeout` is Tina call
//!   delivery truth;
//! - inner service-specific error enum is worker outcome truth after
//!   the bridge admitted SDK work.
//!
//! # Retry truth
//!
//! Each service config disables AWS SDK retries by default
//! ([`SdkRetryPolicy::Disabled`]). If you opt into
//! [`SdkRetryPolicy::Standard`], the SDK may perform multiple HTTP
//! attempts inside one admitted bridge operation. The bridge reports
//! the configured retry attempt budget in metrics; it does not
//! inspect or count every internal SDK retry attempt. When wrapping a
//! caller-supplied SDK client, SDK retry policy is caller-owned and
//! the metrics field `sdk_max_attempts` is `0`.
//!
//! S3 splits out `NotFound`, `AccessDenied`, and `Throttled` from
//! generic SDK errors. A throttled S3 result is retryable service
//! pressure after the SDK's own retry budget, not fatal unknown SDK
//! noise.
//!
//! # Cancellation truth
//!
//! Once the bridge admits an operation and spawns the SDK future, a
//! bridge timeout means Tina stops waiting. The SDK future is not
//! aborted, because aborting a Tokio task does not prove that bytes
//! already accepted by Hyper / AWS were cancelled. When the SDK future
//! eventually finishes, worker-terminal metrics are tallied,
//! `late_results` increments, and only then does the operation leave
//! the bridge's in-flight capacity.
//!
//! # SQS truth
//!
//! SQS support keeps the service lifecycle explicit:
//!
//! - queue URL is supplied on every request;
//! - `SendMessage` is capped by [`SqsConfig::message_body_limit`];
//! - `ReceiveMessage` is capped by [`SqsConfig::max_receive_messages`]
//!   and names `visibility_timeout_seconds`;
//! - empty receive returns [`SqsResponse::ReceivedMessages`] with an
//!   empty vector;
//! - received messages carry a receipt handle, and delete is a
//!   separate caller-owned request.
//!
//! The bridge does not retry, auto-delete after receive, or infer
//! idempotency. It only bounds and observes admitted SDK work.
//!
//! # Shutdown drain
//!
//! `XxxCloser::close_and_drain` is for host-side shutdown code after
//! the caller has stopped submitting new work. It blocks the calling
//! thread with a short sleep/poll loop until the in-flight counter
//! reaches zero or the supplied timeout elapses. Do not call it from a
//! Tina shard handler for the same bridge: that handler would block the
//! shard that must poll completions and decrement the in-flight counter.
//! From isolate code, call `close()` and arrange host-side drain instead.
//!
//! # DynamoDB truth
//!
//! - bridge supports `GetItem`, `PutItem`, `UpdateItem`, `DeleteItem`,
//!   and `Query`;
//! - typed errors split out `ConditionalCheckFailed`,
//!   `ProvisionedThroughputExceeded`, `ResourceNotFound`, `Throttled`,
//!   and `TransactionConflict` from generic SDK errors;
//! - `ReturnCapacity::Total`/`Indexes` opts in to consumed-capacity
//!   facts on each response;
//! - `item_size_limit` rejects encoded items larger than the cap at
//!   admission, before the SDK sees a request;
//! - `Query` limit is bounded by `query_item_limit`.
//!
//! # SNS truth
//!
//! - bridge supports `Publish` only, with explicit destination
//!   (TopicArn / TargetArn / PhoneNumber);
//! - `MessageGroupId` / `MessageDeduplicationId` pass through for FIFO
//!   topics; the bridge does not infer idempotency from these;
//! - bridge does not wrap the message body as JSON message-structure;
//! - typed errors split out `InvalidParameter`, `NotFound`,
//!   `AccessDenied`, and `Throttled`.
//!
//! # Secrets Manager truth
//!
//! - bridge supports `GetSecretValue` only;
//! - `version_id` and `version_stage` are mutually exclusive;
//! - returned material exceeding `secret_size_limit` becomes
//!   `SecretsError::SecretTooLarge`;
//! - typed errors split out `NotFound`, `InvalidParameter`,
//!   `DecryptionFailed`, `AccessDenied`, and `Throttled`.
//!
//! # Supplied client (all services)
//!
//! Each worker exposes a `with_supplied_client` constructor that
//! wraps a caller-built SDK client plus Tokio runtime handle. The
//! supplied client owns credentials, region, endpoint, HTTP connector,
//! TLS, and the SDK retry policy. The bridge does not re-apply config
//! credentials, endpoint, or retry fields on this path, and reports
//! `sdk_max_attempts = 0` (unknown). The bridge still owns Tina-side
//! `mailbox_capacity`, `max_in_flight`, request/response caps, and
//! the per-operation timeout. The caller's Tokio runtime is never
//! shut down by the bridge.
//!
//! # Tracing
//!
//! All workers emit lifecycle and per-call events on the
//! `tina_aws.bridge` and `tina_aws.bridge.call` targets. Events
//! include `kind = "admitted"` / `"timeout"` / `"admission_rejected"`
//! at the call layer, plus `kind = "close"` at the bridge layer.

mod bridge_adapter;
mod classifier;
mod core;
mod dynamodb_metrics;
mod dynamodb_types;
mod dynamodb_worker;
mod helpers;
mod metrics;
mod secrets_metrics;
mod secrets_types;
mod secrets_worker;
mod sns_metrics;
mod sns_types;
mod sns_worker;
mod sqs_metrics;
mod sqs_types;
mod sqs_worker;
mod types;
mod worker;

pub use bridge_adapter::{
    DYNAMODB_BRIDGE_SURFACE, S3_BRIDGE_SURFACE, SECRETS_BRIDGE_SURFACE, SNS_BRIDGE_SURFACE,
    SQS_BRIDGE_SURFACE,
};
pub use classifier::{
    DynamoOutcomeExt, S3OutcomeExt, SecretsOutcomeExt, SnsOutcomeExt, SqsOutcomeExt,
};
// Shared bridge vocabulary lives in `tina_runtime::bridge`. Re-exported
// here so existing callers can keep their imports.
pub use dynamodb_metrics::{DynamoMetrics, DynamoMetricsHandle, DynamoPressureReport};
pub use dynamodb_types::{
    DynamoConfig, DynamoConfigError, DynamoConsistency, DynamoConsumedCapacity, DynamoCredentials,
    DynamoDeleteItem, DynamoDeletedItem, DynamoError, DynamoGetItem, DynamoGotItem, DynamoItem,
    DynamoPutItem, DynamoPutItemOk, DynamoQuery, DynamoQueryResult, DynamoRequest, DynamoResponse,
    DynamoUpdateItem, DynamoUpdatedItem, DynamoValue, ReturnCapacity,
};
pub use dynamodb_worker::{
    DynamoCloser, DynamoDrainReport, DynamoInstallError, DynamoMsg, DynamoWorker,
    InstalledDynamoBridge, install_dynamodb,
};
pub use helpers::{
    DynamoAddress, DynamoCallOutcome, S3Address, S3CallOutcome, SecretsAddress, SecretsCallOutcome,
    SnsAddress, SnsCallOutcome, SqsAddress, SqsCallOutcome, send_dynamodb, send_s3, send_secrets,
    send_sns, send_sqs,
};
pub use metrics::{S3Metrics, S3MetricsHandle, S3PressureReport};
pub use secrets_metrics::{SecretsMetrics, SecretsMetricsHandle, SecretsPressureReport};
pub use secrets_types::{
    SecretsConfig, SecretsConfigError, SecretsCredentials, SecretsError, SecretsGetSecretValue,
    SecretsRequest, SecretsResponse, SecretsValue,
};
pub use secrets_worker::{
    InstalledSecretsBridge, SecretsCloser, SecretsDrainReport, SecretsInstallError, SecretsMsg,
    SecretsWorker, install_secrets,
};
pub use sns_metrics::{SnsMetrics, SnsMetricsHandle, SnsPressureReport};
pub use sns_types::{
    SnsAttributeValue, SnsConfig, SnsConfigError, SnsCredentials, SnsDestination, SnsError,
    SnsPublish, SnsPublished, SnsRequest, SnsResponse,
};
pub use sns_worker::{
    InstalledSnsBridge, SnsCloser, SnsDrainReport, SnsInstallError, SnsMsg, SnsWorker, install_sns,
};
pub use sqs_metrics::{SqsMetrics, SqsMetricsHandle, SqsPressureReport};
pub use sqs_types::{
    SqsConfig, SqsConfigError, SqsCredentials, SqsDeleteMessage, SqsDeletedMessage, SqsError,
    SqsMessage, SqsReceiveMessage, SqsReceivedMessages, SqsRequest, SqsResponse, SqsSendMessage,
    SqsSentMessage,
};
pub use sqs_worker::{
    InstalledSqsBridge, SqsCloser, SqsDrainReport, SqsInstallError, SqsMsg, SqsWorker, install_sqs,
};
pub use tina_runtime::bridge::{
    BridgeFatal, BridgeOutcomeClass, BridgePressure, BridgeRetryable, BridgeUnavailable,
};
pub use types::{
    S3Config, S3ConfigError, S3Credentials, S3DeleteObject, S3DeletedObject, S3Error, S3GetObject,
    S3HeadObject, S3Object, S3ObjectHead, S3PutObject, S3PutObjectOk, S3Request, S3Response,
    SdkRetryPolicy,
};
pub use worker::{
    InstallError, InstalledS3Bridge, S3Closer, S3DrainReport, S3Msg, S3Worker, install_s3,
};
