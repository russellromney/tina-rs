use std::collections::HashMap;

use mini_saas_api::{RunMode, UserObservation, run};
use tina_runtime::lifecycle::{
    CloseAdmission, CloseOutcome, Lifecycle, ResourceKind, ShutdownStep,
};

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

    // Startup summary line names every bounded surface, including
    // the two we cannot measure from this scope (bridge in-flight is
    // sampled live). The line is one greppable key=value sequence.
    let startup = &report.startup_summary_line;
    assert!(
        startup.starts_with("startup topology service=mini_saas_api "),
        "startup_summary_line shape: {startup}"
    );
    assert!(startup.contains("service=mini_saas_api"), "{startup}");
    assert!(startup.contains("measured=5"), "{startup}");
    assert!(startup.contains("unavailable=2"), "{startup}");
    // Plan: runtime summary must include at least one pool, bridge,
    // listener, and body surface.
    assert!(
        report
            .startup_discovery_lines
            .iter()
            .any(|l| l.contains("surface=http.main_listener.mailbox")),
        "missing listener surface in startup discovery: {:?}",
        report.startup_discovery_lines
    );
    assert!(
        report
            .startup_discovery_lines
            .iter()
            .any(|l| l.contains("surface=http.request_body")),
        "missing body surface in startup discovery: {:?}",
        report.startup_discovery_lines
    );
    assert!(
        report
            .startup_discovery_lines
            .iter()
            .any(|l| l.contains("surface=db.pool")),
        "missing db pool surface in startup discovery: {:?}",
        report.startup_discovery_lines
    );
    assert!(
        report
            .startup_discovery_lines
            .iter()
            .any(|l| l.contains("surface=db.bridge_in_flight") && l.contains("state=unavailable")),
        "missing bridge unavailable surface in startup discovery: {:?}",
        report.startup_discovery_lines
    );
    assert!(
        startup.contains("db.bridge_in_flight=unavailable"),
        "{startup}"
    );
    assert!(
        startup.contains("outbound.bridge_in_flight=unavailable"),
        "{startup}"
    );

    // Topology line (the head of the startup output) names both
    // listen addresses.
    let topology = report
        .startup_discovery_lines
        .first()
        .expect("startup_discovery_lines has a topology line");
    assert!(topology.starts_with("topology "), "{topology}");
    assert!(topology.contains("service=mini_saas_api"), "{topology}");
    assert!(topology.contains("main_addr="), "{topology}");
    assert!(topology.contains("notify_addr="), "{topology}");

    // Smoke must not see any unexpected full/drop. The smoke run
    // intentionally produces ONE body_full (request bigger than
    // BODY_CAP_BYTES); every other surface stays clean.
    assert_eq!(capacity["http.body_full"], "1");
    assert_eq!(capacity["outbound.full"], "0");
    assert_eq!(capacity["db.full"], "0");
    assert_eq!(capacity["http.body_timeout"], "0");
    assert_eq!(capacity["http.body_io_error"], "0");

    // Typed lifecycle transitions: the plan's "service starts NotReady,
    // becomes Ready, enters Draining, then Stopped" assertion. The
    // canonical sequence is recorded explicitly by the host so a
    // regression that skips a state is caught here, not implied by
    // separate field equality.
    assert_eq!(
        report.lifecycle_transitions,
        vec![
            Lifecycle::Starting,
            Lifecycle::Ready,
            Lifecycle::Draining,
            Lifecycle::Stopped,
        ],
        "lifecycle transition sequence regressed: {:?}",
        report.lifecycle_transitions,
    );

    // Typed topology: every named started component is reachable via
    // typed report, not just substring matching in the legacy line.
    let topology = report
        .topology
        .as_ref()
        .expect("typed ServiceTopology must be populated");
    assert_eq!(topology.service, "mini_saas_api");
    assert_eq!(topology.state, Lifecycle::Ready);
    let component_by_name: HashMap<&str, &str> = topology
        .components
        .iter()
        .map(|c| (c.name.as_str(), c.kind))
        .collect();
    // Each component must be the typed kind we expect; a regression
    // that registers `outbound.pool` as a "bridge" would be caught
    // here, not just by reading the legacy line.
    assert_eq!(component_by_name.get("main.listener"), Some(&"listener"));
    assert_eq!(component_by_name.get("notify.listener"), Some(&"listener"));
    assert_eq!(component_by_name.get("controller"), Some(&"isolate"));
    assert_eq!(component_by_name.get("db.bridge"), Some(&"bridge"));
    assert_eq!(component_by_name.get("outbound.pool"), Some(&"pool"));
    // Listener components carry the bound socket address.
    let main_listener = topology
        .components
        .iter()
        .find(|c| c.name == "main.listener")
        .unwrap();
    assert!(
        main_listener.address.contains(':'),
        "main.listener should carry a bound socket address: {:?}",
        main_listener.address,
    );

    // Typed health snapshot is in Stopped state with a non-empty
    // summary line that mentions the service and the typed state.
    let health = report
        .health_pre_shutdown
        .as_ref()
        .expect("health snapshot populated");
    assert_eq!(health.service, "mini_saas_api");
    assert_eq!(health.state, Lifecycle::Stopped);
    let health_line = health.summary_line();
    assert!(health_line.contains("state=stopped"), "{health_line}");
    assert!(
        health_line.contains("service=mini_saas_api"),
        "{health_line}",
    );

    // Typed shutdown report: every step the host drove is named, in
    // order, with its outcome. The drain step recorded the in-flight
    // notify finishing; the outbound pool drained cleanly; runtime
    // stopped cleanly.
    let shutdown = report
        .shutdown_report
        .as_ref()
        .expect("typed ServiceShutdownReport must be populated");
    assert!(shutdown.clean, "{shutdown:#?}");
    let step_kinds: Vec<ShutdownStep> = shutdown.steps.iter().map(|s| s.step).collect();
    assert_eq!(
        step_kinds,
        vec![
            ShutdownStep::StopIngress,
            ShutdownStep::DrainInFlight,
            ShutdownStep::CloseResource, // db bridge
            ShutdownStep::CloseResource, // outbound pool
            ShutdownStep::CloseResource, // notify listener
            ShutdownStep::CloseResource, // main listener
            ShutdownStep::StopOwner,
        ],
        "shutdown step order: {:?}",
        shutdown.steps,
    );
    assert!(
        shutdown.steps.iter().all(|s| s.outcome.is_clean()),
        "unclean step outcomes: {:#?}",
        shutdown.steps,
    );
    let close_names: Vec<&str> = shutdown.closes.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        close_names,
        vec![
            "db.bridge",
            "outbound.pool",
            "notify.listener",
            "main.listener",
        ],
    );
    let outbound_close = &shutdown.closes[1];
    assert_eq!(outbound_close.name, "outbound.pool");
    assert_eq!(outbound_close.kind, ResourceKind::Pool);
    assert_eq!(outbound_close.admission, CloseAdmission::Drain);
    assert!(matches!(outbound_close.outcome, CloseOutcome::Clean));
    assert!(
        outbound_close.details.contains("requested=1"),
        "{:?}",
        outbound_close.details,
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
