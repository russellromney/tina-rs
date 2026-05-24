#[test]
fn copied_service_path_smoke() {
    let report = system_copied_service_path::run_copied_service_path();
    assert_eq!(report.join_capacity, 2);
    assert_eq!(report.select_capacity, 2);
    assert_eq!(report.replay_roles, vec!["gateway", "session", "worker"]);
    assert!(matches!(
        report.session_control,
        tina_http::WebSocketSessionMsg::AppControl(tina_http::WebSocketSessionControl::Start)
    ));
}

