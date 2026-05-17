//! Bridge outcome classifier.
//!
//! Tiny vocabulary so caller-owned retry loops can decide a single
//! thing: was this a success, can we safely retry, or is the request
//! dead. The classifier does **not** retry, idempotency-guard, or
//! back off — those remain caller-owned.
//!
//! Each outcome carries the two-layer truth from the bridge:
//!
//! - `CallOutcome::Full` / `Closed` / `Timeout` is bridge-delivery
//!   truth: the IsolateCall did/did not reach the worker;
//! - the inner `Result<Response, Error>` is worker-outcome truth from
//!   the SDK round-trip.
//!
//! The classifier maps the two layers together. The bridge cannot tell
//! a caller whether a failed mutation already happened on AWS — the
//! caller's idempotency story decides whether transient retries are
//! safe.

use tina_runtime::CallOutcome;

use crate::dynamodb_types::{DynamoError, DynamoResponse};
use crate::secrets_types::{SecretsError, SecretsResponse};
use crate::sns_types::{SnsError, SnsResponse};
use crate::sqs_types::{SqsError, SqsResponse};
use crate::types::{S3Error, S3Response};

/// Coarse outcome class for caller-owned retry decisions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeOutcomeClass {
    /// SDK round-trip succeeded.
    Succeeded,
    /// Transient — retry is plausibly safe **if the caller's
    /// idempotency story permits**. Reason names which layer failed.
    Transient(TransientReason),
    /// Fatal — request will not succeed on retry without changing the
    /// inputs or caller setup. Reason names the failure layer.
    Fatal(FatalReason),
}

/// Transient classifications (caller may retry under their own
/// idempotency rules).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransientReason {
    /// Tina bridge admission saturated (`max_in_flight` full).
    BridgeFull,
    /// Tina bridge closed admission while the call was in flight.
    BridgeClosed,
    /// Tina caller's IsolateCall deadline fired before the bridge
    /// replied.
    CallerTimeout,
    /// Bridge per-operation deadline fired. Late SDK completion may
    /// still land; `late_results` will increment.
    BridgeTimeout,
    /// Service-side throttling.
    ServiceThrottled,
    /// Service-side throughput rejection (DynamoDB).
    ProvisionedThroughputExceeded,
    /// Service-side transaction conflict (DynamoDB).
    TransactionConflict,
    /// SDK-level error not otherwise classified.
    SdkError,
}

/// Fatal classifications (retry is not safe without input change).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FatalReason {
    /// Bridge validation rejected the request.
    InvalidRequest,
    /// Request exceeded a configured cap.
    TooLarge,
    /// Conditional check failed (caller-supplied condition expression).
    ConditionalCheckFailed,
    /// Resource (table / queue / secret / topic / endpoint) was not
    /// found.
    NotFound,
    /// SDK / service rejected parameters as invalid.
    InvalidParameter,
    /// Caller has no permission to perform the operation.
    AccessDenied,
    /// KMS decryption failed.
    DecryptionFailed,
    /// Worker had an internal failure (bug or runtime).
    Internal,
}

// --------- S3 -----------

/// Classifier extension trait for [`crate::S3CallOutcome`].
pub trait S3OutcomeExt {
    /// Sorts the outcome into the bridge classifier vocabulary.
    fn classify(&self) -> BridgeOutcomeClass;
}

impl S3OutcomeExt for CallOutcome<Result<S3Response, S3Error>> {
    fn classify(&self) -> BridgeOutcomeClass {
        match self {
            CallOutcome::Full => BridgeOutcomeClass::Transient(TransientReason::BridgeFull),
            CallOutcome::Closed => BridgeOutcomeClass::Transient(TransientReason::BridgeClosed),
            CallOutcome::Timeout => BridgeOutcomeClass::Transient(TransientReason::CallerTimeout),
            CallOutcome::Rejected(_) => BridgeOutcomeClass::Fatal(FatalReason::InvalidRequest),
            CallOutcome::Replied(Ok(_)) => BridgeOutcomeClass::Succeeded,
            CallOutcome::Replied(Err(e)) => classify_s3_error(e),
        }
    }
}

fn classify_s3_error(err: &S3Error) -> BridgeOutcomeClass {
    match err {
        S3Error::Full => BridgeOutcomeClass::Transient(TransientReason::BridgeFull),
        S3Error::Closed => BridgeOutcomeClass::Transient(TransientReason::BridgeClosed),
        S3Error::Timeout => BridgeOutcomeClass::Transient(TransientReason::BridgeTimeout),
        S3Error::RequestTooLarge | S3Error::ResponseTooLarge => {
            BridgeOutcomeClass::Fatal(FatalReason::TooLarge)
        }
        S3Error::InvalidRequest(_) => BridgeOutcomeClass::Fatal(FatalReason::InvalidRequest),
        S3Error::Sdk(_) | S3Error::Body(_) => {
            BridgeOutcomeClass::Transient(TransientReason::SdkError)
        }
        S3Error::Internal(_) => BridgeOutcomeClass::Fatal(FatalReason::Internal),
    }
}

// --------- SQS -----------

/// Classifier extension trait for [`crate::SqsCallOutcome`].
pub trait SqsOutcomeExt {
    /// Sorts the outcome into the bridge classifier vocabulary.
    fn classify(&self) -> BridgeOutcomeClass;
}

impl SqsOutcomeExt for CallOutcome<Result<SqsResponse, SqsError>> {
    fn classify(&self) -> BridgeOutcomeClass {
        match self {
            CallOutcome::Full => BridgeOutcomeClass::Transient(TransientReason::BridgeFull),
            CallOutcome::Closed => BridgeOutcomeClass::Transient(TransientReason::BridgeClosed),
            CallOutcome::Timeout => BridgeOutcomeClass::Transient(TransientReason::CallerTimeout),
            CallOutcome::Rejected(_) => BridgeOutcomeClass::Fatal(FatalReason::InvalidRequest),
            CallOutcome::Replied(Ok(_)) => BridgeOutcomeClass::Succeeded,
            CallOutcome::Replied(Err(e)) => classify_sqs_error(e),
        }
    }
}

fn classify_sqs_error(err: &SqsError) -> BridgeOutcomeClass {
    match err {
        SqsError::Full => BridgeOutcomeClass::Transient(TransientReason::BridgeFull),
        SqsError::Closed => BridgeOutcomeClass::Transient(TransientReason::BridgeClosed),
        SqsError::Timeout => BridgeOutcomeClass::Transient(TransientReason::BridgeTimeout),
        SqsError::MessageTooLarge | SqsError::ResponseTooLarge => {
            BridgeOutcomeClass::Fatal(FatalReason::TooLarge)
        }
        SqsError::InvalidRequest(_) => BridgeOutcomeClass::Fatal(FatalReason::InvalidRequest),
        SqsError::QueueDoesNotExist(_) => BridgeOutcomeClass::Fatal(FatalReason::NotFound),
        SqsError::Throttled(_) => BridgeOutcomeClass::Transient(TransientReason::ServiceThrottled),
        SqsError::Sdk(_) => BridgeOutcomeClass::Transient(TransientReason::SdkError),
        SqsError::Internal(_) => BridgeOutcomeClass::Fatal(FatalReason::Internal),
    }
}

// --------- DynamoDB -----------

/// Classifier extension trait for [`crate::DynamoCallOutcome`].
pub trait DynamoOutcomeExt {
    /// Sorts the outcome into the bridge classifier vocabulary.
    fn classify(&self) -> BridgeOutcomeClass;
}

impl DynamoOutcomeExt for CallOutcome<Result<DynamoResponse, DynamoError>> {
    fn classify(&self) -> BridgeOutcomeClass {
        match self {
            CallOutcome::Full => BridgeOutcomeClass::Transient(TransientReason::BridgeFull),
            CallOutcome::Closed => BridgeOutcomeClass::Transient(TransientReason::BridgeClosed),
            CallOutcome::Timeout => BridgeOutcomeClass::Transient(TransientReason::CallerTimeout),
            CallOutcome::Rejected(_) => BridgeOutcomeClass::Fatal(FatalReason::InvalidRequest),
            CallOutcome::Replied(Ok(_)) => BridgeOutcomeClass::Succeeded,
            CallOutcome::Replied(Err(e)) => classify_dynamo_error(e),
        }
    }
}

fn classify_dynamo_error(err: &DynamoError) -> BridgeOutcomeClass {
    match err {
        DynamoError::Full => BridgeOutcomeClass::Transient(TransientReason::BridgeFull),
        DynamoError::Closed => BridgeOutcomeClass::Transient(TransientReason::BridgeClosed),
        DynamoError::Timeout => BridgeOutcomeClass::Transient(TransientReason::BridgeTimeout),
        DynamoError::ItemTooLarge => BridgeOutcomeClass::Fatal(FatalReason::TooLarge),
        DynamoError::InvalidRequest(_) => BridgeOutcomeClass::Fatal(FatalReason::InvalidRequest),
        DynamoError::ConditionalCheckFailed(_) => {
            BridgeOutcomeClass::Fatal(FatalReason::ConditionalCheckFailed)
        }
        DynamoError::ProvisionedThroughputExceeded(_) => {
            BridgeOutcomeClass::Transient(TransientReason::ProvisionedThroughputExceeded)
        }
        DynamoError::ResourceNotFound(_) => BridgeOutcomeClass::Fatal(FatalReason::NotFound),
        DynamoError::Throttled(_) => {
            BridgeOutcomeClass::Transient(TransientReason::ServiceThrottled)
        }
        DynamoError::TransactionConflict(_) => {
            BridgeOutcomeClass::Transient(TransientReason::TransactionConflict)
        }
        DynamoError::Sdk(_) => BridgeOutcomeClass::Transient(TransientReason::SdkError),
        DynamoError::Internal(_) => BridgeOutcomeClass::Fatal(FatalReason::Internal),
    }
}

// --------- SNS -----------

/// Classifier extension trait for [`crate::SnsCallOutcome`].
pub trait SnsOutcomeExt {
    /// Sorts the outcome into the bridge classifier vocabulary.
    fn classify(&self) -> BridgeOutcomeClass;
}

impl SnsOutcomeExt for CallOutcome<Result<SnsResponse, SnsError>> {
    fn classify(&self) -> BridgeOutcomeClass {
        match self {
            CallOutcome::Full => BridgeOutcomeClass::Transient(TransientReason::BridgeFull),
            CallOutcome::Closed => BridgeOutcomeClass::Transient(TransientReason::BridgeClosed),
            CallOutcome::Timeout => BridgeOutcomeClass::Transient(TransientReason::CallerTimeout),
            CallOutcome::Rejected(_) => BridgeOutcomeClass::Fatal(FatalReason::InvalidRequest),
            CallOutcome::Replied(Ok(_)) => BridgeOutcomeClass::Succeeded,
            CallOutcome::Replied(Err(e)) => classify_sns_error(e),
        }
    }
}

fn classify_sns_error(err: &SnsError) -> BridgeOutcomeClass {
    match err {
        SnsError::Full => BridgeOutcomeClass::Transient(TransientReason::BridgeFull),
        SnsError::Closed => BridgeOutcomeClass::Transient(TransientReason::BridgeClosed),
        SnsError::Timeout => BridgeOutcomeClass::Transient(TransientReason::BridgeTimeout),
        SnsError::MessageTooLarge | SnsError::AttributesTooLarge => {
            BridgeOutcomeClass::Fatal(FatalReason::TooLarge)
        }
        SnsError::InvalidRequest(_) => BridgeOutcomeClass::Fatal(FatalReason::InvalidRequest),
        SnsError::InvalidParameter(_) => BridgeOutcomeClass::Fatal(FatalReason::InvalidParameter),
        SnsError::NotFound(_) => BridgeOutcomeClass::Fatal(FatalReason::NotFound),
        SnsError::AccessDenied(_) => BridgeOutcomeClass::Fatal(FatalReason::AccessDenied),
        SnsError::Throttled(_) => BridgeOutcomeClass::Transient(TransientReason::ServiceThrottled),
        SnsError::Sdk(_) => BridgeOutcomeClass::Transient(TransientReason::SdkError),
        SnsError::Internal(_) => BridgeOutcomeClass::Fatal(FatalReason::Internal),
    }
}

// --------- Secrets Manager -----------

/// Classifier extension trait for [`crate::SecretsCallOutcome`].
pub trait SecretsOutcomeExt {
    /// Sorts the outcome into the bridge classifier vocabulary.
    fn classify(&self) -> BridgeOutcomeClass;
}

impl SecretsOutcomeExt for CallOutcome<Result<SecretsResponse, SecretsError>> {
    fn classify(&self) -> BridgeOutcomeClass {
        match self {
            CallOutcome::Full => BridgeOutcomeClass::Transient(TransientReason::BridgeFull),
            CallOutcome::Closed => BridgeOutcomeClass::Transient(TransientReason::BridgeClosed),
            CallOutcome::Timeout => BridgeOutcomeClass::Transient(TransientReason::CallerTimeout),
            CallOutcome::Rejected(_) => BridgeOutcomeClass::Fatal(FatalReason::InvalidRequest),
            CallOutcome::Replied(Ok(_)) => BridgeOutcomeClass::Succeeded,
            CallOutcome::Replied(Err(e)) => classify_secrets_error(e),
        }
    }
}

fn classify_secrets_error(err: &SecretsError) -> BridgeOutcomeClass {
    match err {
        SecretsError::Full => BridgeOutcomeClass::Transient(TransientReason::BridgeFull),
        SecretsError::Closed => BridgeOutcomeClass::Transient(TransientReason::BridgeClosed),
        SecretsError::Timeout => BridgeOutcomeClass::Transient(TransientReason::BridgeTimeout),
        SecretsError::SecretTooLarge => BridgeOutcomeClass::Fatal(FatalReason::TooLarge),
        SecretsError::InvalidRequest(_) => BridgeOutcomeClass::Fatal(FatalReason::InvalidRequest),
        SecretsError::NotFound(_) => BridgeOutcomeClass::Fatal(FatalReason::NotFound),
        SecretsError::AccessDenied(_) => BridgeOutcomeClass::Fatal(FatalReason::AccessDenied),
        SecretsError::InvalidParameter(_) => {
            BridgeOutcomeClass::Fatal(FatalReason::InvalidParameter)
        }
        SecretsError::Throttled(_) => {
            BridgeOutcomeClass::Transient(TransientReason::ServiceThrottled)
        }
        SecretsError::DecryptionFailed(_) => {
            BridgeOutcomeClass::Fatal(FatalReason::DecryptionFailed)
        }
        SecretsError::Sdk(_) => BridgeOutcomeClass::Transient(TransientReason::SdkError),
        SecretsError::Internal(_) => BridgeOutcomeClass::Fatal(FatalReason::Internal),
    }
}
