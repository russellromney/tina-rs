//! Public message, request, response, value, error, and config types.

use std::path::PathBuf;
use std::time::Duration;

/// Hard cap on `mailbox_capacity` so a stray `usize::MAX` can't make
/// the mailbox effectively unbounded.
pub(crate) const MAX_MAILBOX_CAPACITY: usize = 1 << 20;

/// SQLite value at the bridge boundary. No typed row mapping yet;
/// callers see what SQLite returned.
#[derive(Debug, Clone, PartialEq)]
pub enum SqliteValue {
    /// SQL `NULL`.
    Null,
    /// SQLite `INTEGER` (signed 64-bit).
    Integer(i64),
    /// SQLite `REAL` (IEEE 754 double).
    Real(f64),
    /// SQLite `TEXT` (UTF-8).
    Text(String),
    /// SQLite `BLOB`.
    Blob(Vec<u8>),
}

/// One bridged SQLite operation.
///
/// Multi-statement SQL is not validated here; rusqlite's
/// `execute` / `prepare` reject it with
/// [`SqliteError::Sqlite`]. Use one statement per request.
#[derive(Debug, Clone)]
pub enum SqliteRequest {
    /// Run one row-less statement. Reply carries `rows_changed`.
    Execute {
        /// SQL text.
        sql: String,
        /// Positional parameters bound in order.
        params: Vec<SqliteValue>,
    },
    /// Run one query. Past the row cap, surfaces
    /// [`SqliteError::ResponseTooLarge`] rather than truncating —
    /// callers who want truncation should use a SQL `LIMIT`.
    QueryRows {
        /// SQL text.
        sql: String,
        /// Positional parameters bound in order.
        params: Vec<SqliteValue>,
        /// Caller's row buffer cap. Bridge applies the lesser of this
        /// and [`SqliteConfig::max_response_rows`].
        max_rows: usize,
    },
}

/// One terminal SQLite reply.
#[derive(Debug, Clone, PartialEq)]
pub enum SqliteResponse {
    /// `Execute` outcome.
    Executed {
        /// Rows changed, as reported by SQLite.
        rows_changed: u64,
    },
    /// `QueryRows` outcome.
    Rows {
        /// Column names in result order.
        columns: Vec<String>,
        /// Buffered rows. Cells match column order.
        rows: Vec<Vec<SqliteValue>>,
    },
}

/// Worker-side outcome of one bridged SQLite call. Layered with the
/// runtime's `CallOutcome`: ingress / IsolateCall failures stay there,
/// worker errors come here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqliteError {
    /// Worker `max_in_flight` cap rejected admission.
    Full,
    /// Worker has been closed and rejects new work.
    Closed,
    /// Bridge per-attempt deadline elapsed. Worker thread keeps running;
    /// its terminal outcome lands in metrics as `late_results`.
    Timeout,
    /// Rejected before the worker thread saw it (empty SQL, too many
    /// params, etc.).
    InvalidRequest(String),
    /// Row buffer exceeded the configured cap.
    ResponseTooLarge,
    /// `SQLITE_BUSY` / `SQLITE_LOCKED`.
    Busy,
    /// Constraint violation (UNIQUE, CHECK, ...).
    Constraint(String),
    /// I/O-class SQLite error.
    Io(String),
    /// Catch-all SQLite error with message preserved.
    Sqlite(String),
    /// Bridge invariant failed (e.g. worker thread died mid-request).
    /// Never used to hide a SQLite error.
    Internal(String),
}

impl std::fmt::Display for SqliteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => f.write_str("sqlite worker: bounded ingress full"),
            Self::Closed => f.write_str("sqlite worker: closed"),
            Self::Timeout => f.write_str("sqlite worker: per-attempt timeout"),
            Self::InvalidRequest(msg) => write!(f, "sqlite worker: invalid request: {msg}"),
            Self::ResponseTooLarge => {
                f.write_str("sqlite worker: response exceeds configured row cap")
            }
            Self::Busy => f.write_str("sqlite worker: SQLITE_BUSY / SQLITE_LOCKED"),
            Self::Constraint(msg) => write!(f, "sqlite worker: constraint violation: {msg}"),
            Self::Io(msg) => write!(f, "sqlite worker: io error: {msg}"),
            Self::Sqlite(msg) => write!(f, "sqlite worker: sqlite error: {msg}"),
            Self::Internal(msg) => write!(f, "sqlite worker: internal: {msg}"),
        }
    }
}

impl std::error::Error for SqliteError {}

/// Where the worker opens its connection.
#[derive(Debug, Clone)]
pub enum SqlitePath {
    /// In-memory (`:memory:`). Handy for tests.
    Memory,
    /// On-disk file.
    File(PathBuf),
}

impl SqlitePath {
    /// File-backed database.
    pub fn file(path: impl Into<PathBuf>) -> Self {
        Self::File(path.into())
    }
}

/// Reasons a [`SqliteConfig`] cannot produce a working worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqliteConfigError {
    /// `mailbox_capacity == 0`.
    ZeroMailboxCapacity,
    /// `mailbox_capacity` over the hard cap.
    MailboxCapacityTooLarge {
        /// Requested capacity.
        requested: usize,
        /// Hard cap.
        cap: usize,
    },
    /// `pending_reply_capacity == 0`. Need at least one slot.
    ZeroPendingReplyCapacity,
    /// `pending_reply_capacity < max_in_flight`. Every accepted request
    /// needs a reply slot.
    PendingReplyCapacityBelowMaxInFlight {
        /// Requested `pending_reply_capacity`.
        pending: usize,
        /// Requested `max_in_flight`.
        in_flight: usize,
    },
    /// `max_in_flight` not pinned to `1`.
    InvalidMaxInFlight {
        /// Requested value.
        requested: usize,
    },
    /// `external_pool_size` not pinned to `1`.
    InvalidExternalPoolSize {
        /// Requested value.
        requested: usize,
    },
    /// `default_timeout == 0`.
    ZeroDefaultTimeout,
    /// `poll_interval == 0`. Loop would never yield.
    ZeroPollInterval,
    /// `max_request_params == 0`.
    ZeroMaxRequestParams,
    /// `max_response_rows == 0`. Use a request `max_rows` of `0` for
    /// "no rows expected"; the config cap must be `>= 1`.
    ZeroMaxResponseRows,
}

impl std::fmt::Display for SqliteConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroMailboxCapacity => f.write_str("mailbox_capacity must be >= 1"),
            Self::MailboxCapacityTooLarge { requested, cap } => {
                write!(f, "mailbox_capacity {requested} exceeds cap {cap}")
            }
            Self::ZeroPendingReplyCapacity => f.write_str("pending_reply_capacity must be >= 1"),
            Self::PendingReplyCapacityBelowMaxInFlight { pending, in_flight } => write!(
                f,
                "pending_reply_capacity {pending} < max_in_flight {in_flight}"
            ),
            Self::InvalidMaxInFlight { requested } => {
                write!(f, "max_in_flight must be exactly 1 (got {requested})")
            }
            Self::InvalidExternalPoolSize { requested } => {
                write!(f, "external_pool_size must be exactly 1 (got {requested})")
            }
            Self::ZeroDefaultTimeout => f.write_str("default_timeout must be > 0"),
            Self::ZeroPollInterval => f.write_str("poll_interval must be > 0"),
            Self::ZeroMaxRequestParams => f.write_str("max_request_params must be >= 1"),
            Self::ZeroMaxResponseRows => f.write_str("max_response_rows must be >= 1"),
        }
    }
}

impl std::error::Error for SqliteConfigError {}

/// Bridge worker configuration.
///
/// Every knob is named. No silent clamps; out-of-range values reject
/// through [`SqliteConfig::validate`].
///
/// Pins:
///
/// - `external_pool_size = 1` (one connection, one blocking worker)
/// - `max_in_flight = 1` (sequential admission)
///
/// Both reject any other value at validate time. A pooled form would
/// lift these pins and inherit the same named caps.
#[derive(Debug, Clone)]
pub struct SqliteConfig {
    /// Where to open the SQLite database.
    pub path: SqlitePath,
    /// External worker pool size. Must be `1`.
    pub external_pool_size: usize,
    /// Max simultaneously-in-flight ops. Must be `1`. Past this, the
    /// worker replies [`SqliteError::Full`].
    pub max_in_flight: usize,
    /// Max admitted callers waiting on a deferred reply. Must be `>= 1`.
    /// With `max_in_flight = 1` this is effectively the same; it is
    /// kept named so a pooled form can grow it independently.
    pub pending_reply_capacity: usize,
    /// Worker mailbox capacity (Tina ingress). Past this, callers see
    /// `CallError::TargetFull` at the runtime layer. Hard-capped at
    /// `2^20` to prevent accidental unbounded wiring.
    pub mailbox_capacity: usize,
    /// Per-attempt deadline. Past this, the bridge surfaces
    /// [`SqliteError::Timeout`]; the worker thread keeps running and
    /// its terminal outcome is recorded as `late_results`.
    pub default_timeout: Duration,
    /// SQLite `busy_timeout` applied at connection open. Bounds how
    /// long SQLite itself waits on `SQLITE_BUSY` before returning.
    /// `Duration::ZERO` is left at SQLite's default (no busy waiting;
    /// `SQLITE_BUSY` returns immediately).
    pub busy_timeout: Duration,
    /// Pragmas executed at startup, in order. Bare statements
    /// (e.g. `"journal_mode = WAL"`); the bridge prepends `PRAGMA `.
    /// A failing pragma surfaces as [`InstallError::Pragma`].
    pub pragmas: Vec<String>,
    /// Poll interval for the worker-result wakeup loop. Smaller =
    /// lower latency, more trace chatter.
    pub poll_interval: Duration,
    /// Max positional params per request. Over the cap rejects with
    /// [`SqliteError::InvalidRequest`] before the worker sees it.
    pub max_request_params: usize,
    /// Bridge-side cap on rows per `QueryRows`. The bridge applies the
    /// lesser of this and the request's own `max_rows`.
    pub max_response_rows: usize,
}

impl SqliteConfig {
    /// In-memory database with conservative defaults.
    pub fn memory() -> Self {
        Self::with_path(SqlitePath::Memory)
    }

    /// On-disk database file with conservative defaults.
    pub fn path(path: impl Into<PathBuf>) -> Self {
        Self::with_path(SqlitePath::File(path.into()))
    }

    /// Defaults for the supplied path.
    pub fn with_path(path: SqlitePath) -> Self {
        Self {
            path,
            external_pool_size: 1,
            max_in_flight: 1,
            pending_reply_capacity: 1,
            mailbox_capacity: 64,
            default_timeout: Duration::from_secs(5),
            busy_timeout: Duration::from_secs(5),
            pragmas: Vec::new(),
            poll_interval: Duration::from_millis(2),
            max_request_params: 64,
            max_response_rows: 4096,
        }
    }

    /// Sets `mailbox_capacity`.
    pub fn with_mailbox_capacity(mut self, capacity: usize) -> Self {
        self.mailbox_capacity = capacity;
        self
    }

    /// Sets `pending_reply_capacity`.
    pub fn with_pending_reply_capacity(mut self, capacity: usize) -> Self {
        self.pending_reply_capacity = capacity;
        self
    }

    /// Sets `default_timeout`.
    pub fn with_default_timeout(mut self, timeout: Duration) -> Self {
        self.default_timeout = timeout;
        self
    }

    /// Sets `busy_timeout`.
    pub fn with_busy_timeout(mut self, timeout: Duration) -> Self {
        self.busy_timeout = timeout;
        self
    }

    /// Adds one pragma to run at startup.
    pub fn with_pragma(mut self, pragma: impl Into<String>) -> Self {
        self.pragmas.push(pragma.into());
        self
    }

    /// Sets `poll_interval`.
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Sets `max_request_params`.
    pub fn with_max_request_params(mut self, max: usize) -> Self {
        self.max_request_params = max;
        self
    }

    /// Sets `max_response_rows`.
    pub fn with_max_response_rows(mut self, max: usize) -> Self {
        self.max_response_rows = max;
        self
    }

    /// Sets `max_in_flight`. [`Self::validate`] rejects anything other
    /// than `1`; setter is for tests and forward-compat.
    pub fn with_max_in_flight(mut self, max: usize) -> Self {
        self.max_in_flight = max;
        self
    }

    /// Sets `external_pool_size`. [`Self::validate`] rejects anything
    /// other than `1`.
    pub fn with_external_pool_size(mut self, size: usize) -> Self {
        self.external_pool_size = size;
        self
    }

    /// Validates the config without modifying it.
    pub fn validate(&self) -> Result<(), SqliteConfigError> {
        if self.mailbox_capacity == 0 {
            return Err(SqliteConfigError::ZeroMailboxCapacity);
        }
        if self.mailbox_capacity > MAX_MAILBOX_CAPACITY {
            return Err(SqliteConfigError::MailboxCapacityTooLarge {
                requested: self.mailbox_capacity,
                cap: MAX_MAILBOX_CAPACITY,
            });
        }
        if self.max_in_flight != 1 {
            return Err(SqliteConfigError::InvalidMaxInFlight {
                requested: self.max_in_flight,
            });
        }
        if self.external_pool_size != 1 {
            return Err(SqliteConfigError::InvalidExternalPoolSize {
                requested: self.external_pool_size,
            });
        }
        if self.pending_reply_capacity == 0 {
            return Err(SqliteConfigError::ZeroPendingReplyCapacity);
        }
        if self.pending_reply_capacity < self.max_in_flight {
            return Err(SqliteConfigError::PendingReplyCapacityBelowMaxInFlight {
                pending: self.pending_reply_capacity,
                in_flight: self.max_in_flight,
            });
        }
        if self.default_timeout.is_zero() {
            return Err(SqliteConfigError::ZeroDefaultTimeout);
        }
        if self.poll_interval.is_zero() {
            return Err(SqliteConfigError::ZeroPollInterval);
        }
        if self.max_request_params == 0 {
            return Err(SqliteConfigError::ZeroMaxRequestParams);
        }
        if self.max_response_rows == 0 {
            return Err(SqliteConfigError::ZeroMaxResponseRows);
        }
        Ok(())
    }
}

/// Why install failed. Each variant points at one phase of bring-up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallError {
    /// Config rejected by [`SqliteConfig::validate`].
    Config(SqliteConfigError),
    /// Could not open the SQLite connection.
    Open(String),
    /// `busy_timeout` or other connection setup call failed.
    Setup(String),
    /// One of the configured pragmas failed.
    Pragma(String),
    /// Could not spawn the blocking worker thread.
    Spawn(String),
    /// Tina runtime registration failed.
    Register(String),
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(e) => write!(f, "sqlite bridge install: {e}"),
            Self::Open(msg) => write!(f, "sqlite bridge install: open: {msg}"),
            Self::Setup(msg) => write!(f, "sqlite bridge install: setup: {msg}"),
            Self::Pragma(msg) => write!(f, "sqlite bridge install: pragma: {msg}"),
            Self::Spawn(msg) => write!(f, "sqlite bridge install: spawn: {msg}"),
            Self::Register(msg) => write!(f, "sqlite bridge install: register: {msg}"),
        }
    }
}

impl std::error::Error for InstallError {}

impl From<SqliteConfigError> for InstallError {
    fn from(e: SqliteConfigError) -> Self {
        Self::Config(e)
    }
}
