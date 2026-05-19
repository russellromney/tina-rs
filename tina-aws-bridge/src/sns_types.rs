//! Public SNS request, response, error, and config types.

use std::collections::HashMap;
use std::time::Duration;

use crate::types::{MAX_MAILBOX_CAPACITY, SdkRetryPolicy};

/// Static SNS credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnsCredentials {
    /// Access key id.
    pub access_key_id: String,
    /// Secret access key.
    pub secret_access_key: String,
    /// Optional session token.
    pub session_token: Option<String>,
}

impl SnsCredentials {
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

/// Bridge configuration for SNS.
#[derive(Debug, Clone)]
pub struct SnsConfig {
    /// Worker mailbox capacity.
    pub mailbox_capacity: usize,
    /// Maximum admitted SDK futures running at once.
    pub max_in_flight: usize,
    /// Maximum SNS message body in bytes. SNS itself caps at 256KB for
    /// standard topics, 2GB for FIFO; the bridge cap fires first.
    pub message_body_limit: usize,
    /// Maximum total bytes across message attributes.
    pub attribute_body_limit: usize,
    /// Bridge per-operation deadline.
    pub default_timeout: Duration,
    /// Poll interval.
    pub poll_interval: Duration,
    /// AWS region.
    pub region: String,
    /// Optional endpoint URL.
    pub endpoint_url: Option<String>,
    /// Static credentials.
    pub credentials: Option<SnsCredentials>,
    /// Explicit SDK retry policy.
    pub retry_policy: SdkRetryPolicy,
}

impl Default for SnsConfig {
    fn default() -> Self {
        Self {
            mailbox_capacity: 64,
            max_in_flight: 16,
            message_body_limit: 256 << 10,
            attribute_body_limit: 256 << 10,
            default_timeout: Duration::from_secs(10),
            poll_interval: Duration::from_millis(2),
            region: "us-east-1".into(),
            endpoint_url: None,
            credentials: None,
            retry_policy: SdkRetryPolicy::Disabled,
        }
    }
}

impl SnsConfig {
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

    /// Sets `message_body_limit`.
    pub fn with_message_body_limit(mut self, limit: usize) -> Self {
        self.message_body_limit = limit;
        self
    }

    /// Sets `attribute_body_limit`.
    pub fn with_attribute_body_limit(mut self, limit: usize) -> Self {
        self.attribute_body_limit = limit;
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
    pub fn with_credentials(mut self, credentials: SnsCredentials) -> Self {
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
    pub fn validate(&self) -> Result<(), SnsConfigError> {
        self.validate_bridge_fields()?;
        if self.region.trim().is_empty() {
            return Err(SnsConfigError::EmptyRegion);
        }
        if let Some(credentials) = &self.credentials
            && (credentials.access_key_id.is_empty() || credentials.secret_access_key.is_empty())
        {
            return Err(SnsConfigError::EmptyCredentials);
        }
        if let SdkRetryPolicy::Standard { max_attempts: 0 } = self.retry_policy {
            return Err(SnsConfigError::ZeroRetryAttempts);
        }
        Ok(())
    }

    /// Validates only Tina-owned bridge fields.
    pub fn validate_bridge_fields(&self) -> Result<(), SnsConfigError> {
        if self.mailbox_capacity == 0 {
            return Err(SnsConfigError::ZeroMailboxCapacity);
        }
        if self.mailbox_capacity > MAX_MAILBOX_CAPACITY {
            return Err(SnsConfigError::MailboxCapacityTooLarge {
                requested: self.mailbox_capacity,
                cap: MAX_MAILBOX_CAPACITY,
            });
        }
        if self.max_in_flight == 0 {
            return Err(SnsConfigError::ZeroMaxInFlight);
        }
        if self.message_body_limit == 0 {
            return Err(SnsConfigError::ZeroMessageBodyLimit);
        }
        if self.attribute_body_limit == 0 {
            return Err(SnsConfigError::ZeroAttributeBodyLimit);
        }
        if self.default_timeout.is_zero() {
            return Err(SnsConfigError::ZeroDefaultTimeout);
        }
        if self.poll_interval.is_zero() {
            return Err(SnsConfigError::ZeroPollInterval);
        }
        Ok(())
    }
}

/// Reasons an [`SnsConfig`] is invalid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnsConfigError {
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
    /// `message_body_limit` is zero.
    ZeroMessageBodyLimit,
    /// `attribute_body_limit` is zero.
    ZeroAttributeBodyLimit,
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

impl std::fmt::Display for SnsConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroMailboxCapacity => f.write_str("mailbox_capacity must be >= 1"),
            Self::MailboxCapacityTooLarge { requested, cap } => {
                write!(f, "mailbox_capacity {requested} exceeds cap {cap}")
            }
            Self::ZeroMaxInFlight => f.write_str("max_in_flight must be >= 1"),
            Self::ZeroMessageBodyLimit => f.write_str("message_body_limit must be >= 1"),
            Self::ZeroAttributeBodyLimit => f.write_str("attribute_body_limit must be >= 1"),
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

impl std::error::Error for SnsConfigError {}

/// SNS message attribute. Data type defaults to `String`; binary is
/// modeled as a separate variant rather than a free-form string field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnsAttributeValue {
    /// Plain UTF-8 string. Maps to SNS `String`.
    String(String),
    /// Binary bytes. Maps to SNS `Binary`.
    Binary(Vec<u8>),
}

/// One SNS `Publish` request. The bridge does not infer subject from
/// message body or do JSON message-structure wrapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnsPublish {
    /// Destination — exactly one of TopicArn / TargetArn / PhoneNumber.
    /// The bridge does not route between them; the caller picks the
    /// destination for each request.
    pub destination: SnsDestination,
    /// Message body. UTF-8.
    pub message: String,
    /// Optional subject (Email/Email-JSON delivery).
    pub subject: Option<String>,
    /// Optional `MessageGroupId` (FIFO topics).
    pub message_group_id: Option<String>,
    /// Optional `MessageDeduplicationId` (FIFO topics). Idempotency
    /// remains caller-owned.
    pub message_deduplication_id: Option<String>,
    /// Message attributes.
    pub attributes: HashMap<String, SnsAttributeValue>,
}

/// Destination for a publish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnsDestination {
    /// Publish to a topic ARN. Subscribers fan out.
    TopicArn(String),
    /// Publish to a platform-endpoint ARN (mobile push).
    TargetArn(String),
    /// SMS to an E.164 phone number.
    PhoneNumber(String),
}

/// Typed SNS bridge requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnsRequest {
    /// Publish one message.
    Publish(SnsPublish),
}

impl SnsRequest {
    /// Stable operation name for metrics/tracing.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Publish(_) => "sns_publish",
        }
    }
}

/// Successful SNS publish response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnsPublished {
    /// SNS-assigned message id.
    pub message_id: Option<String>,
    /// FIFO sequence number, if returned.
    pub sequence_number: Option<String>,
}

/// Typed SNS bridge responses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnsResponse {
    /// Message was accepted by SNS.
    Published(SnsPublished),
}

/// Worker-level SNS bridge error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnsError {
    /// `max_in_flight` rejected admission.
    Full,
    /// Worker was closed.
    Closed,
    /// Bridge per-operation timeout fired.
    Timeout,
    /// Message body exceeded configured cap.
    MessageTooLarge,
    /// Attributes exceeded configured cap.
    AttributesTooLarge,
    /// Request failed bridge validation.
    InvalidRequest(String),
    /// SNS rejected the parameters (bad ARN, invalid attribute, etc).
    InvalidParameter(String),
    /// SNS topic / endpoint does not exist.
    NotFound(String),
    /// SNS rejected the caller (no permission, disabled).
    AccessDenied(String),
    /// SNS reported throttling.
    Throttled(String),
    /// Other SDK / service error.
    Sdk(String),
    /// Internal worker failure.
    Internal(String),
}

impl std::fmt::Display for SnsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => f.write_str("sns bridge: bounded ingress full"),
            Self::Closed => f.write_str("sns bridge: closed"),
            Self::Timeout => f.write_str("sns bridge: operation timed out"),
            Self::MessageTooLarge => f.write_str("sns bridge: message body exceeds configured cap"),
            Self::AttributesTooLarge => f.write_str("sns bridge: attributes exceed configured cap"),
            Self::InvalidRequest(msg) => write!(f, "sns bridge: invalid request: {msg}"),
            Self::InvalidParameter(msg) => write!(f, "sns bridge: invalid parameter: {msg}"),
            Self::NotFound(msg) => write!(f, "sns bridge: not found: {msg}"),
            Self::AccessDenied(msg) => write!(f, "sns bridge: access denied: {msg}"),
            Self::Throttled(msg) => write!(f, "sns bridge: throttled: {msg}"),
            Self::Sdk(msg) => write!(f, "sns bridge: sdk error: {msg}"),
            Self::Internal(msg) => write!(f, "sns bridge: internal error: {msg}"),
        }
    }
}

impl std::error::Error for SnsError {}
