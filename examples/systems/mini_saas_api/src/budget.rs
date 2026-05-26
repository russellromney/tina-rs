//! The one place this service declares its budgets.
//!
//! Every hard cap the service runs with is a const here and a row in
//! [`manifest`]. Handlers and `register_*` calls read these consts;
//! they never invent their own numbers. A reader (or a cheap model)
//! copying this service finds all caps in this file, not by grepping
//! handlers, and gets the kind/unit/replay-impact of each from the
//! manifest object.
//!
//! Live numbers (what was used, what was full) never live here — they
//! come from runtime reports and are joined onto the manifest by
//! [`tina_runtime::budget::ServiceBudgetManifest::report`].

use tina::capacity::CapacityPolicy;
use tina_runtime::budget::{
    BudgetCap, BudgetKind, BudgetSurface, BudgetUnit, ServiceBudgetManifest,
};

/// Public request body cap. Oversize bodies get `413`. This is the cap
/// the saved replay case depends on, so its surface is replay-affecting.
pub const BODY_CAP_BYTES: usize = 32;
/// Body cap on the internal notification HTTP service.
pub const NOTIFY_BODY_CAP_BYTES: usize = 1024;
/// Controller ingress mailbox.
pub const CONTROLLER_MAILBOX_CAPACITY: usize = 2;
/// SQLite bridge worker mailbox.
pub const SQLITE_MAILBOX_CAPACITY: usize = 2;
/// SQLite in-flight admission (serial worker).
pub const SQLITE_IN_FLIGHT: usize = 1;
/// Notify-sink isolate mailbox.
pub const NOTIFY_MAILBOX_CAPACITY: usize = 8;
/// Outbound keepalive pool resource capacity.
pub const OUTBOUND_POOL_CAPACITY: usize = 1;
/// Outbound pool per-connection mailbox.
pub const OUTBOUND_CONNECTION_MAILBOX: usize = 8;
/// Outbound pool front-door mailbox.
pub const OUTBOUND_POOL_MAILBOX: usize = 8;
/// Public listener accept mailbox.
pub const MAIN_LISTENER_MAILBOX: usize = 4;
/// Concurrent request scopes the controller admits. One scope per
/// in-flight `POST /items/{id}/notify`; hitting the cap sheds with a typed
/// answer rather than growing.
pub const REQUEST_SCOPE_SET_CAPACITY: usize = 8;
/// Child rails one notify request's scope may register (the outbound
/// keepalive request call is the one cancelable child today).
pub const REQUEST_SCOPE_CHILD_CAP: usize = 2;

/// Build the service budget manifest. Production policy: an
/// unbounded escape would be rejected here, not silently accepted.
pub fn manifest() -> ServiceBudgetManifest {
    let mut m = ServiceBudgetManifest::new("mini_saas_api", CapacityPolicy::Production)
        // A copied service must at least bound its ingress, its public
        // body, and its bridge work. Validation fails loudly if a copy
        // drops one of these rows.
        .require_kind(BudgetKind::Mailbox)
        .require_kind(BudgetKind::BodyBytes)
        .require_kind(BudgetKind::BridgeInFlight);

    m.add(
        BudgetSurface::new(
            "http.request_body",
            BudgetKind::BodyBytes,
            BudgetUnit::Bytes,
            BudgetCap::fixed(BODY_CAP_BYTES),
        )
        .owned_by("main.listener"),
    );
    m.add(
        BudgetSurface::new(
            "controller.mailbox",
            BudgetKind::Mailbox,
            BudgetUnit::Messages,
            BudgetCap::fixed(CONTROLLER_MAILBOX_CAPACITY),
        )
        .owned_by("controller"),
    );
    m.add(
        BudgetSurface::new(
            "db.mailbox",
            BudgetKind::Mailbox,
            BudgetUnit::Messages,
            BudgetCap::fixed(SQLITE_MAILBOX_CAPACITY),
        )
        .owned_by("db.bridge"),
    );
    m.add(
        BudgetSurface::new(
            "db.in_flight",
            BudgetKind::BridgeInFlight,
            BudgetUnit::Calls,
            BudgetCap::fixed(SQLITE_IN_FLIGHT),
        )
        .owned_by("db.bridge"),
    );
    m.add(
        BudgetSurface::new(
            "notify.mailbox",
            BudgetKind::Mailbox,
            BudgetUnit::Messages,
            BudgetCap::fixed(NOTIFY_MAILBOX_CAPACITY),
        )
        .owned_by("notify.sink"),
    );
    m.add(
        BudgetSurface::new(
            "notify.request_body",
            BudgetKind::BodyBytes,
            BudgetUnit::Bytes,
            BudgetCap::fixed(NOTIFY_BODY_CAP_BYTES),
        )
        .owned_by("notify.listener"),
    );
    m.add(
        BudgetSurface::new(
            "outbound.pool",
            BudgetKind::Pool,
            BudgetUnit::Connections,
            BudgetCap::fixed(OUTBOUND_POOL_CAPACITY),
        )
        .owned_by("outbound.pool"),
    );
    m.add(
        BudgetSurface::new(
            "outbound.in_flight",
            BudgetKind::BridgeInFlight,
            BudgetUnit::Calls,
            BudgetCap::fixed(OUTBOUND_POOL_CAPACITY),
        )
        .owned_by("outbound.pool"),
    );
    m.add(
        BudgetSurface::new(
            "outbound.connection.mailbox",
            BudgetKind::Mailbox,
            BudgetUnit::Messages,
            BudgetCap::fixed(OUTBOUND_CONNECTION_MAILBOX),
        )
        .owned_by("outbound.pool"),
    );
    m.add(
        BudgetSurface::new(
            "outbound.pool.mailbox",
            BudgetKind::Mailbox,
            BudgetUnit::Messages,
            BudgetCap::fixed(OUTBOUND_POOL_MAILBOX),
        )
        .owned_by("outbound.pool"),
    );
    // Request-scope set: one scope per in-flight notify request. The cap
    // is the concurrent-request ceiling for the scoped path; admission
    // sheds past it rather than growing. The scope caps shape admission,
    // so they are replay-affecting (the default).
    m.add(
        BudgetSurface::new(
            "request.scope_set",
            BudgetKind::RequestScope,
            BudgetUnit::Sessions,
            BudgetCap::fixed(REQUEST_SCOPE_SET_CAPACITY),
        )
        .owned_by("controller"),
    );
    // Per-request child cap: how many cancelable child rails one request's
    // scope may register. A structural bound, not an aggregate counter.
    m.add(
        BudgetSurface::new(
            "request.scope_child_cap",
            BudgetKind::RequestScope,
            BudgetUnit::Calls,
            BudgetCap::fixed(REQUEST_SCOPE_CHILD_CAP),
        )
        .owned_by("controller"),
    );
    // Accept-queue depth. It bounds queued accepts for operator
    // dashboards but does not enter the saved body-cap replay case, so
    // it is display-only for replay: changing it must not invalidate
    // saved cases.
    m.add(
        BudgetSurface::new(
            "http.main_listener.mailbox",
            BudgetKind::Mailbox,
            BudgetUnit::Messages,
            BudgetCap::fixed(MAIN_LISTENER_MAILBOX),
        )
        .owned_by("main.listener")
        .display_only(),
    );
    m
}

/// The operative caps the service installs, read back *from* the
/// manifest object (not from the consts). `run()` builds these after
/// validation and passes them to the listener/bridge/pool/register
/// sites, so the manifest — not a scattered literal — is what actually
/// configures the running service.
#[derive(Debug, Clone, Copy)]
pub struct ServiceCaps {
    pub body: usize,
    pub notify_body: usize,
    pub controller_mailbox: usize,
    pub sqlite_mailbox: usize,
    pub notify_mailbox: usize,
    pub outbound_pool: usize,
    pub outbound_connection_mailbox: usize,
    pub outbound_pool_mailbox: usize,
    pub main_listener_mailbox: usize,
    pub request_scope_set: usize,
    pub request_scope_child_cap: usize,
}

impl ServiceCaps {
    /// Read every install-time cap from the manifest by name. Returns
    /// the names that were missing or unbounded so startup fails loudly
    /// rather than guessing a default.
    pub fn from_manifest(manifest: &ServiceBudgetManifest) -> Result<Self, Vec<String>> {
        let mut missing = Vec::new();
        let mut read = |name: &str| match manifest.cap_max(name) {
            Some(cap) => cap,
            None => {
                missing.push(name.to_string());
                0
            }
        };
        let caps = Self {
            body: read("http.request_body"),
            notify_body: read("notify.request_body"),
            controller_mailbox: read("controller.mailbox"),
            sqlite_mailbox: read("db.mailbox"),
            notify_mailbox: read("notify.mailbox"),
            outbound_pool: read("outbound.pool"),
            outbound_connection_mailbox: read("outbound.connection.mailbox"),
            outbound_pool_mailbox: read("outbound.pool.mailbox"),
            main_listener_mailbox: read("http.main_listener.mailbox"),
            request_scope_set: read("request.scope_set"),
            request_scope_child_cap: read("request.scope_child_cap"),
        };
        if missing.is_empty() {
            Ok(caps)
        } else {
            Err(missing)
        }
    }
}

/// The caps this service documents in its README and user-guide
/// example. The test pins that every documented cap is a manifest row
/// with the same number — docs and the manifest cannot drift.
pub fn documented_caps() -> Vec<(&'static str, usize)> {
    vec![
        ("http.request_body", BODY_CAP_BYTES),
        ("notify.request_body", NOTIFY_BODY_CAP_BYTES),
        ("controller.mailbox", CONTROLLER_MAILBOX_CAPACITY),
        ("db.mailbox", SQLITE_MAILBOX_CAPACITY),
        ("db.in_flight", SQLITE_IN_FLIGHT),
        ("notify.mailbox", NOTIFY_MAILBOX_CAPACITY),
        ("outbound.pool", OUTBOUND_POOL_CAPACITY),
        ("outbound.in_flight", OUTBOUND_POOL_CAPACITY),
        ("outbound.connection.mailbox", OUTBOUND_CONNECTION_MAILBOX),
        ("outbound.pool.mailbox", OUTBOUND_POOL_MAILBOX),
        ("request.scope_set", REQUEST_SCOPE_SET_CAPACITY),
        ("request.scope_child_cap", REQUEST_SCOPE_CHILD_CAP),
        ("http.main_listener.mailbox", MAIN_LISTENER_MAILBOX),
    ]
}
