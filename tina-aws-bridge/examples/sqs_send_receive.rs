//! SQS request shapes without using real AWS credentials.
//!
//! The integration tests pair these requests with a localhost fake SQS
//! endpoint. Real applications should set explicit credentials or wrap
//! a caller-built `aws_sdk_sqs::Client`.

use std::time::Duration;

use tina_aws_bridge::{
    SqsConfig, SqsCredentials, SqsDeleteMessage, SqsReceiveMessage, SqsRequest, SqsSendMessage,
};

fn main() {
    let _config = SqsConfig::new()
        .with_region("us-east-1")
        .with_credentials(SqsCredentials::new("dummy-access", "dummy-secret"))
        .with_message_body_limit(64 * 1024)
        .with_max_receive_messages(10)
        .with_default_timeout(Duration::from_secs(2));

    let queue_url = "http://localhost:4566/000000000000/example".to_string();

    let _send = SqsRequest::SendMessage(SqsSendMessage {
        queue_url: queue_url.clone(),
        body: "hello from Tina".into(),
        message_group_id: None,
        message_deduplication_id: None,
    });

    let _receive = SqsRequest::ReceiveMessage(SqsReceiveMessage {
        queue_url: queue_url.clone(),
        max_messages: 5,
        visibility_timeout_seconds: 30,
        wait_time_seconds: 0,
    });

    let _delete = SqsRequest::DeleteMessage(SqsDeleteMessage {
        queue_url,
        receipt_handle: "receipt-handle-from-receive".into(),
    });
}
