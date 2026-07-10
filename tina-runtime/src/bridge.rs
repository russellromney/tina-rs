//! Shared bridge vocabulary.
//!
//! Bridges glue Tina to messy outside systems (AWS SDK, sqlx, sqlite,
//! reqwest, …). Tina can bound admission, observe worker-terminal
//! truth, and own deadlines. Tina cannot always stop the outside
//! system. Every bridge says the same boring things:
//!
//! - install
//! - ready or failed
//! - admit or full
//! - close admission
//! - drain
//! - shutdown
//! - late result
//! - metrics
//! - pressure
//!
//! This module is the named vocabulary for those concepts. SDK-specific
//! types stay in each bridge crate; only the names that every bridge
//! agrees on live here.
//!
//! Wrong wiring is a coding mistake, so the rails in this module are
//! compile-time where possible:
//!
//! - [`BridgePressure`] holds private fields; construct only via the
//!   typed constructors or per-bridge `From` impls. No public raw-field
//!   literal can forge an installed-capacity or a name.
//! - [`BridgeOutcomeClass`] is a typed enum with four arms
//!   (`Succeeded`, `Retryable`, `Unavailable`, `Fatal`). A bridge
//!   classifier signature returns this type, not a string. Closed and
//!   pool-closed land in [`BridgeUnavailable`], not in retry fog.
//!   Unknown SDK errors land in [`BridgeFatal::SdkUnknown`], not in
//!   [`BridgeRetryable::SdkRetryable`].
//!
//! The vocabulary is read-shaped: callers, dashboards, and replay
//! suites can grep one set of names across every bridge.

use std::fmt;
use std::time::Duration;

use tina::capacity::{CapacityMode, CapacitySurfaceReport};

use crate::service_pressure::{ServicePressureSurface, SurfaceKind};

// -------------------------------------------------------------------
// Classifier vocabulary
// -------------------------------------------------------------------

/// Coarse outcome class for a bridge call.
///
/// Every per-bridge classifier returns this enum. A caller-owned retry
/// loop only needs to ask one question: was it a success, can I
/// plausibly retry, is the bridge/pool/resource unavailable, or is this
/// dead. The classifier never retries, never backs off, and never
/// guesses idempotency.
///
/// Categories are deliberately narrow:
///
/// - `Succeeded` – worker reached terminal Ok.
/// - `Retryable` – worker can plausibly accept the same request again
///   after backoff, **if the caller's idempotency rules permit it**.
/// - `Unavailable` – the bridge, pool, or resource is closed. A new
///   bridge/pool/service may be needed; blindly retrying the same
///   handle is wrong.
/// - `Fatal` – input, permissions, schema, or code must change first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeOutcomeClass {
    /// SDK round-trip succeeded.
    Succeeded,
    /// Caller may retry, subject to their own idempotency rules.
    Retryable(BridgeRetryable),
    /// Bridge/pool/resource is closed. Not a retry candidate on the
    /// same handle.
    Unavailable(BridgeUnavailable),
    /// Request will not succeed on retry without changing inputs.
    Fatal(BridgeFatal),
}

impl BridgeOutcomeClass {
    /// True iff [`BridgeOutcomeClass::Succeeded`].
    pub fn is_success(&self) -> bool {
        matches!(self, BridgeOutcomeClass::Succeeded)
    }

    /// True iff the caller may retry under their idempotency story.
    pub fn is_retryable(&self) -> bool {
        matches!(self, BridgeOutcomeClass::Retryable(_))
    }

    /// True iff the bridge/pool/resource is closed.
    pub fn is_unavailable(&self) -> bool {
        matches!(self, BridgeOutcomeClass::Unavailable(_))
    }

    /// True iff the request is dead without input or setup changes.
    pub fn is_fatal(&self) -> bool {
        matches!(self, BridgeOutcomeClass::Fatal(_))
    }
}

/// Why the call is retryable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeRetryable {
    /// Tina bridge admission was saturated (`max_in_flight` full).
    BridgeFull,
    /// The caller's own IsolateCall deadline fired before the bridge
    /// replied. The bridge cannot say whether the outside work
    /// finished.
    CallerTimeout,
    /// The bridge's per-operation deadline fired. Late terminal SDK
    /// completion may still land and increment `late_result_count`.
    BridgeTimeout,
    /// Service throttled (HTTP 429, SDK throttle metadata).
    ServiceThrottled,
    /// DynamoDB-style provisioned-throughput rejection.
    ProvisionedThroughputExceeded,
    /// DynamoDB-style transaction conflict.
    TransactionConflict,
    /// SDK metadata explicitly marked the error retryable (e.g.
    /// throttle/conflict). Bridges that cannot prove the SDK said so
    /// must use [`BridgeFatal::SdkUnknown`] instead.
    SdkRetryable,
    /// Connection-pool acquire timed out (sqlx-shaped bridges).
    PoolAcquireTimeout,
}

/// Why the bridge/pool/resource is unavailable.
///
/// "Unavailable" is **not** a retry hint. The caller needs a new
/// bridge handle, a new pool, or a different service. Retrying on the
/// closed handle will only re-observe `Unavailable`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeUnavailable {
    /// Tina bridge closed admission while the call was in flight or
    /// before it was admitted.
    BridgeClosed,
    /// Underlying connection pool is closed (sqlx-shaped bridges).
    PoolClosed,
}

/// Why the request is fatal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeFatal {
    /// Bridge validation rejected the request shape.
    InvalidRequest,
    /// Request payload exceeded a configured cap.
    TooLarge,
    /// Caller-supplied condition expression failed (DynamoDB
    /// conditional writes, similar shapes).
    ConditionalCheckFailed,
    /// Resource (table, queue, topic, secret, key, endpoint) does not
    /// exist.
    NotFound,
    /// SDK or service rejected the parameters as invalid.
    InvalidParameter,
    /// Caller has no permission for the operation.
    AccessDenied,
    /// KMS / envelope-encryption decryption failed.
    DecryptionFailed,
    /// Response could not be decoded into the caller's typed shape.
    Decode,
    /// SDK error with no retryable/throttled metadata. The bridge
    /// must **not** classify it as retryable: doing so is the retry
    /// fog the vocabulary is meant to prevent.
    SdkUnknown,
    /// Worker hit an internal failure (bug, runtime panic, or
    /// invariant violation).
    Internal,
}

// -------------------------------------------------------------------
// Pressure vocabulary
// -------------------------------------------------------------------

/// Why a bridge surface name was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeNameError {
    /// Name is empty after trimming.
    Empty,
    /// Name contains an ASCII control character or whitespace.
    InvalidChar(char),
}

impl fmt::Display for BridgeNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BridgeNameError::Empty => f.write_str("bridge surface name is empty"),
            BridgeNameError::InvalidChar(c) => {
                write!(f, "bridge surface name contains invalid char: {c:?}")
            }
        }
    }
}

impl std::error::Error for BridgeNameError {}

fn validate_bridge_name(raw: &str) -> Result<(), BridgeNameError> {
    if raw.trim().is_empty() {
        return Err(BridgeNameError::Empty);
    }
    if let Some(bad) = raw.chars().find(|c| c.is_control() || c.is_whitespace()) {
        return Err(BridgeNameError::InvalidChar(bad));
    }
    Ok(())
}

/// Measured pressure shape every bridge can render.
///
/// The fields are **private**. Construct one of:
///
/// - [`BridgePressure::measured`] for the validated, named, typed shape
/// - a per-bridge `From<XxxPressureReport>` impl (lives in the bridge
///   crate; the type system makes sure the impl uses the typed
///   constructor too)
///
/// User code cannot forge a `BridgePressure` with a raw struct literal:
/// the fields are not `pub`. That keeps lying about installed capacity
/// (e.g. passing a fresh config to the metrics handle) out of the
/// supported path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgePressure {
    name: String,
    capacity: usize,
    current: usize,
    high_water: u64,
    full_count: u64,
    timeout_count: u64,
    closed_count: u64,
    late_result_count: u64,
    worker_terminal_count: u64,
}

impl BridgePressure {
    /// Build a measured bridge pressure report. Name must be non-empty
    /// and contain no control or whitespace characters; this is what
    /// stops a typo from silently re-naming a dashboard surface.
    ///
    /// `capacity` is the **installed** bridge admission cap, not what
    /// any caller-supplied config currently says. Use the metrics
    /// handle's view, not a fresh config — the rule the AWS metrics
    /// already encode.
    ///
    /// `current` is the in-flight count. `high_water` is the all-time
    /// max in-flight.
    ///
    /// `late_result_count` counts worker terminals that landed after
    /// the caller had given up. Use `0` only when the bridge cannot
    /// observe late terminal completion (then say so in docs).
    ///
    /// `worker_terminal_count` counts worker terminals the bridge
    /// observed without the caller still listening; conservatively use
    /// the same value as `late_result_count` when the bridge can only
    /// see "late" but not "all worker terminals".
    #[allow(clippy::too_many_arguments)]
    pub fn measured(
        name: impl Into<String>,
        capacity: usize,
        current: usize,
        high_water: u64,
        full_count: u64,
        timeout_count: u64,
        closed_count: u64,
        late_result_count: u64,
        worker_terminal_count: u64,
    ) -> Result<Self, BridgeNameError> {
        let name = name.into();
        validate_bridge_name(&name)?;
        Ok(Self {
            name,
            capacity,
            current,
            high_water,
            full_count,
            timeout_count,
            closed_count,
            late_result_count,
            worker_terminal_count,
        })
    }

    /// Build an explicit "this bridge surface cannot be measured"
    /// report. The discovery line stays grep-friendly and never silently
    /// omits the surface — that's the whole point of `Unavailable`
    /// vocabulary in the rest of the runtime.
    pub fn unavailable(
        name: impl Into<String>,
        reason: impl Into<String>,
    ) -> Result<ServicePressureSurface, BridgeNameError> {
        let name = name.into();
        validate_bridge_name(&name)?;
        Ok(ServicePressureSurface::unavailable(
            name,
            "bridge",
            reason.into(),
        ))
    }

    /// Stable, validated surface name (e.g. `aws.s3`, `pg.pool`).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Installed admission capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// In-flight count.
    pub fn current(&self) -> usize {
        self.current
    }

    /// All-time max in-flight.
    pub fn high_water(&self) -> u64 {
        self.high_water
    }

    /// Cumulative admission-full rejections.
    pub fn full_count(&self) -> u64 {
        self.full_count
    }

    /// Cumulative caller/bridge-timeout count.
    pub fn timeout_count(&self) -> u64 {
        self.timeout_count
    }

    /// Cumulative admission-closed rejections.
    pub fn closed_count(&self) -> u64 {
        self.closed_count
    }

    /// Cumulative late worker-terminal results (caller had given up).
    pub fn late_result_count(&self) -> u64 {
        self.late_result_count
    }

    /// Cumulative worker-terminal observations the bridge counted.
    pub fn worker_terminal_count(&self) -> u64 {
        self.worker_terminal_count
    }

    /// Render as a [`CapacitySurfaceReport`] for the runtime capacity
    /// vocabulary. `mode` is the bridge author's choice: typically
    /// [`CapacityMode::Fixed`] for measured surfaces.
    pub fn capacity_surface(&self, mode: CapacityMode) -> CapacitySurfaceReport {
        CapacitySurfaceReport::count(
            self.name.clone(),
            mode,
            self.capacity,
            self.current,
            self.high_water as usize,
            self.full_count,
        )
    }

    /// Render as a `ServicePressureSurface` so service-pressure builders
    /// can ingest it directly. `kind` is the suggested surface kind
    /// (typically `"bridge"`).
    pub fn service_pressure_surface(&self, kind: SurfaceKind) -> ServicePressureSurface {
        ServicePressureSurface::measured(
            self.name.clone(),
            kind,
            self.capacity_surface(CapacityMode::Fixed),
        )
    }
}

// -------------------------------------------------------------------
// Lifecycle vocabulary
// -------------------------------------------------------------------

/// How a bridge close request was interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeCloseMode {
    /// Reject all *new* admission. In-flight work continues.
    CloseAdmission,
    /// Reject new admission *and* wait up to a deadline for in-flight
    /// work to drain.
    CloseAndDrain,
}

/// Was admission already closed when the close call arrived?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeCloseAdmission {
    /// Admission flipped from open to closed by this call.
    NewlyClosed,
    /// Admission was already closed before this call.
    AlreadyClosed,
}

/// Outcome of a `close_and_drain` deadline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeDrainReport {
    closed: bool,
    drained: bool,
    in_flight_remaining: u64,
    in_flight_kinds: Vec<(&'static str, u64)>,
    waited: Duration,
}

impl BridgeDrainReport {
    /// Build a drain report. Bridges call this from their own
    /// `close_and_drain` and convert their richer per-bridge report
    /// into the shared vocabulary via [`From`] when handing the report
    /// back to dashboards.
    pub fn new(
        closed: bool,
        drained: bool,
        in_flight_remaining: u64,
        in_flight_kinds: Vec<(&'static str, u64)>,
        waited: Duration,
    ) -> Self {
        Self {
            closed,
            drained,
            in_flight_remaining,
            in_flight_kinds,
            waited,
        }
    }

    /// True iff admission is closed at end of drain.
    pub fn closed(&self) -> bool {
        self.closed
    }

    /// True iff in-flight count reached zero before the deadline.
    pub fn drained(&self) -> bool {
        self.drained
    }

    /// In-flight count remaining when the report was taken.
    pub fn in_flight_remaining(&self) -> u64 {
        self.in_flight_remaining
    }

    /// Per-operation in-flight breakdown.
    pub fn in_flight_kinds(&self) -> &[(&'static str, u64)] {
        &self.in_flight_kinds
    }

    /// Total wall-clock waited during this drain.
    pub fn waited(&self) -> Duration {
        self.waited
    }
}

/// One worker-terminal observation that landed after the caller had
/// given up (caller timed out, cancelled, or the bridge per-op deadline
/// fired).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeLateResultReport {
    operation: &'static str,
    classification: BridgeOutcomeClass,
}

impl BridgeLateResultReport {
    /// Build a late-result report from a per-op label plus the
    /// shared-vocabulary classification.
    pub fn new(operation: &'static str, classification: BridgeOutcomeClass) -> Self {
        Self {
            operation,
            classification,
        }
    }

    /// Stable per-op label (e.g. `"put_object"`, `"query"`).
    pub fn operation(&self) -> &'static str {
        self.operation
    }

    /// The shared-vocabulary classification of the late terminal.
    pub fn classification(&self) -> &BridgeOutcomeClass {
        &self.classification
    }
}

/// Worker-terminal truth, separate from caller-observed truth.
///
/// A bridge measures `BridgeTerminal::Reached(class)` when the worker
/// finished an SDK round-trip and the class was computed. The caller
/// may or may not have still been listening; if they were not, the
/// bridge also emits a [`BridgeLateResultReport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeTerminal {
    /// Worker reached a terminal outcome (success or classified
    /// failure).
    Reached(BridgeOutcomeClass),
    /// Worker exited without reaching terminal (panic, runtime stop).
    Aborted,
}

/// Honest warning the caller observes about external work.
///
/// Bridges cannot always stop the outside system. When the caller's
/// own deadline fires before the worker terminates, the bridge replies
/// with a [`BridgeCallerWarning::ExternalWorkMayContinue`] flag so the
/// caller knows the SDK call may still land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeCallerWarning {
    /// Caller observed the bridge reply before any deadline; outside
    /// work is fully accounted for.
    None,
    /// Caller's IsolateCall deadline or cancellation fired first. The
    /// outside system may still complete the work; a late terminal
    /// will appear in the bridge late-result metrics.
    ExternalWorkMayContinue,
}

impl BridgeCallerWarning {
    /// Project a shared-vocabulary outcome into the honest warning the
    /// caller should attach to a reply or surface in a log line.
    ///
    /// - [`BridgeOutcomeClass::Succeeded`] and
    ///   [`BridgeOutcomeClass::Fatal`] are caller-observed truths;
    ///   neither implies external work may continue.
    /// - [`BridgeRetryable::CallerTimeout`] and
    ///   [`BridgeRetryable::BridgeTimeout`] are the load-bearing case:
    ///   Tina stopped waiting, but the outside system may still finish.
    /// - Other retryable / unavailable categories happened before the
    ///   bridge dispatched work, so external work is *not* in flight.
    pub fn from_outcome(class: &BridgeOutcomeClass) -> Self {
        match class {
            BridgeOutcomeClass::Retryable(BridgeRetryable::CallerTimeout)
            | BridgeOutcomeClass::Retryable(BridgeRetryable::BridgeTimeout) => {
                BridgeCallerWarning::ExternalWorkMayContinue
            }
            _ => BridgeCallerWarning::None,
        }
    }
}

// -------------------------------------------------------------------
// Install / closer traits — vocabulary marker traits, not framework
// -------------------------------------------------------------------

/// Vocabulary marker for a bridge install handle.
///
/// A bridge install owns a typed bridge address in its concrete type,
/// plus a closer for admission shutdown and a metrics handle for
/// pressure reporting. The shared trait only standardizes the closer
/// and metrics accessors because bridge address shapes differ. It is
/// the documented vocabulary so reviewers can ask "does this bridge
/// produce an install, a closer, and a metrics handle?" without
/// re-reading each crate's docs.
pub trait BridgeInstall {
    /// The closer companion type.
    type Closer: BridgeCloser;
    /// The metrics handle companion type.
    type Metrics;
    /// Borrow the closer.
    fn closer(&self) -> &Self::Closer;
    /// Borrow the metrics handle.
    fn metrics(&self) -> &Self::Metrics;
}

/// Vocabulary marker for a bridge closer.
///
/// Closers implement the common admission-shutdown shape every bridge
/// needs: idempotent `close()` plus visible `is_closed()`. Drain
/// remains per-bridge typed because each bridge reports different
/// operation kinds, but a drain report should convert into
/// [`BridgeDrainReport`] for cross-bridge tooling.
pub trait BridgeCloser {
    /// Close admission without waiting for drain.
    fn close(&self);
    /// Returns true once `close` has been observed at least once.
    fn is_closed(&self) -> bool;
}

// -------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_validation_rejects_empty() {
        assert_eq!(validate_bridge_name(""), Err(BridgeNameError::Empty));
        assert_eq!(validate_bridge_name("   "), Err(BridgeNameError::Empty));
    }

    #[test]
    fn name_validation_rejects_whitespace_and_control() {
        assert!(matches!(
            validate_bridge_name("aws s3"),
            Err(BridgeNameError::InvalidChar(' '))
        ));
        assert!(matches!(
            validate_bridge_name("aws\ts3"),
            Err(BridgeNameError::InvalidChar('\t'))
        ));
        assert!(matches!(
            validate_bridge_name("aws.\u{0007}s3"),
            Err(BridgeNameError::InvalidChar('\u{0007}'))
        ));
    }

    #[test]
    fn name_validation_accepts_dotted_names() {
        validate_bridge_name("aws.s3").unwrap();
        validate_bridge_name("pg.pool").unwrap();
        validate_bridge_name("sqlite").unwrap();
    }

    #[test]
    fn measured_round_trips_named_fields() {
        let p = BridgePressure::measured("aws.s3", 32, 5, 17, 3, 2, 1, 4, 4).unwrap();
        assert_eq!(p.name(), "aws.s3");
        assert_eq!(p.capacity(), 32);
        assert_eq!(p.current(), 5);
        assert_eq!(p.high_water(), 17);
        assert_eq!(p.full_count(), 3);
        assert_eq!(p.timeout_count(), 2);
        assert_eq!(p.closed_count(), 1);
        assert_eq!(p.late_result_count(), 4);
        assert_eq!(p.worker_terminal_count(), 4);
    }

    #[test]
    fn measured_rejects_bad_names() {
        assert_eq!(
            BridgePressure::measured("", 1, 0, 0, 0, 0, 0, 0, 0).unwrap_err(),
            BridgeNameError::Empty
        );
        let err = BridgePressure::measured("aws s3", 1, 0, 0, 0, 0, 0, 0, 0).unwrap_err();
        assert!(matches!(err, BridgeNameError::InvalidChar(' ')));
    }

    #[test]
    fn capacity_surface_uses_installed_capacity() {
        let p = BridgePressure::measured("aws.s3", 32, 5, 17, 3, 2, 1, 4, 4).unwrap();
        let surface = p.capacity_surface(CapacityMode::Fixed);
        assert_eq!(surface.max_messages, Some(32));
        assert_eq!(surface.current_messages, 5);
        assert_eq!(surface.high_water_messages, 17);
        assert_eq!(surface.full_count, 3);
        assert_eq!(surface.name, "aws.s3");
    }

    #[test]
    fn unavailable_returns_service_surface() {
        let surface = BridgePressure::unavailable("aws.s3", "client not configured").unwrap();
        assert_eq!(surface.name, "aws.s3");
        assert_eq!(surface.kind, "bridge");
        let line = surface.discovery_line();
        assert!(line.contains("state=unavailable"));
        assert!(line.contains("reason=\"client not configured\""));
    }

    #[test]
    fn unavailable_rejects_bad_names() {
        assert!(BridgePressure::unavailable("", "no name").is_err());
        assert!(BridgePressure::unavailable("aws s3", "bad").is_err());
    }

    #[test]
    fn service_pressure_surface_carries_bridge_kind() {
        let p = BridgePressure::measured("aws.s3", 32, 0, 0, 0, 0, 0, 0, 0).unwrap();
        let surface = p.service_pressure_surface("bridge");
        assert_eq!(surface.kind, "bridge");
        assert_eq!(surface.name, "aws.s3");
    }

    #[test]
    fn outcome_class_predicate_helpers_are_exhaustive() {
        assert!(BridgeOutcomeClass::Succeeded.is_success());
        assert!(BridgeOutcomeClass::Retryable(BridgeRetryable::BridgeFull).is_retryable());
        assert!(BridgeOutcomeClass::Unavailable(BridgeUnavailable::BridgeClosed).is_unavailable());
        assert!(BridgeOutcomeClass::Fatal(BridgeFatal::Internal).is_fatal());
    }

    #[test]
    fn closed_classifies_as_unavailable_not_retryable() {
        // The whole point of `Unavailable` vs `Retryable`: a closed
        // bridge or pool is not a retry candidate on the same handle.
        let closed = BridgeOutcomeClass::Unavailable(BridgeUnavailable::BridgeClosed);
        assert!(!closed.is_retryable());
        assert!(closed.is_unavailable());

        let pool_closed = BridgeOutcomeClass::Unavailable(BridgeUnavailable::PoolClosed);
        assert!(!pool_closed.is_retryable());
        assert!(pool_closed.is_unavailable());
    }

    #[test]
    fn sdk_unknown_classifies_as_fatal_not_retryable() {
        // The whole point of `SdkUnknown` vs `SdkRetryable`: unknown
        // SDK errors must not be classified as retryable fog.
        let unknown = BridgeOutcomeClass::Fatal(BridgeFatal::SdkUnknown);
        assert!(!unknown.is_retryable());
        assert!(unknown.is_fatal());
    }

    #[test]
    fn debug_tokens_are_stable() {
        assert_eq!(format!("{:?}", BridgeOutcomeClass::Succeeded), "Succeeded");
        assert_eq!(
            format!(
                "{:?}",
                BridgeOutcomeClass::Retryable(BridgeRetryable::BridgeFull)
            ),
            "Retryable(BridgeFull)"
        );
        assert_eq!(
            format!(
                "{:?}",
                BridgeOutcomeClass::Unavailable(BridgeUnavailable::PoolClosed)
            ),
            "Unavailable(PoolClosed)"
        );
        assert_eq!(
            format!("{:?}", BridgeOutcomeClass::Fatal(BridgeFatal::SdkUnknown)),
            "Fatal(SdkUnknown)"
        );
    }

    #[test]
    fn drain_report_exposes_fields_through_typed_accessors() {
        let report = BridgeDrainReport::new(
            true,
            false,
            3,
            vec![("put_object", 2), ("head_object", 1)],
            Duration::from_millis(50),
        );
        assert!(report.closed());
        assert!(!report.drained());
        assert_eq!(report.in_flight_remaining(), 3);
        assert_eq!(report.in_flight_kinds().len(), 2);
        assert_eq!(report.waited(), Duration::from_millis(50));
    }

    #[test]
    fn late_result_report_carries_typed_classification() {
        let late = BridgeLateResultReport::new("put_object", BridgeOutcomeClass::Succeeded);
        assert_eq!(late.operation(), "put_object");
        assert_eq!(late.classification(), &BridgeOutcomeClass::Succeeded);
    }

    #[test]
    fn caller_warning_distinguishes_external_continuation() {
        let warn = BridgeCallerWarning::ExternalWorkMayContinue;
        assert_ne!(warn, BridgeCallerWarning::None);
    }

    #[test]
    fn caller_warning_from_outcome_flags_only_timeout_arms() {
        // Timeouts are the load-bearing case: bridge stopped waiting,
        // outside system may still finish. Everything else is
        // caller-observed truth — no warning to attach.
        assert_eq!(
            BridgeCallerWarning::from_outcome(&BridgeOutcomeClass::Retryable(
                BridgeRetryable::CallerTimeout
            )),
            BridgeCallerWarning::ExternalWorkMayContinue
        );
        assert_eq!(
            BridgeCallerWarning::from_outcome(&BridgeOutcomeClass::Retryable(
                BridgeRetryable::BridgeTimeout
            )),
            BridgeCallerWarning::ExternalWorkMayContinue
        );
        // Succeeded / Fatal / Full / Closed are caller-observed; no
        // external work was admitted, or it terminated cleanly.
        assert_eq!(
            BridgeCallerWarning::from_outcome(&BridgeOutcomeClass::Succeeded),
            BridgeCallerWarning::None
        );
        assert_eq!(
            BridgeCallerWarning::from_outcome(&BridgeOutcomeClass::Retryable(
                BridgeRetryable::BridgeFull
            )),
            BridgeCallerWarning::None
        );
        assert_eq!(
            BridgeCallerWarning::from_outcome(&BridgeOutcomeClass::Unavailable(
                BridgeUnavailable::BridgeClosed
            )),
            BridgeCallerWarning::None
        );
        assert_eq!(
            BridgeCallerWarning::from_outcome(&BridgeOutcomeClass::Fatal(
                BridgeFatal::AccessDenied
            )),
            BridgeCallerWarning::None
        );
    }

    #[test]
    fn name_validation_rejects_newline_tab_and_null() {
        assert!(matches!(
            validate_bridge_name("aws\ns3"),
            Err(BridgeNameError::InvalidChar('\n'))
        ));
        assert!(matches!(
            validate_bridge_name("aws\r\ns3"),
            Err(BridgeNameError::InvalidChar('\r'))
        ));
        assert!(matches!(
            validate_bridge_name("aws\0s3"),
            Err(BridgeNameError::InvalidChar('\0'))
        ));
        // Inner unicode whitespace (NBSP).
        assert!(matches!(
            validate_bridge_name("aws\u{00A0}s3"),
            Err(BridgeNameError::InvalidChar('\u{00A0}'))
        ));
    }

    #[test]
    fn name_validation_accepts_dots_underscores_hyphens_digits() {
        validate_bridge_name("aws.s3.pressure_v2").unwrap();
        validate_bridge_name("pg-bridge").unwrap();
        validate_bridge_name("shard_0").unwrap();
        validate_bridge_name("svc:rpc/1").unwrap();
    }

    #[test]
    fn measured_allows_zero_capacity_for_degenerate_bridges() {
        // A bridge that has not yet installed any capacity (or one
        // that intentionally exposes only the unavailable surface)
        // should still be expressible. The dashboard will see
        // capacity=0 and act accordingly.
        let p = BridgePressure::measured("test.empty", 0, 0, 0, 0, 0, 0, 0, 0).unwrap();
        assert_eq!(p.capacity(), 0);
        let surface = p.capacity_surface(CapacityMode::Fixed);
        assert_eq!(surface.max_messages, Some(0));
    }

    #[test]
    fn capacity_surface_respects_caller_chosen_mode() {
        let p = BridgePressure::measured("aws.s3", 32, 5, 17, 3, 2, 1, 4, 4).unwrap();
        // Bridge author may use Tuning during discovery work; the
        // rendered surface should carry exactly the mode they chose.
        let tuning = p.capacity_surface(CapacityMode::Tuning);
        assert!(matches!(tuning.mode, CapacityMode::Tuning));
        let fixed = p.capacity_surface(CapacityMode::Fixed);
        assert!(matches!(fixed.mode, CapacityMode::Fixed));
    }

    #[test]
    fn service_pressure_report_consumes_bridge_pressure_end_to_end() {
        use crate::service_pressure::ServicePressureReport;
        // The service pressure builder can consume a BridgePressure via
        // service_pressure_surface and produce a meaningful discovery line
        // plus capacity_summary.
        let s3 = BridgePressure::measured("aws.s3", 32, 5, 17, 3, 0, 0, 4, 4).unwrap();
        let pg = BridgePressure::measured("pg.bridge", 8, 2, 6, 1, 0, 0, 0, 0).unwrap();
        let absent = BridgePressure::unavailable("aws.sqs", "no client supplied").unwrap();

        let mut report = ServicePressureReport::new("checkout");
        report.add_surface(s3.service_pressure_surface("bridge"));
        report.add_surface(pg.service_pressure_surface("bridge"));
        report.add_surface(absent);

        let summary = report.summary_line();
        assert!(summary.contains("service=checkout"), "{summary}");
        assert!(summary.contains("surfaces=3"), "{summary}");
        assert!(summary.contains("measured=2"), "{summary}");
        assert!(summary.contains("unavailable=1"), "{summary}");
        // s3 has full_count=3, pg has full_count=1 → 4 across measured.
        assert!(summary.contains("full=4"), "{summary}");
        assert!(summary.contains("aws.sqs=unavailable"), "{summary}");

        let cap = report.capacity_summary().unwrap();
        assert_eq!(cap.len(), 2);
        assert_eq!(
            cap.surface("aws.s3").report().unwrap().max_messages,
            Some(32)
        );
        assert_eq!(
            cap.surface("pg.bridge").report().unwrap().max_messages,
            Some(8)
        );
        assert!(report.any_full(), "{summary}");

        let discovery = report.discovery_report();
        let lines: Vec<&str> = discovery.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().any(|l| l.contains("aws.s3")));
        assert!(lines.iter().any(|l| l.contains("pg.bridge")));
        assert!(
            lines
                .iter()
                .any(|l| l.contains("aws.sqs") && l.contains("state=unavailable"))
        );
    }

    #[test]
    fn bridge_install_trait_implementations_are_callable() {
        // Tiny test types — prove the trait shape is implementable
        // outside the runtime crate (i.e. that downstream bridges can
        // opt in to the vocabulary).
        struct TestCloser {
            closed: std::sync::atomic::AtomicBool,
        }
        impl BridgeCloser for TestCloser {
            fn close(&self) {
                self.closed
                    .store(true, std::sync::atomic::Ordering::Release);
            }
            fn is_closed(&self) -> bool {
                self.closed.load(std::sync::atomic::Ordering::Acquire)
            }
        }
        struct TestMetrics;
        struct TestInstall {
            closer: TestCloser,
            metrics: TestMetrics,
        }
        impl BridgeInstall for TestInstall {
            type Closer = TestCloser;
            type Metrics = TestMetrics;
            fn closer(&self) -> &TestCloser {
                &self.closer
            }
            fn metrics(&self) -> &TestMetrics {
                &self.metrics
            }
        }
        let install = TestInstall {
            closer: TestCloser {
                closed: std::sync::atomic::AtomicBool::new(false),
            },
            metrics: TestMetrics,
        };
        assert!(!install.closer().is_closed());
        install.closer().close();
        assert!(install.closer().is_closed());
        // Suppress unused-binding warning on the metrics accessor.
        let _ = install.metrics();
    }
}
