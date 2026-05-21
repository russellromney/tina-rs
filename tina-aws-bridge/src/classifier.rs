//! AWS bridge outcome classifier.
//!
//! Each service exposes a `XxxOutcomeExt::classify(...)` that sorts
//! the two-layer truth Tina gives the caller — outer
//! [`CallOutcome::Full`] / `Closed` / `Timeout` (bridge delivery) plus
//! inner `Result<Response, Error>` (worker outcome) — into the shared
//! [`BridgeOutcomeClass`] vocabulary.
//!
//! Rules the classifier upholds:
//!
//! - `CallOutcome::Closed` is [`BridgeUnavailable::BridgeClosed`], not a
//!   retry hint. A closed admission means a new handle is needed.
//! - Unknown SDK errors land in [`BridgeFatal::SdkUnknown`]. The AWS
//!   SDK error wrapper does not carry retryable/throttled metadata at
//!   this layer; classifying it as retryable would be the exact retry
//!   fog the shared vocabulary is meant to prevent. Bridges that map
//!   typed AWS error variants to retryable (throttled, conditional
//!   conflict, provisioned-throughput) do so per-variant, not on the
//!   generic `Sdk(_)` wrapper.
//!
//! The classifier never retries, never backs off, and never guesses
//! idempotency.
//!
//! See [`tina_runtime::bridge`] for the full vocabulary doc.

use tina_runtime::CallOutcome;
use tina_runtime::bridge::{BridgeFatal, BridgeOutcomeClass, BridgeRetryable, BridgeUnavailable};

use crate::dynamodb_types::{DynamoError, DynamoResponse};
use crate::secrets_types::{SecretsError, SecretsResponse};
use crate::sns_types::{SnsError, SnsResponse};
use crate::sqs_types::{SqsError, SqsResponse};
use crate::types::{S3Error, S3Response};

// --------- S3 -----------

/// Classifier extension trait for [`crate::S3CallOutcome`].
pub trait S3OutcomeExt {
    /// Sorts the outcome into the bridge classifier vocabulary.
    fn classify(&self) -> BridgeOutcomeClass;
}

impl S3OutcomeExt for CallOutcome<Result<S3Response, S3Error>> {
    fn classify(&self) -> BridgeOutcomeClass {
        match self {
            CallOutcome::Full => BridgeOutcomeClass::Retryable(BridgeRetryable::BridgeFull),
            CallOutcome::Closed => BridgeOutcomeClass::Unavailable(BridgeUnavailable::BridgeClosed),
            CallOutcome::Timeout => BridgeOutcomeClass::Retryable(BridgeRetryable::CallerTimeout),
            CallOutcome::Rejected(_) => BridgeOutcomeClass::Fatal(BridgeFatal::InvalidRequest),
            CallOutcome::Replied(Ok(_)) => BridgeOutcomeClass::Succeeded,
            CallOutcome::Replied(Err(e)) => classify_s3_error(e),
        }
    }
}

fn classify_s3_error(err: &S3Error) -> BridgeOutcomeClass {
    match err {
        S3Error::Full => BridgeOutcomeClass::Retryable(BridgeRetryable::BridgeFull),
        S3Error::Closed => BridgeOutcomeClass::Unavailable(BridgeUnavailable::BridgeClosed),
        S3Error::Timeout => BridgeOutcomeClass::Retryable(BridgeRetryable::BridgeTimeout),
        S3Error::RequestTooLarge | S3Error::ResponseTooLarge => {
            BridgeOutcomeClass::Fatal(BridgeFatal::TooLarge)
        }
        S3Error::InvalidRequest(_) => BridgeOutcomeClass::Fatal(BridgeFatal::InvalidRequest),
        S3Error::NotFound(_) => BridgeOutcomeClass::Fatal(BridgeFatal::NotFound),
        S3Error::AccessDenied(_) => BridgeOutcomeClass::Fatal(BridgeFatal::AccessDenied),
        S3Error::Throttled(_) => BridgeOutcomeClass::Retryable(BridgeRetryable::ServiceThrottled),
        S3Error::Sdk(_) | S3Error::Body(_) => BridgeOutcomeClass::Fatal(BridgeFatal::SdkUnknown),
        S3Error::Internal(_) => BridgeOutcomeClass::Fatal(BridgeFatal::Internal),
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
            CallOutcome::Full => BridgeOutcomeClass::Retryable(BridgeRetryable::BridgeFull),
            CallOutcome::Closed => BridgeOutcomeClass::Unavailable(BridgeUnavailable::BridgeClosed),
            CallOutcome::Timeout => BridgeOutcomeClass::Retryable(BridgeRetryable::CallerTimeout),
            CallOutcome::Rejected(_) => BridgeOutcomeClass::Fatal(BridgeFatal::InvalidRequest),
            CallOutcome::Replied(Ok(_)) => BridgeOutcomeClass::Succeeded,
            CallOutcome::Replied(Err(e)) => classify_sqs_error(e),
        }
    }
}

fn classify_sqs_error(err: &SqsError) -> BridgeOutcomeClass {
    match err {
        SqsError::Full => BridgeOutcomeClass::Retryable(BridgeRetryable::BridgeFull),
        SqsError::Closed => BridgeOutcomeClass::Unavailable(BridgeUnavailable::BridgeClosed),
        SqsError::Timeout => BridgeOutcomeClass::Retryable(BridgeRetryable::BridgeTimeout),
        SqsError::MessageTooLarge | SqsError::ResponseTooLarge => {
            BridgeOutcomeClass::Fatal(BridgeFatal::TooLarge)
        }
        SqsError::InvalidRequest(_) => BridgeOutcomeClass::Fatal(BridgeFatal::InvalidRequest),
        SqsError::QueueDoesNotExist(_) => BridgeOutcomeClass::Fatal(BridgeFatal::NotFound),
        SqsError::Throttled(_) => BridgeOutcomeClass::Retryable(BridgeRetryable::ServiceThrottled),
        SqsError::Sdk(_) => BridgeOutcomeClass::Fatal(BridgeFatal::SdkUnknown),
        SqsError::Internal(_) => BridgeOutcomeClass::Fatal(BridgeFatal::Internal),
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
            CallOutcome::Full => BridgeOutcomeClass::Retryable(BridgeRetryable::BridgeFull),
            CallOutcome::Closed => BridgeOutcomeClass::Unavailable(BridgeUnavailable::BridgeClosed),
            CallOutcome::Timeout => BridgeOutcomeClass::Retryable(BridgeRetryable::CallerTimeout),
            CallOutcome::Rejected(_) => BridgeOutcomeClass::Fatal(BridgeFatal::InvalidRequest),
            CallOutcome::Replied(Ok(_)) => BridgeOutcomeClass::Succeeded,
            CallOutcome::Replied(Err(e)) => classify_dynamo_error(e),
        }
    }
}

fn classify_dynamo_error(err: &DynamoError) -> BridgeOutcomeClass {
    match err {
        DynamoError::Full => BridgeOutcomeClass::Retryable(BridgeRetryable::BridgeFull),
        DynamoError::Closed => BridgeOutcomeClass::Unavailable(BridgeUnavailable::BridgeClosed),
        DynamoError::Timeout => BridgeOutcomeClass::Retryable(BridgeRetryable::BridgeTimeout),
        DynamoError::ItemTooLarge => BridgeOutcomeClass::Fatal(BridgeFatal::TooLarge),
        DynamoError::InvalidRequest(_) => BridgeOutcomeClass::Fatal(BridgeFatal::InvalidRequest),
        DynamoError::ConditionalCheckFailed(_) => {
            BridgeOutcomeClass::Fatal(BridgeFatal::ConditionalCheckFailed)
        }
        DynamoError::ProvisionedThroughputExceeded(_) => {
            BridgeOutcomeClass::Retryable(BridgeRetryable::ProvisionedThroughputExceeded)
        }
        DynamoError::ResourceNotFound(_) => BridgeOutcomeClass::Fatal(BridgeFatal::NotFound),
        DynamoError::Throttled(_) => {
            BridgeOutcomeClass::Retryable(BridgeRetryable::ServiceThrottled)
        }
        DynamoError::TransactionConflict(_) => {
            BridgeOutcomeClass::Retryable(BridgeRetryable::TransactionConflict)
        }
        DynamoError::Sdk(_) => BridgeOutcomeClass::Fatal(BridgeFatal::SdkUnknown),
        DynamoError::Internal(_) => BridgeOutcomeClass::Fatal(BridgeFatal::Internal),
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
            CallOutcome::Full => BridgeOutcomeClass::Retryable(BridgeRetryable::BridgeFull),
            CallOutcome::Closed => BridgeOutcomeClass::Unavailable(BridgeUnavailable::BridgeClosed),
            CallOutcome::Timeout => BridgeOutcomeClass::Retryable(BridgeRetryable::CallerTimeout),
            CallOutcome::Rejected(_) => BridgeOutcomeClass::Fatal(BridgeFatal::InvalidRequest),
            CallOutcome::Replied(Ok(_)) => BridgeOutcomeClass::Succeeded,
            CallOutcome::Replied(Err(e)) => classify_sns_error(e),
        }
    }
}

fn classify_sns_error(err: &SnsError) -> BridgeOutcomeClass {
    match err {
        SnsError::Full => BridgeOutcomeClass::Retryable(BridgeRetryable::BridgeFull),
        SnsError::Closed => BridgeOutcomeClass::Unavailable(BridgeUnavailable::BridgeClosed),
        SnsError::Timeout => BridgeOutcomeClass::Retryable(BridgeRetryable::BridgeTimeout),
        SnsError::MessageTooLarge | SnsError::AttributesTooLarge => {
            BridgeOutcomeClass::Fatal(BridgeFatal::TooLarge)
        }
        SnsError::InvalidRequest(_) => BridgeOutcomeClass::Fatal(BridgeFatal::InvalidRequest),
        SnsError::InvalidParameter(_) => BridgeOutcomeClass::Fatal(BridgeFatal::InvalidParameter),
        SnsError::NotFound(_) => BridgeOutcomeClass::Fatal(BridgeFatal::NotFound),
        SnsError::AccessDenied(_) => BridgeOutcomeClass::Fatal(BridgeFatal::AccessDenied),
        SnsError::Throttled(_) => BridgeOutcomeClass::Retryable(BridgeRetryable::ServiceThrottled),
        SnsError::Sdk(_) => BridgeOutcomeClass::Fatal(BridgeFatal::SdkUnknown),
        SnsError::Internal(_) => BridgeOutcomeClass::Fatal(BridgeFatal::Internal),
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
            CallOutcome::Full => BridgeOutcomeClass::Retryable(BridgeRetryable::BridgeFull),
            CallOutcome::Closed => BridgeOutcomeClass::Unavailable(BridgeUnavailable::BridgeClosed),
            CallOutcome::Timeout => BridgeOutcomeClass::Retryable(BridgeRetryable::CallerTimeout),
            CallOutcome::Rejected(_) => BridgeOutcomeClass::Fatal(BridgeFatal::InvalidRequest),
            CallOutcome::Replied(Ok(_)) => BridgeOutcomeClass::Succeeded,
            CallOutcome::Replied(Err(e)) => classify_secrets_error(e),
        }
    }
}

fn classify_secrets_error(err: &SecretsError) -> BridgeOutcomeClass {
    match err {
        SecretsError::Full => BridgeOutcomeClass::Retryable(BridgeRetryable::BridgeFull),
        SecretsError::Closed => BridgeOutcomeClass::Unavailable(BridgeUnavailable::BridgeClosed),
        SecretsError::Timeout => BridgeOutcomeClass::Retryable(BridgeRetryable::BridgeTimeout),
        SecretsError::SecretTooLarge => BridgeOutcomeClass::Fatal(BridgeFatal::TooLarge),
        SecretsError::InvalidRequest(_) => BridgeOutcomeClass::Fatal(BridgeFatal::InvalidRequest),
        SecretsError::NotFound(_) => BridgeOutcomeClass::Fatal(BridgeFatal::NotFound),
        SecretsError::AccessDenied(_) => BridgeOutcomeClass::Fatal(BridgeFatal::AccessDenied),
        SecretsError::InvalidParameter(_) => {
            BridgeOutcomeClass::Fatal(BridgeFatal::InvalidParameter)
        }
        SecretsError::Throttled(_) => {
            BridgeOutcomeClass::Retryable(BridgeRetryable::ServiceThrottled)
        }
        SecretsError::DecryptionFailed(_) => {
            BridgeOutcomeClass::Fatal(BridgeFatal::DecryptionFailed)
        }
        SecretsError::Sdk(_) => BridgeOutcomeClass::Fatal(BridgeFatal::SdkUnknown),
        SecretsError::Internal(_) => BridgeOutcomeClass::Fatal(BridgeFatal::Internal),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqs_types::SqsSentMessage;
    use crate::types::S3PutObjectOk;

    #[test]
    fn s3_closed_classifies_as_unavailable_not_retryable() {
        let outcome: CallOutcome<Result<S3Response, S3Error>> = CallOutcome::Closed;
        assert!(matches!(
            outcome.classify(),
            BridgeOutcomeClass::Unavailable(BridgeUnavailable::BridgeClosed)
        ));
        let outcome: CallOutcome<Result<S3Response, S3Error>> =
            CallOutcome::Replied(Err(S3Error::Closed));
        assert!(matches!(
            outcome.classify(),
            BridgeOutcomeClass::Unavailable(BridgeUnavailable::BridgeClosed)
        ));
    }

    #[test]
    fn s3_unknown_sdk_error_classifies_as_fatal_not_retryable() {
        let outcome: CallOutcome<Result<S3Response, S3Error>> =
            CallOutcome::Replied(Err(S3Error::Sdk("eyebrow-raising 5xx".into())));
        assert!(matches!(
            outcome.classify(),
            BridgeOutcomeClass::Fatal(BridgeFatal::SdkUnknown)
        ));
    }

    #[test]
    fn s3_throttled_classifies_as_retryable_service_pressure() {
        let outcome: CallOutcome<Result<S3Response, S3Error>> =
            CallOutcome::Replied(Err(S3Error::Throttled("SlowDown".into())));
        assert_eq!(
            outcome.classify(),
            BridgeOutcomeClass::Retryable(BridgeRetryable::ServiceThrottled)
        );
    }

    #[test]
    fn s3_access_denied_and_not_found_are_fatal_typed_causes() {
        let denied: CallOutcome<Result<S3Response, S3Error>> =
            CallOutcome::Replied(Err(S3Error::AccessDenied("no".into())));
        assert_eq!(
            denied.classify(),
            BridgeOutcomeClass::Fatal(BridgeFatal::AccessDenied)
        );

        let not_found: CallOutcome<Result<S3Response, S3Error>> =
            CallOutcome::Replied(Err(S3Error::NotFound("missing".into())));
        assert_eq!(
            not_found.classify(),
            BridgeOutcomeClass::Fatal(BridgeFatal::NotFound)
        );
    }

    #[test]
    fn s3_succeeded_classifies_as_succeeded() {
        let outcome: CallOutcome<Result<S3Response, S3Error>> =
            CallOutcome::Replied(Ok(S3Response::PutObject(S3PutObjectOk {
                e_tag: None,
                version_id: None,
            })));
        assert!(outcome.classify().is_success());
    }

    #[test]
    fn s3_full_classifies_as_retryable_bridge_full() {
        let outcome: CallOutcome<Result<S3Response, S3Error>> = CallOutcome::Full;
        assert!(matches!(
            outcome.classify(),
            BridgeOutcomeClass::Retryable(BridgeRetryable::BridgeFull)
        ));
    }

    #[test]
    fn s3_timeout_classifies_as_caller_timeout() {
        let outcome: CallOutcome<Result<S3Response, S3Error>> = CallOutcome::Timeout;
        assert!(matches!(
            outcome.classify(),
            BridgeOutcomeClass::Retryable(BridgeRetryable::CallerTimeout)
        ));
        let outcome: CallOutcome<Result<S3Response, S3Error>> =
            CallOutcome::Replied(Err(S3Error::Timeout));
        assert!(matches!(
            outcome.classify(),
            BridgeOutcomeClass::Retryable(BridgeRetryable::BridgeTimeout)
        ));
    }

    #[test]
    fn sqs_throttled_classifies_as_retryable_service_throttled() {
        let outcome: CallOutcome<Result<SqsResponse, SqsError>> =
            CallOutcome::Replied(Err(SqsError::Throttled("RateExceeded".into())));
        assert!(matches!(
            outcome.classify(),
            BridgeOutcomeClass::Retryable(BridgeRetryable::ServiceThrottled)
        ));
    }

    #[test]
    fn sqs_unknown_sdk_classifies_as_fatal_sdk_unknown() {
        let outcome: CallOutcome<Result<SqsResponse, SqsError>> =
            CallOutcome::Replied(Err(SqsError::Sdk("unrecognized".into())));
        assert!(matches!(
            outcome.classify(),
            BridgeOutcomeClass::Fatal(BridgeFatal::SdkUnknown)
        ));
    }

    #[test]
    fn sqs_queue_does_not_exist_classifies_as_fatal_not_found() {
        let outcome: CallOutcome<Result<SqsResponse, SqsError>> =
            CallOutcome::Replied(Err(SqsError::QueueDoesNotExist("q".into())));
        assert!(matches!(
            outcome.classify(),
            BridgeOutcomeClass::Fatal(BridgeFatal::NotFound)
        ));
    }

    #[test]
    fn sqs_succeeded_classifies_as_succeeded() {
        let outcome: CallOutcome<Result<SqsResponse, SqsError>> =
            CallOutcome::Replied(Ok(SqsResponse::SentMessage(SqsSentMessage {
                message_id: Some("m".into()),
                md5_of_body: None,
                sequence_number: None,
            })));
        assert!(outcome.classify().is_success());
    }

    #[test]
    fn dynamo_conditional_failure_classifies_as_fatal() {
        let outcome: CallOutcome<Result<DynamoResponse, DynamoError>> =
            CallOutcome::Replied(Err(DynamoError::ConditionalCheckFailed("cond".into())));
        assert!(matches!(
            outcome.classify(),
            BridgeOutcomeClass::Fatal(BridgeFatal::ConditionalCheckFailed)
        ));
    }

    #[test]
    fn dynamo_throttled_classifies_as_retryable() {
        let outcome: CallOutcome<Result<DynamoResponse, DynamoError>> =
            CallOutcome::Replied(Err(DynamoError::Throttled("RateExceeded".into())));
        assert!(matches!(
            outcome.classify(),
            BridgeOutcomeClass::Retryable(BridgeRetryable::ServiceThrottled)
        ));
    }

    #[test]
    fn dynamo_provisioned_throughput_classifies_as_retryable() {
        let outcome: CallOutcome<Result<DynamoResponse, DynamoError>> =
            CallOutcome::Replied(Err(DynamoError::ProvisionedThroughputExceeded("x".into())));
        assert!(matches!(
            outcome.classify(),
            BridgeOutcomeClass::Retryable(BridgeRetryable::ProvisionedThroughputExceeded)
        ));
    }

    #[test]
    fn dynamo_unknown_sdk_classifies_as_fatal_sdk_unknown() {
        let outcome: CallOutcome<Result<DynamoResponse, DynamoError>> =
            CallOutcome::Replied(Err(DynamoError::Sdk("unrecognized".into())));
        assert!(matches!(
            outcome.classify(),
            BridgeOutcomeClass::Fatal(BridgeFatal::SdkUnknown)
        ));
    }

    #[test]
    fn sns_access_denied_classifies_as_fatal() {
        let outcome: CallOutcome<Result<SnsResponse, SnsError>> =
            CallOutcome::Replied(Err(SnsError::AccessDenied("no".into())));
        assert!(matches!(
            outcome.classify(),
            BridgeOutcomeClass::Fatal(BridgeFatal::AccessDenied)
        ));
    }

    #[test]
    fn sns_unknown_sdk_classifies_as_fatal_sdk_unknown() {
        let outcome: CallOutcome<Result<SnsResponse, SnsError>> =
            CallOutcome::Replied(Err(SnsError::Sdk("?".into())));
        assert!(matches!(
            outcome.classify(),
            BridgeOutcomeClass::Fatal(BridgeFatal::SdkUnknown)
        ));
    }

    #[test]
    fn secrets_decryption_failed_classifies_as_fatal() {
        let outcome: CallOutcome<Result<SecretsResponse, SecretsError>> =
            CallOutcome::Replied(Err(SecretsError::DecryptionFailed("kms".into())));
        assert!(matches!(
            outcome.classify(),
            BridgeOutcomeClass::Fatal(BridgeFatal::DecryptionFailed)
        ));
    }

    #[test]
    fn secrets_not_found_classifies_as_fatal() {
        let outcome: CallOutcome<Result<SecretsResponse, SecretsError>> =
            CallOutcome::Replied(Err(SecretsError::NotFound("s".into())));
        assert!(matches!(
            outcome.classify(),
            BridgeOutcomeClass::Fatal(BridgeFatal::NotFound)
        ));
    }

    // -----------------------------------------------------------------
    // Plan-required exhaustive classifier coverage:
    // - Succeeded                              (covered above per service)
    // - Retryable(ServiceThrottled)            (covered above per service)
    // - Retryable(BridgeTimeout)               (covered above per service)
    // - Unavailable(BridgeClosed)              (covered above per service)
    // - Fatal(AccessDenied)                    (covered above per service)
    // - Fatal(NotFound)                        (covered above per service)
    // - Fatal(InvalidRequest)                  (filled below)
    // - Fatal(Internal)                        (filled below)
    // -----------------------------------------------------------------

    #[test]
    fn s3_internal_classifies_as_fatal_internal() {
        let outcome: CallOutcome<Result<S3Response, S3Error>> =
            CallOutcome::Replied(Err(S3Error::Internal("worker bug".into())));
        assert!(matches!(
            outcome.classify(),
            BridgeOutcomeClass::Fatal(BridgeFatal::Internal)
        ));
    }

    #[test]
    fn sqs_internal_classifies_as_fatal_internal() {
        let outcome: CallOutcome<Result<SqsResponse, SqsError>> =
            CallOutcome::Replied(Err(SqsError::Internal("worker bug".into())));
        assert!(matches!(
            outcome.classify(),
            BridgeOutcomeClass::Fatal(BridgeFatal::Internal)
        ));
    }

    #[test]
    fn dynamo_internal_classifies_as_fatal_internal() {
        let outcome: CallOutcome<Result<DynamoResponse, DynamoError>> =
            CallOutcome::Replied(Err(DynamoError::Internal("worker bug".into())));
        assert!(matches!(
            outcome.classify(),
            BridgeOutcomeClass::Fatal(BridgeFatal::Internal)
        ));
    }

    #[test]
    fn sns_internal_classifies_as_fatal_internal() {
        let outcome: CallOutcome<Result<SnsResponse, SnsError>> =
            CallOutcome::Replied(Err(SnsError::Internal("worker bug".into())));
        assert!(matches!(
            outcome.classify(),
            BridgeOutcomeClass::Fatal(BridgeFatal::Internal)
        ));
    }

    #[test]
    fn secrets_internal_classifies_as_fatal_internal() {
        let outcome: CallOutcome<Result<SecretsResponse, SecretsError>> =
            CallOutcome::Replied(Err(SecretsError::Internal("worker bug".into())));
        assert!(matches!(
            outcome.classify(),
            BridgeOutcomeClass::Fatal(BridgeFatal::Internal)
        ));
    }

    #[test]
    fn s3_invalid_request_classifies_as_fatal_invalid_request() {
        let outcome: CallOutcome<Result<S3Response, S3Error>> =
            CallOutcome::Replied(Err(S3Error::InvalidRequest("empty bucket".into())));
        assert!(matches!(
            outcome.classify(),
            BridgeOutcomeClass::Fatal(BridgeFatal::InvalidRequest)
        ));
    }

    #[test]
    fn sqs_invalid_request_classifies_as_fatal_invalid_request() {
        let outcome: CallOutcome<Result<SqsResponse, SqsError>> =
            CallOutcome::Replied(Err(SqsError::InvalidRequest("bad queue url".into())));
        assert!(matches!(
            outcome.classify(),
            BridgeOutcomeClass::Fatal(BridgeFatal::InvalidRequest)
        ));
    }

    // -----------------------------------------------------------------
    // Cross-AWS-bridge consistency: the load-bearing claim of the
    // shared vocabulary is that every bridge agrees on the
    // categorization of the boring shapes. If S3 says `Closed` is
    // `Unavailable` but SQS says it's something else, dashboards drift.
    // -----------------------------------------------------------------

    #[test]
    fn every_aws_bridge_classifies_closed_as_unavailable_bridge_closed() {
        let expected = BridgeOutcomeClass::Unavailable(BridgeUnavailable::BridgeClosed);
        let s3: CallOutcome<Result<S3Response, S3Error>> = CallOutcome::Closed;
        let sqs: CallOutcome<Result<SqsResponse, SqsError>> = CallOutcome::Closed;
        let dynamo: CallOutcome<Result<DynamoResponse, DynamoError>> = CallOutcome::Closed;
        let sns: CallOutcome<Result<SnsResponse, SnsError>> = CallOutcome::Closed;
        let secrets: CallOutcome<Result<SecretsResponse, SecretsError>> = CallOutcome::Closed;
        assert_eq!(s3.classify(), expected);
        assert_eq!(sqs.classify(), expected);
        assert_eq!(dynamo.classify(), expected);
        assert_eq!(sns.classify(), expected);
        assert_eq!(secrets.classify(), expected);
    }

    #[test]
    fn every_aws_bridge_classifies_full_as_retryable_bridge_full() {
        let expected = BridgeOutcomeClass::Retryable(BridgeRetryable::BridgeFull);
        let s3: CallOutcome<Result<S3Response, S3Error>> = CallOutcome::Full;
        let sqs: CallOutcome<Result<SqsResponse, SqsError>> = CallOutcome::Full;
        let dynamo: CallOutcome<Result<DynamoResponse, DynamoError>> = CallOutcome::Full;
        let sns: CallOutcome<Result<SnsResponse, SnsError>> = CallOutcome::Full;
        let secrets: CallOutcome<Result<SecretsResponse, SecretsError>> = CallOutcome::Full;
        assert_eq!(s3.classify(), expected);
        assert_eq!(sqs.classify(), expected);
        assert_eq!(dynamo.classify(), expected);
        assert_eq!(sns.classify(), expected);
        assert_eq!(secrets.classify(), expected);
    }

    #[test]
    fn every_aws_bridge_classifies_timeout_as_retryable_caller_timeout() {
        let expected = BridgeOutcomeClass::Retryable(BridgeRetryable::CallerTimeout);
        let s3: CallOutcome<Result<S3Response, S3Error>> = CallOutcome::Timeout;
        let sqs: CallOutcome<Result<SqsResponse, SqsError>> = CallOutcome::Timeout;
        let dynamo: CallOutcome<Result<DynamoResponse, DynamoError>> = CallOutcome::Timeout;
        let sns: CallOutcome<Result<SnsResponse, SnsError>> = CallOutcome::Timeout;
        let secrets: CallOutcome<Result<SecretsResponse, SecretsError>> = CallOutcome::Timeout;
        assert_eq!(s3.classify(), expected);
        assert_eq!(sqs.classify(), expected);
        assert_eq!(dynamo.classify(), expected);
        assert_eq!(sns.classify(), expected);
        assert_eq!(secrets.classify(), expected);
    }
}
