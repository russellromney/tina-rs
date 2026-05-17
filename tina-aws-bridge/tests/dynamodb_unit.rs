//! Unit-shaped tests for the DynamoDB bridge that do not require a
//! fake AWS endpoint — config validation, classifier shape, and
//! admission rejection paths.

use std::time::Duration;

use tina_aws_bridge::{
    BridgeOutcomeClass, DynamoConfig, DynamoConfigError, DynamoCredentials, DynamoError,
    DynamoOutcomeExt, DynamoRequest, DynamoResponse, DynamoValue, ReturnCapacity, SdkRetryPolicy,
};
use tina_runtime::CallOutcome;

#[test]
fn config_validation_rejects_zero_and_invalid_fields() {
    let cfg = DynamoConfig::new().with_mailbox_capacity(0);
    assert_eq!(cfg.validate(), Err(DynamoConfigError::ZeroMailboxCapacity));

    let cfg = DynamoConfig::new().with_max_in_flight(0);
    assert_eq!(cfg.validate(), Err(DynamoConfigError::ZeroMaxInFlight));

    let cfg = DynamoConfig::new().with_item_size_limit(0);
    assert_eq!(cfg.validate(), Err(DynamoConfigError::ZeroItemSizeLimit));

    let cfg = DynamoConfig::new().with_query_item_limit(0);
    assert_eq!(cfg.validate(), Err(DynamoConfigError::ZeroQueryItemLimit));

    let cfg = DynamoConfig::new().with_default_timeout(Duration::from_secs(0));
    assert_eq!(cfg.validate(), Err(DynamoConfigError::ZeroDefaultTimeout));

    let cfg = DynamoConfig::new().with_poll_interval(Duration::from_secs(0));
    assert_eq!(cfg.validate(), Err(DynamoConfigError::ZeroPollInterval));

    let cfg = DynamoConfig::new().with_region("");
    assert_eq!(cfg.validate(), Err(DynamoConfigError::EmptyRegion));

    let cfg = DynamoConfig::new().with_credentials(DynamoCredentials::new("", "secret"));
    assert_eq!(cfg.validate(), Err(DynamoConfigError::EmptyCredentials));

    let cfg = DynamoConfig::new().with_retry_policy(SdkRetryPolicy::Standard { max_attempts: 0 });
    assert_eq!(cfg.validate(), Err(DynamoConfigError::ZeroRetryAttempts));
}

#[test]
fn supplied_client_path_skips_region_and_credentials_validation() {
    // Bridge-fields-only validation still rejects bad bridge config,
    // but lets region/credentials be empty since the supplied client
    // owns them.
    let cfg = DynamoConfig::new()
        .with_region("")
        .with_credentials(DynamoCredentials::new("", ""));
    assert!(cfg.validate_bridge_fields().is_ok());

    let cfg = DynamoConfig::new()
        .with_max_in_flight(0)
        .with_region("us-east-1");
    assert_eq!(
        cfg.validate_bridge_fields(),
        Err(DynamoConfigError::ZeroMaxInFlight)
    );
}

#[test]
fn return_capacity_modes_are_independent_of_other_config() {
    let cfg = DynamoConfig::new().with_return_capacity(ReturnCapacity::Total);
    assert_eq!(cfg.return_capacity, ReturnCapacity::Total);
    assert!(cfg.validate().is_ok());

    let cfg = DynamoConfig::new().with_return_capacity(ReturnCapacity::Indexes);
    assert_eq!(cfg.return_capacity, ReturnCapacity::Indexes);
    assert!(cfg.validate().is_ok());

    let cfg = DynamoConfig::new();
    assert_eq!(cfg.return_capacity, ReturnCapacity::None);
}

#[test]
fn request_kind_names_are_stable() {
    use std::collections::HashMap;
    let key: HashMap<String, DynamoValue> =
        std::iter::once(("pk".to_string(), DynamoValue::S("k".into()))).collect();
    assert_eq!(
        DynamoRequest::GetItem(tina_aws_bridge::DynamoGetItem {
            table_name: "t".into(),
            key: key.clone(),
            consistency: tina_aws_bridge::DynamoConsistency::Eventual,
        })
        .kind(),
        "dynamodb_get_item"
    );
    assert_eq!(
        DynamoRequest::PutItem(tina_aws_bridge::DynamoPutItem {
            table_name: "t".into(),
            item: key.clone(),
            condition_expression: None,
            expression_attribute_names: HashMap::new(),
            expression_attribute_values: HashMap::new(),
        })
        .kind(),
        "dynamodb_put_item"
    );
}

#[test]
fn classifier_sorts_outcomes_into_bridge_vocabulary() {
    use tina_aws_bridge::{FatalReason, TransientReason};

    let timeout: CallOutcome<Result<DynamoResponse, DynamoError>> = CallOutcome::Timeout;
    assert_eq!(
        timeout.classify(),
        BridgeOutcomeClass::Transient(TransientReason::CallerTimeout)
    );

    let full: CallOutcome<Result<DynamoResponse, DynamoError>> = CallOutcome::Full;
    assert_eq!(
        full.classify(),
        BridgeOutcomeClass::Transient(TransientReason::BridgeFull)
    );

    let closed: CallOutcome<Result<DynamoResponse, DynamoError>> = CallOutcome::Closed;
    assert_eq!(
        closed.classify(),
        BridgeOutcomeClass::Transient(TransientReason::BridgeClosed)
    );

    let bridge_timeout: CallOutcome<Result<DynamoResponse, DynamoError>> =
        CallOutcome::Replied(Err(DynamoError::Timeout));
    assert_eq!(
        bridge_timeout.classify(),
        BridgeOutcomeClass::Transient(TransientReason::BridgeTimeout)
    );

    let ccf: CallOutcome<Result<DynamoResponse, DynamoError>> =
        CallOutcome::Replied(Err(DynamoError::ConditionalCheckFailed("test".into())));
    assert_eq!(
        ccf.classify(),
        BridgeOutcomeClass::Fatal(FatalReason::ConditionalCheckFailed)
    );

    let throughput: CallOutcome<Result<DynamoResponse, DynamoError>> = CallOutcome::Replied(Err(
        DynamoError::ProvisionedThroughputExceeded("test".into()),
    ));
    assert_eq!(
        throughput.classify(),
        BridgeOutcomeClass::Transient(TransientReason::ProvisionedThroughputExceeded)
    );

    let conflict: CallOutcome<Result<DynamoResponse, DynamoError>> =
        CallOutcome::Replied(Err(DynamoError::TransactionConflict("test".into())));
    assert_eq!(
        conflict.classify(),
        BridgeOutcomeClass::Transient(TransientReason::TransactionConflict)
    );

    let too_large: CallOutcome<Result<DynamoResponse, DynamoError>> =
        CallOutcome::Replied(Err(DynamoError::ItemTooLarge));
    assert_eq!(
        too_large.classify(),
        BridgeOutcomeClass::Fatal(FatalReason::TooLarge)
    );

    let invalid: CallOutcome<Result<DynamoResponse, DynamoError>> =
        CallOutcome::Replied(Err(DynamoError::InvalidRequest("bad".into())));
    assert_eq!(
        invalid.classify(),
        BridgeOutcomeClass::Fatal(FatalReason::InvalidRequest)
    );

    let not_found: CallOutcome<Result<DynamoResponse, DynamoError>> =
        CallOutcome::Replied(Err(DynamoError::ResourceNotFound("test".into())));
    assert_eq!(
        not_found.classify(),
        BridgeOutcomeClass::Fatal(FatalReason::NotFound)
    );

    let sdk: CallOutcome<Result<DynamoResponse, DynamoError>> =
        CallOutcome::Replied(Err(DynamoError::Sdk("boom".into())));
    assert_eq!(
        sdk.classify(),
        BridgeOutcomeClass::Transient(TransientReason::SdkError)
    );
}
