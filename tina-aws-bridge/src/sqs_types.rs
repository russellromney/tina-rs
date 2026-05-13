//! Public SQS request, response, error, and config types.

use std::time::Duration;

use crate::types::{MAX_MAILBOX_CAPACITY, SdkRetryPolicy};

/// Static SQS credentials used when the bridge builds its own client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqsCredentials {
    /// Access key id.
    pub access_key_id: String,
    /// Secret access key.
    pub secret_access_key: String,
    /// Optional session token.
    pub session_token: Option<String>,
}

impl SqsCredentials {
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

/// Bridge configuration for SQS.
#[derive(Debug, Clone)]
pub struct SqsConfig {
    /// Worker mailbox capacity.
    pub mailbox_capacity: usize,
    /// Maximum admitted SDK futures running at once.
    pub max_in_flight: usize,
    /// Maximum SQS message body in bytes.
    pub message_body_limit: usize,
    /// Maximum messages returned by one receive call.
    pub max_receive_messages: usize,
    /// Bridge per-operation deadline. On timeout, Tina stops waiting;
    /// already admitted SDK work may finish late.
    pub default_timeout: Duration,
    /// Poll interval for checking SDK task completion from Tina.
    pub poll_interval: Duration,
    /// SQS region, e.g. `us-east-1`.
    pub region: String,
    /// Optional endpoint URL for fake/local endpoints.
    pub endpoint_url: Option<String>,
    /// Static credentials for client-built install.
    pub credentials: Option<SqsCredentials>,
    /// Explicit SDK retry policy.
    pub retry_policy: SdkRetryPolicy,
}

impl Default for SqsConfig {
    fn default() -> Self {
        Self {
            mailbox_capacity: 64,
            max_in_flight: 16,
            message_body_limit: 256 << 10,
            max_receive_messages: 10,
            default_timeout: Duration::from_secs(10),
            poll_interval: Duration::from_millis(2),
            region: "us-east-1".into(),
            endpoint_url: None,
            credentials: None,
            retry_policy: SdkRetryPolicy::Disabled,
        }
    }
}

impl SqsConfig {
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

    /// Sets `max_receive_messages`.
    pub fn with_max_receive_messages(mut self, max: usize) -> Self {
        self.max_receive_messages = max;
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
    pub fn with_credentials(mut self, credentials: SqsCredentials) -> Self {
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
    pub fn validate(&self) -> Result<(), SqsConfigError> {
        self.validate_bridge_fields()?;
        if self.region.trim().is_empty() {
            return Err(SqsConfigError::EmptyRegion);
        }
        if let Some(credentials) = &self.credentials
            && (credentials.access_key_id.is_empty() || credentials.secret_access_key.is_empty())
        {
            return Err(SqsConfigError::EmptyCredentials);
        }
        if let SdkRetryPolicy::Standard { max_attempts: 0 } = self.retry_policy {
            return Err(SqsConfigError::ZeroRetryAttempts);
        }
        Ok(())
    }

    /// Validates only Tina-owned bridge fields.
    pub fn validate_bridge_fields(&self) -> Result<(), SqsConfigError> {
        if self.mailbox_capacity == 0 {
            return Err(SqsConfigError::ZeroMailboxCapacity);
        }
        if self.mailbox_capacity > MAX_MAILBOX_CAPACITY {
            return Err(SqsConfigError::MailboxCapacityTooLarge {
                requested: self.mailbox_capacity,
                cap: MAX_MAILBOX_CAPACITY,
            });
        }
        if self.max_in_flight == 0 {
            return Err(SqsConfigError::ZeroMaxInFlight);
        }
        if self.message_body_limit == 0 {
            return Err(SqsConfigError::ZeroMessageBodyLimit);
        }
        if self.max_receive_messages == 0 || self.max_receive_messages > 10 {
            return Err(SqsConfigError::InvalidMaxReceiveMessages {
                requested: self.max_receive_messages,
            });
        }
        if self.default_timeout.is_zero() {
            return Err(SqsConfigError::ZeroDefaultTimeout);
        }
        if self.poll_interval.is_zero() {
            return Err(SqsConfigError::ZeroPollInterval);
        }
        Ok(())
    }
}

/// Reasons an [`SqsConfig`] is invalid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqsConfigError {
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
    /// `max_receive_messages` is outside SQS's 1..=10 range.
    InvalidMaxReceiveMessages {
        /// Requested value.
        requested: usize,
    },
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

impl std::fmt::Display for SqsConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroMailboxCapacity => f.write_str("mailbox_capacity must be >= 1"),
            Self::MailboxCapacityTooLarge { requested, cap } => {
                write!(f, "mailbox_capacity {requested} exceeds cap {cap}")
            }
            Self::ZeroMaxInFlight => f.write_str("max_in_flight must be >= 1"),
            Self::ZeroMessageBodyLimit => f.write_str("message_body_limit must be >= 1"),
            Self::InvalidMaxReceiveMessages { requested } => {
                write!(f, "max_receive_messages {requested} must be in 1..=10")
            }
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

impl std::error::Error for SqsConfigError {}

/// One SQS `SendMessage` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqsSendMessage {
    /// Full queue URL.
    pub queue_url: String,
    /// Message body.
    pub body: String,
    /// Optional FIFO message group id.
    pub message_group_id: Option<String>,
    /// Optional FIFO deduplication id. Idempotency remains caller-owned.
    pub message_deduplication_id: Option<String>,
}

/// One SQS `ReceiveMessage` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqsReceiveMessage {
    /// Full queue URL.
    pub queue_url: String,
    /// Maximum messages to receive. Effective cap is bounded by config
    /// and SQS's 10-message API cap.
    pub max_messages: usize,
    /// Visibility timeout in seconds. This is SQS message-invisibility
    /// truth, not a Tina rollback or auto-delete mechanism.
    pub visibility_timeout_seconds: i32,
    /// Long-poll wait time in seconds.
    pub wait_time_seconds: i32,
}

/// One SQS `DeleteMessage` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqsDeleteMessage {
    /// Full queue URL.
    pub queue_url: String,
    /// Receipt handle returned by `ReceiveMessage`.
    pub receipt_handle: String,
}

/// Typed SQS bridge requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqsRequest {
    /// Send one message.
    SendMessage(SqsSendMessage),
    /// Receive up to a bounded count of messages.
    ReceiveMessage(SqsReceiveMessage),
    /// Delete one previously received message.
    DeleteMessage(SqsDeleteMessage),
}

impl SqsRequest {
    /// Stable operation name for metrics/tracing.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::SendMessage(_) => "sqs_send_message",
            Self::ReceiveMessage(_) => "sqs_receive_message",
            Self::DeleteMessage(_) => "sqs_delete_message",
        }
    }
}

/// Successful SQS send response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqsSentMessage {
    /// Message id if returned by SQS.
    pub message_id: Option<String>,
    /// MD5 of the message body if returned by SQS.
    pub md5_of_body: Option<String>,
    /// FIFO sequence number if returned by SQS.
    pub sequence_number: Option<String>,
}

/// One received SQS message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqsMessage {
    /// Message id if returned by SQS.
    pub message_id: Option<String>,
    /// Receipt handle required for `DeleteMessage`.
    pub receipt_handle: String,
    /// Message body.
    pub body: String,
}

/// Successful SQS receive response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqsReceivedMessages {
    /// Received messages. Empty receive is a successful typed response.
    pub messages: Vec<SqsMessage>,
}

/// Successful SQS delete response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqsDeletedMessage;

/// Typed SQS bridge responses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqsResponse {
    /// Message was sent.
    SentMessage(SqsSentMessage),
    /// Messages were received; may be empty.
    ReceivedMessages(SqsReceivedMessages),
    /// Message was deleted.
    DeletedMessage(SqsDeletedMessage),
}

/// Worker-level SQS bridge error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqsError {
    /// `max_in_flight` rejected admission.
    Full,
    /// Worker was closed.
    Closed,
    /// Bridge per-operation timeout fired.
    Timeout,
    /// Message body exceeded configured cap.
    MessageTooLarge,
    /// Received messages exceeded configured cap.
    ResponseTooLarge,
    /// Request failed bridge validation.
    InvalidRequest(String),
    /// AWS SDK returned a queue-not-found error.
    QueueDoesNotExist(String),
    /// AWS SDK returned a throttling error.
    Throttled(String),
    /// AWS SDK returned another service/transport error.
    Sdk(String),
    /// Internal worker failure.
    Internal(String),
}

impl std::fmt::Display for SqsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => f.write_str("sqs bridge: bounded ingress full"),
            Self::Closed => f.write_str("sqs bridge: closed"),
            Self::Timeout => f.write_str("sqs bridge: operation timed out"),
            Self::MessageTooLarge => f.write_str("sqs bridge: message body exceeds configured cap"),
            Self::ResponseTooLarge => f.write_str("sqs bridge: response exceeds configured cap"),
            Self::InvalidRequest(msg) => write!(f, "sqs bridge: invalid request: {msg}"),
            Self::QueueDoesNotExist(msg) => write!(f, "sqs bridge: queue does not exist: {msg}"),
            Self::Throttled(msg) => write!(f, "sqs bridge: throttled: {msg}"),
            Self::Sdk(msg) => write!(f, "sqs bridge: sdk error: {msg}"),
            Self::Internal(msg) => write!(f, "sqs bridge: internal error: {msg}"),
        }
    }
}

impl std::error::Error for SqsError {}
