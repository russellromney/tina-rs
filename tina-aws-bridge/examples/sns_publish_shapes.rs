//! SNS request shapes without using a real AWS account.

use std::collections::HashMap;
use std::time::Duration;

use tina_aws_bridge::{
    SnsAttributeValue, SnsConfig, SnsCredentials, SnsDestination, SnsPublish, SnsRequest,
};

fn main() {
    let _config = SnsConfig::new()
        .with_region("us-east-1")
        .with_endpoint_url("http://localhost:4566")
        .with_credentials(SnsCredentials::new("dummy-access", "dummy-secret"))
        .with_message_body_limit(256 * 1024)
        .with_default_timeout(Duration::from_secs(2));

    let mut attributes: HashMap<String, SnsAttributeValue> = HashMap::new();
    attributes.insert(
        "event-type".into(),
        SnsAttributeValue::String("order.placed".into()),
    );

    let _topic_publish = SnsRequest::Publish(SnsPublish {
        destination: SnsDestination::TopicArn(
            "arn:aws:sns:us-east-1:000000000000:order-events".into(),
        ),
        message: r#"{"order_id": 1234, "status": "placed"}"#.into(),
        subject: None,
        message_group_id: None,
        message_deduplication_id: None,
        attributes: attributes.clone(),
    });

    let _sms_publish = SnsRequest::Publish(SnsPublish {
        destination: SnsDestination::PhoneNumber("+15555550100".into()),
        message: "Your order has shipped.".into(),
        subject: None,
        message_group_id: None,
        message_deduplication_id: None,
        attributes: HashMap::new(),
    });
}
