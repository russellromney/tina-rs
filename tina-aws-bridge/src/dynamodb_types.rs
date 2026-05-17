//! Public DynamoDB request, response, error, and config types.

use std::collections::HashMap;
use std::time::Duration;

use crate::types::{MAX_MAILBOX_CAPACITY, SdkRetryPolicy};

/// Static DynamoDB credentials used when the bridge builds its own
/// client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DynamoCredentials {
    /// Access key id.
    pub access_key_id: String,
    /// Secret access key.
    pub secret_access_key: String,
    /// Optional session token.
    pub session_token: Option<String>,
}

impl DynamoCredentials {
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

/// Whether to ask DynamoDB for consumed-capacity facts on each call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ReturnCapacity {
    /// Do not ask. `consumed_capacity` is always `None`.
    #[default]
    None,
    /// Ask for table-level totals.
    Total,
    /// Ask for index-level breakdown.
    Indexes,
}

/// Bridge configuration for DynamoDB.
#[derive(Debug, Clone)]
pub struct DynamoConfig {
    /// Worker mailbox capacity.
    pub mailbox_capacity: usize,
    /// Maximum admitted SDK futures running at once.
    pub max_in_flight: usize,
    /// Maximum encoded item size in bytes (rough estimate; DynamoDB
    /// itself caps items at 400KB, but the bridge cap fires first).
    pub item_size_limit: usize,
    /// Maximum Query result items returned in one call.
    pub query_item_limit: usize,
    /// Bridge per-operation deadline. On timeout, Tina stops waiting;
    /// already admitted SDK work may finish late.
    pub default_timeout: Duration,
    /// Poll interval for checking SDK task completion from Tina.
    pub poll_interval: Duration,
    /// AWS region.
    pub region: String,
    /// Optional endpoint URL for fake/local endpoints (DynamoDB Local,
    /// LocalStack).
    pub endpoint_url: Option<String>,
    /// Static credentials for client-built install.
    pub credentials: Option<DynamoCredentials>,
    /// Explicit SDK retry policy.
    pub retry_policy: SdkRetryPolicy,
    /// What capacity facts to ask the service to return.
    pub return_capacity: ReturnCapacity,
}

impl Default for DynamoConfig {
    fn default() -> Self {
        Self {
            mailbox_capacity: 64,
            max_in_flight: 16,
            item_size_limit: 400 << 10,
            query_item_limit: 100,
            default_timeout: Duration::from_secs(10),
            poll_interval: Duration::from_millis(2),
            region: "us-east-1".into(),
            endpoint_url: None,
            credentials: None,
            retry_policy: SdkRetryPolicy::Disabled,
            return_capacity: ReturnCapacity::None,
        }
    }
}

impl DynamoConfig {
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

    /// Sets `item_size_limit`.
    pub fn with_item_size_limit(mut self, limit: usize) -> Self {
        self.item_size_limit = limit;
        self
    }

    /// Sets `query_item_limit`.
    pub fn with_query_item_limit(mut self, limit: usize) -> Self {
        self.query_item_limit = limit;
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
    pub fn with_credentials(mut self, credentials: DynamoCredentials) -> Self {
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

    /// Sets `return_capacity`.
    pub fn with_return_capacity(mut self, mode: ReturnCapacity) -> Self {
        self.return_capacity = mode;
        self
    }

    /// Validates config.
    pub fn validate(&self) -> Result<(), DynamoConfigError> {
        self.validate_bridge_fields()?;
        if self.region.trim().is_empty() {
            return Err(DynamoConfigError::EmptyRegion);
        }
        if let Some(credentials) = &self.credentials
            && (credentials.access_key_id.is_empty() || credentials.secret_access_key.is_empty())
        {
            return Err(DynamoConfigError::EmptyCredentials);
        }
        if let SdkRetryPolicy::Standard { max_attempts: 0 } = self.retry_policy {
            return Err(DynamoConfigError::ZeroRetryAttempts);
        }
        Ok(())
    }

    /// Validates only Tina-owned bridge fields.
    pub fn validate_bridge_fields(&self) -> Result<(), DynamoConfigError> {
        if self.mailbox_capacity == 0 {
            return Err(DynamoConfigError::ZeroMailboxCapacity);
        }
        if self.mailbox_capacity > MAX_MAILBOX_CAPACITY {
            return Err(DynamoConfigError::MailboxCapacityTooLarge {
                requested: self.mailbox_capacity,
                cap: MAX_MAILBOX_CAPACITY,
            });
        }
        if self.max_in_flight == 0 {
            return Err(DynamoConfigError::ZeroMaxInFlight);
        }
        if self.item_size_limit == 0 {
            return Err(DynamoConfigError::ZeroItemSizeLimit);
        }
        if self.query_item_limit == 0 {
            return Err(DynamoConfigError::ZeroQueryItemLimit);
        }
        if self.default_timeout.is_zero() {
            return Err(DynamoConfigError::ZeroDefaultTimeout);
        }
        if self.poll_interval.is_zero() {
            return Err(DynamoConfigError::ZeroPollInterval);
        }
        Ok(())
    }
}

/// Reasons a [`DynamoConfig`] is invalid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DynamoConfigError {
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
    /// `item_size_limit` is zero.
    ZeroItemSizeLimit,
    /// `query_item_limit` is zero.
    ZeroQueryItemLimit,
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

impl std::fmt::Display for DynamoConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroMailboxCapacity => f.write_str("mailbox_capacity must be >= 1"),
            Self::MailboxCapacityTooLarge { requested, cap } => {
                write!(f, "mailbox_capacity {requested} exceeds cap {cap}")
            }
            Self::ZeroMaxInFlight => f.write_str("max_in_flight must be >= 1"),
            Self::ZeroItemSizeLimit => f.write_str("item_size_limit must be >= 1"),
            Self::ZeroQueryItemLimit => f.write_str("query_item_limit must be >= 1"),
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

impl std::error::Error for DynamoConfigError {}

/// Bounded DynamoDB attribute value.
///
/// Covers the common scalar plus document types. The deprecated
/// `SS` / `NS` / `BS` set types are not supported; use `L` with the
/// matching scalar instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DynamoValue {
    /// String.
    S(String),
    /// Number (DynamoDB sends numbers as decimal strings).
    N(String),
    /// Binary bytes.
    B(Vec<u8>),
    /// Boolean.
    Bool(bool),
    /// Null.
    Null,
    /// Ordered list of values.
    L(Vec<DynamoValue>),
    /// Unordered map of attributes.
    M(HashMap<String, DynamoValue>),
}

/// One DynamoDB item: attribute name -> value.
pub type DynamoItem = HashMap<String, DynamoValue>;

/// Read consistency for [`DynamoGetItem`] and [`DynamoQuery`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DynamoConsistency {
    /// Eventually consistent read (default DynamoDB behavior).
    #[default]
    Eventual,
    /// Strongly consistent read.
    Strong,
}

/// One DynamoDB `GetItem` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DynamoGetItem {
    /// Table name.
    pub table_name: String,
    /// Primary key attributes.
    pub key: DynamoItem,
    /// Read consistency.
    pub consistency: DynamoConsistency,
}

/// One DynamoDB `PutItem` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DynamoPutItem {
    /// Table name.
    pub table_name: String,
    /// Item attributes to write.
    pub item: DynamoItem,
    /// Optional condition expression.
    pub condition_expression: Option<String>,
    /// Expression attribute names (for reserved-word escaping).
    pub expression_attribute_names: HashMap<String, String>,
    /// Expression attribute values referenced by the condition
    /// expression.
    pub expression_attribute_values: HashMap<String, DynamoValue>,
}

/// One DynamoDB `DeleteItem` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DynamoDeleteItem {
    /// Table name.
    pub table_name: String,
    /// Primary key attributes.
    pub key: DynamoItem,
    /// Optional condition expression.
    pub condition_expression: Option<String>,
    /// Expression attribute names.
    pub expression_attribute_names: HashMap<String, String>,
    /// Expression attribute values.
    pub expression_attribute_values: HashMap<String, DynamoValue>,
}

/// One DynamoDB `UpdateItem` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DynamoUpdateItem {
    /// Table name.
    pub table_name: String,
    /// Primary key attributes.
    pub key: DynamoItem,
    /// `UpdateExpression` (e.g. `"SET attr = :v"`).
    pub update_expression: String,
    /// Optional condition expression.
    pub condition_expression: Option<String>,
    /// Expression attribute names.
    pub expression_attribute_names: HashMap<String, String>,
    /// Expression attribute values referenced by the expressions.
    pub expression_attribute_values: HashMap<String, DynamoValue>,
}

/// One DynamoDB `Query` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DynamoQuery {
    /// Table name.
    pub table_name: String,
    /// Optional secondary index name.
    pub index_name: Option<String>,
    /// `KeyConditionExpression` (e.g. `"pk = :p"`).
    pub key_condition_expression: String,
    /// Optional `FilterExpression`.
    pub filter_expression: Option<String>,
    /// Expression attribute names.
    pub expression_attribute_names: HashMap<String, String>,
    /// Expression attribute values referenced by the expressions.
    pub expression_attribute_values: HashMap<String, DynamoValue>,
    /// Optional caller-supplied limit. The effective limit is bounded
    /// by [`DynamoConfig::query_item_limit`].
    pub limit: Option<i32>,
    /// Read consistency. Note: strongly consistent reads are not
    /// supported by DynamoDB on global secondary indexes; the SDK will
    /// reject that combination.
    pub consistency: DynamoConsistency,
    /// Optional pagination start key.
    pub exclusive_start_key: Option<DynamoItem>,
    /// If false (the default), scan in ascending order on the sort key.
    pub scan_index_forward: bool,
}

/// Typed DynamoDB bridge requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DynamoRequest {
    /// Get one item by primary key.
    GetItem(DynamoGetItem),
    /// Put one item.
    PutItem(DynamoPutItem),
    /// Update one item.
    UpdateItem(DynamoUpdateItem),
    /// Delete one item.
    DeleteItem(DynamoDeleteItem),
    /// Query a partition.
    Query(DynamoQuery),
}

impl DynamoRequest {
    /// Stable operation name for metrics/tracing.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::GetItem(_) => "dynamodb_get_item",
            Self::PutItem(_) => "dynamodb_put_item",
            Self::UpdateItem(_) => "dynamodb_update_item",
            Self::DeleteItem(_) => "dynamodb_delete_item",
            Self::Query(_) => "dynamodb_query",
        }
    }
}

/// Consumed-capacity fact reported by DynamoDB.
///
/// Populated only when [`DynamoConfig::return_capacity`] is not
/// [`ReturnCapacity::None`]. The bridge does not synthesize these
/// numbers; if DynamoDB omits them, the field is `None`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DynamoConsumedCapacity {
    /// Total capacity units consumed.
    pub capacity_units: Option<f64>,
    /// Read capacity units consumed.
    pub read_capacity_units: Option<f64>,
    /// Write capacity units consumed.
    pub write_capacity_units: Option<f64>,
}

impl Eq for DynamoConsumedCapacity {}

/// Successful `GetItem` response.
#[derive(Debug, Clone, PartialEq)]
pub struct DynamoGotItem {
    /// Item, or `None` if the key was absent.
    pub item: Option<DynamoItem>,
    /// Consumed capacity, if requested.
    pub consumed_capacity: Option<DynamoConsumedCapacity>,
}

impl Eq for DynamoGotItem {}

/// Successful `PutItem` response.
#[derive(Debug, Clone, PartialEq)]
pub struct DynamoPutItemOk {
    /// Consumed capacity, if requested.
    pub consumed_capacity: Option<DynamoConsumedCapacity>,
}

impl Eq for DynamoPutItemOk {}

/// Successful `UpdateItem` response.
#[derive(Debug, Clone, PartialEq)]
pub struct DynamoUpdatedItem {
    /// Consumed capacity, if requested.
    pub consumed_capacity: Option<DynamoConsumedCapacity>,
}

impl Eq for DynamoUpdatedItem {}

/// Successful `DeleteItem` response.
#[derive(Debug, Clone, PartialEq)]
pub struct DynamoDeletedItem {
    /// Consumed capacity, if requested.
    pub consumed_capacity: Option<DynamoConsumedCapacity>,
}

impl Eq for DynamoDeletedItem {}

/// Successful `Query` response.
#[derive(Debug, Clone, PartialEq)]
pub struct DynamoQueryResult {
    /// Returned items.
    pub items: Vec<DynamoItem>,
    /// Pagination key for the next page, or `None` if exhausted.
    pub last_evaluated_key: Option<DynamoItem>,
    /// Count returned (after any filter).
    pub count: i32,
    /// Scanned count (before any filter).
    pub scanned_count: i32,
    /// Consumed capacity, if requested.
    pub consumed_capacity: Option<DynamoConsumedCapacity>,
}

impl Eq for DynamoQueryResult {}

/// Typed DynamoDB bridge responses.
#[derive(Debug, Clone, PartialEq)]
pub enum DynamoResponse {
    /// Item was fetched (possibly empty).
    GetItem(DynamoGotItem),
    /// Item was written.
    PutItem(DynamoPutItemOk),
    /// Item was updated.
    UpdateItem(DynamoUpdatedItem),
    /// Item was deleted.
    DeleteItem(DynamoDeletedItem),
    /// Query returned a page.
    Query(DynamoQueryResult),
}

impl Eq for DynamoResponse {}

/// Worker-level DynamoDB bridge error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DynamoError {
    /// `max_in_flight` rejected admission.
    Full,
    /// Worker was closed.
    Closed,
    /// Bridge per-operation timeout fired.
    Timeout,
    /// Encoded request exceeded configured cap.
    ItemTooLarge,
    /// Request failed bridge validation.
    InvalidRequest(String),
    /// Conditional check (`condition_expression`) failed.
    ConditionalCheckFailed(String),
    /// Throughput exceeded the table's provisioned/on-demand capacity.
    ProvisionedThroughputExceeded(String),
    /// Resource (table/index) not found.
    ResourceNotFound(String),
    /// SDK reported throttling.
    Throttled(String),
    /// Transactional conflict on an `UpdateItem`.
    TransactionConflict(String),
    /// AWS SDK returned another error.
    Sdk(String),
    /// Internal worker failure.
    Internal(String),
}

impl std::fmt::Display for DynamoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => f.write_str("dynamodb bridge: bounded ingress full"),
            Self::Closed => f.write_str("dynamodb bridge: closed"),
            Self::Timeout => f.write_str("dynamodb bridge: operation timed out"),
            Self::ItemTooLarge => {
                f.write_str("dynamodb bridge: encoded item exceeds configured cap")
            }
            Self::InvalidRequest(msg) => write!(f, "dynamodb bridge: invalid request: {msg}"),
            Self::ConditionalCheckFailed(msg) => {
                write!(f, "dynamodb bridge: conditional check failed: {msg}")
            }
            Self::ProvisionedThroughputExceeded(msg) => {
                write!(f, "dynamodb bridge: provisioned throughput exceeded: {msg}")
            }
            Self::ResourceNotFound(msg) => write!(f, "dynamodb bridge: resource not found: {msg}"),
            Self::Throttled(msg) => write!(f, "dynamodb bridge: throttled: {msg}"),
            Self::TransactionConflict(msg) => {
                write!(f, "dynamodb bridge: transaction conflict: {msg}")
            }
            Self::Sdk(msg) => write!(f, "dynamodb bridge: sdk error: {msg}"),
            Self::Internal(msg) => write!(f, "dynamodb bridge: internal error: {msg}"),
        }
    }
}

impl std::error::Error for DynamoError {}
