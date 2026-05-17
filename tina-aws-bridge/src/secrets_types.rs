//! Public Secrets Manager request, response, error, and config types.

use std::time::Duration;

use crate::types::{MAX_MAILBOX_CAPACITY, SdkRetryPolicy};

/// Static Secrets Manager credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretsCredentials {
    /// Access key id.
    pub access_key_id: String,
    /// Secret access key.
    pub secret_access_key: String,
    /// Optional session token.
    pub session_token: Option<String>,
}

impl SecretsCredentials {
    /// Builds static credentials.
    pub fn new(access_key_id: impl Into<String>, secret_access_key: impl Into<String>) -> Self {
        Self {
            access_key_id: access_key_id.into(),
            secret_access_key: secret_access_key.into(),
            session_token: None,
        }
    }

    /// Adds a session token.
    pub fn with_session_token(mut self, token: impl Into<String>) -> Self {
        self.session_token = Some(token.into());
        self
    }
}

/// Bridge configuration for Secrets Manager.
#[derive(Debug, Clone)]
pub struct SecretsConfig {
    /// Worker mailbox capacity.
    pub mailbox_capacity: usize,
    /// Maximum admitted SDK futures running at once.
    pub max_in_flight: usize,
    /// Maximum returned secret size in bytes. AWS itself caps secret
    /// material at 64KB; the bridge cap fires first if smaller.
    pub secret_size_limit: usize,
    /// Bridge per-operation deadline.
    pub default_timeout: Duration,
    /// Poll interval.
    pub poll_interval: Duration,
    /// AWS region.
    pub region: String,
    /// Optional endpoint URL.
    pub endpoint_url: Option<String>,
    /// Static credentials.
    pub credentials: Option<SecretsCredentials>,
    /// Explicit SDK retry policy.
    pub retry_policy: SdkRetryPolicy,
}

impl Default for SecretsConfig {
    fn default() -> Self {
        Self {
            mailbox_capacity: 32,
            max_in_flight: 8,
            secret_size_limit: 64 << 10,
            default_timeout: Duration::from_secs(10),
            poll_interval: Duration::from_millis(2),
            region: "us-east-1".into(),
            endpoint_url: None,
            credentials: None,
            retry_policy: SdkRetryPolicy::Disabled,
        }
    }
}

impl SecretsConfig {
    /// Returns default config.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets `mailbox_capacity`.
    pub fn with_mailbox_capacity(mut self, capacity: usize) -> Self {
        self.mailbox_capacity = capacity;
        self
    }

    /// Sets `max_in_flight`.
    pub fn with_max_in_flight(mut self, max: usize) -> Self {
        self.max_in_flight = max;
        self
    }

    /// Sets `secret_size_limit`.
    pub fn with_secret_size_limit(mut self, limit: usize) -> Self {
        self.secret_size_limit = limit;
        self
    }

    /// Sets `default_timeout`.
    pub fn with_default_timeout(mut self, timeout: Duration) -> Self {
        self.default_timeout = timeout;
        self
    }

    /// Sets `poll_interval`.
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Sets the AWS region.
    pub fn with_region(mut self, region: impl Into<String>) -> Self {
        self.region = region.into();
        self
    }

    /// Sets a custom endpoint URL.
    pub fn with_endpoint_url(mut self, endpoint_url: impl Into<String>) -> Self {
        self.endpoint_url = Some(endpoint_url.into());
        self
    }

    /// Sets static credentials.
    pub fn with_credentials(mut self, credentials: SecretsCredentials) -> Self {
        self.credentials = Some(credentials);
        self
    }

    /// Clears static credentials.
    pub fn without_credentials(mut self) -> Self {
        self.credentials = None;
        self
    }

    /// Sets SDK retry policy.
    pub fn with_retry_policy(mut self, policy: SdkRetryPolicy) -> Self {
        self.retry_policy = policy;
        self
    }

    /// Validates config.
    pub fn validate(&self) -> Result<(), SecretsConfigError> {
        self.validate_bridge_fields()?;
        if self.region.trim().is_empty() {
            return Err(SecretsConfigError::EmptyRegion);
        }
        if let Some(credentials) = &self.credentials
            && (credentials.access_key_id.is_empty() || credentials.secret_access_key.is_empty())
        {
            return Err(SecretsConfigError::EmptyCredentials);
        }
        if let SdkRetryPolicy::Standard { max_attempts: 0 } = self.retry_policy {
            return Err(SecretsConfigError::ZeroRetryAttempts);
        }
        Ok(())
    }

    /// Validates only Tina-owned bridge fields.
    pub fn validate_bridge_fields(&self) -> Result<(), SecretsConfigError> {
        if self.mailbox_capacity == 0 {
            return Err(SecretsConfigError::ZeroMailboxCapacity);
        }
        if self.mailbox_capacity > MAX_MAILBOX_CAPACITY {
            return Err(SecretsConfigError::MailboxCapacityTooLarge {
                requested: self.mailbox_capacity,
                cap: MAX_MAILBOX_CAPACITY,
            });
        }
        if self.max_in_flight == 0 {
            return Err(SecretsConfigError::ZeroMaxInFlight);
        }
        if self.secret_size_limit == 0 {
            return Err(SecretsConfigError::ZeroSecretSizeLimit);
        }
        if self.default_timeout.is_zero() {
            return Err(SecretsConfigError::ZeroDefaultTimeout);
        }
        if self.poll_interval.is_zero() {
            return Err(SecretsConfigError::ZeroPollInterval);
        }
        Ok(())
    }
}

/// Reasons a [`SecretsConfig`] is invalid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretsConfigError {
    /// `mailbox_capacity` is zero.
    ZeroMailboxCapacity,
    /// `mailbox_capacity` exceeds the crate's hard cap.
    MailboxCapacityTooLarge {
        /// Requested capacity.
        requested: usize,
        /// Maximum permitted capacity.
        cap: usize,
    },
    /// `max_in_flight` is zero.
    ZeroMaxInFlight,
    /// `secret_size_limit` is zero.
    ZeroSecretSizeLimit,
    /// `default_timeout` is zero.
    ZeroDefaultTimeout,
    /// `poll_interval` is zero.
    ZeroPollInterval,
    /// Region is empty.
    EmptyRegion,
    /// Access key id or secret access key is empty.
    EmptyCredentials,
    /// `SdkRetryPolicy::Standard.max_attempts` is zero.
    ZeroRetryAttempts,
}

impl std::fmt::Display for SecretsConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroMailboxCapacity => f.write_str("mailbox_capacity must be >= 1"),
            Self::MailboxCapacityTooLarge { requested, cap } => {
                write!(f, "mailbox_capacity {requested} exceeds cap {cap}")
            }
            Self::ZeroMaxInFlight => f.write_str("max_in_flight must be >= 1"),
            Self::ZeroSecretSizeLimit => f.write_str("secret_size_limit must be >= 1"),
            Self::ZeroDefaultTimeout => f.write_str("default_timeout must be > 0"),
            Self::ZeroPollInterval => f.write_str("poll_interval must be > 0"),
            Self::EmptyRegion => f.write_str("region must not be empty"),
            Self::EmptyCredentials => f.write_str("credentials must not be empty"),
            Self::ZeroRetryAttempts => {
                f.write_str("SdkRetryPolicy::Standard.max_attempts must be >= 1")
            }
        }
    }
}

impl std::error::Error for SecretsConfigError {}

/// One `GetSecretValue` request.
///
/// One of `secret_id` (name or ARN) is required. Version selection is
/// optional and mutually exclusive (`version_id` xor `version_stage`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretsGetSecretValue {
    /// Secret name or ARN.
    pub secret_id: String,
    /// Optional explicit version id.
    pub version_id: Option<String>,
    /// Optional staging label (e.g. `AWSCURRENT`).
    pub version_stage: Option<String>,
}

/// Typed Secrets Manager bridge requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretsRequest {
    /// Read one secret value.
    GetSecretValue(SecretsGetSecretValue),
}

impl SecretsRequest {
    /// Stable operation name for metrics/tracing.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::GetSecretValue(_) => "secrets_get_secret_value",
        }
    }
}

/// Materialized secret payload.
///
/// Exactly one of `secret_string` and `secret_binary` is populated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretsValue {
    /// Secret ARN.
    pub arn: Option<String>,
    /// Secret name.
    pub name: Option<String>,
    /// Version id returned for the read.
    pub version_id: Option<String>,
    /// Staging labels for this version.
    pub version_stages: Vec<String>,
    /// UTF-8 secret value, if stored as a string.
    pub secret_string: Option<String>,
    /// Binary secret value, if stored as bytes.
    pub secret_binary: Option<Vec<u8>>,
}

/// Typed Secrets Manager bridge responses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretsResponse {
    /// Secret value was read.
    SecretValue(SecretsValue),
}

/// Worker-level Secrets Manager bridge error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretsError {
    /// `max_in_flight` rejected admission.
    Full,
    /// Worker was closed.
    Closed,
    /// Bridge per-operation timeout fired.
    Timeout,
    /// Returned secret exceeded configured cap.
    SecretTooLarge,
    /// Request failed bridge validation.
    InvalidRequest(String),
    /// Secret (or version) does not exist.
    NotFound(String),
    /// Caller is not allowed to read this secret.
    AccessDenied(String),
    /// Service rejected parameters as invalid.
    InvalidParameter(String),
    /// Secret value is currently being rotated and not retrievable.
    Rotating(String),
    /// Service reported throttling.
    Throttled(String),
    /// KMS decryption failed.
    DecryptionFailed(String),
    /// Other SDK / service error.
    Sdk(String),
    /// Internal worker failure.
    Internal(String),
}

impl std::fmt::Display for SecretsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => f.write_str("secrets bridge: bounded ingress full"),
            Self::Closed => f.write_str("secrets bridge: closed"),
            Self::Timeout => f.write_str("secrets bridge: operation timed out"),
            Self::SecretTooLarge => f.write_str("secrets bridge: secret exceeds configured cap"),
            Self::InvalidRequest(msg) => write!(f, "secrets bridge: invalid request: {msg}"),
            Self::NotFound(msg) => write!(f, "secrets bridge: not found: {msg}"),
            Self::AccessDenied(msg) => write!(f, "secrets bridge: access denied: {msg}"),
            Self::InvalidParameter(msg) => write!(f, "secrets bridge: invalid parameter: {msg}"),
            Self::Rotating(msg) => write!(f, "secrets bridge: rotating: {msg}"),
            Self::Throttled(msg) => write!(f, "secrets bridge: throttled: {msg}"),
            Self::DecryptionFailed(msg) => write!(f, "secrets bridge: decryption failed: {msg}"),
            Self::Sdk(msg) => write!(f, "secrets bridge: sdk error: {msg}"),
            Self::Internal(msg) => write!(f, "secrets bridge: internal error: {msg}"),
        }
    }
}

impl std::error::Error for SecretsError {}
