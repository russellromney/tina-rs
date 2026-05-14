#![forbid(unsafe_code)]
#![deny(missing_docs)]

//! Bounded AWS SDK bridge for Tina services.
//!
//! First form started with S3. Phase 092 adds SQS. The AWS Rust SDK owns SigV4,
//! credentials, endpoints, HTTP, TLS, and service protocol details.
//! Tina owns bounded admission, per-operation deadline truth, visible
//! pressure, typed request/response enums, and lifecycle handles.
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
//!         call: RuntimeCall<AppMsg>,
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
//! - inner [`S3Error`] is worker outcome truth after the bridge
//!   admitted SDK work.
//!
//! # Retry truth
//!
//! [`S3Config`] disables AWS SDK retries by default
//! ([`SdkRetryPolicy::Disabled`]). If you opt into
//! [`SdkRetryPolicy::Standard`], the SDK may perform multiple HTTP
//! attempts inside one admitted bridge operation. The bridge reports
//! the configured retry attempt budget in metrics; it does not
//! inspect or count every internal SDK retry attempt. When wrapping a
//! caller-supplied `aws_sdk_s3::Client`, SDK retry policy is
//! caller-owned and the metrics field `sdk_max_attempts` is `0`.
//!
//! # Cancellation truth
//!
//! Once the bridge admits an operation and spawns the SDK future, a
//! bridge timeout means Tina stops waiting. The SDK future is not
//! aborted, because aborting a Tokio task does not prove that bytes
//! already accepted by Hyper/S3 were cancelled. When the SDK future
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

mod helpers;
mod metrics;
mod sqs_metrics;
mod sqs_types;
mod sqs_worker;
mod types;
mod worker;

pub use helpers::{S3Address, S3CallOutcome, SqsAddress, SqsCallOutcome, send_s3, send_sqs};
pub use metrics::{S3Metrics, S3MetricsHandle, S3PressureReport};
pub use sqs_metrics::{SqsMetrics, SqsMetricsHandle, SqsPressureReport};
pub use sqs_types::{
    SqsConfig, SqsConfigError, SqsCredentials, SqsDeleteMessage, SqsDeletedMessage, SqsError,
    SqsMessage, SqsReceiveMessage, SqsReceivedMessages, SqsRequest, SqsResponse, SqsSendMessage,
    SqsSentMessage,
};
pub use sqs_worker::{
    InstalledSqsBridge, SqsCloser, SqsDrainReport, SqsInstallError, SqsMsg, SqsWorker, install_sqs,
};
pub use types::{
    S3Config, S3ConfigError, S3Credentials, S3DeleteObject, S3DeletedObject, S3Error, S3GetObject,
    S3HeadObject, S3Object, S3ObjectHead, S3PutObject, S3PutObjectOk, S3Request, S3Response,
    SdkRetryPolicy,
};
pub use worker::{
    InstallError, InstalledS3Bridge, S3Closer, S3DrainReport, S3Msg, S3Worker, install_s3,
};
