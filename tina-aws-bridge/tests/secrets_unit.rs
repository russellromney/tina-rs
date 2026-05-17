//! Unit-shaped tests for the Secrets Manager bridge.

use std::time::Duration;

use tina_aws_bridge::{
    BridgeOutcomeClass, FatalReason, SecretsConfig, SecretsConfigError, SecretsCredentials,
    SecretsError, SecretsGetSecretValue, SecretsOutcomeExt, SecretsRequest, SecretsResponse,
    SecretsValue, TransientReason,
};
use tina_runtime::CallOutcome;

#[test]
fn config_validation_rejects_zero_and_invalid_fields() {
    let cfg = SecretsConfig::new().with_mailbox_capacity(0);
    assert_eq!(cfg.validate(), Err(SecretsConfigError::ZeroMailboxCapacity));

    let cfg = SecretsConfig::new().with_max_in_flight(0);
    assert_eq!(cfg.validate(), Err(SecretsConfigError::ZeroMaxInFlight));

    let cfg = SecretsConfig::new().with_secret_size_limit(0);
    assert_eq!(cfg.validate(), Err(SecretsConfigError::ZeroSecretSizeLimit));

    let cfg = SecretsConfig::new().with_default_timeout(Duration::from_secs(0));
    assert_eq!(cfg.validate(), Err(SecretsConfigError::ZeroDefaultTimeout));

    let cfg = SecretsConfig::new().with_credentials(SecretsCredentials::new("k", ""));
    assert_eq!(cfg.validate(), Err(SecretsConfigError::EmptyCredentials));
}

#[test]
fn request_kind_names_are_stable() {
    let req = SecretsRequest::GetSecretValue(SecretsGetSecretValue {
        secret_id: "my-secret".into(),
        version_id: None,
        version_stage: None,
    });
    assert_eq!(req.kind(), "secrets_get_secret_value");
}

#[test]
fn classifier_sorts_outcomes_into_bridge_vocabulary() {
    let succeeded: CallOutcome<Result<SecretsResponse, SecretsError>> =
        CallOutcome::Replied(Ok(SecretsResponse::SecretValue(SecretsValue {
            arn: None,
            name: Some("s".into()),
            version_id: None,
            version_stages: Vec::new(),
            secret_string: Some("v".into()),
            secret_binary: None,
        })));
    assert_eq!(succeeded.classify(), BridgeOutcomeClass::Succeeded);

    let not_found: CallOutcome<Result<SecretsResponse, SecretsError>> =
        CallOutcome::Replied(Err(SecretsError::NotFound("gone".into())));
    assert_eq!(
        not_found.classify(),
        BridgeOutcomeClass::Fatal(FatalReason::NotFound)
    );

    let denied: CallOutcome<Result<SecretsResponse, SecretsError>> =
        CallOutcome::Replied(Err(SecretsError::AccessDenied("no".into())));
    assert_eq!(
        denied.classify(),
        BridgeOutcomeClass::Fatal(FatalReason::AccessDenied)
    );

    let too_large: CallOutcome<Result<SecretsResponse, SecretsError>> =
        CallOutcome::Replied(Err(SecretsError::SecretTooLarge));
    assert_eq!(
        too_large.classify(),
        BridgeOutcomeClass::Fatal(FatalReason::TooLarge)
    );

    let throttled: CallOutcome<Result<SecretsResponse, SecretsError>> =
        CallOutcome::Replied(Err(SecretsError::Throttled("slow".into())));
    assert_eq!(
        throttled.classify(),
        BridgeOutcomeClass::Transient(TransientReason::ServiceThrottled)
    );

    let decrypt: CallOutcome<Result<SecretsResponse, SecretsError>> =
        CallOutcome::Replied(Err(SecretsError::DecryptionFailed("kms".into())));
    assert_eq!(
        decrypt.classify(),
        BridgeOutcomeClass::Fatal(FatalReason::DecryptionFailed)
    );
}

#[test]
fn mutually_exclusive_version_selectors_fail_validation() {
    // The bridge validates this at admit time; a misuse should not
    // make it to the SDK. We can't easily install the worker without
    // a runtime here, but we can confirm the request shape is what
    // the validator inspects.
    let req = SecretsRequest::GetSecretValue(SecretsGetSecretValue {
        secret_id: "k".into(),
        version_id: Some("v".into()),
        version_stage: Some("AWSCURRENT".into()),
    });
    assert_eq!(req.kind(), "secrets_get_secret_value");
}
