#[test]
fn tina_smoke() {
    let report = specimen_multi_turn_request_context::tina_run(
        specimen_multi_turn_request_context::TinaConfig {
            probe_delay_ms: 10,
            db_delay_ms: 10,
        },
    )
    .unwrap();
    assert_eq!(report.replies, ["ready"]);
}

#[test]
fn tina_reports_not_ready_when_probe_times_out() {
    let report = specimen_multi_turn_request_context::tina_run(
        specimen_multi_turn_request_context::TinaConfig {
            probe_delay_ms: 60,
            db_delay_ms: 10,
        },
    )
    .unwrap();
    assert_eq!(report.replies, ["not_ready"]);
}

#[test]
fn tina_reports_not_ready_when_db_times_out() {
    let report = specimen_multi_turn_request_context::tina_run(
        specimen_multi_turn_request_context::TinaConfig {
            probe_delay_ms: 10,
            db_delay_ms: 60,
        },
    )
    .unwrap();
    assert_eq!(report.replies, ["not_ready"]);
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
    assert_eq!(report.replies, ["ready"]);
}
