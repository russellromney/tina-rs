//! Small helper aliases and `call(...)` shortcuts per service.

use std::time::Duration;

use tina::Address;
use tina_runtime::{CallOutcome, IsolateCall, call};

use crate::dynamodb_types::{DynamoError, DynamoRequest, DynamoResponse};
use crate::dynamodb_worker::DynamoMsg;
use crate::secrets_types::{SecretsError, SecretsRequest, SecretsResponse};
use crate::secrets_worker::SecretsMsg;
use crate::sns_types::{SnsError, SnsRequest, SnsResponse};
use crate::sns_worker::SnsMsg;
use crate::sqs_types::{SqsError, SqsRequest, SqsResponse};
use crate::sqs_worker::SqsMsg;
use crate::types::{S3Error, S3Request, S3Response};
use crate::worker::S3Msg;

/// Tina address for the S3 worker.
pub type S3Address = Address<S3Msg, Result<S3Response, S3Error>>;

/// Default call outcome shape.
pub type S3CallOutcome = CallOutcome<Result<S3Response, S3Error>>;

/// Build a Tina call effect for one S3 bridge request.
pub fn send_s3(
    address: S3Address,
    request: S3Request,
    timeout: Duration,
) -> IsolateCall<S3Msg, Result<S3Response, S3Error>> {
    call(address, S3Msg::Send(request), timeout)
}

/// Tina address for the SQS worker.
pub type SqsAddress = Address<SqsMsg, Result<SqsResponse, SqsError>>;

/// Default SQS call outcome shape.
pub type SqsCallOutcome = CallOutcome<Result<SqsResponse, SqsError>>;

/// Build a Tina call effect for one SQS bridge request.
pub fn send_sqs(
    address: SqsAddress,
    request: SqsRequest,
    timeout: Duration,
) -> IsolateCall<SqsMsg, Result<SqsResponse, SqsError>> {
    call(address, SqsMsg::Send(request), timeout)
}

/// Tina address for the DynamoDB worker.
pub type DynamoAddress = Address<DynamoMsg, Result<DynamoResponse, DynamoError>>;

/// Default DynamoDB call outcome shape.
pub type DynamoCallOutcome = CallOutcome<Result<DynamoResponse, DynamoError>>;

/// Build a Tina call effect for one DynamoDB bridge request.
pub fn send_dynamodb(
    address: DynamoAddress,
    request: DynamoRequest,
    timeout: Duration,
) -> IsolateCall<DynamoMsg, Result<DynamoResponse, DynamoError>> {
    call(address, DynamoMsg::Send(request), timeout)
}

/// Tina address for the SNS worker.
pub type SnsAddress = Address<SnsMsg, Result<SnsResponse, SnsError>>;

/// Default SNS call outcome shape.
pub type SnsCallOutcome = CallOutcome<Result<SnsResponse, SnsError>>;

/// Build a Tina call effect for one SNS bridge request.
pub fn send_sns(
    address: SnsAddress,
    request: SnsRequest,
    timeout: Duration,
) -> IsolateCall<SnsMsg, Result<SnsResponse, SnsError>> {
    call(address, SnsMsg::Send(request), timeout)
}

/// Tina address for the Secrets Manager worker.
pub type SecretsAddress = Address<SecretsMsg, Result<SecretsResponse, SecretsError>>;

/// Default Secrets Manager call outcome shape.
pub type SecretsCallOutcome = CallOutcome<Result<SecretsResponse, SecretsError>>;

/// Build a Tina call effect for one Secrets Manager bridge request.
pub fn send_secrets(
    address: SecretsAddress,
    request: SecretsRequest,
    timeout: Duration,
) -> IsolateCall<SecretsMsg, Result<SecretsResponse, SecretsError>> {
    call(address, SecretsMsg::Send(request), timeout)
}
