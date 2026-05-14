use mini_saas_api::{RunMode, run};

#[test]
fn smoke_covers_service_layers() {
    let report = run(RunMode::Smoke).expect("mini_saas_api smoke ran");
    assert!(report.health_ok);
    assert!(report.ready_ok);
    assert!(report.created_item);
    assert!(report.read_item);
    assert!(report.notified_item);
    assert!(report.missing_404);
    assert!(report.method_405);
    assert!(report.bad_request_400);
    assert!(report.body_cap_413);
    assert!(report.db_constraint_409);
    assert!(report.ready_after_db_close_503);
    assert!(report.ready_during_shutdown_503);
    assert!(report.shutdown_clean);
    assert!(report.multi_turn_notify);
    assert!(report.capacity_line.contains("db."));
    assert!(report.capacity_line.contains("outbound."));
    assert!(report.live_replay_fact.contains("status_413"));
}

#[test]
fn pressure_covers_outbound_pool_full() {
    let report = run(RunMode::Pressure).expect("mini_saas_api pressure ran");
    assert!(report.outbound_pressure_503);
    assert!(report.shutdown_clean);
}
