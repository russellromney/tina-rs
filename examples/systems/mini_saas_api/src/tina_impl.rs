use std::path::Path;
use std::time::Duration;

use rusqlite::Connection;
use tina_http::{
    HttpResponse, HttpResponseBody, HttpServerConfig,
};
use tina_runtime::lifecycle::{
    Lifecycle, ServiceTopology, TopologyComponent,
};


mod controller;
mod harness;
mod serve;
mod shutdown;

// Entrypoints exposed to the crate root / `main`. Submodules reach shared
// helpers and each other's isolates through `super::` / sibling paths.
pub use harness::{prove_drain_cancels_active_scope, run, run_soak};
pub use serve::{prove_graceful_drain_completes_in_flight, serve};

// Caps are declared once in `crate::budget`; the startup summary reads the
// two consts the manifest is built from (a test ties them to the rows).
use crate::budget::{BODY_CAP_BYTES, CONTROLLER_MAILBOX_CAPACITY};

pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

/// Shared live counters for the controller's request-scope set.
///
/// The controller updates these as it admits and retires notify scopes;
/// the host snapshots them at shutdown to join the `request.scope_set`
/// budget surface with real numbers instead of declared config alone.
#[derive(Clone, Default)]
pub(crate) struct ScopeSetMetrics {
    inner: std::sync::Arc<ScopeSetMetricsInner>,
}

#[derive(Default)]
struct ScopeSetMetricsInner {
    capacity: std::sync::atomic::AtomicUsize,
    in_use: std::sync::atomic::AtomicUsize,
    high_water: std::sync::atomic::AtomicUsize,
    full_count: std::sync::atomic::AtomicU64,
}

impl ScopeSetMetrics {
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        let metrics = Self::default();
        metrics
            .inner
            .capacity
            .store(capacity, std::sync::atomic::Ordering::Relaxed);
        metrics
    }

    /// Record that `in_use` scopes are now admitted (caller passes the set
    /// length after the insert). Bumps the high-water mark.
    pub(crate) fn observe_in_use(&self, in_use: usize) {
        use std::sync::atomic::Ordering;
        self.inner.in_use.store(in_use, Ordering::Relaxed);
        self.inner.high_water.fetch_max(in_use, Ordering::Relaxed);
    }

    pub(crate) fn on_full(&self) {
        self.inner
            .full_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// `(capacity, in_use, high_water, full_count)`.
    pub(crate) fn snapshot(&self) -> (usize, usize, usize, u64) {
        use std::sync::atomic::Ordering;
        (
            self.inner.capacity.load(Ordering::Relaxed),
            self.inner.in_use.load(Ordering::Relaxed),
            self.inner.high_water.load(Ordering::Relaxed),
            self.inner.full_count.load(Ordering::Relaxed),
        )
    }
}

/// Extract the buffered body of a control-call response as text. Control
/// replies are always small buffered bodies built by `text`.
pub(crate) fn response_body_text(response: &HttpResponse) -> String {
    match &response.body {
        HttpResponseBody::Buffered(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        _ => String::new(),
    }
}

pub(crate) fn listener_config(max_body_bytes: usize) -> HttpServerConfig {
    let mut config = HttpServerConfig::pressure();
    config.limits.max_body_bytes = max_body_bytes;
    config.limits.keepalive_idle_timeout = Some(Duration::from_millis(500));
    config.service_call_timeout = Duration::from_secs(3);
    config
}

pub(crate) fn seed_db(path: &Path) -> anyhow::Result<()> {
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "CREATE TABLE items (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL UNIQUE
        );",
    )?;
    Ok(())
}
pub(crate) struct StartupSummary {
    pub(crate) summary_line: String,
    pub(crate) discovery_lines: Vec<String>,
    pub(crate) topology: ServiceTopology,
}

pub(crate) fn build_startup_summary(
    main_addr: std::net::SocketAddr,
    notify_addr: std::net::SocketAddr,
) -> StartupSummary {
    use tina::capacity::{CapacityMode, CapacitySurfaceReport};
    use tina_runtime::ServicePressureReport;

    // Surfaces declared at startup. We do not sample live counters
    // here — that happens later via `/debug/capacity`. The startup
    // line is a *topology* snapshot: names + caps, plus explicit
    // Unavailable markers for surfaces we know exist but cannot
    // measure from this scope.
    let body_cap = CapacitySurfaceReport::weighted(
        "http.request_body",
        CapacityMode::Fixed,
        BODY_CAP_BYTES,
        0,
        0,
        0,
        "bytes",
    );
    let controller_mailbox = CapacitySurfaceReport::count(
        "controller.mailbox",
        CapacityMode::Fixed,
        CONTROLLER_MAILBOX_CAPACITY,
        0,
        0,
        0,
    );
    let db_pool = CapacitySurfaceReport::count("db.pool", CapacityMode::Fixed, 1, 0, 0, 0);
    let outbound_pool =
        CapacitySurfaceReport::count("outbound.pool", CapacityMode::Fixed, 1, 0, 0, 0);
    // Listener mailbox is bounded by the HTTP listener config; we
    // declare its name here so on-call sees the surface exists even
    // when live depth/accept counters live behind `LiveQueueReport`
    // and aren't sampled from this scope.
    let main_listener = CapacitySurfaceReport::count(
        "http.main_listener.mailbox",
        CapacityMode::Fixed,
        listener_config(BODY_CAP_BYTES).listener_mailbox_capacity,
        0,
        0,
        0,
    );
    let mut report = ServicePressureReport::new("mini_saas_api");
    report.add_measured("body", body_cap);
    report.add_measured("mailbox", controller_mailbox);
    report.add_measured("pool", db_pool);
    report.add_measured("pool", outbound_pool);
    report.add_measured("listener", main_listener);
    // The sqlite bridge measures its own pressure but the bridge is
    // sampled live; at startup the count cap is the only fact we own
    // here. The other live counters are reported via `/debug/capacity`
    // and `terminal_line`. Name them so on-call sees "we plan to
    // measure this".
    report.add_unavailable(
        "db.bridge_in_flight",
        "bridge",
        "sampled live via SqliteMetricsHandle",
    );
    report.add_unavailable(
        "outbound.bridge_in_flight",
        "bridge",
        "sampled live via WorkerPool reports",
    );

    let topology_line =
        format!("topology service=mini_saas_api main_addr={main_addr} notify_addr={notify_addr}");
    let summary_line = format!("startup {} | {}", topology_line, report.summary_line());
    // Reuse `ServicePressureSurface::discovery_line()` instead of
    // duplicating the measured/unavailable format. The previous inline
    // copy used `reason:?` instead of the surface helper's quoting,
    // which would silently drift if the helper's escape rules
    // tightened. The smoke test asserts the resulting `state=unavailable`
    // / `reason="..."` shape.
    let mut discovery_lines = vec![topology_line];
    for surface in &report.surfaces {
        discovery_lines.push(surface.discovery_line());
    }

    // Build the typed `ServiceTopology` covering every started component
    // and the pressure surfaces. The legacy `summary_line` /
    // `discovery_lines` strings stay byte-identical for compatibility;
    // the typed report is the structured lifecycle/health/topology proof.
    let mut topology = ServiceTopology::new("mini_saas_api", Lifecycle::Ready);
    topology
        .push_component(
            TopologyComponent::new("main.listener", "listener", main_addr.to_string())
                .with_notes("public HTTP ingress"),
        )
        .push_component(
            TopologyComponent::new("notify.listener", "listener", notify_addr.to_string())
                .with_notes("notification HTTP service"),
        )
        .push_component(
            TopologyComponent::new("controller", "isolate", "")
                .with_notes("singleton controller + drain helper"),
        )
        .push_component(
            TopologyComponent::new("db.bridge", "bridge", "sqlite")
                .with_notes("tina-sqlite-bridge worker"),
        )
        .push_component(
            TopologyComponent::new("outbound.pool", "pool", notify_addr.to_string())
                .with_notes("keepalive pool to notify_addr"),
        );
    let topology = topology.with_pressure(report);

    StartupSummary {
        summary_line,
        discovery_lines,
        topology,
    }
}
