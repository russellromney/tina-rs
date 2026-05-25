//! Manifest adapters for the HTTP/1, HTTP/2, and WebSocket configs.
//!
//! Each `budget_surfaces(prefix)` reads a concrete config and emits
//! [`BudgetSurface`] rows so a service declares its protocol caps in
//! one manifest instead of re-typing them at each `register_*` call.
//! The mapping is exact: one config field, one row. Adapters describe
//! what the config already says; they never invent caps.
//!
//! Time deadlines (`service_call_timeout`, `header_read_timeout`,
//! `request_timeout`, ...) are deliberately *not* surfaced: this
//! phase's [`BudgetUnit`] vocabulary has no time unit, so a deadline
//! would have to be faked as a count. Deadlines stay manual config.

use tina_runtime::budget::{BudgetCap, BudgetKind, BudgetSurface, BudgetUnit};

use crate::http2::{Http2ClientLimits, Http2ServerConfig};
use crate::types::{HttpClientConfig, HttpServerConfig, PoolConfig};
use crate::websocket::WebSocketLimits;

/// A fixed count surface (messages / sessions / connections).
fn count(
    name: String,
    kind: BudgetKind,
    unit: BudgetUnit,
    cap: usize,
    owner: &str,
) -> BudgetSurface {
    BudgetSurface::new(name, kind, unit, BudgetCap::fixed(cap)).owned_by(owner)
}

/// A fixed byte budget.
fn bytes(name: String, cap: usize, owner: &str) -> BudgetSurface {
    BudgetSurface::new(
        name,
        BudgetKind::BodyBytes,
        BudgetUnit::Bytes,
        BudgetCap::fixed(cap),
    )
    .owned_by(owner)
}

impl HttpServerConfig {
    /// Manifest rows for the listener/connection mailboxes and the
    /// per-request byte caps this server config names.
    pub fn budget_surfaces(&self, prefix: &str) -> Vec<BudgetSurface> {
        vec![
            count(
                format!("{prefix}.listener.mailbox"),
                BudgetKind::Mailbox,
                BudgetUnit::Messages,
                self.listener_mailbox_capacity,
                "http",
            ),
            count(
                format!("{prefix}.connection.mailbox"),
                BudgetKind::Mailbox,
                BudgetUnit::Messages,
                self.connection_mailbox_capacity,
                "http",
            ),
            bytes(
                format!("{prefix}.request.body"),
                self.limits.max_body_bytes,
                "http",
            ),
            bytes(
                format!("{prefix}.request.headers"),
                self.limits.max_header_bytes,
                "http",
            ),
        ]
    }
}

impl HttpClientConfig {
    /// Manifest rows for the response byte caps this client config names.
    pub fn budget_surfaces(&self, prefix: &str) -> Vec<BudgetSurface> {
        vec![
            bytes(
                format!("{prefix}.response.body"),
                self.limits.max_body_bytes,
                "http",
            ),
            bytes(
                format!("{prefix}.response.headers"),
                self.limits.max_header_bytes,
                "http",
            ),
        ]
    }
}

impl PoolConfig {
    /// Manifest rows for the pool capacity and mailbox this config names.
    pub fn budget_surfaces(&self, prefix: &str) -> Vec<BudgetSurface> {
        vec![
            count(
                format!("{prefix}.pool"),
                BudgetKind::Pool,
                BudgetUnit::Connections,
                self.capacity,
                "http",
            ),
            count(
                format!("{prefix}.pool.mailbox"),
                BudgetKind::Mailbox,
                BudgetUnit::Messages,
                self.mailbox_capacity,
                "http",
            ),
        ]
    }
}

impl Http2ServerConfig {
    /// Manifest rows for the HTTP/2 listener: mailboxes, concurrent
    /// streams, body caps, and the outbound frame queue.
    pub fn budget_surfaces(&self, prefix: &str) -> Vec<BudgetSurface> {
        vec![
            count(
                format!("{prefix}.listener.mailbox"),
                BudgetKind::Mailbox,
                BudgetUnit::Messages,
                self.listener_mailbox_capacity,
                "http2",
            ),
            count(
                format!("{prefix}.connection.mailbox"),
                BudgetKind::Mailbox,
                BudgetUnit::Messages,
                self.connection_mailbox_capacity,
                "http2",
            ),
            count(
                format!("{prefix}.streams"),
                BudgetKind::ProtocolSession,
                BudgetUnit::Sessions,
                self.limits.max_concurrent_streams,
                "http2",
            ),
            bytes(
                format!("{prefix}.request.body"),
                self.limits.max_body_bytes,
                "http2",
            ),
            bytes(
                format!("{prefix}.response.body"),
                self.limits.max_response_body_bytes,
                "http2",
            ),
            count(
                format!("{prefix}.outbound_queue"),
                BudgetKind::Mailbox,
                BudgetUnit::Messages,
                self.limits.connection_outbound_queue_capacity,
                "http2",
            ),
        ]
    }
}

impl Http2ClientLimits {
    /// Manifest rows for the HTTP/2 client: concurrent streams, body
    /// cap, outbound queue, and the pre-connect submit queue.
    pub fn budget_surfaces(&self, prefix: &str) -> Vec<BudgetSurface> {
        vec![
            count(
                format!("{prefix}.streams"),
                BudgetKind::ProtocolSession,
                BudgetUnit::Sessions,
                self.max_concurrent_streams,
                "http2",
            ),
            bytes(
                format!("{prefix}.response.body"),
                self.max_response_body_bytes,
                "http2",
            ),
            count(
                format!("{prefix}.outbound_queue"),
                BudgetKind::Mailbox,
                BudgetUnit::Messages,
                self.connection_outbound_queue_capacity,
                "http2",
            ),
            count(
                format!("{prefix}.pre_connect_submit"),
                BudgetKind::Mailbox,
                BudgetUnit::Messages,
                self.pre_connect_submit_capacity,
                "http2",
            ),
        ]
    }
}

impl WebSocketLimits {
    /// Manifest rows for one WebSocket session's mailboxes, byte
    /// budgets, and broadcast fan-out cap.
    pub fn budget_surfaces(&self, prefix: &str) -> Vec<BudgetSurface> {
        vec![
            count(
                format!("{prefix}.app.mailbox"),
                BudgetKind::Mailbox,
                BudgetUnit::Messages,
                self.inbound_app_mailbox_capacity,
                "websocket",
            ),
            count(
                format!("{prefix}.outbound_frame_queue"),
                BudgetKind::Mailbox,
                BudgetUnit::Messages,
                self.outbound_frame_queue_capacity,
                "websocket",
            ),
            bytes(
                format!("{prefix}.message"),
                self.max_message_bytes,
                "websocket",
            ),
            bytes(format!("{prefix}.frame"), self.max_frame_bytes, "websocket"),
            bytes(
                format!("{prefix}.outbound_bytes"),
                self.max_queued_outbound_bytes,
                "websocket",
            ),
            count(
                format!("{prefix}.broadcast_fanout"),
                BudgetKind::EventSink,
                BudgetUnit::Connections,
                self.broadcast_fanout_max_targets,
                "websocket",
            ),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tina::capacity::CapacityPolicy;
    use tina_runtime::budget::ServiceBudgetManifest;

    fn validates(prefix: &str, surfaces: Vec<BudgetSurface>) {
        let mut manifest = ServiceBudgetManifest::new(prefix, CapacityPolicy::Production);
        manifest.extend(surfaces);
        manifest
            .validate()
            .unwrap_or_else(|e| panic!("{prefix} adapter rows must validate: {e:?}"));
    }

    #[test]
    fn http1_server_surfaces_validate() {
        let surfaces = HttpServerConfig::dev().budget_surfaces("http");
        let names: Vec<&str> = surfaces.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"http.listener.mailbox"));
        assert!(names.contains(&"http.request.body"));
        validates("http", surfaces);
    }

    #[test]
    fn http1_client_and_pool_surfaces_validate() {
        validates("client", HttpClientConfig::dev().budget_surfaces("client"));
        validates("pool", PoolConfig::dev().budget_surfaces("pool"));
    }

    #[test]
    fn http2_server_and_client_surfaces_validate() {
        let server = Http2ServerConfig::dev().budget_surfaces("h2");
        assert!(server.iter().any(|s| s.kind == BudgetKind::ProtocolSession));
        validates("h2", server);
        validates("h2c", Http2ClientLimits::default().budget_surfaces("h2c"));
    }

    #[test]
    fn websocket_surfaces_validate_and_name_fanout() {
        let surfaces = WebSocketLimits::default().budget_surfaces("ws");
        assert!(
            surfaces
                .iter()
                .any(|s| s.kind == BudgetKind::EventSink && s.name == "ws.broadcast_fanout")
        );
        validates("ws", surfaces);
    }
}
