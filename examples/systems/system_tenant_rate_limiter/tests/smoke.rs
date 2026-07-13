use system_tenant_rate_limiter::{RunConfig, run};

#[test]
fn hot_tenant_is_limited_while_cold_tenant_progresses() {
    let config = RunConfig {
        rate_per_sec: 1,
        ..RunConfig::default()
    };
    let report = run(config).expect("run");

    // A live runtime owns wall-clock scheduling, so later requests may see
    // refill credit. Assert accounting and visible pressure, not an exact
    // admitted/limited split.
    assert_eq!(
        report.hot_admitted + report.hot_limited,
        config.hot_requests,
        "every hot request must receive an admitted or limited reply, got {report:?}"
    );
    assert!(
        report.hot_admitted >= config.burst as usize,
        "the initial burst must admit, got {report:?}"
    );
    assert!(
        report.hot_limited > 0,
        "the tight live burst must expose rate pressure, got {report:?}"
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
fn retry_after_uses_owner_time_and_stays_within_the_token_window() {
    let config = RunConfig {
        rate_per_sec: 1,
        ..RunConfig::default()
    };
    let report = run(config).expect("run");
    assert_eq!(report.hot_retry_afters_ms.len(), report.hot_limited);
    assert!(
        report
            .hot_retry_afters_ms
            .iter()
            .all(|ms| *ms > 0 && *ms <= 1_000),
        "retry_after must stay within the one-second token window: {:?}",
        report.hot_retry_afters_ms
    );
}

#[test]
fn key_capacity_full_returns_typed_tenant_capacity_full() {
    // Drive enough distinct tenants that the table fills, prove the typed
    // outcome. `max_tenants=2` means the third distinct tenant is
    // rejected with `TenantCapacityFull`.
    use std::sync::Arc;
    use std::time::Duration;

    use tina::prelude::*;
    use tina_runtime::{
        CallOutcome, DefaultThreadedMailboxFactory, ServiceHandle, ThreadedRuntime,
    };

    let config = RunConfig {
        max_tenants: 2,
        hot_requests: 0,
        cold_requests: 0,
        ..RunConfig::default()
    };

    // Spin up the limiter ourselves so we can drive three distinct
    // tenants without changing the public `run` shape.
    let runtime = Arc::new(
        ThreadedRuntime::try_new(SingleShard, DefaultThreadedMailboxFactory)
            .expect("start runtime"),
    );
    let shutdown = runtime.shutdown_handle();
    let rate = tina_runtime::RateLimit::<&'static str>::new(
        "tenant.rate",
        tina_runtime::RateLimitConfig {
            max_keys: config.max_tenants,
            rate_per_sec: config.rate_per_sec,
            burst: config.burst,
        },
    );
    use system_tenant_rate_limiter::{Gateway, GatewayMsg, GatewayReply};
    let gateway: ServiceHandle<GatewayMsg, GatewayReply> = runtime
        .register_service::<_, std::convert::Infallible>(Gateway::new(rate), config.mailbox)
        .expect("register");

    let timeout = Duration::from_millis(config.call_timeout_ms);
    let mut outcomes: Vec<GatewayReply> = Vec::new();
    for tenant in ["t.one", "t.two", "t.three"] {
        let outcome = runtime
            .call_blocking_typed(
                gateway.call,
                GatewayMsg::Request { tenant },
                timeout,
            )
            .expect("call");
        match outcome {
            CallOutcome::Replied(reply) => outcomes.push(reply),
            other => panic!("unexpected outcome: {other:?}"),
        }
    }
    assert!(matches!(outcomes[0], GatewayReply::Ok { .. }));
    assert!(matches!(outcomes[1], GatewayReply::Ok { .. }));
    match &outcomes[2] {
        GatewayReply::TenantCapacityFull { tenant } => {
            assert_eq!(*tenant, "t.three");
        }
        other => panic!("expected TenantCapacityFull, got {other:?}"),
    }

    let terminal = shutdown
        .request_and_wait_report(Duration::from_secs(5))
        .expect("request and await shutdown");
    drop(runtime);
    terminal.ensure_clean().expect("clean terminal report");
}
