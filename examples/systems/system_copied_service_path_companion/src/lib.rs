use tina_http::{WebSocketSessionControl, WebSocketSessionMsg};
use tina_proof_harness::{LoadRun, LoadStop, OpOutcome, cold_work_made_progress};
use tina_runtime::{CallJoinSet, CallSelectSet};

pub fn companion_proof_line() -> String {
    let report = system_copied_service_path::run_copied_service_path();
    format!(
        "companion request={} roles={}",
        report.request_entry,
        report.replay_roles.len()
    )
}

pub fn compile_api_proof() {
    let _: CallJoinSet<&'static str, usize> = CallJoinSet::with_capacity(2);
    let _: CallSelectSet<&'static str, usize> = CallSelectSet::with_capacity(2);
    let control = WebSocketSessionMsg::AppControl(WebSocketSessionControl::Drain);
    assert!(matches!(
        control,
        WebSocketSessionMsg::AppControl(WebSocketSessionControl::Drain)
    ));
    let load = tina_proof_harness::load::run(
        LoadRun {
            workers: 1,
            stop: LoadStop::ops(1),
            label: "companion",
        },
        |_| OpOutcome::Ok,
        None::<fn() -> bool>,
    );
    cold_work_made_progress(&load).unwrap();
}

