//! Canonical copied Tina service path.
//!
//! This crate is the compact skeleton. The companion crate owns the heavier
//! proof steps so this file stays copyable.

use std::time::Duration;

use tina_http::{WebSocketSessionControl, WebSocketSessionMsg};
use tina_proof_harness::{
    LoadObservation, LoadRun, LoadStop, OpOutcome, RunCapture, assert_cold_work_made_progress,
    assert_no_leaked_capacity_at_shutdown, assert_timer_kept_firing,
};
use tina_runtime::{CallJoinSet, CallSelectSet, ServiceTopology};
use tina_sim::dst::ReplayConfig;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopiedServiceReport {
    pub request_entry: &'static str,
    pub protocol_client: &'static str,
    pub session_control: WebSocketSessionMsg,
    pub durable_restore: &'static str,
    pub limit_policy: &'static str,
    pub topology: ServiceTopology,
    pub capture_events: usize,
    pub join_capacity: usize,
    pub select_capacity: usize,
    pub replay_roles: Vec<&'static str>,
}

pub fn run_copied_service_path() -> CopiedServiceReport {
    let capture = RunCapture::new("copied_service_path");
    let _observer = capture.observer();

    let load = tina_proof_harness::load::run_with_observation(
        LoadRun {
            workers: 1,
            stop: LoadStop::ops(3),
            label: "copied_service_path",
        },
        |_| OpOutcome::Ok,
        Some(LoadObservation::default),
    );
    assert_cold_work_made_progress(&load);
    assert_timer_kept_firing(&load);
    assert_no_leaked_capacity_at_shutdown(&load);

    let join: CallJoinSet<&'static str, ()> = CallJoinSet::with_capacity(2);
    let select: CallSelectSet<&'static str, ()> = CallSelectSet::with_capacity(2);
    let replay_config = ReplayConfig::new()
        .with_mailbox("gateway", 16)
        .with_mailbox("session", 16)
        .with_mailbox("worker", 8);

    let mut topology = ServiceTopology::new(
        "system_copied_service_path",
        tina_runtime::Lifecycle::Ready,
    );
    topology.push_component(tina_runtime::TopologyComponent::new(
        "gateway.http",
        "http",
        "native HTTP/1 request entry",
    ));
    topology.push_component(tina_runtime::TopologyComponent::new(
        "client.grpc",
        "protocol-client",
        "native HTTP/2/gRPC client path",
    ));
    topology.push_component(tina_runtime::TopologyComponent::new(
        "state.outbox",
        "durable-state",
        "restore before accepting work",
    ));

    let _shutdown_tick = Duration::from_millis(10);

    CopiedServiceReport {
        request_entry: "HTTP request -> gateway isolate -> bounded reply",
        protocol_client: "call another service with native HTTP/2/gRPC/WebSocket client",
        session_control: WebSocketSessionMsg::AppControl(WebSocketSessionControl::Start),
        durable_restore: "recover state before readiness",
        limit_policy: "admit or return visible Full/Rejected/Timeout",
        topology,
        capture_events: capture.trace().len(),
        join_capacity: join.capacity(),
        select_capacity: select.capacity(),
        replay_roles: replay_config.mailboxes.keys().copied().collect(),
    }
}

pub fn smoke_line() -> String {
    let report = run_copied_service_path();
    format!(
        "copied_service_path request={} client={} roles={} join={} select={}",
        report.request_entry,
        report.protocol_client,
        report.replay_roles.len(),
        report.join_capacity,
        report.select_capacity
    )
}
