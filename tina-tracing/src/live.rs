//! Per-`LiveTopologyReport` snapshot emission.
//!
//! [`emit_snapshot`] walks one topology snapshot and emits one
//! `tracing::Event` per shard plus one per remote queue. The caller
//! decides when to call this — at shutdown, on a periodic timer, after
//! a deploy probe, etc. This module never starts its own thread.
//!
//! Per-shard level reflects [`tina_runtime::LiveShardState`]:
//!
//! - `Running` → `INFO`
//! - `Stopped` → `WARN`
//! - `Failed` → `ERROR`
//!
//! Per-remote-queue events are `DEBUG` unless any `rejected_full` is
//! non-zero, in which case they are `WARN`. The honest signal is
//! "remote backpressure happened"; lifecycle-closed numbers are
//! reported but do not raise the level.

use tina_runtime::{
    AffinityStatus, LiveQueueReport, LiveRemoteQueueReport, LiveShardReport, LiveShardState,
    LiveTopologyReport,
};
use tracing::{Level, event};

use crate::LIVE_TOPOLOGY_TARGET;

/// Emits one tracing event per shard and one per remote queue in
/// `report`. See module docs for level mapping.
pub fn emit_snapshot(report: &LiveTopologyReport) {
    for shard in report.shards() {
        emit_shard(shard);
    }
    for queue in report.remote_queues() {
        emit_remote_queue(queue);
    }
}

fn emit_shard(shard: &LiveShardReport) {
    let state = shard_state_name(shard.state());
    let ingress = shard.ingress();
    let storage_lane_capacity = shard.storage_lane().capacity() as u64;
    let dns_lane_capacity = shard.dns_lane().capacity() as u64;
    let tls_lane_capacity = shard.tls_lane().capacity() as u64;
    let process_lane_capacity = shard.process_lane().capacity() as u64;
    let signal_lane_capacity = shard.signal_lane().capacity() as u64;
    let owned_resource_count = shard.owned_resource_count() as u64;
    let worker_held_resource_count = shard.worker_held_resource_count() as u64;
    let pending_driver_call_count = shard.pending_driver_call_count() as u64;
    let trace_dropped = shard.trace_dropped();
    let configured_core = shard.configured_core();
    let observed_core = shard.observed_core();
    let worker_name = shard.worker_name();
    let worker_thread_id = shard.worker_thread_id();
    let affinity = affinity_status_name(shard.affinity_status());

    let queue = ingress;
    match shard.state() {
        LiveShardState::Running => event!(
            target: LIVE_TOPOLOGY_TARGET,
            Level::INFO,
            kind = "live_shard",
            state,
            shard = shard.shard().get(),
            worker_name = ?worker_name,
            worker_thread_id = ?worker_thread_id,
            configured_core = ?configured_core,
            observed_core = ?observed_core,
            affinity_status = affinity,
            ingress_capacity = queue.capacity() as u64,
            ingress_depth = ?queue.depth(),
            ingress_accepted = ?queue.accepted(),
            ingress_rejected_full = ?queue.rejected_full(),
            ingress_rejected_closed = ?queue.rejected_closed(),
            storage_lane_capacity,
            dns_lane_capacity,
            tls_lane_capacity,
            process_lane_capacity,
            signal_lane_capacity,
            trace_retention = ?shard.trace_retention(),
            trace_dropped = ?trace_dropped,
            owned_resource_count,
            worker_held_resource_count,
            pending_driver_call_count,
        ),
        LiveShardState::Stopped => event!(
            target: LIVE_TOPOLOGY_TARGET,
            Level::WARN,
            kind = "live_shard",
            state,
            shard = shard.shard().get(),
            worker_name = ?worker_name,
            worker_thread_id = ?worker_thread_id,
            configured_core = ?configured_core,
            observed_core = ?observed_core,
            affinity_status = affinity,
            ingress_capacity = queue.capacity() as u64,
            ingress_depth = ?queue.depth(),
            ingress_accepted = ?queue.accepted(),
            ingress_rejected_full = ?queue.rejected_full(),
            ingress_rejected_closed = ?queue.rejected_closed(),
            storage_lane_capacity,
            dns_lane_capacity,
            tls_lane_capacity,
            process_lane_capacity,
            signal_lane_capacity,
            trace_retention = ?shard.trace_retention(),
            trace_dropped = ?trace_dropped,
            owned_resource_count,
            worker_held_resource_count,
            pending_driver_call_count,
        ),
        LiveShardState::Failed => event!(
            target: LIVE_TOPOLOGY_TARGET,
            Level::ERROR,
            kind = "live_shard",
            state,
            shard = shard.shard().get(),
            worker_name = ?worker_name,
            worker_thread_id = ?worker_thread_id,
            configured_core = ?configured_core,
            observed_core = ?observed_core,
            affinity_status = affinity,
            ingress_capacity = queue.capacity() as u64,
            ingress_depth = ?queue.depth(),
            ingress_accepted = ?queue.accepted(),
            ingress_rejected_full = ?queue.rejected_full(),
            ingress_rejected_closed = ?queue.rejected_closed(),
            storage_lane_capacity,
            dns_lane_capacity,
            tls_lane_capacity,
            process_lane_capacity,
            signal_lane_capacity,
            trace_retention = ?shard.trace_retention(),
            trace_dropped = ?trace_dropped,
            owned_resource_count,
            worker_held_resource_count,
            pending_driver_call_count,
        ),
    }
}

fn emit_remote_queue(queue: &LiveRemoteQueueReport) {
    let q: &LiveQueueReport = queue.queue();
    let any_full = matches!(q.rejected_full(), Some(n) if n > 0);
    if any_full {
        event!(
            target: LIVE_TOPOLOGY_TARGET,
            Level::WARN,
            kind = "live_remote_queue",
            source = queue.source().get(),
            target = queue.target().get(),
            capacity = q.capacity() as u64,
            depth = ?q.depth(),
            accepted = ?q.accepted(),
            rejected_full = ?q.rejected_full(),
            rejected_closed = ?q.rejected_closed(),
        );
    } else {
        event!(
            target: LIVE_TOPOLOGY_TARGET,
            Level::DEBUG,
            kind = "live_remote_queue",
            source = queue.source().get(),
            target = queue.target().get(),
            capacity = q.capacity() as u64,
            depth = ?q.depth(),
            accepted = ?q.accepted(),
            rejected_full = ?q.rejected_full(),
            rejected_closed = ?q.rejected_closed(),
        );
    }
}

/// Stable string name for a [`LiveShardState`].
pub fn shard_state_name(state: LiveShardState) -> &'static str {
    match state {
        LiveShardState::Running => "Running",
        LiveShardState::Stopped => "Stopped",
        LiveShardState::Failed => "Failed",
    }
}

/// Stable string name for an [`AffinityStatus`].
///
/// `Failed(reason)` flattens to `"Failed"` here; the structured `reason`
/// is preserved on the underlying status value but is not surfaced as a
/// separate field in this first form. Operators who need it can read
/// the report directly.
pub fn affinity_status_name(status: &AffinityStatus) -> &'static str {
    match status {
        AffinityStatus::NotRequested => "NotRequested",
        AffinityStatus::Applied => "Applied",
        AffinityStatus::Unsupported => "Unsupported",
        AffinityStatus::Failed(_) => "Failed",
        AffinityStatus::AdvisoryOnly => "AdvisoryOnly",
    }
}
