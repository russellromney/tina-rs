use std::collections::HashMap;

use mini_saas_api::{RunMode, UserObservation, run};

#[test]
fn smoke_covers_service_layers() {
    let report = run(RunMode::Smoke).expect("mini_saas_api smoke ran");
    assert!(report.health_ok);
    assert!(report.ready_ok);
    assert!(report.created_item);
    assert!(report.read_item);
    assert!(report.notified_item);
    assert!(report.notify_after_peer_close);
    assert!(report.missing_404);
    assert!(report.method_405);
    assert!(report.bad_request_400);
    assert!(report.body_cap_413);
    assert!(report.db_constraint_409);
    assert!(report.ready_after_db_close_503);
    assert!(report.ready_during_shutdown_503);
    assert!(report.ingress_rejects_after_close);
    assert!(report.shutdown_in_flight_typed);
    assert!(report.shutdown_clean);
    assert!(report.multi_turn_notify);
    assert_eq!(
        report.observations,
        vec![
            obs("health", 200, "alive"),
            obs("ready", 200, "ready"),
            obs("create_item", 201, "id=1"),
            obs("read_item", 200, "id=1 name=alpha"),
            obs("notify_item", 200, "notified"),
            obs("notify_peer_close", 200, "notified"),
            obs("notify_after_peer_close", 200, "notified"),
            obs("missing_item", 404, "not_found"),
            obs("method_not_allowed", 405, "method_not_allowed"),
            obs("bad_create_body", 400, "bad_request"),
            obs("parser_body_cap", 413, ""),
            obs("duplicate_create", 409, "db_constraint"),
            obs("shutdown_in_flight_notify", 200, "notified"),
            obs(
                "ready_during_shutdown",
                503,
                "not_ready reasons=ingress_stopped",
            ),
            obs("post_after_ingress_close", 503, "ingress_stopped"),
            obs(
                "ready_after_db_close",
                503,
                "not_ready reasons=ingress_stopped,db_closed",
            ),
        ]
    );

    let capacity = capacity_fields(&report.capacity_before_shutdown_line);
    assert_eq!(capacity["http.body_cap"], "32");
    assert_eq!(capacity["http.request_body_current"], "0");
    assert_at_least(&capacity, "http.request_body_high_water", 10);
    assert_at_least(&capacity, "http.response_body_high_water", 19);
    assert_eq!(capacity["http.body_full"], "1");
    assert_eq!(capacity["http.body_timeout"], "0");
    assert_eq!(capacity["http.body_io_error"], "0");
    assert_eq!(capacity["controller.mailbox"], "2");
    assert_eq!(capacity["drain.stage"], "open");
    assert_eq!(capacity["drain.admits_after_drain"], "0");
    assert_at_least(&capacity, "drain.admitted", 5);
    assert_eq!(capacity["db.capacity"], "1");
    assert_eq!(capacity["db.waiters"], "0");
    assert_eq!(capacity["db.max_waiters"], "0");
    assert_eq!(capacity["db.in_flight"], "0");
    assert_eq!(capacity["db.high_water"], "1");
    assert_eq!(capacity["db.full"], "0");
    assert_eq!(capacity["db.timeout"], "0");
    assert_eq!(capacity["outbound.capacity"], "1");
    assert_eq!(capacity["outbound.waiters"], "0");
    assert_eq!(capacity["outbound.max_waiters"], "0");
    assert_eq!(capacity["outbound.in_flight"], "0");
    assert_eq!(capacity["outbound.high_water_waiters"], "0");
    assert_eq!(capacity["outbound.full"], "0");
    assert_eq!(capacity["outbound.closed"], "false");
    assert_eq!(capacity["outbound.closed_count"], "0");
    assert_eq!(capacity["outbound.cancel"], "0");

    let shutdown_capacity = capacity_fields(&report.capacity_during_shutdown_line);
    assert_eq!(shutdown_capacity["drain.stage"], "draining");
    assert_at_least(&shutdown_capacity, "drain.admits_after_drain", 1);

    let terminal = capacity_fields(&report.terminal_line);
    assert_eq!(terminal["db.capacity"], "1");
    assert_eq!(terminal["db.closed"], "1");
    assert_eq!(terminal["outbound.drain"], "Drained");
    assert_eq!(terminal["outbound.stop_requested"], "1");
    assert_eq!(terminal["outbound.stop_stopped"], "1");
    assert_eq!(terminal["outbound.stop_timed_out"], "0");
    assert_eq!(terminal["outbound.stop_rejected"], "0");
    assert_eq!(terminal["outbound.stop_already_closed"], "0");
    assert_eq!(terminal["outbound.stop_failures"], "0");
    assert!(report.terminal_line.contains("trace_pressure=completion["));
    assert_eq!(
        report.live_replay_fact,
        "case=mini_saas_body_full ops=[post:/items:41bytes] fact=status_413 cap=32"
    );
}

#[test]
fn pressure_covers_outbound_pool_full() {
    let report = run(RunMode::Pressure).expect("mini_saas_api pressure ran");
    assert!(report.outbound_pressure_503);
    assert!(report.shutdown_clean);
    assert!(
        report
            .observations
            .contains(&obs("pressure_first", 200, "notified"))
    );
    assert!(
        report
            .observations
            .contains(&obs("pressure_second", 503, "outbound_full"))
    );

    let capacity = capacity_fields(&report.capacity_before_shutdown_line);
    assert_eq!(capacity["outbound.full"], "1");
    assert_eq!(capacity["outbound.capacity"], "1");
    assert_eq!(capacity["outbound.max_waiters"], "0");

    let terminal = capacity_fields(&report.terminal_line);
    assert_eq!(terminal["outbound.drain"], "Drained");
    assert_eq!(terminal["outbound.stop_requested"], "1");
    assert_eq!(terminal["outbound.stop_stopped"], "1");
    assert_eq!(terminal["outbound.stop_timed_out"], "0");
    assert_eq!(terminal["outbound.stop_rejected"], "0");
    assert_eq!(terminal["outbound.stop_already_closed"], "0");
    assert_eq!(terminal["outbound.stop_failures"], "0");
}

fn capacity_fields(line: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for field in line.split_whitespace() {
        let Some((key, value)) = field.split_once('=') else {
            continue;
        };
        assert!(
            out.insert(key.to_owned(), value.trim_end_matches(',').to_owned())
                .is_none(),
            "duplicate capacity key {key:?} in {line:?}"
        );
    }
    out
}

fn assert_at_least(fields: &HashMap<String, String>, key: &str, expected: usize) {
    let actual = fields[key]
        .parse::<usize>()
        .unwrap_or_else(|err| panic!("{key} should be a usize, got {:?}: {err}", fields[key]));
    assert!(
        actual >= expected,
        "{key} should be at least {expected}, got {actual}"
    );
}

fn obs(label: &'static str, status: u16, body: &str) -> UserObservation {
    UserObservation {
        label,
        status,
        body: body.to_owned(),
    }
}
