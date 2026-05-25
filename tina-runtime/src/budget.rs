//! Service budget manifest.
//!
//! One structured place for the budgets that bound a real Tina
//! service: mailboxes, pools, body bytes, lanes, protocol sessions,
//! connect attempts, bridge in-flight, event sinks, shard pairs,
//! pending replies/calls, request scopes, and runtime ingress.
//!
//! The manifest does not run anything. It declares caps, validates
//! them before the runtime starts, builds rows from existing configs
//! through adapters, and joins what was declared with what the live
//! [`crate::ServicePressureReport`] / [`crate::CapacitySummary`]
//! actually observed. Capacity, full, and closed facts always come
//! from those live reports — the manifest never guesses live numbers.
//!
//! ```
//! use tina_runtime::budget::{
//!     BudgetCap, BudgetKind, BudgetSurface, BudgetUnit, ServiceBudgetManifest,
//! };
//! use tina::capacity::CapacityPolicy;
//!
//! let mut manifest = ServiceBudgetManifest::new("billing", CapacityPolicy::Production);
//! manifest.add(
//!     BudgetSurface::new(
//!         "billing.controller.mailbox",
//!         BudgetKind::Mailbox,
//!         BudgetUnit::Messages,
//!         BudgetCap::fixed(32),
//!     )
//!     .owned_by("controller"),
//! );
//! manifest.validate().expect("a real cap validates before startup");
//! ```
//!
//! Vocabulary that scores live surfaces lives in [`tina::capacity`];
//! this module sits on top of it.

use std::collections::BTreeMap;
use std::fmt;

use tina::capacity::{CapacityMode, CapacityPolicy, CapacityPolicyError, CapacitySurfaceReport};

use crate::capacity::CapacitySummary;
use crate::service_pressure::{ServicePressureReport, ServiceSurfaceState};

/// Schema version stamped on a manifest, its report, and its replay
/// export. Bump when the surface shape changes in a way that should
/// invalidate a saved replay case.
pub const BUDGET_SCHEMA_VERSION: u32 = 1;

/// What a budget surface bounds. Closed set for this phase: a new
/// kind is a deliberate vocabulary change, not a free-text string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetKind {
    /// Runtime control/ingress queue feeding a shard worker.
    RuntimeIngress,
    /// One source-shard -> destination-shard transport.
    ShardPair,
    /// A bounded runtime IO/work lane (storage, dns, tls, process,
    /// signal, remote-inbound drain).
    RuntimeLane,
    /// An isolate mailbox.
    Mailbox,
    /// A `PendingReplies` slot budget.
    PendingReply,
    /// In-flight runtime calls owed back to a mailbox.
    PendingCall,
    /// A `RequestScope` set.
    RequestScope,
    /// A bounded worker/connection pool.
    Pool,
    /// A byte budget: request/response body or framed payload.
    BodyBytes,
    /// Open protocol sessions/streams (HTTP/2 streams, WS sessions).
    ProtocolSession,
    /// Outbound connect/retry attempts.
    ConnectAttempt,
    /// In-flight work admitted to a bridge.
    BridgeInFlight,
    /// A bounded event sink / fan-out target budget.
    EventSink,
}

impl BudgetKind {
    /// Short, dotted-token-safe label for reports.
    pub fn label(self) -> &'static str {
        match self {
            Self::RuntimeIngress => "runtime_ingress",
            Self::ShardPair => "shard_pair",
            Self::RuntimeLane => "runtime_lane",
            Self::Mailbox => "mailbox",
            Self::PendingReply => "pending_reply",
            Self::PendingCall => "pending_call",
            Self::RequestScope => "request_scope",
            Self::Pool => "pool",
            Self::BodyBytes => "body_bytes",
            Self::ProtocolSession => "protocol_session",
            Self::ConnectAttempt => "connect_attempt",
            Self::BridgeInFlight => "bridge_in_flight",
            Self::EventSink => "event_sink",
        }
    }
}

impl fmt::Display for BudgetKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// What a budget counts. `Weight` carries a user-declared label; Tina
/// never claims this is heap memory unless the user names `bytes`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetUnit {
    /// Messages in a mailbox/lane/queue.
    Messages,
    /// In-flight runtime calls.
    Calls,
    /// Payload bytes (a weight dimension).
    Bytes,
    /// Open connections.
    Connections,
    /// Protocol sessions/streams.
    Sessions,
    /// Connect/retry attempts.
    Attempts,
    /// User-defined weight. `label` is what the user calls the unit
    /// (`rows`, `jobs`, `handles`); it appears verbatim in reports.
    Weight {
        /// User-declared unit label.
        label: String,
    },
}

impl BudgetUnit {
    /// Label for reports. `Weight` returns the user's label.
    pub fn label(&self) -> &str {
        match self {
            Self::Messages => "messages",
            Self::Calls => "calls",
            Self::Bytes => "bytes",
            Self::Connections => "connections",
            Self::Sessions => "sessions",
            Self::Attempts => "attempts",
            Self::Weight { label } => label,
        }
    }

    /// True if this unit is a *weight* dimension (`Bytes` or
    /// `Weight`). The live [`CapacitySurfaceReport`] distinguishes the
    /// count dimension from the weight dimension, so consistency
    /// checks compare dimension here, not the finer count flavors.
    pub fn is_weight(&self) -> bool {
        matches!(self, Self::Bytes | Self::Weight { .. })
    }
}

/// A budget's capacity: a hard numeric cap with how it was chosen, or
/// an explicit, policy-checked unbounded escape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetCap {
    /// Hard numeric cap. `mode` is [`CapacityMode::Fixed`] (measured)
    /// or [`CapacityMode::Tuning`] (discovery). Both are hard upper
    /// bounds; validation rejects an unbounded mode here.
    Bounded {
        /// The hard cap.
        max: usize,
        /// How the cap was chosen.
        mode: CapacityMode,
    },
    /// Explicit unbounded escape. `mode` must be one of the
    /// `Unbounded*` variants of [`CapacityMode`]; validation rejects a
    /// bare `Fixed`/`Tuning` here. Production policy rejects unbounded.
    Unbounded {
        /// The unbounded escape mode (carries reason / expiry).
        mode: CapacityMode,
    },
}

impl BudgetCap {
    /// A measured fixed cap.
    pub fn fixed(max: usize) -> Self {
        Self::Bounded {
            max,
            mode: CapacityMode::Fixed,
        }
    }

    /// A discovery cap. Still a hard upper bound.
    pub fn tuning(max: usize) -> Self {
        Self::Bounded {
            max,
            mode: CapacityMode::Tuning,
        }
    }

    /// An explicit unbounded escape. `mode` must be an `Unbounded*`
    /// variant or validation will reject it.
    pub fn unbounded(mode: CapacityMode) -> Self {
        Self::Unbounded { mode }
    }

    /// The configured cap, or `None` for an unbounded escape.
    pub fn max(&self) -> Option<usize> {
        match self {
            Self::Bounded { max, .. } => Some(*max),
            Self::Unbounded { .. } => None,
        }
    }

    /// The mode behind this cap.
    pub fn mode(&self) -> &CapacityMode {
        match self {
            Self::Bounded { mode, .. } | Self::Unbounded { mode } => mode,
        }
    }

    /// True if the mode/shape are self-consistent (Bounded carries a
    /// hard mode; Unbounded carries an unbounded mode).
    fn shape_is_consistent(&self) -> bool {
        match self {
            Self::Bounded { mode, .. } => {
                matches!(mode, CapacityMode::Fixed | CapacityMode::Tuning)
            }
            Self::Unbounded { mode } => matches!(
                mode,
                CapacityMode::UnboundedForNow { .. }
                    | CapacityMode::UnboundedWithoutExpiryIKnowThisIsBad { .. }
            ),
        }
    }
}

/// Whether a surface's value changes deterministic replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayImpact {
    /// Changing this cap can change replayed behaviour. It is hashed
    /// into the replay export.
    ReplayAffecting,
    /// Informational only (operator dashboards). Ignored by the
    /// replay export hash.
    DisplayOnly,
}

impl ReplayImpact {
    /// Short report label.
    pub fn label(self) -> &'static str {
        match self {
            Self::ReplayAffecting => "replay_affecting",
            Self::DisplayOnly => "display_only",
        }
    }
}

/// Where a surface row came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetSource {
    /// Declared by the user or read from a config struct.
    Config,
    /// Projected from a live report.
    Report,
    /// Derived/computed from other rows.
    Derived,
}

impl BudgetSource {
    /// Short report label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Report => "report",
            Self::Derived => "derived",
        }
    }
}

/// One declared budget. The stable `name` must match the live
/// capacity/report surface name exactly — "close enough" is a service
/// bug, so consistency checks compare names byte-for-byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetSurface {
    /// Stable, dotted-token surface name (e.g. `billing.controller.mailbox`).
    pub name: String,
    /// What this surface bounds.
    pub kind: BudgetKind,
    /// What this surface counts.
    pub unit: BudgetUnit,
    /// The cap (or explicit unbounded escape).
    pub cap: BudgetCap,
    /// Owning component label, when known.
    pub owner: String,
    /// Shard label, when the surface is shard-local.
    pub shard: Option<String>,
    /// Whether this cap affects deterministic replay.
    pub replay_impact: ReplayImpact,
    /// Where the row came from.
    pub source: BudgetSource,
}

impl BudgetSurface {
    /// New surface. Defaults: replay-affecting, config source, no
    /// owner/shard. Use the builder methods to refine.
    pub fn new(
        name: impl Into<String>,
        kind: BudgetKind,
        unit: BudgetUnit,
        cap: BudgetCap,
    ) -> Self {
        Self {
            name: name.into(),
            kind,
            unit,
            cap,
            owner: String::new(),
            shard: None,
            replay_impact: ReplayImpact::ReplayAffecting,
            source: BudgetSource::Config,
        }
    }

    /// Set the owning component label.
    pub fn owned_by(mut self, owner: impl Into<String>) -> Self {
        self.owner = owner.into();
        self
    }

    /// Pin this surface to a shard.
    pub fn on_shard(mut self, shard: impl Into<String>) -> Self {
        self.shard = Some(shard.into());
        self
    }

    /// Mark this cap display-only (ignored by replay).
    pub fn display_only(mut self) -> Self {
        self.replay_impact = ReplayImpact::DisplayOnly;
        self
    }

    /// Mark this row as projected from a live report.
    pub fn from_report(mut self) -> Self {
        self.source = BudgetSource::Report;
        self
    }

    /// Mark this row as derived from other rows.
    pub fn derived(mut self) -> Self {
        self.source = BudgetSource::Derived;
        self
    }
}

/// Why [`ServiceBudgetManifest::validate`] rejected the manifest. All
/// failures are collected so one validate call reports every offender.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetValidationError {
    /// Two surfaces share a name.
    DuplicateName(String),
    /// A surface name is empty.
    EmptyName,
    /// A surface name has whitespace/control chars that would break
    /// the `key=value` discovery line.
    InvalidName(String),
    /// A bounded cap is zero. Zero would deadlock a queue, fake EOF on
    /// a byte budget, or disable a rail — never a real budget.
    ZeroCap {
        /// Offending surface name.
        surface: String,
        /// Kind, so the message can say why zero is broken.
        kind: BudgetKind,
    },
    /// A `Bounded` cap carried an unbounded mode, or an `Unbounded` cap
    /// carried `Fixed`/`Tuning`.
    CapModeConflict {
        /// Offending surface name.
        surface: String,
        /// Human detail.
        detail: &'static str,
    },
    /// The capacity policy rejected the mode (expired unbounded,
    /// production rejecting unbounded, empty reason).
    Policy(CapacityPolicyError),
    /// A printable field looks like it holds a secret value.
    SecretLike {
        /// Offending surface name.
        surface: String,
        /// Which field (`name`, `owner`, `shard`).
        field: &'static str,
        /// What pattern matched.
        hint: &'static str,
    },
    /// A required surface kind has no row in the manifest.
    MissingRequiredKind {
        /// The kind the skeleton requires.
        kind: BudgetKind,
    },
    /// A user-defined weight unit label is empty or has control chars.
    InvalidUnitLabel {
        /// Offending surface name.
        surface: String,
        /// The bad label.
        label: String,
    },
}

impl fmt::Display for BudgetValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateName(name) => write!(f, "duplicate budget surface name {name:?}"),
            Self::EmptyName => f.write_str("budget surface name is empty"),
            Self::InvalidName(name) => write!(
                f,
                "budget surface name {name:?} has whitespace or control characters; \
                 use a dotted token form like \"billing.controller.mailbox\""
            ),
            Self::ZeroCap { surface, kind } => write!(
                f,
                "budget surface {surface:?} ({kind}) has a zero cap; zero would deadlock, \
                 fake EOF, or disable the rail — pick a real cap or remove the surface"
            ),
            Self::CapModeConflict { surface, detail } => {
                write!(f, "budget surface {surface:?} cap/mode conflict: {detail}")
            }
            Self::Policy(err) => write!(f, "{err}"),
            Self::SecretLike {
                surface,
                field,
                hint,
            } => write!(
                f,
                "budget surface {surface:?} field {field} looks like a secret value ({hint}); \
                 manifests carry env var names and file paths, never secret values"
            ),
            Self::MissingRequiredKind { kind } => {
                write!(
                    f,
                    "copied service skeleton requires a {kind} surface, none found"
                )
            }
            Self::InvalidUnitLabel { surface, label } => write!(
                f,
                "budget surface {surface:?} has an empty or control-character weight unit \
                 label {label:?}"
            ),
        }
    }
}

impl std::error::Error for BudgetValidationError {}

/// Why building a config *from* the manifest failed. Returned by
/// [`ServiceBudgetManifest::cap_of_kind`] and the `from_manifest`
/// config builders, which only exist where the mapping is exact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetBuildError {
    /// No surface with the requested name.
    MissingSurface(String),
    /// The surface exists but is the wrong kind.
    WrongKind {
        /// Surface name.
        surface: String,
        /// Kind the builder needed.
        expected: BudgetKind,
        /// Kind the surface actually declared.
        found: BudgetKind,
    },
    /// The surface is an explicit unbounded escape; a config needs a
    /// concrete number.
    Unbounded(String),
}

impl fmt::Display for BudgetBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSurface(name) => {
                write!(f, "no budget surface named {name:?} to build a config from")
            }
            Self::WrongKind {
                surface,
                expected,
                found,
            } => write!(
                f,
                "budget surface {surface:?} is {found}, builder needs {expected}"
            ),
            Self::Unbounded(name) => write!(
                f,
                "budget surface {name:?} is unbounded; a config needs a concrete cap"
            ),
        }
    }
}

impl std::error::Error for BudgetBuildError {}

/// True if `name` is a valid surface name: non-empty, no whitespace or
/// control characters. Matches the rule the live capacity summary uses
/// so manifest names and live names obey one grammar.
fn name_is_valid(name: &str) -> bool {
    !name.is_empty() && !name.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Returns a hint if `value` matches a common leaked-credential shape.
/// This is a best-effort guard for obvious mistakes, not a complete
/// secret scanner: it catches PEM blocks, AWS key ids, credential URLs,
/// `<secret-word>=/:value` assignments, and a few distinctive token
/// prefixes. Env var *names* (no value) and file paths are allowed.
fn secret_hint(value: &str) -> Option<&'static str> {
    if value.contains("-----BEGIN") {
        return Some("pem block");
    }
    if let Some(hit) = aws_access_key_id(value) {
        return Some(hit);
    }
    if url_has_credentials(value) {
        return Some("url credentials");
    }
    if let Some(hit) = token_prefix(value) {
        return Some(hit);
    }
    if secret_assignment(value) {
        return Some("secret assignment");
    }
    None
}

/// Distinctive credential token prefixes at a word boundary (start of
/// string or after a non-alphanumeric char). Kept narrow so common
/// words like `task-...` cannot trip it.
fn token_prefix(value: &str) -> Option<&'static str> {
    const PREFIXES: &[&str] = &["ghp_", "github_pat_", "glpat-", "xoxb-", "xoxp-", "xoxa-"];
    let lower = value.to_ascii_lowercase();
    for prefix in PREFIXES {
        let mut from = 0;
        while let Some(rel) = lower[from..].find(prefix) {
            let at = from + rel;
            let boundary = at == 0
                || !lower[..at]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
            // Require at least one char of token body after the prefix.
            let has_body = lower[at + prefix.len()..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric());
            if boundary && has_body {
                return Some("credential token prefix");
            }
            from = at + prefix.len();
        }
    }
    None
}

/// `AKIA` + 16 uppercase alnum chars is an AWS access key id.
fn aws_access_key_id(value: &str) -> Option<&'static str> {
    let bytes = value.as_bytes();
    bytes.windows(20).find_map(|w| {
        let looks_like = w.starts_with(b"AKIA")
            && w[4..]
                .iter()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit());
        looks_like.then_some("aws access key id")
    })
}

/// `scheme://user:password@host` carries a literal password.
fn url_has_credentials(value: &str) -> bool {
    let Some(after_scheme) = value.split_once("://").map(|(_, rest)| rest) else {
        return false;
    };
    // userinfo is everything before the first '/', '?', or '#'.
    let authority_end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    let authority = &after_scheme[..authority_end];
    match authority.split_once('@') {
        Some((userinfo, _host)) => userinfo
            .split_once(':')
            .is_some_and(|(_, pw)| !pw.is_empty()),
        None => false,
    }
}

/// `<secret-word><=|:><non-empty value>` is a leaked assignment.
/// A bare env var name like `DATABASE_PASSWORD` (no `=value`) is
/// allowed. The root words match as substrings, so `db_password=` and
/// `client_secret=` are caught via `password`/`secret`.
fn secret_assignment(value: &str) -> bool {
    const SECRET_ROOTS: &[&str] = &[
        "password",
        "passwd",
        "secret",
        "apikey",
        "api_key",
        "access_key",
        "token",
        "credential",
    ];
    let lower = value.to_ascii_lowercase();
    SECRET_ROOTS.iter().any(|root| {
        let mut from = 0;
        while let Some(rel) = lower[from..].find(root) {
            let after = from + rel + root.len();
            // The char right after the root must be a `=` or `:`
            // delimiter, and the value after it must be non-empty.
            let mut rest = lower[after..].chars();
            if matches!(rest.next(), Some('=') | Some(':'))
                && rest.next().is_some_and(|c| !c.is_whitespace())
            {
                return true;
            }
            from = after;
        }
        false
    })
}

/// User-facing manifest of every budget that bounds one service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceBudgetManifest {
    /// Free-text service label.
    pub service: String,
    /// Deployment policy used to validate unbounded modes.
    pub policy: CapacityPolicy,
    /// Schema version stamped onto the report and replay export.
    pub schema_version: u32,
    /// Kinds a copied skeleton must declare. Validation fails if any
    /// is absent. Empty by default.
    pub required_kinds: Vec<BudgetKind>,
    surfaces: Vec<BudgetSurface>,
}

impl ServiceBudgetManifest {
    /// New, empty manifest for `service` under `policy`.
    pub fn new(service: impl Into<String>, policy: CapacityPolicy) -> Self {
        Self {
            service: service.into(),
            policy,
            schema_version: BUDGET_SCHEMA_VERSION,
            required_kinds: Vec::new(),
            surfaces: Vec::new(),
        }
    }

    /// Append a surface. No validation here; call [`Self::validate`].
    pub fn add(&mut self, surface: BudgetSurface) -> &mut Self {
        self.surfaces.push(surface);
        self
    }

    /// Append many surfaces (e.g. from an adapter).
    pub fn extend<I>(&mut self, surfaces: I) -> &mut Self
    where
        I: IntoIterator<Item = BudgetSurface>,
    {
        self.surfaces.extend(surfaces);
        self
    }

    /// Builder form of [`Self::add`].
    pub fn with(mut self, surface: BudgetSurface) -> Self {
        self.surfaces.push(surface);
        self
    }

    /// Declare a kind the skeleton must include.
    pub fn require_kind(mut self, kind: BudgetKind) -> Self {
        if !self.required_kinds.contains(&kind) {
            self.required_kinds.push(kind);
        }
        self
    }

    /// All declared surfaces, in registration order.
    pub fn surfaces(&self) -> &[BudgetSurface] {
        &self.surfaces
    }

    /// Number of declared surfaces.
    pub fn len(&self) -> usize {
        self.surfaces.len()
    }

    /// True if no surfaces are declared.
    pub fn is_empty(&self) -> bool {
        self.surfaces.is_empty()
    }

    /// Look up one surface by exact name.
    pub fn surface(&self, name: &str) -> Option<&BudgetSurface> {
        self.surfaces.iter().find(|s| s.name == name)
    }

    /// The fixed/tuning cap declared for `name`, or `None` if the
    /// surface is missing or unbounded. Lets a service read its caps
    /// from one declared place instead of scattered constants.
    pub fn cap_max(&self, name: &str) -> Option<usize> {
        self.surface(name).and_then(|s| s.cap.max())
    }

    /// Read the numeric cap a config builder needs from the manifest:
    /// the surface must exist, be of `kind`, and be bounded. This is
    /// the "install a config from the manifest" primitive — it returns
    /// typed [`BudgetBuildError`]s so a builder fails loudly instead of
    /// guessing a default. Used only for *exact* config mappings.
    pub fn cap_of_kind(&self, name: &str, kind: BudgetKind) -> Result<usize, BudgetBuildError> {
        let surface = self
            .surface(name)
            .ok_or_else(|| BudgetBuildError::MissingSurface(name.to_string()))?;
        if surface.kind != kind {
            return Err(BudgetBuildError::WrongKind {
                surface: name.to_string(),
                expected: kind,
                found: surface.kind,
            });
        }
        surface
            .cap
            .max()
            .ok_or_else(|| BudgetBuildError::Unbounded(name.to_string()))
    }

    /// Validate the whole manifest before runtime startup. Returns
    /// every error found, not just the first, so a bad manifest is one
    /// readable failure rather than a fix-rerun loop.
    pub fn validate(&self) -> Result<(), Vec<BudgetValidationError>> {
        let mut errors = Vec::new();
        let mut seen: BTreeMap<&str, ()> = BTreeMap::new();

        for surface in &self.surfaces {
            // Name shape + uniqueness.
            if surface.name.is_empty() {
                errors.push(BudgetValidationError::EmptyName);
            } else if !name_is_valid(&surface.name) {
                errors.push(BudgetValidationError::InvalidName(surface.name.clone()));
            } else if seen.insert(surface.name.as_str(), ()).is_some() {
                errors.push(BudgetValidationError::DuplicateName(surface.name.clone()));
            }

            // Cap shape vs mode.
            let shape_ok = surface.cap.shape_is_consistent();
            if !shape_ok {
                let detail = match &surface.cap {
                    BudgetCap::Bounded { .. } => {
                        "a hard cap must use Fixed or Tuning, not an unbounded mode"
                    }
                    BudgetCap::Unbounded { .. } => {
                        "an unbounded cap must use an UnboundedForNow / UnboundedWithoutExpiry mode"
                    }
                };
                errors.push(BudgetValidationError::CapModeConflict {
                    surface: surface.name.clone(),
                    detail,
                });
            }
            // Zero is broken for every kind in this manifest. Checked
            // regardless of mode shape so a zero cap is always flagged.
            if let BudgetCap::Bounded { max: 0, .. } = surface.cap {
                errors.push(BudgetValidationError::ZeroCap {
                    surface: surface.name.clone(),
                    kind: surface.kind,
                });
            }
            // Policy check (expiry, production, empty reason). Only
            // meaningful once the shape is consistent.
            if shape_ok {
                if let Err(err) = self.policy.validate_mode(&surface.name, surface.cap.mode()) {
                    errors.push(BudgetValidationError::Policy(err));
                }
            }

            // A user-defined weight label appears in reports and is
            // hashed for replay; reject control characters and an empty
            // label so it stays a clean, visible token.
            if let BudgetUnit::Weight { label } = &surface.unit {
                if label.trim().is_empty() || label.chars().any(|c| c.is_control()) {
                    errors.push(BudgetValidationError::InvalidUnitLabel {
                        surface: surface.name.clone(),
                        label: label.clone(),
                    });
                }
            }

            // Secret-looking printable fields.
            for (field, value) in [
                ("name", Some(surface.name.as_str())),
                ("owner", Some(surface.owner.as_str())),
                ("shard", surface.shard.as_deref()),
            ] {
                if let Some(value) = value {
                    if let Some(hint) = secret_hint(value) {
                        errors.push(BudgetValidationError::SecretLike {
                            surface: surface.name.clone(),
                            field,
                            hint,
                        });
                    }
                }
            }
        }

        // Required kinds for a copied skeleton.
        for kind in &self.required_kinds {
            if !self.surfaces.iter().any(|s| s.kind == *kind) {
                errors.push(BudgetValidationError::MissingRequiredKind { kind: *kind });
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// Compare declared surfaces against a live [`CapacitySummary`].
    /// Names must match exactly; mismatched cap/unit/mode and
    /// missing/extra surfaces are reported as typed rows.
    pub fn compare_capacity_summary(&self, summary: &CapacitySummary) -> BudgetConsistencyReport {
        let mut rows = Vec::new();
        let mut matched: Vec<&str> = Vec::new();

        for surface in &self.surfaces {
            match summary.surface(&surface.name).report() {
                Ok(report) => {
                    matched.push(surface.name.as_str());
                    rows.extend(compare_surface_to_report(surface, report));
                }
                Err(_) => rows.push(BudgetConsistencyRow::Missing {
                    name: surface.name.clone(),
                }),
            }
        }
        for report in summary.reports() {
            if !matched.contains(&report.name.as_str()) {
                rows.push(BudgetConsistencyRow::Extra {
                    name: report.name.clone(),
                });
            }
        }
        BudgetConsistencyReport {
            service: self.service.clone(),
            rows,
        }
    }

    /// Compare declared surfaces against a live
    /// [`ServicePressureReport`]. Measured surfaces are compared by
    /// number. An `Unavailable` live surface with a manifest row is
    /// consistent (declared, not yet measured); an `Unavailable`
    /// surface with no manifest row is `Extra`.
    pub fn compare_service_pressure(
        &self,
        report: &ServicePressureReport,
    ) -> BudgetConsistencyReport {
        let mut rows = Vec::new();

        for surface in &self.surfaces {
            match report.surfaces.iter().find(|s| s.name == surface.name) {
                Some(live) => match &live.state {
                    ServiceSurfaceState::Measured(report) => {
                        rows.extend(compare_surface_to_report(surface, report));
                    }
                    // Declared but not currently measured: consistent.
                    ServiceSurfaceState::Unavailable { .. } => {}
                },
                None => rows.push(BudgetConsistencyRow::Missing {
                    name: surface.name.clone(),
                }),
            }
        }
        for live in &report.surfaces {
            if self.surface(&live.name).is_none() {
                rows.push(BudgetConsistencyRow::Extra {
                    name: live.name.clone(),
                });
            }
        }
        BudgetConsistencyReport {
            service: self.service.clone(),
            rows,
        }
    }

    /// Compare two manifests by name. This is the only comparison that
    /// can see [`ReplayImpact`] on both sides, so it is the one that
    /// emits [`BudgetConsistencyRow::ReplayImpactMismatch`]. Useful for
    /// "does my hand-written manifest match the adapter-derived one?".
    pub fn compare_manifest(&self, other: &ServiceBudgetManifest) -> BudgetConsistencyReport {
        let mut rows = Vec::new();
        for surface in &self.surfaces {
            match other.surface(&surface.name) {
                Some(rhs) => rows.extend(compare_surface_to_surface(surface, rhs)),
                None => rows.push(BudgetConsistencyRow::Missing {
                    name: surface.name.clone(),
                }),
            }
        }
        for rhs in &other.surfaces {
            if self.surface(&rhs.name).is_none() {
                rows.push(BudgetConsistencyRow::Extra {
                    name: rhs.name.clone(),
                });
            }
        }
        BudgetConsistencyReport {
            service: self.service.clone(),
            rows,
        }
    }

    /// Join the declared manifest with live observed facts from a
    /// [`ServicePressureReport`]. Every declared surface produces one
    /// row; observed capacity/full numbers come from the live report,
    /// never from the manifest. The report also carries the
    /// consistency check so callers see missing/extra surfaces.
    pub fn report(&self, observed: &ServicePressureReport) -> BudgetManifestReport {
        let mut rows = Vec::with_capacity(self.surfaces.len());
        for surface in &self.surfaces {
            let observed_budget = observed
                .surfaces
                .iter()
                .find(|s| s.name == surface.name)
                .and_then(|live| match &live.state {
                    ServiceSurfaceState::Measured(report) => {
                        Some(ObservedBudget::from_report(report))
                    }
                    ServiceSurfaceState::Unavailable { .. } => None,
                });
            rows.push(BudgetManifestRow {
                surface: surface.clone(),
                observed: observed_budget,
            });
        }
        BudgetManifestReport {
            service: self.service.clone(),
            schema_version: self.schema_version,
            rows,
            consistency: self.compare_service_pressure(observed),
        }
    }

    /// Export the config that deterministic replay depends on. The
    /// hash covers every [`ReplayImpact::ReplayAffecting`] surface and
    /// ignores display-only ones. Changing a replay-affecting cap
    /// changes the hash; changing a display-only cap does not.
    pub fn replay_export(&self) -> BudgetReplayExport {
        let mut affecting: Vec<ReplayBudgetEntry> = self
            .surfaces
            .iter()
            .filter(|s| s.replay_impact == ReplayImpact::ReplayAffecting)
            .map(ReplayBudgetEntry::from_surface)
            .collect();
        affecting.sort_by(|a, b| a.name.cmp(&b.name));

        let mut display_only: Vec<String> = self
            .surfaces
            .iter()
            .filter(|s| s.replay_impact == ReplayImpact::DisplayOnly)
            .map(|s| s.name.clone())
            .collect();
        display_only.sort();

        let hash = hash_replay_entries(self.schema_version, &affecting);
        BudgetReplayExport {
            service: self.service.clone(),
            schema_version: self.schema_version,
            replay_affecting_hash: hash,
            replay_affecting: affecting,
            ignored_for_replay: display_only,
        }
    }
}

/// Compare a manifest surface to a live capacity report. Returns the
/// mismatch rows (possibly empty).
fn compare_surface_to_report(
    surface: &BudgetSurface,
    report: &CapacitySurfaceReport,
) -> Vec<BudgetConsistencyRow> {
    let mut rows = Vec::new();
    let name = surface.name.clone();

    // Dimension: does the live report measure count or weight?
    let report_is_weight = report.max_messages.is_none() && report.max_weight.is_some();
    if surface.unit.is_weight() != report_is_weight {
        rows.push(BudgetConsistencyRow::UnitMismatch {
            name: name.clone(),
            manifest_unit: surface.unit.label().to_string(),
            observed_unit: if report_is_weight {
                report
                    .weight_unit
                    .clone()
                    .unwrap_or_else(|| "weight".into())
            } else {
                "messages".into()
            },
        });
    } else if report_is_weight {
        // Same dimension and weighted: compare the unit label.
        let observed = report.weight_unit.as_deref().unwrap_or("weight");
        if observed != surface.unit.label() {
            rows.push(BudgetConsistencyRow::UnitMismatch {
                name: name.clone(),
                manifest_unit: surface.unit.label().to_string(),
                observed_unit: observed.to_string(),
            });
        }
    }

    // Cap.
    let observed_max = if report_is_weight {
        report.max_weight
    } else {
        report.max_messages
    };
    if surface.cap.max() != observed_max {
        rows.push(BudgetConsistencyRow::CapMismatch {
            name: name.clone(),
            manifest_max: surface.cap.max(),
            observed_max,
        });
    }

    // Mode.
    if surface.cap.mode().label() != report.mode.label() {
        rows.push(BudgetConsistencyRow::ModeMismatch {
            name,
            manifest_mode: surface.cap.mode().label().to_string(),
            observed_mode: report.mode.label().to_string(),
        });
    }
    rows
}

/// Compare two manifest surfaces (cap/unit/mode/replay-impact).
fn compare_surface_to_surface(
    lhs: &BudgetSurface,
    rhs: &BudgetSurface,
) -> Vec<BudgetConsistencyRow> {
    let mut rows = Vec::new();
    let name = lhs.name.clone();
    if lhs.cap.max() != rhs.cap.max() {
        rows.push(BudgetConsistencyRow::CapMismatch {
            name: name.clone(),
            manifest_max: lhs.cap.max(),
            observed_max: rhs.cap.max(),
        });
    }
    if lhs.unit.label() != rhs.unit.label() {
        rows.push(BudgetConsistencyRow::UnitMismatch {
            name: name.clone(),
            manifest_unit: lhs.unit.label().to_string(),
            observed_unit: rhs.unit.label().to_string(),
        });
    }
    if lhs.cap.mode().label() != rhs.cap.mode().label() {
        rows.push(BudgetConsistencyRow::ModeMismatch {
            name: name.clone(),
            manifest_mode: lhs.cap.mode().label().to_string(),
            observed_mode: rhs.cap.mode().label().to_string(),
        });
    }
    if lhs.replay_impact != rhs.replay_impact {
        rows.push(BudgetConsistencyRow::ReplayImpactMismatch {
            name,
            manifest_impact: lhs.replay_impact,
            observed_impact: rhs.replay_impact,
        });
    }
    rows
}

/// One difference between a manifest and a live report (or another
/// manifest).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetConsistencyRow {
    /// Declared in the manifest, absent from the live side.
    Missing {
        /// Surface name.
        name: String,
    },
    /// Present on the live side, not declared in the manifest.
    Extra {
        /// Surface name.
        name: String,
    },
    /// Cap differs.
    CapMismatch {
        /// Surface name.
        name: String,
        /// Manifest cap (`None` = unbounded).
        manifest_max: Option<usize>,
        /// Observed cap (`None` = unbounded / count-vs-weight).
        observed_max: Option<usize>,
    },
    /// Unit / dimension differs.
    UnitMismatch {
        /// Surface name.
        name: String,
        /// Manifest unit label.
        manifest_unit: String,
        /// Observed unit label.
        observed_unit: String,
    },
    /// Capacity mode differs.
    ModeMismatch {
        /// Surface name.
        name: String,
        /// Manifest mode label.
        manifest_mode: String,
        /// Observed mode label.
        observed_mode: String,
    },
    /// Replay impact differs (manifest-vs-manifest only).
    ReplayImpactMismatch {
        /// Surface name.
        name: String,
        /// Manifest impact.
        manifest_impact: ReplayImpact,
        /// Other-side impact.
        observed_impact: ReplayImpact,
    },
}

impl BudgetConsistencyRow {
    /// Surface name this row is about.
    pub fn name(&self) -> &str {
        match self {
            Self::Missing { name }
            | Self::Extra { name }
            | Self::CapMismatch { name, .. }
            | Self::UnitMismatch { name, .. }
            | Self::ModeMismatch { name, .. }
            | Self::ReplayImpactMismatch { name, .. } => name,
        }
    }
}

impl fmt::Display for BudgetConsistencyRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { name } => write!(f, "missing surface={name}"),
            Self::Extra { name } => write!(f, "extra surface={name}"),
            Self::CapMismatch {
                name,
                manifest_max,
                observed_max,
            } => write!(
                f,
                "cap_mismatch surface={name} manifest={} observed={}",
                opt_max(*manifest_max),
                opt_max(*observed_max),
            ),
            Self::UnitMismatch {
                name,
                manifest_unit,
                observed_unit,
            } => write!(
                f,
                "unit_mismatch surface={name} manifest={manifest_unit} observed={observed_unit}"
            ),
            Self::ModeMismatch {
                name,
                manifest_mode,
                observed_mode,
            } => write!(
                f,
                "mode_mismatch surface={name} manifest={manifest_mode} observed={observed_mode}"
            ),
            Self::ReplayImpactMismatch {
                name,
                manifest_impact,
                observed_impact,
            } => write!(
                f,
                "replay_impact_mismatch surface={name} manifest={} observed={}",
                manifest_impact.label(),
                observed_impact.label(),
            ),
        }
    }
}

fn opt_max(max: Option<usize>) -> String {
    match max {
        Some(m) => m.to_string(),
        None => "unbounded".to_string(),
    }
}

/// Result of comparing a manifest with the live side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetConsistencyReport {
    /// Service label.
    pub service: String,
    /// One row per difference. Empty means consistent.
    pub rows: Vec<BudgetConsistencyRow>,
}

impl BudgetConsistencyReport {
    /// True if the manifest and the live side agree.
    pub fn is_consistent(&self) -> bool {
        self.rows.is_empty()
    }

    /// Compact one-line summary.
    pub fn summary_line(&self) -> String {
        let mut missing = 0;
        let mut extra = 0;
        let mut mismatch = 0;
        for row in &self.rows {
            match row {
                BudgetConsistencyRow::Missing { .. } => missing += 1,
                BudgetConsistencyRow::Extra { .. } => extra += 1,
                _ => mismatch += 1,
            }
        }
        format!(
            "budget_consistency service={service} consistent={consistent} missing={missing} \
             extra={extra} mismatch={mismatch}",
            service = crate::capacity::discovery_value(&self.service),
            consistent = self.is_consistent(),
        )
    }
}

/// Observed live facts joined onto a manifest row. Always sourced from
/// the live report, never the manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedBudget {
    /// Live count/weight right now.
    pub current: usize,
    /// High-water count/weight observed.
    pub high_water: usize,
    /// Cumulative `Full` rejections for this dimension.
    pub full_count: u64,
    /// `count` or `weight`.
    pub dimension: &'static str,
}

impl ObservedBudget {
    /// Read observed facts from the live report. The dimension is taken
    /// from the *report* (what was actually measured), not the
    /// manifest's declared unit — so a manifest/live dimension
    /// disagreement is surfaced by the consistency check rather than
    /// silently zeroing the observed numbers here.
    fn from_report(report: &CapacitySurfaceReport) -> Self {
        let is_weight = report.max_messages.is_none() && report.max_weight.is_some();
        if is_weight {
            Self {
                current: report.current_weight.unwrap_or(0),
                high_water: report.high_water_weight.unwrap_or(0),
                full_count: report.weight_full_count,
                dimension: "weight",
            }
        } else {
            Self {
                current: report.current_messages,
                high_water: report.high_water_messages,
                full_count: report.full_count,
                dimension: "count",
            }
        }
    }
}

/// One declared budget joined with what was observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetManifestRow {
    /// The declared surface.
    pub surface: BudgetSurface,
    /// Live facts, or `None` if the surface was not measured.
    pub observed: Option<ObservedBudget>,
}

impl BudgetManifestRow {
    /// `key=value` discovery line: declared cap + observed facts.
    pub fn discovery_line(&self) -> String {
        let cap = match &self.surface.cap {
            BudgetCap::Bounded { max, .. } => max.to_string(),
            BudgetCap::Unbounded { .. } => "unbounded".to_string(),
        };
        let mut line = format!(
            "budget surface={name} kind={kind} unit={unit} cap={cap} mode={mode} \
             replay={replay} source={source} owner={owner}",
            name = crate::capacity::discovery_value(&self.surface.name),
            kind = self.surface.kind.label(),
            unit = crate::capacity::discovery_value(self.surface.unit.label()),
            mode = self.surface.cap.mode().label(),
            replay = self.surface.replay_impact.label(),
            source = self.surface.source.label(),
            owner = crate::capacity::discovery_value(if self.surface.owner.is_empty() {
                "-"
            } else {
                &self.surface.owner
            }),
        );
        if let Some(shard) = &self.surface.shard {
            line.push_str(&format!(
                " shard={}",
                crate::capacity::discovery_value(shard)
            ));
        }
        match &self.observed {
            Some(observed) => line.push_str(&format!(
                " observed={dim} cur={cur} high={high} full={full}",
                dim = observed.dimension,
                cur = observed.current,
                high = observed.high_water,
                full = observed.full_count,
            )),
            None => line.push_str(" observed=none"),
        }
        line
    }
}

/// A manifest joined with live observed facts. The copyable answer to
/// "what did I configure, what was used, what was full, and what
/// belongs in replay?".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetManifestReport {
    /// Service label.
    pub service: String,
    /// Schema version.
    pub schema_version: u32,
    /// One row per declared surface.
    pub rows: Vec<BudgetManifestRow>,
    /// Manifest-vs-live consistency check.
    pub consistency: BudgetConsistencyReport,
}

impl BudgetManifestReport {
    /// Look up one row by surface name.
    pub fn row(&self, name: &str) -> Option<&BudgetManifestRow> {
        self.rows.iter().find(|r| r.surface.name == name)
    }

    /// True if any observed surface ever hit `Full`.
    pub fn any_full(&self) -> bool {
        self.rows
            .iter()
            .any(|r| r.observed.as_ref().is_some_and(|o| o.full_count > 0))
    }

    /// One discovery line per surface.
    pub fn discovery_lines(&self) -> Vec<String> {
        self.rows
            .iter()
            .map(BudgetManifestRow::discovery_line)
            .collect()
    }

    /// Compact one-line summary.
    pub fn summary_line(&self) -> String {
        let observed = self.rows.iter().filter(|r| r.observed.is_some()).count();
        format!(
            "budget service={service} schema={schema} surfaces={surfaces} observed={observed} \
             full={full} consistent={consistent}",
            service = crate::capacity::discovery_value(&self.service),
            schema = self.schema_version,
            surfaces = self.rows.len(),
            full = self.any_full(),
            consistent = self.consistency.is_consistent(),
        )
    }
}

/// One replay-affecting budget, frozen for the export hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayBudgetEntry {
    /// Surface name.
    pub name: String,
    /// Kind label.
    pub kind: BudgetKind,
    /// Unit label.
    pub unit: String,
    /// Cap (`None` = unbounded).
    pub max: Option<usize>,
    /// Mode label.
    pub mode: String,
}

impl ReplayBudgetEntry {
    fn from_surface(surface: &BudgetSurface) -> Self {
        Self {
            name: surface.name.clone(),
            kind: surface.kind,
            unit: surface.unit.label().to_string(),
            max: surface.cap.max(),
            mode: surface.cap.mode().label().to_string(),
        }
    }
}

/// The config a saved replay case depends on. Pin this into a DST case
/// so replay does not silently depend on ambient defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetReplayExport {
    /// Service label.
    pub service: String,
    /// Schema version (part of the hash).
    pub schema_version: u32,
    /// Stable hash over the replay-affecting surfaces.
    pub replay_affecting_hash: u64,
    /// The replay-affecting surfaces, sorted by name.
    pub replay_affecting: Vec<ReplayBudgetEntry>,
    /// Display-only surfaces ignored by the hash, sorted by name.
    pub ignored_for_replay: Vec<String>,
}

impl BudgetReplayExport {
    /// Compact one-line summary.
    pub fn summary_line(&self) -> String {
        format!(
            "budget_replay service={service} schema={schema} hash={hash:016x} affecting={affecting} \
             ignored={ignored}",
            service = crate::capacity::discovery_value(&self.service),
            schema = self.schema_version,
            hash = self.replay_affecting_hash,
            affecting = self.replay_affecting.len(),
            ignored = self.ignored_for_replay.len(),
        )
    }
}

/// Feed bytes into a running FNV-1a hash.
fn fnv_feed(hash: &mut u64, bytes: &[u8]) {
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    for &b in bytes {
        *hash ^= u64::from(b);
        *hash = hash.wrapping_mul(PRIME);
    }
}

/// Feed a length-prefixed field. The length prefix means no field
/// content — a unit label with a delimiter, say — can fake a field or
/// entry boundary, so two different manifests cannot collide by
/// shifting bytes across the `key|value` seam.
fn fnv_field(hash: &mut u64, bytes: &[u8]) {
    fnv_feed(hash, &(bytes.len() as u64).to_le_bytes());
    fnv_feed(hash, bytes);
}

/// FNV-1a over the schema version and length-prefixed replay entries.
/// Portable and stable across runs so a saved replay case can pin it.
/// `entries` must be sorted by the caller for a stable result.
fn hash_replay_entries(schema_version: u32, entries: &[ReplayBudgetEntry]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    let mut hash = OFFSET;
    fnv_field(&mut hash, &schema_version.to_le_bytes());
    fnv_feed(&mut hash, &(entries.len() as u64).to_le_bytes());
    for entry in entries {
        fnv_field(&mut hash, entry.name.as_bytes());
        fnv_field(&mut hash, entry.kind.label().as_bytes());
        fnv_field(&mut hash, entry.unit.as_bytes());
        fnv_field(&mut hash, opt_max(entry.max).as_bytes());
        fnv_field(&mut hash, entry.mode.as_bytes());
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};
    use tina::capacity::CapacitySurfaceReport;

    fn manifest() -> ServiceBudgetManifest {
        let mut m = ServiceBudgetManifest::new("svc", CapacityPolicy::Production);
        m.add(
            BudgetSurface::new(
                "svc.controller.mailbox",
                BudgetKind::Mailbox,
                BudgetUnit::Messages,
                BudgetCap::fixed(32),
            )
            .owned_by("controller"),
        );
        m.add(
            BudgetSurface::new(
                "svc.http.body",
                BudgetKind::BodyBytes,
                BudgetUnit::Bytes,
                BudgetCap::fixed(64),
            )
            .owned_by("listener"),
        );
        m
    }

    #[test]
    fn valid_manifest_validates() {
        manifest().validate().expect("valid manifest validates");
    }

    #[test]
    fn duplicate_names_fail() {
        let mut m = manifest();
        m.add(BudgetSurface::new(
            "svc.controller.mailbox",
            BudgetKind::Mailbox,
            BudgetUnit::Messages,
            BudgetCap::fixed(4),
        ));
        let errors = m.validate().unwrap_err();
        assert!(errors.iter().any(|e| matches!(
            e,
            BudgetValidationError::DuplicateName(name) if name == "svc.controller.mailbox"
        )));
    }

    #[test]
    fn empty_and_invalid_names_fail() {
        let mut m = ServiceBudgetManifest::new("svc", CapacityPolicy::Test);
        m.add(BudgetSurface::new(
            "",
            BudgetKind::Mailbox,
            BudgetUnit::Messages,
            BudgetCap::fixed(4),
        ));
        m.add(BudgetSurface::new(
            "has space",
            BudgetKind::Mailbox,
            BudgetUnit::Messages,
            BudgetCap::fixed(4),
        ));
        let errors = m.validate().unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, BudgetValidationError::EmptyName))
        );
        assert!(errors.iter().any(|e| matches!(
            e,
            BudgetValidationError::InvalidName(name) if name == "has space"
        )));
    }

    #[test]
    fn zero_cap_fails_for_every_kind() {
        for kind in [
            BudgetKind::Mailbox,
            BudgetKind::Pool,
            BudgetKind::BodyBytes,
            BudgetKind::ShardPair,
            BudgetKind::RuntimeLane,
        ] {
            let mut m = ServiceBudgetManifest::new("svc", CapacityPolicy::Test);
            m.add(BudgetSurface::new(
                "svc.zero",
                kind,
                BudgetUnit::Messages,
                BudgetCap::fixed(0),
            ));
            let errors = m.validate().unwrap_err();
            assert!(
                errors.iter().any(
                    |e| matches!(e, BudgetValidationError::ZeroCap { kind: k, .. } if *k == kind)
                ),
                "zero cap must fail for {kind}"
            );
        }
    }

    #[test]
    fn cap_mode_conflict_fails_both_ways() {
        // Bounded with an unbounded mode.
        let mut m = ServiceBudgetManifest::new("svc", CapacityPolicy::Development);
        m.add(BudgetSurface::new(
            "svc.a",
            BudgetKind::Mailbox,
            BudgetUnit::Messages,
            BudgetCap::Bounded {
                max: 4,
                mode: CapacityMode::unbounded_for_now("x"),
            },
        ));
        // Unbounded with a hard mode.
        m.add(BudgetSurface::new(
            "svc.b",
            BudgetKind::Mailbox,
            BudgetUnit::Messages,
            BudgetCap::Unbounded {
                mode: CapacityMode::Fixed,
            },
        ));
        let errors = m.validate().unwrap_err();
        assert_eq!(
            errors
                .iter()
                .filter(|e| matches!(e, BudgetValidationError::CapModeConflict { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn unbounded_for_now_validates_before_expiry_fails_after() {
        // Validates before expiry under a dev policy.
        let mut ok = ServiceBudgetManifest::new("svc", CapacityPolicy::Development);
        ok.add(BudgetSurface::new(
            "svc.import",
            BudgetKind::Mailbox,
            BudgetUnit::Messages,
            BudgetCap::unbounded(CapacityMode::unbounded_for_now("temporary import")),
        ));
        ok.validate()
            .expect("live unbounded-for-now validates under dev policy");

        // Expired -> policy rejects.
        let mut expired = ServiceBudgetManifest::new("svc", CapacityPolicy::Development);
        expired.add(BudgetSurface::new(
            "svc.import",
            BudgetKind::Mailbox,
            BudgetUnit::Messages,
            BudgetCap::unbounded(CapacityMode::unbounded_for_now_until(
                "temporary import",
                SystemTime::now() - Duration::from_secs(1),
            )),
        ));
        let errors = expired.validate().unwrap_err();
        assert!(errors.iter().any(|e| matches!(
            e,
            BudgetValidationError::Policy(CapacityPolicyError::UnboundedExpired { .. })
        )));
    }

    #[test]
    fn production_rejects_unbounded() {
        let mut m = ServiceBudgetManifest::new("svc", CapacityPolicy::Production);
        m.add(BudgetSurface::new(
            "svc.import",
            BudgetKind::Mailbox,
            BudgetUnit::Messages,
            BudgetCap::unbounded(CapacityMode::unbounded_for_now("temporary import")),
        ));
        let errors = m.validate().unwrap_err();
        assert!(errors.iter().any(|e| matches!(
            e,
            BudgetValidationError::Policy(CapacityPolicyError::ModeNotAllowed { .. })
        )));
    }

    #[test]
    fn secret_like_values_are_rejected() {
        let cases = [
            ("svc.url", "postgres://user:hunter2@db:5432/app", true),
            ("svc.assign", "password=hunter2", true),
            ("svc.nested", "db_password=hunter2", true),
            ("svc.client", "client_secret=abc", true),
            ("svc.colon", "token:abcdef", true),
            ("svc.pem", "-----BEGIN PRIVATE KEY-----", true),
            ("svc.aws", "AKIAIOSFODNN7EXAMPLE", true),
            ("svc.ghp", "ghp_aBcDeF0123456789", true),
            ("svc.slack", "xoxb-12345-67890-abcdef", true),
            // Allowed: env var name (no value), a file path, and a
            // hyphenated label that merely starts like a word.
            ("svc.envname", "DATABASE_PASSWORD", false),
            ("svc.path", "/var/lib/app/db.sqlite", false),
            ("svc.task", "task-runner", false),
        ];
        for (name, owner, should_fail) in cases {
            let mut m = ServiceBudgetManifest::new("svc", CapacityPolicy::Test);
            m.add(
                BudgetSurface::new(
                    name,
                    BudgetKind::Mailbox,
                    BudgetUnit::Messages,
                    BudgetCap::fixed(4),
                )
                .owned_by(owner),
            );
            let result = m.validate();
            if should_fail {
                let errors = result.unwrap_err();
                assert!(
                    errors
                        .iter()
                        .any(|e| matches!(e, BudgetValidationError::SecretLike { .. })),
                    "{owner} should be flagged as secret"
                );
            } else {
                assert!(result.is_ok(), "{owner} should be allowed: {result:?}");
            }
        }
    }

    #[test]
    fn missing_required_kind_fails() {
        let m = ServiceBudgetManifest::new("svc", CapacityPolicy::Test)
            .require_kind(BudgetKind::Pool)
            .with(BudgetSurface::new(
                "svc.mailbox",
                BudgetKind::Mailbox,
                BudgetUnit::Messages,
                BudgetCap::fixed(4),
            ));
        let errors = m.validate().unwrap_err();
        assert!(errors.iter().any(|e| matches!(
            e,
            BudgetValidationError::MissingRequiredKind {
                kind: BudgetKind::Pool
            }
        )));
    }

    #[test]
    fn capacity_summary_compare_catches_missing_extra_and_mismatch() {
        let m = manifest();
        let mut summary = CapacitySummary::new();
        // Matching name but wrong cap -> CapMismatch.
        summary
            .push(CapacitySurfaceReport::count(
                "svc.controller.mailbox",
                CapacityMode::Fixed,
                16,
                0,
                0,
                0,
            ))
            .unwrap();
        // Extra surface not in the manifest.
        summary
            .push(CapacitySurfaceReport::count(
                "svc.extra",
                CapacityMode::Fixed,
                4,
                0,
                0,
                0,
            ))
            .unwrap();
        let report = m.compare_capacity_summary(&summary);
        assert!(report.rows.iter().any(|r| matches!(
            r,
            BudgetConsistencyRow::CapMismatch { name, manifest_max: Some(32), observed_max: Some(16) }
            if name == "svc.controller.mailbox"
        )));
        assert!(report.rows.iter().any(|r| matches!(
            r, BudgetConsistencyRow::Missing { name } if name == "svc.http.body"
        )));
        assert!(report.rows.iter().any(|r| matches!(
            r, BudgetConsistencyRow::Extra { name } if name == "svc.extra"
        )));
    }

    #[test]
    fn weighted_dimension_mismatch_is_caught() {
        // Manifest says count (Messages) but live report is weighted.
        let mut m = ServiceBudgetManifest::new("svc", CapacityPolicy::Test);
        m.add(BudgetSurface::new(
            "svc.body",
            BudgetKind::BodyBytes,
            BudgetUnit::Messages,
            BudgetCap::fixed(64),
        ));
        let mut summary = CapacitySummary::new();
        summary
            .push(CapacitySurfaceReport::weighted(
                "svc.body",
                CapacityMode::Fixed,
                64,
                0,
                0,
                0,
                "bytes",
            ))
            .unwrap();
        let report = m.compare_capacity_summary(&summary);
        assert!(report.rows.iter().any(|r| matches!(
            r, BudgetConsistencyRow::UnitMismatch { name, .. } if name == "svc.body"
        )));
    }

    #[test]
    fn user_defined_weight_unit_is_visible_and_count_truth_preserved() {
        // A user-defined weight ("rows") must round-trip as weight, not
        // be collapsed to a count, and its label must stay visible.
        let mut m = ServiceBudgetManifest::new("svc", CapacityPolicy::Production);
        m.add(BudgetSurface::new(
            "svc.query.rows",
            BudgetKind::BridgeInFlight,
            BudgetUnit::Weight {
                label: "rows".into(),
            },
            BudgetCap::fixed(500),
        ));

        // Same unit label + cap -> consistent, and the report reads the
        // weight high-water from the weight fields.
        let mut live = ServicePressureReport::new("svc");
        live.add_measured(
            "rows",
            CapacitySurfaceReport::weighted(
                "svc.query.rows",
                CapacityMode::Fixed,
                500,
                10,
                420,
                3,
                "rows",
            ),
        );
        let report = m.report(&live);
        assert!(
            report.consistency.is_consistent(),
            "{:?}",
            report.consistency.rows
        );
        let observed = report
            .row("svc.query.rows")
            .unwrap()
            .observed
            .as_ref()
            .unwrap();
        assert_eq!(observed.dimension, "weight");
        assert_eq!(observed.high_water, 420);
        assert_eq!(observed.full_count, 3);
        // The user's unit label survives into the discovery line.
        assert!(
            report
                .row("svc.query.rows")
                .unwrap()
                .discovery_line()
                .contains("unit=rows")
        );

        // A live "bytes" report against a declared "rows" weight is a
        // typed unit mismatch — Tina makes no silent memory claim.
        let mut wrong_unit = ServicePressureReport::new("svc");
        wrong_unit.add_measured(
            "bytes",
            CapacitySurfaceReport::weighted(
                "svc.query.rows",
                CapacityMode::Fixed,
                500,
                0,
                0,
                0,
                "bytes",
            ),
        );
        let mismatch = m.compare_service_pressure(&wrong_unit);
        assert!(mismatch.rows.iter().any(|r| matches!(
            r,
            BudgetConsistencyRow::UnitMismatch { manifest_unit, observed_unit, .. }
                if manifest_unit == "rows" && observed_unit == "bytes"
        )));
    }

    #[test]
    fn service_pressure_compare_catches_missing_extra_and_cap_mismatch() {
        let m = manifest();
        let mut live = ServicePressureReport::new("svc");
        // svc.controller.mailbox present but wrong cap -> CapMismatch.
        live.add_measured(
            "mailbox",
            CapacitySurfaceReport::count("svc.controller.mailbox", CapacityMode::Fixed, 8, 0, 0, 0),
        );
        // A live surface the manifest never declared -> Extra.
        live.add_measured(
            "extra",
            CapacitySurfaceReport::count("svc.extra", CapacityMode::Fixed, 4, 0, 0, 0),
        );
        // svc.http.body is in the manifest but absent here -> Missing.
        let report = m.compare_service_pressure(&live);
        assert!(report.rows.iter().any(|r| matches!(
            r,
            BudgetConsistencyRow::CapMismatch { name, manifest_max: Some(32), observed_max: Some(8) }
                if name == "svc.controller.mailbox"
        )));
        assert!(report.rows.iter().any(|r| matches!(
            r, BudgetConsistencyRow::Missing { name } if name == "svc.http.body"
        )));
        assert!(report.rows.iter().any(|r| matches!(
            r, BudgetConsistencyRow::Extra { name } if name == "svc.extra"
        )));
        assert!(!report.is_consistent());
    }

    #[test]
    fn service_pressure_compare_treats_unavailable_as_declared() {
        let m = manifest();
        let mut live = ServicePressureReport::new("svc");
        live.add_measured(
            "mailbox",
            CapacitySurfaceReport::count(
                "svc.controller.mailbox",
                CapacityMode::Fixed,
                32,
                0,
                9,
                0,
            ),
        );
        live.add_unavailable("svc.http.body", "body", "metrics not wired yet");
        let report = m.compare_service_pressure(&live);
        // Declared + unavailable is consistent: no rows at all.
        assert!(report.is_consistent(), "rows: {:?}", report.rows);
    }

    #[test]
    fn manifest_report_joins_observed_facts() {
        let m = manifest();
        let mut live = ServicePressureReport::new("svc");
        live.add_measured(
            "mailbox",
            CapacitySurfaceReport::count(
                "svc.controller.mailbox",
                CapacityMode::Fixed,
                32,
                3,
                12,
                2,
            ),
        );
        live.add_measured(
            "body",
            CapacitySurfaceReport::weighted(
                "svc.http.body",
                CapacityMode::Fixed,
                64,
                0,
                40,
                1,
                "bytes",
            ),
        );
        let report = m.report(&live);
        let mailbox = report.row("svc.controller.mailbox").unwrap();
        let observed = mailbox.observed.as_ref().unwrap();
        assert_eq!(observed.dimension, "count");
        assert_eq!(observed.high_water, 12);
        assert_eq!(observed.full_count, 2);
        let body = report.row("svc.http.body").unwrap();
        let body_observed = body.observed.as_ref().unwrap();
        assert_eq!(body_observed.dimension, "weight");
        assert_eq!(body_observed.high_water, 40);
        assert!(report.any_full());
        assert!(report.consistency.is_consistent());
    }

    #[test]
    fn omitted_optional_budget_shows_explicit_default_in_report() {
        // A surface the live side does not measure still appears in the
        // report with observed=none — an explicit "declared, not used".
        let m = manifest();
        let live = ServicePressureReport::new("svc");
        let report = m.report(&live);
        for row in &report.rows {
            assert!(row.observed.is_none());
            assert!(row.discovery_line().contains("observed=none"));
        }
    }

    #[test]
    fn replay_export_hash_changes_on_replay_affecting_change() {
        let base = manifest().replay_export();

        let mut bigger_body = manifest();
        // svc.http.body is replay-affecting by default.
        bigger_body.surfaces[1].cap = BudgetCap::fixed(128);
        let changed = bigger_body.replay_export();
        assert_ne!(
            base.replay_affecting_hash, changed.replay_affecting_hash,
            "changing a replay-affecting cap must change the hash"
        );
    }

    #[test]
    fn replay_export_ignores_display_only_change() {
        let mut with_display = manifest();
        with_display.add(
            BudgetSurface::new(
                "svc.listener.mailbox",
                BudgetKind::Mailbox,
                BudgetUnit::Messages,
                BudgetCap::fixed(8),
            )
            .display_only(),
        );
        let base = with_display.replay_export();
        assert!(
            base.ignored_for_replay
                .contains(&"svc.listener.mailbox".to_string())
        );

        let mut changed_display = with_display.clone();
        // Mutate the display-only surface's cap.
        let idx = changed_display
            .surfaces
            .iter()
            .position(|s| s.name == "svc.listener.mailbox")
            .unwrap();
        changed_display.surfaces[idx].cap = BudgetCap::fixed(64);
        let changed = changed_display.replay_export();
        assert_eq!(
            base.replay_affecting_hash, changed.replay_affecting_hash,
            "changing a display-only cap must not change the replay hash"
        );
        assert!(
            changed
                .ignored_for_replay
                .contains(&"svc.listener.mailbox".to_string())
        );
    }

    #[test]
    fn replay_hash_is_stable_across_calls() {
        let m = manifest();
        assert_eq!(
            m.replay_export().replay_affecting_hash,
            m.replay_export().replay_affecting_hash
        );
    }

    #[test]
    fn compare_manifest_catches_replay_impact_mismatch() {
        let lhs = manifest();
        let mut rhs = manifest();
        rhs.surfaces[1].replay_impact = ReplayImpact::DisplayOnly;
        let report = lhs.compare_manifest(&rhs);
        assert!(report.rows.iter().any(|r| matches!(
            r,
            BudgetConsistencyRow::ReplayImpactMismatch { name, .. } if name == "svc.http.body"
        )));
    }

    #[test]
    fn invalid_unit_label_fails_validation() {
        for bad in ["", " ", "ro\nws"] {
            let mut m = ServiceBudgetManifest::new("svc", CapacityPolicy::Test);
            m.add(BudgetSurface::new(
                "svc.weighted",
                BudgetKind::BodyBytes,
                BudgetUnit::Weight { label: bad.into() },
                BudgetCap::fixed(10),
            ));
            let errors = m.validate().unwrap_err();
            assert!(
                errors
                    .iter()
                    .any(|e| matches!(e, BudgetValidationError::InvalidUnitLabel { .. })),
                "label {bad:?} must fail validation"
            );
        }
    }

    #[test]
    fn zero_cap_with_mode_conflict_still_flags_zero() {
        // A zero cap that *also* has a mode conflict must still report
        // the zero — the "would deadlock" diagnostic is not swallowed.
        let mut m = ServiceBudgetManifest::new("svc", CapacityPolicy::Development);
        m.add(BudgetSurface::new(
            "svc.x",
            BudgetKind::Mailbox,
            BudgetUnit::Messages,
            BudgetCap::Bounded {
                max: 0,
                mode: CapacityMode::unbounded_for_now("x"),
            },
        ));
        let errors = m.validate().unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, BudgetValidationError::ZeroCap { .. }))
        );
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, BudgetValidationError::CapModeConflict { .. }))
        );
    }

    #[test]
    fn replay_hash_resists_unit_label_delimiter_injection() {
        // Two manifests whose only difference is where a delimiter-ish
        // string sits must hash differently. Under a naive `|`-joined
        // canonical form these could collide; length-prefixing prevents
        // it. (Validation would reject a control char, but `|` and `:`
        // are legal label characters.)
        let mut a = ServiceBudgetManifest::new("svc", CapacityPolicy::Production);
        a.add(BudgetSurface::new(
            "svc.s",
            BudgetKind::BodyBytes,
            BudgetUnit::Weight {
                label: "rows|fixed".into(),
            },
            BudgetCap::fixed(10),
        ));
        let mut b = ServiceBudgetManifest::new("svc", CapacityPolicy::Production);
        b.add(BudgetSurface::new(
            "svc.s",
            BudgetKind::BodyBytes,
            BudgetUnit::Weight {
                label: "rows".into(),
            },
            BudgetCap::fixed(10),
        ));
        assert_ne!(
            a.replay_export().replay_affecting_hash,
            b.replay_export().replay_affecting_hash,
        );
    }

    #[test]
    fn report_observed_dimension_follows_live_not_manifest() {
        // Manifest declares a count unit (Calls) but the live report is
        // a count report carrying real numbers. Observed must read the
        // count fields (not silently zero them), even though the
        // consistency check is what flags any dimension disagreement.
        let mut m = ServiceBudgetManifest::new("svc", CapacityPolicy::Test);
        m.add(BudgetSurface::new(
            "svc.calls",
            BudgetKind::BridgeInFlight,
            BudgetUnit::Calls,
            BudgetCap::fixed(4),
        ));
        let mut live = ServicePressureReport::new("svc");
        live.add_measured(
            "bridge",
            CapacitySurfaceReport::count("svc.calls", CapacityMode::Fixed, 4, 1, 3, 2),
        );
        let report = m.report(&live);
        let observed = report.row("svc.calls").unwrap().observed.as_ref().unwrap();
        assert_eq!(observed.dimension, "count");
        assert_eq!(observed.high_water, 3);
        assert_eq!(observed.full_count, 2);
    }
}
