#[test]
fn tina_smoke() {
    let report = specimen_multi_turn_request_context::tina_run(
        specimen_multi_turn_request_context::TinaConfig {
            probe_delay_ms: 10,
            db_delay_ms: 10,
        },
    )
    .unwrap();
    assert_eq!(report.replies.len(), 1);
}

#[tokio::test]
async fn tokio_smoke() {
    let report = specimen_multi_turn_request_context::tokio_run(
        specimen_multi_turn_request_context::TokioConfig {
            probe_delay_ms: 10,
            db_delay_ms: 10,
        },
    )
    .await
    .unwrap();
    assert_eq!(report.replies.len(), 1);
}
