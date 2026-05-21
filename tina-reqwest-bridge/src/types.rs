//! Public message, request, response, error, and config types.

use std::time::Duration;

use http::header::HeaderMap;
use http::method::Method;
use http::status::StatusCode;

/// Cap on `mailbox_capacity` to keep accidental "max usize" wiring from
/// quietly turning the mailbox into an unbounded queue.
pub(crate) const MAX_MAILBOX_CAPACITY: usize = 1 << 20;

/// One outbound HTTP request.
///
/// First form is full-response: the body field is a fully-buffered
/// `Vec<u8>` for both request and response. Streaming is a non-goal in
/// this slice.
#[derive(Debug, Clone)]
pub struct ReqwestRequest {
    /// HTTP method.
    pub method: Method,
    /// Absolute URL (`http://...` or `https://...`).
    pub url: String,
    /// Request headers. The bridge does not inject `Content-Length`,
    /// `User-Agent`, or any other header — set them here if you want
    /// them on the wire.
    pub headers: HeaderMap,
    /// Request body. Capped by [`ReqwestConfig::request_body_limit`].
    pub body: Vec<u8>,
    /// Optional per-request timeout that overrides
    /// [`ReqwestConfig::default_timeout`].
    ///
    /// The timeout applies to the reqwest attempt itself (connect +
    /// send + receive). Once an attempt exceeds this duration the
    /// bridge aborts the spawned Tokio task, surfaces
    /// [`ReqwestError::Timeout`], and counts the outcome. Bytes already
    /// on the wire may still be observed by the upstream peer; timeout
    /// is not a proof that the server saw no request.
    pub timeout: Option<Duration>,
}

impl ReqwestRequest {
    /// Builds a `GET` request to `url` with no headers and an empty body.
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            method: Method::GET,
            url: url.into(),
            headers: HeaderMap::new(),
            body: Vec::new(),
            timeout: None,
        }
    }

    /// Builds a `POST` request to `url` with `body`.
    pub fn post(url: impl Into<String>, body: Vec<u8>) -> Self {
        Self {
            method: Method::POST,
            url: url.into(),
            headers: HeaderMap::new(),
            body,
            timeout: None,
        }
    }
}

/// One successful HTTP response.
#[derive(Debug, Clone)]
pub struct ReqwestResponse {
    /// HTTP status code from the upstream server.
    pub status: StatusCode,
    /// Response headers as observed on the wire.
    pub headers: HeaderMap,
    /// Response body. Capped by [`ReqwestConfig::response_body_limit`];
    /// a body that exceeds the cap surfaces as
    /// [`ReqwestError::ResponseTooLarge`] instead of a truncated body.
    pub body: Vec<u8>,
}

/// Outcome of one bridged HTTP call.
///
/// Every error path the bridge can produce maps to exactly one variant.
/// Reqwest-internal errors collapse to [`ReqwestError::Reqwest`] with
/// the underlying message preserved; the bridge does not swallow.
/// Bridge-internal invariant breaks use [`ReqwestError::Internal`] so
/// retry policy cannot mistake them for transient upstream IO.
///
/// `PartialEq`/`Eq` compare string-bearing variants
/// ([`ReqwestError::InvalidRequest`], [`ReqwestError::Reqwest`],
/// [`ReqwestError::Internal`])
/// byte-for-byte. Treat those strings as opaque — the underlying
/// reqwest message text can change between releases. Use
/// `matches!(err, ReqwestError::Reqwest(_))` or
/// `matches!(err, ReqwestError::Internal(_))` for variant-only tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReqwestError {
    /// Worker mailbox or `max_in_flight` cap rejected admission.
    Full,
    /// Worker has been closed and rejects new work.
    Closed,
    /// The bridge's per-attempt deadline elapsed and the spawned
    /// reqwest task was aborted.
    ///
    /// This is different from bridges that keep external work leased
    /// until a physical terminal result. Reqwest timeout is a local
    /// abort/cancel outcome: bytes already written to the upstream may
    /// still have effects, but the bridge will not observe or count a
    /// later reqwest terminal result for this attempt.
    Timeout,
    /// Request body exceeded [`ReqwestConfig::request_body_limit`].
    RequestTooLarge,
    /// Response body exceeded [`ReqwestConfig::response_body_limit`].
    ResponseTooLarge,
    /// Request was rejected before reqwest ever saw it (URL parse,
    /// invalid method, header construction).
    InvalidRequest(String),
    /// Underlying reqwest reported an error.
    Reqwest(String),
    /// The bridge observed an internal worker invariant break, such as
    /// a spawned task ending without sending its result.
    Internal(String),
}

impl std::fmt::Display for ReqwestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => f.write_str("reqwest worker: bounded ingress full"),
            Self::Closed => f.write_str("reqwest worker: closed"),
            Self::Timeout => f.write_str("reqwest worker: request timed out"),
            Self::RequestTooLarge => {
                f.write_str("reqwest worker: request body exceeds configured limit")
            }
            Self::ResponseTooLarge => {
                f.write_str("reqwest worker: response body exceeds configured limit")
            }
            Self::InvalidRequest(msg) => write!(f, "reqwest worker: invalid request: {msg}"),
            Self::Reqwest(msg) => write!(f, "reqwest worker: reqwest error: {msg}"),
            Self::Internal(msg) => write!(f, "reqwest worker: internal error: {msg}"),
        }
    }
}

impl std::error::Error for ReqwestError {}

/// Redirect handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirectPolicy {
    /// Do not follow redirects. The 3xx response is surfaced verbatim.
    None,
    /// Follow up to `n` redirects before surfacing the final response or
    /// erroring if the limit is exceeded.
    Limited(u8),
}

impl Default for RedirectPolicy {
    /// First-form default is `Limited(0)` — no redirects. Redirects
    /// are not hidden behind a mature default; teams must opt in.
    fn default() -> Self {
        Self::Limited(0)
    }
}

/// Retry handling.
///
/// Default is no retry. When configured, the worker uses the
/// per-attempt timeout from the request (or `default_timeout`) and
/// retries up to `max_attempts` total attempts with `delay` between
/// them. Each attempt resets its own clock.
///
/// # Bound
///
/// Total wall-clock for one bridged call is at most:
///
/// ```text
/// max_attempts * per_attempt_timeout + (max_attempts - 1) * delay
/// ```
///
/// # Idempotency
///
/// Configuring retry is the user's promise that the configured
/// retry-safe requests are safe to repeat. The bridge also applies a
/// conservative method gate: only HTTP methods that are retry-safe by
/// default (`GET`, `HEAD`, `OPTIONS`, `TRACE`, `PUT`, `DELETE`) are
/// retried. `POST` and `PATCH` surface the first terminal error even
/// when this policy enables timeout or IO retry, because a response
/// body read error can happen after the upstream committed the request.
/// If a non-idempotent operation is safe in your application, encode
/// idempotency at the application layer and use a retry-safe method or
/// a dedicated bridge worker/API that makes that promise explicit.
///
/// # Not retried
///
/// `Full`, `Closed`, `RequestTooLarge`, `ResponseTooLarge`,
/// `InvalidRequest`, and `Internal` are never retried by the worker.
/// They are caller-visible decisions. In particular `Full` is the
/// explicit pressure signal — the caller decides what to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryPolicy {
    /// No retry. One attempt, surface the outcome.
    None,
    /// Bounded retry on transient transport-level failures.
    Bounded {
        /// Total attempts including the first. Must be `>= 1`.
        max_attempts: u8,
        /// Delay between attempts.
        delay: Duration,
        /// Retry [`ReqwestError::Timeout`].
        on_timeout: bool,
        /// Retry [`ReqwestError::Reqwest`] (connect / IO class only).
        on_reqwest_io: bool,
    },
}

impl Default for RetryPolicy {
    /// First-form default is no retry.
    fn default() -> Self {
        Self::None
    }
}

/// Reasons a [`ReqwestConfig`] cannot produce a working worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReqwestConfigError {
    /// `mailbox_capacity` is `0`.
    ZeroMailboxCapacity,
    /// `max_in_flight` is `0`.
    ZeroMaxInFlight,
    /// `poll_interval` is `Duration::ZERO`. The poll loop would not
    /// yield to other isolates.
    ZeroPollInterval,
    /// `mailbox_capacity` exceeds the cap kept to avoid accidentally
    /// wiring an effectively unbounded queue.
    MailboxCapacityTooLarge {
        /// Requested capacity.
        requested: usize,
        /// Maximum permitted capacity.
        cap: usize,
    },
    /// `default_timeout` is `Duration::ZERO`.
    ZeroDefaultTimeout,
    /// `RetryPolicy::Bounded { max_attempts: 0 }`.
    ZeroRetryAttempts,
}

impl std::fmt::Display for ReqwestConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroMailboxCapacity => f.write_str("mailbox_capacity must be >= 1"),
            Self::ZeroMaxInFlight => f.write_str("max_in_flight must be >= 1"),
            Self::ZeroPollInterval => f.write_str("poll_interval must be > 0"),
            Self::MailboxCapacityTooLarge { requested, cap } => {
                write!(f, "mailbox_capacity {requested} exceeds cap {cap}")
            }
            Self::ZeroDefaultTimeout => f.write_str("default_timeout must be > 0"),
            Self::ZeroRetryAttempts => {
                f.write_str("RetryPolicy::Bounded.max_attempts must be >= 1")
            }
        }
    }
}

impl std::error::Error for ReqwestConfigError {}

/// Bridge worker configuration.
///
/// Every knob is named. The bridge does not expose hidden defaults that
/// can change behavior across releases.
#[derive(Debug, Clone)]
pub struct ReqwestConfig {
    /// Worker mailbox capacity (Tina ingress side). Past this, the
    /// caller observes `BridgeError::Full` (or `CallError::TargetFull`
    /// when called via `tina_runtime::call`). Capped at
    /// `2^20` to keep accidental wiring from quietly creating an
    /// unbounded queue.
    pub mailbox_capacity: usize,
    /// Maximum simultaneously-in-flight reqwest tasks. Past this, the
    /// worker replies [`ReqwestError::Full`] before reqwest ever sees
    /// the request.
    pub max_in_flight: usize,
    /// Maximum request body size in bytes. Requests with a larger body
    /// are rejected with [`ReqwestError::RequestTooLarge`] before
    /// reqwest sees them.
    pub request_body_limit: usize,
    /// Maximum response body size in bytes. Responses larger than this
    /// surface as [`ReqwestError::ResponseTooLarge`] instead of being
    /// silently truncated.
    pub response_body_limit: usize,
    /// Default per-request timeout when [`ReqwestRequest::timeout`] is
    /// `None`.
    pub default_timeout: Duration,
    /// Redirect policy.
    pub redirect: RedirectPolicy,
    /// Retry policy.
    pub retry: RetryPolicy,
    /// Poll interval for the bridge's reqwest-task wakeup loop.
    ///
    /// The worker spawns reqwest on a Tokio runtime; the Tina shard
    /// thread needs a deterministic cadence for re-checking the result
    /// channel. Smaller values reduce response latency at the cost of
    /// more sleep + wakeup messages through the runtime trace. The
    /// default is small enough for interactive use and visible in the
    /// trace.
    pub poll_interval: Duration,
}

impl Default for ReqwestConfig {
    /// First-form defaults aimed at "small, visible, conservative."
    fn default() -> Self {
        Self {
            mailbox_capacity: 64,
            max_in_flight: 16,
            request_body_limit: 1 << 20,  // 1 MiB
            response_body_limit: 4 << 20, // 4 MiB
            default_timeout: Duration::from_secs(10),
            redirect: RedirectPolicy::default(),
            retry: RetryPolicy::default(),
            poll_interval: Duration::from_millis(2),
        }
    }
}

impl ReqwestConfig {
    /// Returns a builder-friendly default. Equivalent to
    /// [`ReqwestConfig::default`].
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

    /// Sets `redirect`.
    pub fn with_redirect(mut self, redirect: RedirectPolicy) -> Self {
        self.redirect = redirect;
        self
    }

    /// Sets `retry`.
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Sets `poll_interval`.
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Validates the config without modifying it. Returns a typed
    /// error when any required budget is `0` or out of range.
    pub fn validate(&self) -> Result<(), ReqwestConfigError> {
        if self.mailbox_capacity == 0 {
            return Err(ReqwestConfigError::ZeroMailboxCapacity);
        }
        if self.mailbox_capacity > MAX_MAILBOX_CAPACITY {
            return Err(ReqwestConfigError::MailboxCapacityTooLarge {
                requested: self.mailbox_capacity,
                cap: MAX_MAILBOX_CAPACITY,
            });
        }
        if self.max_in_flight == 0 {
            return Err(ReqwestConfigError::ZeroMaxInFlight);
        }
        if self.poll_interval.is_zero() {
            return Err(ReqwestConfigError::ZeroPollInterval);
        }
        if self.default_timeout.is_zero() {
            return Err(ReqwestConfigError::ZeroDefaultTimeout);
        }
        if let RetryPolicy::Bounded { max_attempts, .. } = self.retry {
            if max_attempts == 0 {
                return Err(ReqwestConfigError::ZeroRetryAttempts);
            }
        }
        Ok(())
    }
}
