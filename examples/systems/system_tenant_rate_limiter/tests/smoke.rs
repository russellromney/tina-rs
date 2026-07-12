use system_tenant_rate_limiter::{RunConfig, run};

#[test]
fn hot_tenant_is_limited_while_cold_tenant_progresses() {
    let config = RunConfig::default();
    let report = run(config).expect("run");

    // Hot tenant: burst (3) admissions, the rest Limited.
    assert_eq!(
        report.hot_admitted, config.burst as usize,
        "hot must admit exactly `burst` requests at t=0, got {report:?}"
    );
    assert_eq!(
        report.hot_limited,
        config.hot_requests - config.burst as usize,
        "hot must rate-limit every request past burst, got {report:?}"
    );

    // Cold tenant: every request admitted (cold_requests <= burst).
    assert_eq!(
        report.cold_admitted, config.cold_requests,
        "cold must admit every request, got {report:?}"
    );
    assert_eq!(
        report.cold_limited, 0,
        "cold must never be rate-limited under hot pressure, got {report:?}"
    );

    // Snapshot counts must agree with observed outcomes.
    assert_eq!(
        report.snapshot.rate_limited_count, report.hot_limited as u64,
        "snapshot rate_limited_count must match observed Limited replies, got {report:?}"
    );
    assert_eq!(
        report.snapshot.full_count, 0,
        "table cap is not exhausted in this run, got {report:?}"
    );
    // Both tenants left state behind at the end of the run.
    assert_eq!(report.snapshot.live_tenants, 2);

    // Discovery line is grep-friendly.
    assert!(
        report
            .snapshot
            .discovery_line
            .contains("surface=tenant.rate"),
        "missing surface name: {}",
        report.snapshot.discovery_line
    );
}

#[test]
fn retry_after_is_deterministic_across_runs() {
    // Same config, two runs: hot_retry_afters_ms must be byte-identical.
    let config = RunConfig::default();
    let a = run(config).expect("run a");
    let b = run(config).expect("run b");
    assert_eq!(
        a.hot_retry_afters_ms, b.hot_retry_afters_ms,
        "retry_after sequence must be deterministic: a={:?} b={:?}",
        a.hot_retry_afters_ms, b.hot_retry_afters_ms
    );
    // And it must look like the expected rate (10 tokens/sec → 100ms gap
    // for the very next retry; subsequent rejections at the same `now`
    // also report 100ms because no time has elapsed).
    assert!(
        a.hot_retry_afters_ms.iter().all(|ms| *ms == 100),
        "all retry_after values should be 100ms at t=0, got {:?}",
        a.hot_retry_afters_ms
    );
}

#[test]
fn host_call_failure_returns_after_bounded_shutdown() {
    let error = run(RunConfig {
        mailbox: 0,
        ..RunConfig::default()
    })
    .expect_err("zero-capacity mailbox must refuse host calls");

    assert!(
        format!("{error:#}").contains("hot outcome: Full"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn key_table_full_returns_typed_tenant_table_full() {
    // Drive enough distinct tenants that the table fills, prove the typed
    // outcome. `max_tenants=2` means the third distinct tenant is
    // rejected with `TenantTableFull`.
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use tina::prelude::*;
    use tina_runtime::{CallOutcome, DefaultThreadedMailboxFactory, LocalSystem};

    let config = RunConfig {
        max_tenants: 2,
        hot_requests: 0,
        cold_requests: 0,
        ..RunConfig::default()
    };

    // Spin up the limiter ourselves so we can drive three distinct
    // tenants without changing the public `run` shape.
    let runtime = Arc::new(
        LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory)
            .try_build()
            .expect("start runtime"),
    );
    let shutdown = runtime.shutdown_handle();
    let rate = tina_runtime::RateLimit::<&'static str>::new(
        "tenant.rate",
        config.max_tenants,
        config.rate_per_sec,
        config.burst,
    );
    use system_tenant_rate_limiter::{Gateway, GatewayMsg, GatewayReply};
    let gateway = runtime
        .register_root::<_, std::convert::Infallible>(Gateway::new(rate), config.mailbox)
        .expect("register");

    let base = Instant::now();
    let timeout = Duration::from_millis(config.call_timeout_ms);
    let mut outcomes: Vec<GatewayReply> = Vec::new();
    for tenant in ["t.one", "t.two", "t.three"] {
        let outcome = runtime
            .call_blocking(gateway, GatewayMsg::Request { tenant, now: base }, timeout)
            .expect("call");
        match outcome {
            CallOutcome::Replied(reply) => outcomes.push(reply),
            other => panic!("unexpected outcome: {other:?}"),
        }
    }
    assert!(matches!(outcomes[0], GatewayReply::Ok { .. }));
    assert!(matches!(outcomes[1], GatewayReply::Ok { .. }));
    match &outcomes[2] {
        GatewayReply::TenantTableFull { tenant } => {
            assert_eq!(*tenant, "t.three");
        }
        other => panic!("expected TenantTableFull, got {other:?}"),
    }

    let terminal = shutdown
        .request_and_wait_report(Duration::from_secs(5))
        .expect("request and await shutdown");
    drop(runtime);
    terminal.ensure_clean().expect("clean terminal report");
}
