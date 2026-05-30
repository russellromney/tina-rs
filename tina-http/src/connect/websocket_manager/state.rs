//! Pure bookkeeping for the reconnecting WebSocket client manager.
//!
//! [`WebSocketManagerState`] owns the truths the manager isolate must keep
//! exactly right: the current session and its generation, the bounded
//! reconnect budget, the bounded retained closed/stale session reports, and
//! the manager-level counters. It is a plain state machine with no effects,
//! so it can be unit-tested directly — the same shape as
//! [`crate::WebSocketMemberTable`].
//!
//! The generation guard is the load-bearing rule: a reply that names an old
//! generation can never replace the current session or its pressure.

use std::collections::VecDeque;
use std::net::SocketAddr;

use crate::connect::endpoint::{EndpointGeneration, EndpointId};
use crate::websocket_client::WebSocketClientReport;

/// Why a session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEndReason {
    /// The peer closed the connection.
    ClosedByPeer,
    /// The manager closed the session on request.
    ClosedLocal,
    /// A newer generation replaced this session before it ended cleanly.
    Stale,
    /// The session never opened: every connect attempt failed.
    ConnectFailed,
}

/// One retained report for a session that has ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedSessionReport {
    /// The ended session's generation.
    pub generation: EndpointGeneration,
    /// The address it had connected to, if it ever connected.
    pub addr: Option<SocketAddr>,
    /// Why it ended.
    pub reason: SessionEndReason,
    /// The last pressure snapshot seen for it, if any.
    pub pressure: Option<WebSocketClientReport>,
}

/// The live session, if one is current.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LiveSession {
    generation: EndpointGeneration,
    conn_index: usize,
    addr: SocketAddr,
    pressure: Option<WebSocketClientReport>,
}

/// Why installing a session failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallError {
    /// A current session already occupies the only session slot.
    SessionsFull,
    /// The generation is not the one currently being connected.
    StaleGeneration,
}

/// Bounded manager bookkeeping for one endpoint.
///
/// First form: one current session per endpoint. `max_sessions` bounds the
/// connection-isolate pool the manager owns; this state tracks the single
/// current session plus the bounded retained reports.
#[derive(Debug, Clone)]
pub struct WebSocketManagerState {
    endpoint_id: EndpointId,
    max_sessions: usize,
    max_reconnects: usize,
    retained_cap: usize,
    next_generation: EndpointGeneration,
    connecting_generation: Option<EndpointGeneration>,
    current: Option<LiveSession>,
    reconnects_used: usize,
    retained: VecDeque<RetainedSessionReport>,
    sessions_opened: u64,
    reconnects_total: u64,
    stale_replies_ignored: u64,
    full_rejections: u64,
    closed_count: u64,
    stale_count: u64,
    connect_failed_count: u64,
    no_healthy_count: u64,
}

impl WebSocketManagerState {
    /// Build state for one endpoint.
    ///
    /// `max_sessions` and `retained_cap` must be positive; `max_reconnects`
    /// may be zero (no reconnect budget).
    pub fn new(
        endpoint_id: EndpointId,
        max_sessions: usize,
        max_reconnects: usize,
        retained_cap: usize,
    ) -> Self {
        assert!(max_sessions > 0, "max_sessions must be positive");
        assert!(retained_cap > 0, "retained_cap must be positive");
        Self {
            endpoint_id,
            max_sessions,
            max_reconnects,
            retained_cap,
            next_generation: EndpointGeneration::first(),
            connecting_generation: None,
            current: None,
            reconnects_used: 0,
            retained: VecDeque::new(),
            sessions_opened: 0,
            reconnects_total: 0,
            stale_replies_ignored: 0,
            full_rejections: 0,
            closed_count: 0,
            stale_count: 0,
            connect_failed_count: 0,
            no_healthy_count: 0,
        }
    }

    /// Open a fresh connect generation and mark it as the one in progress.
    pub fn begin_generation(&mut self) -> EndpointGeneration {
        let generation = self.next_generation;
        self.next_generation = self.next_generation.next();
        self.connecting_generation = Some(generation);
        generation
    }

    /// The generation currently being connected, if any.
    pub fn connecting_generation(&self) -> Option<EndpointGeneration> {
        self.connecting_generation
    }

    /// True when `generation` names the current session.
    pub fn is_current_session(&self, generation: EndpointGeneration) -> bool {
        self.current
            .as_ref()
            .is_some_and(|s| s.generation == generation)
    }

    /// True when `generation` is the connect in progress.
    pub fn is_connecting(&self, generation: EndpointGeneration) -> bool {
        self.connecting_generation == Some(generation)
    }

    /// Count a reply that named a generation no longer current/connecting.
    pub fn note_stale_reply(&mut self) {
        self.stale_replies_ignored += 1;
    }

    /// Install a freshly connected session as current.
    ///
    /// Rejects a stale generation (an old connect that lost a reconnect
    /// race) and rejects a second session when the single slot is taken.
    /// A successful install ends the connect-in-progress and resets the
    /// reconnect budget — a healthy session means the storm is over.
    pub fn install_session(
        &mut self,
        generation: EndpointGeneration,
        conn_index: usize,
        addr: SocketAddr,
    ) -> Result<(), InstallError> {
        if self.connecting_generation != Some(generation) {
            self.note_stale_reply();
            return Err(InstallError::StaleGeneration);
        }
        if self.current.is_some() {
            self.full_rejections += 1;
            return Err(InstallError::SessionsFull);
        }
        self.current = Some(LiveSession {
            generation,
            conn_index,
            addr,
            pressure: None,
        });
        self.connecting_generation = None;
        self.reconnects_used = 0;
        self.sessions_opened += 1;
        Ok(())
    }

    /// Record a pressure snapshot, but only for the current session.
    /// A snapshot from an old generation is ignored and counted.
    pub fn record_pressure(
        &mut self,
        generation: EndpointGeneration,
        report: WebSocketClientReport,
    ) {
        match &mut self.current {
            Some(session) if session.generation == generation => {
                session.pressure = Some(report);
            }
            _ => self.note_stale_reply(),
        }
    }

    /// The connection-pool index of the current session, if open.
    pub fn current_conn_index(&self) -> Option<usize> {
        self.current.as_ref().map(|s| s.conn_index)
    }

    /// The current session's generation, if open.
    pub fn current_generation(&self) -> Option<EndpointGeneration> {
        self.current.as_ref().map(|s| s.generation)
    }

    /// Retire the current session, retaining a bounded report for it.
    ///
    /// No-op when there is no current session. Returns the conn-pool index
    /// freed, if any, so the manager can return that connection to idle.
    pub fn retire_current(&mut self, reason: SessionEndReason) -> Option<usize> {
        let session = self.current.take()?;
        match reason {
            SessionEndReason::Stale => self.stale_count += 1,
            SessionEndReason::ClosedByPeer | SessionEndReason::ClosedLocal => {
                self.closed_count += 1
            }
            SessionEndReason::ConnectFailed => self.connect_failed_count += 1,
        }
        self.push_retained(RetainedSessionReport {
            generation: session.generation,
            addr: Some(session.addr),
            reason,
            pressure: session.pressure,
        });
        Some(session.conn_index)
    }

    /// Retain a report for a connect that never produced a session.
    pub fn retain_failed_connect(&mut self, generation: EndpointGeneration, no_healthy: bool) {
        if no_healthy {
            self.no_healthy_count += 1;
        } else {
            self.connect_failed_count += 1;
        }
        self.connecting_generation = None;
        self.push_retained(RetainedSessionReport {
            generation,
            addr: None,
            reason: SessionEndReason::ConnectFailed,
            pressure: None,
        });
    }

    /// True when another reconnect is within the policy budget.
    pub fn can_reconnect(&self) -> bool {
        self.reconnects_used < self.max_reconnects
    }

    /// Spend one reconnect from the budget. Returns false when the budget is
    /// exhausted (the manager must surface `NoHealthyEndpoint`).
    pub fn record_reconnect(&mut self) -> bool {
        if !self.can_reconnect() {
            return false;
        }
        self.reconnects_used += 1;
        self.reconnects_total += 1;
        true
    }

    /// Whether a session is currently open.
    pub fn has_session(&self) -> bool {
        self.current.is_some()
    }

    /// Retained closed/stale reports, oldest first.
    pub fn retained(&self) -> impl Iterator<Item = &RetainedSessionReport> {
        self.retained.iter()
    }

    /// The retained report cap.
    pub fn retained_cap(&self) -> usize {
        self.retained_cap
    }

    /// A manager-level report snapshot.
    pub fn report(&self) -> WebSocketManagerReport {
        WebSocketManagerReport {
            endpoint: self.endpoint_id,
            current_generation: self.current_generation(),
            has_session: self.current.is_some(),
            sessions_open: usize::from(self.current.is_some()),
            max_sessions: self.max_sessions,
            reconnects_used: self.reconnects_used,
            max_reconnects: self.max_reconnects,
            sessions_opened: self.sessions_opened,
            reconnects_total: self.reconnects_total,
            stale_replies_ignored: self.stale_replies_ignored,
            full_rejections: self.full_rejections,
            closed_count: self.closed_count,
            stale_count: self.stale_count,
            connect_failed_count: self.connect_failed_count,
            no_healthy_count: self.no_healthy_count,
            current_pressure: self.current.as_ref().and_then(|s| s.pressure.clone()),
            retained: self.retained.iter().cloned().collect(),
        }
    }

    fn push_retained(&mut self, report: RetainedSessionReport) {
        if self.retained.len() == self.retained_cap {
            self.retained.pop_front();
        }
        self.retained.push_back(report);
    }
}

/// Snapshot of a manager's live state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSocketManagerReport {
    /// Endpoint identity.
    pub endpoint: EndpointId,
    /// Current session generation, if open.
    pub current_generation: Option<EndpointGeneration>,
    /// Whether a session is open.
    pub has_session: bool,
    /// Open sessions right now (0 or 1 in the first form).
    pub sessions_open: usize,
    /// Configured session cap.
    pub max_sessions: usize,
    /// Reconnects spent since the last healthy session.
    pub reconnects_used: usize,
    /// Configured reconnect budget.
    pub max_reconnects: usize,
    /// Sessions opened over the manager's life.
    pub sessions_opened: u64,
    /// Reconnects spent over the manager's life.
    pub reconnects_total: u64,
    /// Replies ignored because they named an old generation.
    pub stale_replies_ignored: u64,
    /// Install rejections because the session slot was full.
    pub full_rejections: u64,
    /// Sessions that ended closed.
    pub closed_count: u64,
    /// Sessions retired stale.
    pub stale_count: u64,
    /// Connects that failed to open any session.
    pub connect_failed_count: u64,
    /// Connects that exhausted reconnects with no healthy endpoint.
    pub no_healthy_count: u64,
    /// Last pressure snapshot of the current session.
    pub current_pressure: Option<WebSocketClientReport>,
    /// Bounded retained reports for ended sessions, oldest first.
    pub retained: Vec<RetainedSessionReport>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(n: u16) -> SocketAddr {
        format!("127.0.0.1:{n}").parse().unwrap()
    }

    fn state() -> WebSocketManagerState {
        WebSocketManagerState::new(EndpointId::new(1), 1, 3, 2)
    }

    #[test]
    fn install_then_retire_tracks_generation_and_retained() {
        let mut s = state();
        let g = s.begin_generation();
        assert!(s.is_connecting(g));
        s.install_session(g, 0, addr(8080)).unwrap();
        assert!(s.is_current_session(g));
        assert!(s.has_session());
        let freed = s.retire_current(SessionEndReason::ClosedByPeer);
        assert_eq!(freed, Some(0));
        assert!(!s.has_session());
        let report = s.report();
        assert_eq!(report.closed_count, 1);
        assert_eq!(report.retained.len(), 1);
        assert_eq!(report.retained[0].reason, SessionEndReason::ClosedByPeer);
    }

    #[test]
    fn stale_generation_install_is_rejected_and_counted() {
        let mut s = state();
        let g1 = s.begin_generation();
        // A newer connect supersedes g1.
        let _g2 = s.begin_generation();
        let err = s.install_session(g1, 0, addr(1)).unwrap_err();
        assert_eq!(err, InstallError::StaleGeneration);
        assert_eq!(s.report().stale_replies_ignored, 1);
    }

    #[test]
    fn old_generation_pressure_is_ignored() {
        let mut s = state();
        let g1 = s.begin_generation();
        s.install_session(g1, 0, addr(1)).unwrap();
        // Retire and open a new generation.
        s.retire_current(SessionEndReason::Stale);
        let g2 = s.begin_generation();
        s.install_session(g2, 0, addr(2)).unwrap();
        // A late pressure snapshot from g1 must not touch g2.
        let old = WebSocketClientReport {
            queued_outbound_bytes: 999,
            ..Default::default()
        };
        s.record_pressure(g1, old);
        assert!(
            s.report()
                .current_pressure
                .is_none_or(|p| p.queued_outbound_bytes != 999)
        );
        assert!(s.report().stale_replies_ignored >= 1);
    }

    #[test]
    fn second_session_is_rejected_when_slot_full() {
        let mut s = state();
        let g1 = s.begin_generation();
        s.install_session(g1, 0, addr(1)).unwrap();
        // A second connect that resolves while a session is live.
        let g2 = s.begin_generation();
        let err = s.install_session(g2, 1, addr(2)).unwrap_err();
        assert_eq!(err, InstallError::SessionsFull);
        assert_eq!(s.report().full_rejections, 1);
    }

    #[test]
    fn reconnect_budget_is_bounded_and_resets_on_healthy_session() {
        let mut s = WebSocketManagerState::new(EndpointId::new(1), 1, 2, 2);
        assert!(s.record_reconnect());
        assert!(s.record_reconnect());
        assert!(!s.can_reconnect());
        assert!(!s.record_reconnect());
        // A healthy session resets the budget.
        let g = s.begin_generation();
        s.install_session(g, 0, addr(1)).unwrap();
        assert!(s.can_reconnect());
        assert_eq!(s.report().reconnects_total, 2);
    }

    #[test]
    fn retained_reports_are_bounded_to_cap() {
        let mut s = WebSocketManagerState::new(EndpointId::new(1), 1, 5, 2);
        for i in 0..4u16 {
            let g = s.begin_generation();
            s.install_session(g, 0, addr(i)).unwrap();
            s.retire_current(SessionEndReason::ClosedByPeer);
        }
        let report = s.report();
        assert_eq!(report.retained.len(), 2, "cap is 2");
        // Oldest evicted: the two newest remain.
        assert_eq!(report.retained[0].addr, Some(addr(2)));
        assert_eq!(report.retained[1].addr, Some(addr(3)));
    }

    #[test]
    fn failed_connect_with_no_healthy_endpoint_is_retained() {
        let mut s = state();
        let g = s.begin_generation();
        s.retain_failed_connect(g, true);
        let report = s.report();
        assert_eq!(report.no_healthy_count, 1);
        assert_eq!(report.retained.len(), 1);
        assert_eq!(report.retained[0].reason, SessionEndReason::ConnectFailed);
        assert!(report.retained[0].addr.is_none());
    }
}
