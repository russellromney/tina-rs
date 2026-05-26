//! Reconnecting WebSocket client manager.
//!
//! [`WebSocketClientManager`] owns a bounded pool of
//! [`crate::WebSocketClientConnection`] isolates and one current session per
//! endpoint. It connects through the bounded [`crate::connect::ConnectAttempts`]
//! helper, reconnects only up to a policy budget, and keeps every cap and
//! report honest:
//!
//! - typed connect / send / receive replies, including `Full`, `Closed`,
//!   `NoHealthyEndpoint`, `ConnectFailed`, `TimedOut`, and `Stale`;
//! - a generation guard so an old session's reply can never replace the
//!   current one;
//! - bounded retained closed/stale session reports;
//! - a drain-on-shutdown report;
//! - per-session outbound queue/byte pressure;
//! - budget surfaces for sessions, reconnects, connect attempts, and the
//!   session's queued events/bytes, with a live pressure join.
//!
//! The pure bookkeeping lives in [`state`] so it can be unit-tested without
//! a runtime; the isolate in [`isolate`] wires it to effects.

pub mod isolate;
pub mod state;

use tina_runtime::budget::{BudgetCap, BudgetKind, BudgetSurface, BudgetUnit};

use crate::connect::policy::{ConnectPolicy, ConnectPolicyError};
use crate::websocket::WebSocketLimits;

pub use isolate::{
    WebSocketClientManager, WebSocketConnectOutcome, WebSocketManagerAddr, WebSocketManagerHandles,
    WebSocketManagerMsg, WebSocketManagerReply, WebSocketManagerShutdownReport,
    WebSocketSessionError, WsConnAddr, build_websocket_client_manager,
};
pub use state::{
    InstallError, RetainedSessionReport, SessionEndReason, WebSocketManagerReport,
    WebSocketManagerState,
};

/// Configuration for a [`WebSocketClientManager`].
#[derive(Debug, Clone)]
pub struct WebSocketManagerConfig {
    /// Bounded DNS + connect + Happy Eyeballs policy.
    pub connect_policy: ConnectPolicy,
    /// Subprotocols offered on the WebSocket upgrade.
    pub subprotocols: Vec<String>,
    /// Per-session WebSocket limits (mailboxes, byte caps, queues).
    pub session_limits: WebSocketLimits,
    /// Maximum concurrent sessions the manager owns (first form: 1). Also
    /// bounds the connection-isolate pool used for connect races.
    pub max_sessions: usize,
    /// Maximum reconnects between healthy sessions. Zero means no reconnect.
    pub max_reconnects: usize,
    /// Maximum retained closed/stale session reports.
    pub retained_reports: usize,
}

impl WebSocketManagerConfig {
    /// A config with the given connect policy and conservative manager caps:
    /// one session, three reconnects, four retained reports.
    pub fn new(connect_policy: ConnectPolicy) -> Self {
        Self {
            connect_policy,
            subprotocols: Vec::new(),
            session_limits: WebSocketLimits::default(),
            max_sessions: 1,
            max_reconnects: 3,
            retained_reports: 4,
        }
    }

    /// Validate the config before first use.
    pub fn validate(&self) -> Result<(), WebSocketManagerConfigError> {
        self.connect_policy
            .validate()
            .map_err(WebSocketManagerConfigError::Policy)?;
        if self.max_sessions == 0 {
            return Err(WebSocketManagerConfigError::ZeroSessions);
        }
        if self.retained_reports == 0 {
            return Err(WebSocketManagerConfigError::ZeroRetainedReports);
        }
        Ok(())
    }

    /// Manifest rows for every cap this manager names.
    ///
    /// Combines the connect policy's surfaces with the manager's session and
    /// reconnect caps and the per-session WebSocket limits, under stable
    /// `{prefix}.*` names.
    pub fn budget_surfaces(&self, prefix: &str) -> Vec<BudgetSurface> {
        let mut surfaces = self.connect_policy.budget_surfaces(prefix);
        surfaces.push(
            BudgetSurface::new(
                format!("{prefix}.sessions"),
                BudgetKind::ProtocolSession,
                BudgetUnit::Sessions,
                BudgetCap::fixed(self.max_sessions),
            )
            .owned_by("ws.manager"),
        );
        surfaces.push(
            BudgetSurface::new(
                format!("{prefix}.reconnects"),
                BudgetKind::ConnectAttempt,
                BudgetUnit::Attempts,
                BudgetCap::fixed(self.max_reconnects.max(1)),
            )
            .owned_by("ws.manager"),
        );
        surfaces.extend(
            self.session_limits
                .budget_surfaces(&format!("{prefix}.session")),
        );
        surfaces
    }
}

/// Why a [`WebSocketManagerConfig`] failed validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebSocketManagerConfigError {
    /// The connect policy was invalid.
    Policy(ConnectPolicyError),
    /// `max_sessions` was zero.
    ZeroSessions,
    /// `retained_reports` was zero.
    ZeroRetainedReports,
}

impl std::fmt::Display for WebSocketManagerConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Policy(e) => write!(f, "connect policy invalid: {e}"),
            Self::ZeroSessions => f.write_str("max_sessions must be positive"),
            Self::ZeroRetainedReports => f.write_str("retained_reports must be positive"),
        }
    }
}

impl std::error::Error for WebSocketManagerConfigError {}

#[cfg(test)]
mod tests {
    use super::*;
    use tina::capacity::CapacityPolicy;
    use tina_runtime::budget::ServiceBudgetManifest;

    #[test]
    fn config_validates_and_rejects_zero_caps() {
        let cfg = WebSocketManagerConfig::new(ConnectPolicy::balanced());
        cfg.validate().unwrap();

        let mut bad = cfg.clone();
        bad.max_sessions = 0;
        assert_eq!(
            bad.validate(),
            Err(WebSocketManagerConfigError::ZeroSessions)
        );

        let mut bad = cfg.clone();
        bad.retained_reports = 0;
        assert_eq!(
            bad.validate(),
            Err(WebSocketManagerConfigError::ZeroRetainedReports)
        );
    }

    #[test]
    fn budget_surfaces_name_sessions_and_reconnects_and_validate() {
        let cfg = WebSocketManagerConfig::new(ConnectPolicy::balanced());
        let surfaces = cfg.budget_surfaces("rooms.upstream");
        let names: Vec<&str> = surfaces.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"rooms.upstream.sessions"));
        assert!(names.contains(&"rooms.upstream.reconnects"));
        assert!(names.contains(&"rooms.upstream.connect.attempts"));
        assert!(names.iter().any(|n| n.starts_with("rooms.upstream.session.")));
        let mut m = ServiceBudgetManifest::new("rooms", CapacityPolicy::Production);
        m.extend(surfaces);
        m.validate().unwrap();
    }
}
