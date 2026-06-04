use perf_native::{http_body_pressure_probe, run_all};
use tina_proof_harness::SemanticMatch;

#[test]
fn native_perf_comparison_rows_are_printable_and_bounded() {
    let reports = run_all().expect("run native perf comparisons");
    assert_eq!(reports.len(), 7);
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
fn http_body_pressure_probe_reports_full_and_drained() {
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
