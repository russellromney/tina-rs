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

// Phase 111: the typed service product surface built through
// `ServiceReportBuilder` must be present after a successful run, must
// name every component, must keep the shutdown summary alive after
// stop, and must explicitly mark the bridge surfaces sampled live as
// `Unavailable` rather than silently omit them.
#[test]
fn smoke_service_report_threads_every_component() {
    let report = run(RunMode::Smoke).expect("mini_saas_api smoke ran");
    let svc = report
        .service_report
        .as_ref()
        .expect("Phase 111 service_report must be populated after a clean run");

    // 1. Summary line names the service and one-line state.
    let summary = svc.summary_line();
    assert!(summary.contains("service=mini_saas_api"), "{summary}");
    assert!(summary.contains("lifecycle=stopped"), "{summary}");
    assert!(summary.contains("ready=false"), "{summary}");
    assert!(summary.contains("shutdown=clean:true"), "{summary}");
    assert!(summary.contains("replay=available"), "{summary}");

    // 2. Discovery lines name every started component.
    let lines = svc.discovery_lines();
    let names = [
        "main.listener",
        "notify.listener",
        "controller",
        "db.bridge",
        "outbound.pool",
    ];
    for name in names {
        assert!(
            lines.iter().any(|l| l.contains(name)),
            "service_report discovery_lines missing {name}: {lines:#?}"
        );
    }

    // 3. Bridge surfaces sampled live are `Unavailable` (not silently
    //    dropped). The smoke run uses the live-pressure path which only
    //    measures `db.bridge.capacity`; the startup-time
    //    `outbound.bridge_in_flight` and `db.bridge_in_flight` rolls into
    //    the pressure builder when present, but at the post-shutdown
    //    pressure snapshot only the bridge capacity is measured. Either
    //    way, every surface in the report is explicit.
    let pressure = svc.pressure();
    assert!(
        pressure
            .surfaces
            .iter()
            .any(|s| s.name == "db.bridge.capacity"),
        "pressure surfaces should name db.bridge.capacity: {:?}",
        pressure.surfaces.iter().map(|s| &s.name).collect::<Vec<_>>(),
    );

    // 4. Shutdown summary survives the run. The choreography ran in
    //    drive_script and its finished report was folded in.
    let shutdown_line = svc.shutdown_summary_line();
    assert!(shutdown_line.contains("service=mini_saas_api"), "{shutdown_line}");
    assert!(shutdown_line.contains("clean=true"), "{shutdown_line}");
    assert!(
        shutdown_line.contains("steps=7"),
        "shutdown should have 7 steps: {shutdown_line}",
    );

    // 5. The replay status is `Available` because the smoke run captured
    //    the `mini_saas_body_full` live fact.
    match svc.replay() {
        tina_runtime::service_report::ServiceReplayStatus::Available {
            case_name,
            projected_events,
        } => {
            assert_eq!(case_name, "mini_saas_api.live_replay");
            assert!(*projected_events >= 1);
        }
        other => panic!("expected Available replay status, got {other:?}"),
    }

    // 6. Capacity summary returns OK without name conflicts and
    //    includes the live db bridge capacity surface.
    let cap = svc
        .capacity_summary()
        .expect("capacity_summary builds from measured surfaces");
    assert!(cap.len() >= 1, "capacity_summary should not be empty");
}

// Phase 111: README documents one copied service-report path. The README
// pinned lines must remain present in the live output so a copy-paste
// from the docs reflects what the report actually says.
#[test]
fn smoke_service_report_lines_match_readme() {
    let report = run(RunMode::Smoke).expect("mini_saas_api smoke ran");
    let svc = report
        .service_report
        .as_ref()
        .expect("Phase 111 service_report must be populated after a clean run");
    let summary = svc.summary_line();
    // The README copy advertises these key=value pairs. If a future
    // change drops one, the docs are wrong; this test catches that.
    for fragment in [
        "service=mini_saas_api",
        "lifecycle=stopped",
        "ready=false",
        "health=stopped",
        "shutdown=clean:true",
        "replay=available",
    ] {
        assert!(
            summary.contains(fragment),
            "README-pinned fragment missing from summary_line: {fragment:?}\n{summary}",
        );
    }
}

// Phase 111: two runs of the smoke harness must produce the same
// `ServiceReport` fingerprint. The fingerprint deliberately strips
// wall-clock fields (shutdown step elapsed times, total elapsed, bound
// listener addresses) and absolute counters; the surviving subset is
// the shape a user is asked to grep for. A regression that introduces a
// hidden non-deterministic field — for instance, an embedded timestamp
// in a topology component or a random id in a surface name — surfaces
// here before it surfaces in CI dashboards.
#[test]
fn smoke_service_report_fingerprint_is_deterministic_across_runs() {
    let first = run(RunMode::Smoke).expect("first smoke ran");
    let second = run(RunMode::Smoke).expect("second smoke ran");

    let fp_first = service_report_fingerprint(
        first
            .service_report
            .as_ref()
            .expect("first service_report populated"),
    );
    let fp_second = service_report_fingerprint(
        second
            .service_report
            .as_ref()
            .expect("second service_report populated"),
    );
    assert_eq!(
        fp_first, fp_second,
        "ServiceReport fingerprint must be deterministic across runs;\n\
         first:\n{first}\nsecond:\n{second}",
        first = fp_first.join("\n"),
        second = fp_second.join("\n"),
    );
}

/// Deterministic projection of a [`ServiceReport`] for cross-run
/// equality. Drops wall-clock fields (shutdown elapsed, total elapsed,
/// listener bind addresses) and absolute pressure counters, keeping the
/// shape: service name, lifecycle, readiness, health state, component
/// names+kinds, pressure surface names+kinds+state, replay variant +
/// case name, shutdown clean / step-count / close-count.
fn service_report_fingerprint(svc: &tina_runtime::service_report::ServiceReport) -> Vec<String> {
    use tina_runtime::service_pressure::ServiceSurfaceState;
    use tina_runtime::service_report::ServiceReplayStatus;
    let mut out = vec![
        format!("service={}", svc.service()),
        format!("lifecycle={}", svc.lifecycle()),
        format!("ready={}", svc.readiness().ready),
        format!("health.state={}", svc.health().state),
    ];
    for component in &svc.topology().components {
        out.push(format!(
            "component name={} kind={}",
            component.name, component.kind,
        ));
    }
    for surface in &svc.pressure().surfaces {
        let state = match surface.state {
            ServiceSurfaceState::Measured(_) => "measured",
            ServiceSurfaceState::Unavailable { .. } => "unavailable",
        };
        out.push(format!(
            "surface name={} kind={} state={state}",
            surface.name, surface.kind,
        ));
    }
    match svc.replay() {
        ServiceReplayStatus::Available { case_name, .. } => {
            out.push(format!("replay=available case={case_name}"));
        }
        ServiceReplayStatus::Unsupported { facts } => {
            out.push(format!("replay=unsupported count={}", facts.len()));
        }
        ServiceReplayStatus::NotCaptured { reason } => {
            out.push(format!("replay=not_captured reason={reason}"));
        }
    }
    if let Some(shutdown) = svc.shutdown() {
        out.push(format!(
            "shutdown clean={} steps={} closes={}",
            shutdown.clean,
            shutdown.steps.len(),
            shutdown.closes.len(),
        ));
        // Step kinds in order (without elapsed durations).
        for step in &shutdown.steps {
            out.push(format!(
                "step kind={} outcome={}",
                step.step,
                step.outcome.kind_str(),
            ));
        }
        for close in &shutdown.closes {
            out.push(format!(
                "close name={} kind={} outcome={}",
                close.name,
                close.kind,
                close.outcome.kind_str(),
            ));
        }
    } else {
        out.push("shutdown=not_recorded".to_owned());
    }
    out
}

// Phase 111: pressure under load must include a `Full` surface in the
// service report. The pressure mode of the smoke harness forces one
// `outbound.full` admission rejection.
#[test]
fn pressure_service_report_names_full_surface() {
    let report = run(RunMode::Pressure).expect("mini_saas_api pressure ran");
    let svc = report
        .service_report
        .as_ref()
        .expect("service_report populated under pressure too");
    let summary = svc.summary_line();
    assert!(summary.contains("service=mini_saas_api"), "{summary}");
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
