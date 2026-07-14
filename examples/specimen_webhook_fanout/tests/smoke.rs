use specimen_webhook_fanout::{
    MAX_ENDPOINTS, WebhookTerminal, assert_report_invariants, tina_impl, tokio_impl,
    upstream::{self, Behavior},
};
use tina_reqwest_bridge::ReqwestTransientReason;

#[test]
fn tokio_smoke() {
    assert_report_invariants("tokio", &tokio_impl::run().expect("tokio"));
}

#[test]
fn tina_smoke() {
    let report = tina_impl::run().expect("tina");
    assert_report_invariants("tina", &report);
    assert_eq!(report.tina_terminals.len(), 2, "{report:?}");
    assert!(report.tina_terminals.iter().any(|terminal| matches!(
        terminal,
        WebhookTerminal::Transient(ReqwestTransientReason::UpstreamServer { status })
            if status.as_u16() == 503
    )));
    assert!(report.tina_terminals.iter().any(|terminal| matches!(
        terminal,
        WebhookTerminal::Transient(
            ReqwestTransientReason::BridgeTimeout | ReqwestTransientReason::WorkerTimeout
        )
    )));
}

#[test]
fn upstream_rejects_request_sized_endpoint_sets_before_startup() {
    let behaviors = vec![Behavior::Ok; MAX_ENDPOINTS + 1];
    assert!(upstream::spawn(&behaviors).is_err());
}
