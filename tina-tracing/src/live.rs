//! Per-`LiveTopologyReport` snapshot emission.
//!
//! [`emit_snapshot`] emits one event per shard plus one per remote
//! queue. Caller decides when to call. No background thread.
//!
//! Per-shard level: `Running` → INFO, `Stopped` → WARN, `Failed` → ERROR.
//! Per-remote-queue: DEBUG unless `rejected_full > 0`, then WARN.

use tina_runtime::{
    AffinityStatus, LiveQueueReport, LiveRemoteQueueReport, LiveShardReport, LiveShardState,
    LiveTopologyReport,
};
use tracing::{Level, event};

use crate::LIVE_TOPOLOGY_TARGET;
use crate::events::OptValue;

/// One event per shard, one per remote queue. Levels per module docs.
pub fn emit_snapshot(report: &LiveTopologyReport) {
    for shard in report.shards() {
        emit_shard(shard);
    }
    for queue in report.remote_queues() {
        emit_remote_queue(queue);
    }
}

fn emit_shard(shard: &LiveShardReport) {
    // tracing::event! bakes the level into a static callsite, so the
    // three states must each be their own event! invocation. We dedupe
    // the field set with an inner macro that expands once per state.
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
    let trace_dropped = OptValue(shard.trace_dropped());
    let configured_core = OptValue(shard.configured_core());
    let observed_core = OptValue(shard.observed_core());
    let worker_name = OptValue(shard.worker_name());
    let worker_thread_id = OptValue(shard.worker_thread_id());
    let affinity = affinity_status_name(shard.affinity_status());
    let shard_id = shard.shard().get();
    let trace_retention = shard.trace_retention();

    let queue = ingress;
    let ingress_capacity = queue.capacity() as u64;
    let depth = OptValue(queue.depth());
    let accepted = OptValue(queue.accepted());
    let rejected_full = OptValue(queue.rejected_full());
    let rejected_closed = OptValue(queue.rejected_closed());

    macro_rules! shard_event {
        ($lvl:expr) => {
            event!(
                target: LIVE_TOPOLOGY_TARGET,
                $lvl,
                kind = "live_shard",
                state,
                shard = shard_id,
                worker_name = ?worker_name,
                worker_thread_id = ?worker_thread_id,
                configured_core = ?configured_core,
                observed_core = ?observed_core,
                affinity_status = affinity,
                ingress_capacity,
                ingress_depth = ?depth,
                ingress_accepted = ?accepted,
                ingress_rejected_full = ?rejected_full,
                ingress_rejected_closed = ?rejected_closed,
                storage_lane_capacity,
                dns_lane_capacity,
                tls_lane_capacity,
                process_lane_capacity,
                signal_lane_capacity,
                trace_retention = ?trace_retention,
                trace_dropped = ?trace_dropped,
                owned_resource_count,
                worker_held_resource_count,
                pending_driver_call_count,
            )
        };
    }

    match shard.state() {
        LiveShardState::Running => shard_event!(Level::INFO),
        LiveShardState::Stopped => shard_event!(Level::WARN),
        LiveShardState::Failed => shard_event!(Level::ERROR),
    }
}

fn emit_remote_queue(queue: &LiveRemoteQueueReport) {
    let q: &LiveQueueReport = queue.queue();
    let source = queue.source().get();
    let target = queue.target().get();
    let capacity = q.capacity() as u64;
    let depth = OptValue(q.depth());
    let accepted = OptValue(q.accepted());
    let rejected_full = OptValue(q.rejected_full());
    let rejected_closed = OptValue(q.rejected_closed());

    macro_rules! remote_queue_event {
        ($lvl:expr) => {
            event!(
                target: LIVE_TOPOLOGY_TARGET,
                $lvl,
                kind = "live_remote_queue",
                source,
                target,
                capacity,
                depth = ?depth,
                accepted = ?accepted,
                rejected_full = ?rejected_full,
                rejected_closed = ?rejected_closed,
            )
        };
    }

    let any_full = matches!(q.rejected_full(), Some(n) if n > 0);
    if any_full {
        remote_queue_event!(Level::WARN);
    } else {
        remote_queue_event!(Level::DEBUG);
    }
}

/// Stable name for a [`LiveShardState`].
pub fn shard_state_name(state: LiveShardState) -> &'static str {
    match state {
        LiveShardState::Running => "Running",
        LiveShardState::Stopped => "Stopped",
        LiveShardState::Failed => "Failed",
    }
}

/// Stable name for an [`AffinityStatus`].
///
/// `Failed(reason)` flattens to `"Failed"`. The reason stays on the
/// underlying value; operators who need it read the report directly.
pub fn affinity_status_name(status: &AffinityStatus) -> &'static str {
    match status {
        AffinityStatus::NotRequested => "NotRequested",
        AffinityStatus::Applied => "Applied",
        AffinityStatus::Unsupported => "Unsupported",
        AffinityStatus::Failed(_) => "Failed",
        AffinityStatus::AdvisoryOnly => "AdvisoryOnly",
    }
}
