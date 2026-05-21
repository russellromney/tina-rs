//! Public request, response, error, and config types.

use std::time::Duration;

/// Hard cap on `mailbox_capacity` to avoid accidental effective
/// unbounded queues.
pub(crate) const MAX_MAILBOX_CAPACITY: usize = 1 << 20;

/// Static S3 credentials used when the bridge builds its own client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Credentials {
    /// Access key id.
    pub access_key_id: String,
    /// Secret access key.
    pub secret_access_key: String,
    /// Optional session token.
    pub session_token: Option<String>,
}

impl S3Credentials {
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

/// AWS SDK retry policy.
///
/// Disabled is the default. Standard retry is explicit because one
/// bridge operation can become multiple SDK HTTP attempts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SdkRetryPolicy {
    /// Disable SDK retries (`max_attempts = 1`).
    #[default]
    Disabled,
    /// AWS SDK standard retry mode with a bounded total attempt count.
    Standard {
        /// Total attempts including the first. Must be `>= 1`.
        max_attempts: u32,
    },
}

impl SdkRetryPolicy {
    /// Total SDK attempts configured for one bridge operation.
    pub fn max_attempts(self) -> u32 {
        match self {
            Self::Disabled => 1,
            Self::Standard { max_attempts } => max_attempts,
        }
    }
}

/// Bridge configuration.
#[derive(Debug, Clone)]
pub struct S3Config {
    /// Worker mailbox capacity (Tina ingress). Past this, callers see
    /// `CallOutcome::Full` / `CallError::TargetFull`.
    pub mailbox_capacity: usize,
    /// Maximum admitted SDK futures running at once. Past this, the
    /// worker replies [`S3Error::Full`] before the SDK sees work.
    pub max_in_flight: usize,
    /// Maximum S3 put body in bytes.
    pub request_body_limit: usize,
    /// Maximum buffered S3 get body in bytes. The bridge reads the
    /// stream chunk by chunk and fails once this cap would be crossed.
    pub response_body_limit: usize,
    /// Bridge per-operation deadline. On timeout, Tina stops waiting;
    /// already admitted SDK work may finish late.
    pub default_timeout: Duration,
    /// Poll interval for checking SDK task completion from Tina.
    pub poll_interval: Duration,
    /// S3 region, e.g. `us-east-1`.
    pub region: String,
    /// Optional endpoint URL for LocalStack, MinIO, or test servers.
    pub endpoint_url: Option<String>,
    /// Force path-style S3 addressing. Usually needed for local
    /// S3-compatible services.
    pub force_path_style: bool,
    /// Static credentials for client-built install.
    ///
    /// `None` means the built S3 client has no credentials provider.
    /// That is useful only for unsigned/local test endpoints. Real
    /// S3 callers should set this or use `with_supplied_client` once
    /// they need the AWS default provider chain, assume-role, SSO,
    /// or other SDK-owned credential behavior.
    pub credentials: Option<S3Credentials>,
    /// Explicit SDK retry policy.
    pub retry_policy: SdkRetryPolicy,
}

impl Default for S3Config {
    fn default() -> Self {
        Self {
            mailbox_capacity: 64,
            max_in_flight: 16,
            request_body_limit: 8 << 20,
            response_body_limit: 8 << 20,
            default_timeout: Duration::from_secs(10),
            poll_interval: Duration::from_millis(2),
            region: "us-east-1".into(),
            endpoint_url: None,
            force_path_style: false,
            credentials: None,
            retry_policy: SdkRetryPolicy::Disabled,
        }
    }
}

impl S3Config {
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

    /// Sets `request_body_limit`.
    pub fn with_request_body_limit(mut self, limit: usize) -> Self {
        self.request_body_limit = limit;
        self
    }

    /// Sets `response_body_limit`.
    pub fn with_response_body_limit(mut self, limit: usize) -> Self {
        self.response_body_limit = limit;
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

    /// Sets path-style S3 addressing.
    pub fn with_force_path_style(mut self, force: bool) -> Self {
        self.force_path_style = force;
        self
    }

    /// Sets static credentials.
    pub fn with_credentials(mut self, credentials: S3Credentials) -> Self {
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
    pub fn validate(&self) -> Result<(), S3ConfigError> {
        self.validate_bridge_fields()?;
        if self.region.trim().is_empty() {
            return Err(S3ConfigError::EmptyRegion);
        }
        if let Some(credentials) = &self.credentials
            && (credentials.access_key_id.is_empty() || credentials.secret_access_key.is_empty())
        {
            return Err(S3ConfigError::EmptyCredentials);
        }
        if let SdkRetryPolicy::Standard { max_attempts: 0 } = self.retry_policy {
            return Err(S3ConfigError::ZeroRetryAttempts);
        }
        Ok(())
    }

    /// Validates only Tina-owned bridge fields.
    ///
    /// Use this for caller-supplied S3 clients, where region,
    /// credentials, endpoint, path-style addressing, and SDK retry
    /// policy live entirely on the supplied client.
    pub fn validate_bridge_fields(&self) -> Result<(), S3ConfigError> {
        if self.mailbox_capacity == 0 {
            return Err(S3ConfigError::ZeroMailboxCapacity);
        }
        if self.mailbox_capacity > MAX_MAILBOX_CAPACITY {
            return Err(S3ConfigError::MailboxCapacityTooLarge {
                requested: self.mailbox_capacity,
                cap: MAX_MAILBOX_CAPACITY,
            });
        }
        if self.max_in_flight == 0 {
            return Err(S3ConfigError::ZeroMaxInFlight);
        }
        if self.request_body_limit == 0 {
            return Err(S3ConfigError::ZeroRequestBodyLimit);
        }
        if self.response_body_limit == 0 {
            return Err(S3ConfigError::ZeroResponseBodyLimit);
        }
        if self.default_timeout.is_zero() {
            return Err(S3ConfigError::ZeroDefaultTimeout);
        }
        if self.poll_interval.is_zero() {
            return Err(S3ConfigError::ZeroPollInterval);
        }
        Ok(())
    }
}

/// Reasons an [`S3Config`] is invalid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum S3ConfigError {
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
    /// `request_body_limit` is zero.
    ZeroRequestBodyLimit,
    /// `response_body_limit` is zero.
    ZeroResponseBodyLimit,
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

impl std::fmt::Display for S3ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroMailboxCapacity => f.write_str("mailbox_capacity must be >= 1"),
            Self::MailboxCapacityTooLarge { requested, cap } => {
                write!(f, "mailbox_capacity {requested} exceeds cap {cap}")
            }
            Self::ZeroMaxInFlight => f.write_str("max_in_flight must be >= 1"),
            Self::ZeroRequestBodyLimit => f.write_str("request_body_limit must be >= 1"),
            Self::ZeroResponseBodyLimit => f.write_str("response_body_limit must be >= 1"),
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

impl std::error::Error for S3ConfigError {}

/// One S3 `PutObject` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3PutObject {
    /// Bucket name.
    pub bucket: String,
    /// Object key.
    pub key: String,
    /// Full request body.
    pub body: Vec<u8>,
    /// Optional content type.
    pub content_type: Option<String>,
}

/// One S3 `GetObject` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3GetObject {
    /// Bucket name.
    pub bucket: String,
    /// Object key.
    pub key: String,
    /// Optional per-request response cap. The effective cap is the
    /// minimum of this value and [`S3Config::response_body_limit`].
    pub max_bytes: Option<usize>,
}

/// One S3 `HeadObject` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3HeadObject {
    /// Bucket name.
    pub bucket: String,
    /// Object key.
    pub key: String,
}

/// One S3 `DeleteObject` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3DeleteObject {
    /// Bucket name.
    pub bucket: String,
    /// Object key.
    pub key: String,
}

/// Typed S3 bridge requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum S3Request {
    /// Put a full S3 object body.
    PutObject(S3PutObject),
    /// Get a full S3 object body capped by request/config.
    GetObject(S3GetObject),
    /// Fetch S3 object metadata without reading a body.
    HeadObject(S3HeadObject),
    /// Delete one S3 object.
    DeleteObject(S3DeleteObject),
}

impl S3Request {
    /// Stable operation name for metrics/tracing.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::PutObject(_) => "s3_put_object",
            Self::GetObject(_) => "s3_get_object",
            Self::HeadObject(_) => "s3_head_object",
            Self::DeleteObject(_) => "s3_delete_object",
        }
    }
}

/// Successful S3 put response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3PutObjectOk {
    /// ETag if returned by S3.
    pub e_tag: Option<String>,
    /// Version id if bucket versioning returned one.
    pub version_id: Option<String>,
}

/// Successful S3 get response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Object {
    /// Object bytes.
    pub body: Vec<u8>,
    /// Content length if returned by S3.
    pub content_length: Option<i64>,
    /// Content type if returned by S3.
    pub content_type: Option<String>,
    /// ETag if returned by S3.
    pub e_tag: Option<String>,
}

/// Successful S3 head response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3ObjectHead {
    /// Content length if returned by S3.
    pub content_length: Option<i64>,
    /// Content type if returned by S3.
    pub content_type: Option<String>,
    /// ETag if returned by S3.
    pub e_tag: Option<String>,
}

/// Successful S3 delete response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3DeletedObject {
    /// Version id if returned by S3.
    pub version_id: Option<String>,
    /// Delete marker flag if returned by S3.
    pub delete_marker: Option<bool>,
}

/// Typed S3 bridge responses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum S3Response {
    /// Object was put.
    PutObject(S3PutObjectOk),
    /// Object body was returned.
    Object(S3Object),
    /// Object metadata was returned.
    HeadObject(S3ObjectHead),
    /// Object was deleted.
    DeletedObject(S3DeletedObject),
}

/// Worker-level S3 bridge error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum S3Error {
    /// `max_in_flight` rejected admission.
    Full,
    /// Worker was closed.
    Closed,
    /// Bridge per-operation timeout fired.
    Timeout,
    /// Request body exceeded configured cap.
    RequestTooLarge,
    /// Response body exceeded configured cap.
    ResponseTooLarge,
    /// Request failed bridge validation.
    InvalidRequest(String),
    /// S3 reported the bucket or object was not found.
    NotFound(String),
    /// S3 denied the request.
    AccessDenied(String),
    /// S3 throttled the request after the SDK retry budget.
    Throttled(String),
    /// AWS SDK returned an error.
    Sdk(String),
    /// Stream body read failed.
    Body(String),
    /// Internal worker failure.
    Internal(String),
}

impl std::fmt::Display for S3Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => f.write_str("s3 bridge: bounded ingress full"),
            Self::Closed => f.write_str("s3 bridge: closed"),
            Self::Timeout => f.write_str("s3 bridge: operation timed out"),
            Self::RequestTooLarge => f.write_str("s3 bridge: request body exceeds configured cap"),
            Self::ResponseTooLarge => {
                f.write_str("s3 bridge: response body exceeds configured cap")
            }
            Self::InvalidRequest(msg) => write!(f, "s3 bridge: invalid request: {msg}"),
            Self::NotFound(msg) => write!(f, "s3 bridge: not found: {msg}"),
            Self::AccessDenied(msg) => write!(f, "s3 bridge: access denied: {msg}"),
            Self::Throttled(msg) => write!(f, "s3 bridge: throttled: {msg}"),
            Self::Sdk(msg) => write!(f, "s3 bridge: sdk error: {msg}"),
            Self::Body(msg) => write!(f, "s3 bridge: body error: {msg}"),
            Self::Internal(msg) => write!(f, "s3 bridge: internal error: {msg}"),
        }
    }
}

impl std::error::Error for S3Error {}
