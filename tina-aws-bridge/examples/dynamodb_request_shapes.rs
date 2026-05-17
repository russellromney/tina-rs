//! DynamoDB request shapes without using a real AWS account.
//!
//! Pair with DynamoDB Local or LocalStack via
//! `DynamoConfig::with_endpoint_url`. Real applications should either
//! set explicit static credentials or wrap a caller-built
//! `aws_sdk_dynamodb::Client` for the default provider chain.

use std::collections::HashMap;
use std::time::Duration;

use tina_aws_bridge::{
    DynamoConfig, DynamoConsistency, DynamoCredentials, DynamoDeleteItem, DynamoGetItem,
    DynamoPutItem, DynamoQuery, DynamoRequest, DynamoUpdateItem, DynamoValue, ReturnCapacity,
};

fn main() {
    // Install path (bridge owns the runtime and SDK client). The
    // production app would also pass this to `install_dynamodb`.
    let _config = DynamoConfig::new()
        .with_region("us-east-1")
        .with_endpoint_url("http://localhost:8000")
        .with_credentials(DynamoCredentials::new("dummy-access", "dummy-secret"))
        .with_max_in_flight(8)
        .with_item_size_limit(64 * 1024)
        .with_query_item_limit(50)
        .with_default_timeout(Duration::from_secs(2))
        .with_return_capacity(ReturnCapacity::Total);

    let key: HashMap<String, DynamoValue> =
        std::iter::once(("pk".to_string(), DynamoValue::S("tenant#42".into()))).collect();

    let _get = DynamoRequest::GetItem(DynamoGetItem {
        table_name: "kv".into(),
        key: key.clone(),
        consistency: DynamoConsistency::Strong,
    });

    let mut item = key.clone();
    item.insert("v".into(), DynamoValue::N("1".into()));
    let _put = DynamoRequest::PutItem(DynamoPutItem {
        table_name: "kv".into(),
        item,
        condition_expression: Some("attribute_not_exists(pk)".into()),
        expression_attribute_names: HashMap::new(),
        expression_attribute_values: HashMap::new(),
    });

    let mut values = HashMap::new();
    values.insert(":delta".to_string(), DynamoValue::N("1".into()));
    let _update = DynamoRequest::UpdateItem(DynamoUpdateItem {
        table_name: "kv".into(),
        key: key.clone(),
        update_expression: "ADD v :delta".into(),
        condition_expression: None,
        expression_attribute_names: HashMap::new(),
        expression_attribute_values: values,
    });

    let _delete = DynamoRequest::DeleteItem(DynamoDeleteItem {
        table_name: "kv".into(),
        key: key.clone(),
        condition_expression: None,
        expression_attribute_names: HashMap::new(),
        expression_attribute_values: HashMap::new(),
    });

    let mut query_values = HashMap::new();
    query_values.insert(":p".to_string(), DynamoValue::S("tenant#42".into()));
    let _query = DynamoRequest::Query(DynamoQuery {
        table_name: "kv".into(),
        index_name: None,
        key_condition_expression: "pk = :p".into(),
        filter_expression: None,
        expression_attribute_names: HashMap::new(),
        expression_attribute_values: query_values,
        limit: Some(25),
        consistency: DynamoConsistency::Eventual,
        exclusive_start_key: None,
        scan_index_forward: true,
    });
}
