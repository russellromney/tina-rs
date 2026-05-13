//! Small helper aliases.

use std::time::Duration;

use tina::Address;
use tina_runtime::{CallOutcome, IsolateCall, call};

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
