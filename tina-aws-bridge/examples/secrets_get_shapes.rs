//! Secrets Manager request shapes without using a real AWS account.

use std::time::Duration;

use tina_aws_bridge::{SecretsConfig, SecretsCredentials, SecretsGetSecretValue, SecretsRequest};

fn main() {
    let _config = SecretsConfig::new()
        .with_region("us-east-1")
        .with_endpoint_url("http://localhost:4566")
        .with_credentials(SecretsCredentials::new("dummy-access", "dummy-secret"))
        .with_secret_size_limit(64 * 1024)
        .with_default_timeout(Duration::from_secs(2));

    let _current = SecretsRequest::GetSecretValue(SecretsGetSecretValue {
        secret_id: "prod/db/password".into(),
        version_id: None,
        version_stage: None,
    });

    let _pinned_version = SecretsRequest::GetSecretValue(SecretsGetSecretValue {
        secret_id: "prod/db/password".into(),
        version_id: Some("a1b2c3d4".into()),
        version_stage: None,
    });

    let _staging = SecretsRequest::GetSecretValue(SecretsGetSecretValue {
        secret_id: "prod/db/password".into(),
        version_id: None,
        version_stage: Some("AWSPREVIOUS".into()),
    });
}
