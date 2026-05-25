//! Manifest adapter for the SQLite bridge install config.
//!
//! `budget_surfaces(prefix)` emits [`BudgetSurface`] rows for the
//! bounded surfaces this bridge exposes at install time: the worker
//! mailbox, in-flight admission, the pending-reply slots, and the
//! external worker pool. The database path is deliberately *not* a
//! surface — file paths are config, not a capacity budget, so a
//! connection string (which may embed a password) never becomes a row.

use tina_runtime::budget::{BudgetCap, BudgetKind, BudgetSurface, BudgetUnit};

use crate::types::SqliteConfig;

impl SqliteConfig {
    /// Manifest rows for the bounded surfaces this bridge install
    /// names. `prefix` is dotted onto each surface name (e.g. `db`).
    pub fn budget_surfaces(&self, prefix: &str) -> Vec<BudgetSurface> {
        let row = |name: String, kind: BudgetKind, unit: BudgetUnit, cap: usize| {
            BudgetSurface::new(name, kind, unit, BudgetCap::fixed(cap)).owned_by("sqlite")
        };
        vec![
            row(
                format!("{prefix}.mailbox"),
                BudgetKind::Mailbox,
                BudgetUnit::Messages,
                self.mailbox_capacity,
            ),
            row(
                format!("{prefix}.in_flight"),
                BudgetKind::BridgeInFlight,
                BudgetUnit::Calls,
                self.max_in_flight,
            ),
            row(
                format!("{prefix}.pending_reply"),
                BudgetKind::PendingReply,
                BudgetUnit::Calls,
                self.pending_reply_capacity,
            ),
            row(
                format!("{prefix}.pool"),
                BudgetKind::Pool,
                BudgetUnit::Connections,
                self.external_pool_size,
            ),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tina::capacity::CapacityPolicy;
    use tina_runtime::budget::ServiceBudgetManifest;

    #[test]
    fn sqlite_config_surfaces_validate_and_name_bridge() {
        let surfaces = SqliteConfig::memory().budget_surfaces("db");
        let names: Vec<&str> = surfaces.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"db.mailbox"));
        assert!(names.contains(&"db.in_flight"));
        assert!(names.contains(&"db.pending_reply"));
        assert!(
            surfaces
                .iter()
                .any(|s| s.kind == BudgetKind::BridgeInFlight)
        );

        let mut manifest = ServiceBudgetManifest::new("db", CapacityPolicy::Production);
        manifest.extend(surfaces);
        manifest
            .validate()
            .expect("sqlite bridge surfaces validate");
    }
}
