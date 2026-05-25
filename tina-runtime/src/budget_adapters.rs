//! Manifest adapters for the runtime config structs.
//!
//! Each adapter reads a concrete config and emits [`BudgetSurface`]
//! rows so a service does not have to hand-copy ingress/lane/shard
//! caps into its manifest. The mapping is exact: one config field, one
//! row, named with the caller's prefix.
//!
//! Adapters never invent caps. They only describe what the config
//! already says. Lane and ingress caps change scheduling and
//! backpressure under DST, so they are marked
//! [`ReplayImpact::ReplayAffecting`].

use crate::budget::{
    BudgetBuildError, BudgetCap, BudgetKind, BudgetSurface, BudgetUnit, ServiceBudgetManifest,
};
use crate::local_system::LocalSystemConfig;
use crate::multi_shard::MultiShardRuntimeConfig;
use crate::threaded::ThreadedRuntimeConfig;

/// Build a fixed runtime-owned surface row.
fn lane(name: String, kind: BudgetKind, cap: usize) -> BudgetSurface {
    BudgetSurface::new(name, kind, BudgetUnit::Messages, BudgetCap::fixed(cap)).owned_by("runtime")
}

impl LocalSystemConfig {
    /// Manifest rows for every bounded surface this config names. The
    /// `prefix` is dotted onto each surface name (e.g. `runtime`).
    pub fn budget_surfaces(&self, prefix: &str) -> Vec<BudgetSurface> {
        vec![
            lane(
                format!("{prefix}.ingress"),
                BudgetKind::RuntimeIngress,
                self.ingress_capacity,
            ),
            lane(
                format!("{prefix}.shard_pair"),
                BudgetKind::ShardPair,
                self.shard_pair_capacity,
            ),
            lane(
                format!("{prefix}.lane.storage"),
                BudgetKind::RuntimeLane,
                self.storage_lane_capacity,
            ),
            lane(
                format!("{prefix}.lane.dns"),
                BudgetKind::RuntimeLane,
                self.dns_lane_capacity,
            ),
            lane(
                format!("{prefix}.lane.tls"),
                BudgetKind::RuntimeLane,
                self.tls_lane_capacity,
            ),
            lane(
                format!("{prefix}.lane.process"),
                BudgetKind::RuntimeLane,
                self.process_lane_capacity,
            ),
            lane(
                format!("{prefix}.lane.signal"),
                BudgetKind::RuntimeLane,
                self.signal_capacity,
            ),
            lane(
                format!("{prefix}.remote_inbound_drain"),
                BudgetKind::RuntimeLane,
                self.remote_inbound_drain_budget,
            ),
        ]
    }
}

impl ThreadedRuntimeConfig {
    /// Manifest rows for every bounded surface this config names.
    pub fn budget_surfaces(&self, prefix: &str) -> Vec<BudgetSurface> {
        vec![
            lane(
                format!("{prefix}.ingress"),
                BudgetKind::RuntimeIngress,
                self.command_capacity,
            ),
            lane(
                format!("{prefix}.shard_pair"),
                BudgetKind::ShardPair,
                self.shard_pair_capacity,
            ),
            lane(
                format!("{prefix}.lane.storage"),
                BudgetKind::RuntimeLane,
                self.storage_lane_capacity,
            ),
            lane(
                format!("{prefix}.lane.dns"),
                BudgetKind::RuntimeLane,
                self.dns_lane_capacity,
            ),
            lane(
                format!("{prefix}.lane.tls"),
                BudgetKind::RuntimeLane,
                self.tls_lane_capacity,
            ),
            lane(
                format!("{prefix}.lane.process"),
                BudgetKind::RuntimeLane,
                self.process_lane_capacity,
            ),
            lane(
                format!("{prefix}.lane.signal"),
                BudgetKind::RuntimeLane,
                self.signal_capacity,
            ),
            lane(
                format!("{prefix}.remote_inbound_drain"),
                BudgetKind::RuntimeLane,
                self.remote_inbound_drain_budget,
            ),
        ]
    }
}

impl MultiShardRuntimeConfig {
    /// Manifest rows for the cross-shard transport this config names.
    pub fn budget_surfaces(&self, prefix: &str) -> Vec<BudgetSurface> {
        vec![lane(
            format!("{prefix}.shard_pair"),
            BudgetKind::ShardPair,
            self.shard_pair_capacity,
        )]
    }

    /// Build this config *from* the manifest. The mapping is exact:
    /// `MultiShardRuntimeConfig` has exactly one bounded field, so the
    /// `{prefix}.shard_pair` surface is the whole config. Builders only
    /// exist for exact mappings — configs with time deadlines or other
    /// non-budget fields stay hand-written and are not built here.
    pub fn from_manifest(
        manifest: &ServiceBudgetManifest,
        prefix: &str,
    ) -> Result<Self, BudgetBuildError> {
        Ok(Self {
            shard_pair_capacity: manifest
                .cap_of_kind(&format!("{prefix}.shard_pair"), BudgetKind::ShardPair)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::{ReplayImpact, ServiceBudgetManifest};
    use tina::capacity::CapacityPolicy;

    #[test]
    fn local_system_config_produces_named_rows() {
        let config = LocalSystemConfig::default();
        let surfaces = config.budget_surfaces("runtime");
        let names: Vec<&str> = surfaces.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"runtime.ingress"));
        assert!(names.contains(&"runtime.shard_pair"));
        assert!(names.contains(&"runtime.lane.dns"));
        assert!(names.contains(&"runtime.lane.tls"));
        // Lanes are replay-affecting.
        assert!(
            surfaces
                .iter()
                .all(|s| s.replay_impact == ReplayImpact::ReplayAffecting)
        );
    }

    #[test]
    fn adapter_rows_validate_in_a_manifest() {
        let config = LocalSystemConfig::default();
        let mut manifest = ServiceBudgetManifest::new("runtime", CapacityPolicy::Production);
        manifest.extend(config.budget_surfaces("runtime"));
        manifest
            .validate()
            .expect("default runtime caps are all non-zero and validate");
    }

    #[test]
    fn multi_shard_config_names_shard_pair() {
        let surfaces = MultiShardRuntimeConfig::default().budget_surfaces("ms");
        assert_eq!(surfaces.len(), 1);
        assert_eq!(surfaces[0].name, "ms.shard_pair");
        assert_eq!(surfaces[0].kind, BudgetKind::ShardPair);
    }

    #[test]
    fn multi_shard_config_round_trips_through_manifest() {
        // Exact mapping: config -> surfaces -> manifest -> config.
        let original = MultiShardRuntimeConfig {
            shard_pair_capacity: 123,
        };
        let mut manifest = ServiceBudgetManifest::new("ms", CapacityPolicy::Production);
        manifest.extend(original.budget_surfaces("ms"));
        manifest.validate().expect("surfaces validate");
        let rebuilt = MultiShardRuntimeConfig::from_manifest(&manifest, "ms")
            .expect("exact mapping rebuilds the config");
        assert_eq!(rebuilt, original);
    }

    #[test]
    fn from_manifest_reports_typed_errors() {
        use crate::budget::{BudgetCap, BudgetSurface, BudgetUnit};
        // Missing surface.
        let empty = ServiceBudgetManifest::new("ms", CapacityPolicy::Production);
        assert!(matches!(
            MultiShardRuntimeConfig::from_manifest(&empty, "ms"),
            Err(BudgetBuildError::MissingSurface(_))
        ));
        // Wrong kind under the expected name.
        let mut wrong = ServiceBudgetManifest::new("ms", CapacityPolicy::Production);
        wrong.add(BudgetSurface::new(
            "ms.shard_pair",
            BudgetKind::Mailbox,
            BudgetUnit::Messages,
            BudgetCap::fixed(4),
        ));
        assert!(matches!(
            MultiShardRuntimeConfig::from_manifest(&wrong, "ms"),
            Err(BudgetBuildError::WrongKind { .. })
        ));
    }
}
