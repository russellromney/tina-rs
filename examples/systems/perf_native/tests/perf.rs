use perf_native::{
    http_body_pressure_probe, run_all, run_native_rows, websocket_capacity_fill_probe,
};
use std::sync::{Mutex, MutexGuard};
use tina_proof_harness::SemanticMatch;

static PERF_TEST_LOCK: Mutex<()> = Mutex::new(());

fn perf_test_guard() -> MutexGuard<'static, ()> {
    PERF_TEST_LOCK
        .lock()
        .expect("perf tests must serialize process-wide counters")
}

#[test]
fn native_perf_comparison_rows_are_printable_and_bounded() {
    let _guard = perf_test_guard();
    let reports = run_all().expect("run native perf comparisons");
    assert_eq!(reports.len(), 9);
    let labels: Vec<_> = reports.iter().map(|report| report.label).collect();
    assert_eq!(
        labels,
        vec![
            "host_enqueue",
            "observed_admission",
            "host_request_reply",
            "service_request_reply_chain",
            "http1_close_request",
            "http1_keepalive_sequential",
            "http1_fixed_body_close",
            "http1_keepalive_steady_state_small",
            "http1_keepalive_steady_state_fixed",
        ]
    );
    for report in reports {
        println!("{}", report.tina.summary_line());
        println!("{}", report.baseline.summary_line());
        println!("{}", report.summary_line());
        println!("{}", report.json_line());

        assert_eq!(
            report.tina.load.ops_attempted, report.baseline.load.ops_attempted,
            "same work count for {}",
            report.label,
        );
        assert!(
            report.tina.load.ops_ok > 0 && report.baseline.load.ops_ok > 0,
            "both sides must do useful work for {}",
            report.label,
        );
        assert_eq!(
            report.tina.load.ops_err, 0,
            "Tina side should not shed native perf work for {}",
            report.label,
        );
        assert_eq!(
            report.baseline.load.ops_err, 0,
            "baseline side should not shed native perf work for {}",
            report.label,
        );
        assert_eq!(
            report.tina.load.ops_timeout, 0,
            "Tina side should not time out native perf work for {}",
            report.label,
        );
        assert_eq!(
            report.baseline.load.ops_timeout, 0,
            "baseline side should not time out native perf work for {}",
            report.label,
        );
        if report.semantic_match == SemanticMatch::Exact {
            assert_eq!(
                report.tina.load.ops_ok, report.tina.load.ops_attempted,
                "exact Tina row should admit all configured work for {}",
                report.label,
            );
            assert_eq!(
                report.baseline.load.ops_ok, report.baseline.load.ops_attempted,
                "exact baseline row should admit all configured work for {}",
                report.label,
            );
        }
        assert!(
            report.tina.load.leak_clean && report.baseline.load.leak_clean,
            "both sides must end clean for {}",
            report.label,
        );
        assert!(
            report.summary_line().contains("perf-compare "),
            "comparison line shape"
        );
        assert_eq!(
            report.samples, 5,
            "median-of-five samples for {}",
            report.label
        );
        assert!(
            report.summary_line().contains("samples=5"),
            "comparison samples line shape"
        );
        assert!(
            report.summary_line().contains("tina_allocations="),
            "comparison allocation line shape"
        );
        assert!(
            report.tina.allocations.is_some() && report.baseline.allocations.is_some(),
            "both sides should carry allocation evidence for {}",
            report.label,
        );
        assert!(
            report
                .json_line()
                .contains("\"schema\":\"tina.perf_compare.v1\""),
            "comparison json shape"
        );
        assert!(
            report.json_line().contains("\"samples\":5"),
            "comparison json samples shape"
        );
    }
}

#[test]
fn native_protocol_rows_are_printable_and_bounded() {
    let _guard = perf_test_guard();
    let reports = run_native_rows().expect("run native protocol rows");
    let labels: Vec<_> = reports.iter().map(|report| report.label).collect();
    assert_eq!(
        labels,
        vec![
            "http2_h2c_close_request",
            "http2_h2c_keepalive_sequential",
            "http2_h2c_steady_state_small",
            "http2_h2c_client_steady_state_post",
            "grpc_h2c_unary_close",
            "grpc_h2c_unary_warmed",
            "grpc_h2c_unary_pooled_concurrent",
            "grpc_h2c_server_streaming_steady_state",
            "websocket_open_close",
            "websocket_text_round_trip",
            "websocket_steady_state_small",
        ],
        "native protocol row labels are pinned so a row cannot silently disappear",
    );

    // Every native row declares its setup-vs-reuse class in `kind`, and
    // both a connection-setup row and a steady-state-reuse row exist for each
    // protocol so setup cost is never silently mixed with steady-state service.
    let kinds: Vec<_> = reports
        .iter()
        .map(|report| (report.label, report.kind))
        .collect();
    for (label, kind) in &kinds {
        assert!(
            matches!(
                *kind,
                "connection_setup" | "connection_setup_amortized" | "steady_state_reuse"
            ),
            "row {label} declares a known setup-vs-reuse kind, got {kind}",
        );
    }
    for protocol in ["http2_", "websocket_"] {
        assert!(
            kinds
                .iter()
                .any(|(label, kind)| label.starts_with(protocol) && *kind == "connection_setup"),
            "{protocol} has an explicit connection-setup row",
        );
        assert!(
            kinds
                .iter()
                .any(|(label, kind)| label.starts_with(protocol) && *kind == "steady_state_reuse"),
            "{protocol} has an explicit steady-state-reuse row",
        );
    }

    // Print every row's evidence first, before any bounds assertion. A later
    // assertion that fails (e.g. the macOS-only `leak=unchecked` on the
    // connection-setup row) must not hide the p50/p90/p99 + allocation line of
    // the rows after it, so the gRPC latency rows always reach stdout.
    for report in &reports {
        println!("{}", report.summary_line());
        println!("{}", report.json_line());
    }

    for report in &reports {
        let line = report.summary_line();
        let json = report.json_line();

        assert!(
            line.starts_with("perf "),
            "native row uses the stable perf line shape: {}",
            report.label,
        );
        assert!(
            line.contains("comparison_baseline=none"),
            "native row is Tina-only (no fake baseline): {}",
            report.label,
        );
        assert_eq!(
            report.samples, 5,
            "native row reports median-of-five samples for {}",
            report.label,
        );
        assert!(
            line.contains("samples=5"),
            "native row line carries sample count for {}",
            report.label,
        );
        assert!(
            line.contains("sample_policy=median_p50_after_warmup"),
            "native row line carries sample policy for {}",
            report.label,
        );
        assert!(
            report.load.ops_ok > 0,
            "native row must do useful work: {}",
            report.label,
        );
        assert_eq!(
            report.load.ops_err, 0,
            "native row should not shed work: {}",
            report.label,
        );
        assert_eq!(
            report.load.ops_timeout, 0,
            "native row should not time out: {}",
            report.label,
        );
        assert!(
            report.load.leak_clean,
            "native row must end clean: {}",
            report.label,
        );
        assert!(
            report.allocations.is_some(),
            "native row must carry allocation evidence: {}",
            report.label,
        );
        // p50/p90/p99 are present and ordered.
        assert!(
            report.load.latency_p99_ns >= report.load.latency_p50_ns,
            "p99 >= p50 for {}",
            report.label,
        );
        assert!(
            report.load.latency_p90_ns >= report.load.latency_p50_ns,
            "p90 >= p50 for {}",
            report.label,
        );
        for field in ["p50_us\":", "p90_us\":", "p99_us\":"] {
            assert!(
                json.contains(field),
                "native row json carries {field} for {}",
                report.label,
            );
        }
        assert!(
            json.contains("\"schema\":\"tina.perf_report.v2\""),
            "native row json schema: {}",
            report.label,
        );
        assert!(
            json.contains("\"samples\":5"),
            "native row json carries sample count: {}",
            report.label,
        );
        assert!(
            json.contains("\"sample_policy\":\"median_p50_after_warmup\""),
            "native row json carries sample policy: {}",
            report.label,
        );
    }
}

#[test]
fn websocket_capacity_fill_probe_reports_typed_pressure() {
    let _guard = perf_test_guard();
    let report = websocket_capacity_fill_probe().expect("run websocket capacity fill probe");
    println!("{}", report.summary_line());

    assert_eq!(report.label, "tina_websocket_capacity_fill");
    assert_eq!(report.ops_attempted, 8);
    assert_eq!(report.ops_ok, 0, "every overfill op must hit pressure");
    assert_eq!(report.ops_err, 8);
    assert_eq!(report.ops_timeout, 0);
    assert_eq!(
        report.pressure.by_kind,
        vec![("full".to_string(), 8)],
        "over-cap outbound stays typed as full pressure",
    );
    assert!(
        report.leak_clean,
        "every session raised one typed SessionPressure and closed clean",
    );
}

#[test]
fn http_body_pressure_probe_reports_full_and_drained() {
    let _guard = perf_test_guard();
    let report = http_body_pressure_probe().expect("run HTTP body pressure probe");
    println!("{}", report.summary_line());
    for surface in &report.surface_plateaus {
        println!("{}", surface.summary_line());
    }

    assert_eq!(report.label, "tina_http_body_pressure");
    assert_eq!(report.ops_attempted, 8);
    assert_eq!(report.ops_ok, 0);
    assert_eq!(report.ops_err, 8);
    assert_eq!(report.ops_timeout, 0);
    assert_eq!(
        report.pressure.by_kind,
        vec![("full".to_string(), 8)],
        "expected max-body overload to stay typed as full",
    );
    assert!(report.leak_clean, "body pressure should drain cleanly");

    let max_body = report
        .surface_plateaus
        .iter()
        .find(|surface| surface.name == "perf.http.max_body_bytes")
        .expect("max body capacity surface");
    assert_eq!(max_body.full, 8);
    assert_eq!(max_body.final_current, 0);
    assert!(max_body.leak_clean);
}
