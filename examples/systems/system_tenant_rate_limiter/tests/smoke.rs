use std::cell::RefCell;
use std::convert::Infallible;
use std::rc::Rc;
use std::time::Duration;

use system_tenant_rate_limiter::{
    Gateway, GatewayReply, GatewayRequest, RunConfig, RunConfigError, RunError, WorkloadError, run,
};
use tina::prelude::*;
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, LocalSystem, RateLimit, RequestServiceHandle,
    RunToShutdownError, call_request,
};
use tina_sim::{Simulator, SimulatorConfig};

#[test]
fn hot_tenant_is_limited_while_cold_tenant_progresses() {
    let config = RunConfig::default();
    let report = run(config).expect("run");

    assert!(report.hot_admitted >= config.burst as usize);
    assert_eq!(
        report.hot_admitted + report.hot_limited,
        config.hot_requests
    );
    assert!(report.hot_limited > 0, "hot tenant never reached its limit");
    assert_eq!(report.cold_admitted, config.cold_requests);
    assert_eq!(report.cold_limited, 0);
    assert_eq!(
        report.snapshot.rate_limited_count,
        report.hot_limited as u64
    );
    assert_eq!(report.snapshot.full_count, 0);
    assert_eq!(report.snapshot.live_tenants, 2);
    assert_eq!(
        report.snapshot.grants_settled,
        report.snapshot.grants_admitted
    );
    assert_eq!(
        report.snapshot.grants_admitted,
        (report.hot_admitted + report.cold_admitted) as u64
    );
    assert!(
        report
            .snapshot
            .discovery_line
            .contains("surface=tenant.rate")
    );
}

#[test]
fn invalid_configs_are_typed_and_do_not_start_a_runtime() {
    let cases = [
        (
            RunConfig {
                mailbox: 0,
                ..RunConfig::default()
            },
            "mailbox",
        ),
        (
            RunConfig {
                max_tenants: 0,
                ..RunConfig::default()
            },
            "max_tenants",
        ),
        (
            RunConfig {
                rate_per_sec: 0,
                ..RunConfig::default()
            },
            "rate_per_sec",
        ),
        (
            RunConfig {
                burst: 0,
                ..RunConfig::default()
            },
            "burst",
        ),
        (
            RunConfig {
                hot_requests: 0,
                ..RunConfig::default()
            },
            "hot_requests",
        ),
        (
            RunConfig {
                cold_requests: 0,
                ..RunConfig::default()
            },
            "cold_requests",
        ),
        (
            RunConfig {
                call_timeout_ms: 0,
                ..RunConfig::default()
            },
            "call_timeout_ms",
        ),
    ];
    for (config, field) in cases {
        assert!(matches!(
            run(config),
            Err(RunError::InvalidConfig(RunConfigError::Zero { field: actual }))
                if actual == field
        ));
    }

    let oversized = [
        (
            RunConfig {
                mailbox: 65_537,
                ..RunConfig::default()
            },
            "mailbox",
        ),
        (
            RunConfig {
                max_tenants: 65_537,
                ..RunConfig::default()
            },
            "max_tenants",
        ),
        (
            RunConfig {
                rate_per_sec: 1_000_000_001,
                ..RunConfig::default()
            },
            "rate_per_sec",
        ),
        (
            RunConfig {
                burst: 1_000_001,
                ..RunConfig::default()
            },
            "burst",
        ),
        (
            RunConfig {
                hot_requests: 2_000_001,
                ..RunConfig::default()
            },
            "hot_requests",
        ),
        (
            RunConfig {
                cold_requests: 2_000_001,
                ..RunConfig::default()
            },
            "cold_requests",
        ),
        (
            RunConfig {
                call_timeout_ms: 60_001,
                ..RunConfig::default()
            },
            "call_timeout_ms",
        ),
    ];
    for (config, field) in oversized {
        assert!(matches!(
            run(config),
            Err(RunError::InvalidConfig(RunConfigError::TooLarge { field: actual, .. }))
                if actual == field
        ));
    }
    assert!(matches!(
        run(RunConfig {
            hot_requests: 1_500_000,
            cold_requests: 1_500_000,
            ..RunConfig::default()
        }),
        Err(RunError::InvalidConfig(RunConfigError::TotalRequests {
            hot: 1_500_000,
            cold: 1_500_000,
        }))
    ));
}

#[test]
fn live_owner_maps_hot_cold_table_full_closed_and_refill() {
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory)
        .try_build()
        .expect("runtime");
    app.run_to_shutdown_reported(Duration::from_secs(5), |app| -> anyhow::Result<()> {
        let gateway = app.register_request_service::<Gateway, GatewayRequest, Infallible>(
            Gateway::new(RateLimit::new("test.live", 2, 20, 1)),
            16,
        )?;
        let timeout = Duration::from_secs(1);

        assert!(matches!(
            live_call(
                app,
                gateway,
                GatewayRequest::Admit { tenant: "hot" },
                timeout
            )?,
            GatewayReply::Admitted { tenant: "hot" }
        ));
        assert!(matches!(
            live_call(
                app,
                gateway,
                GatewayRequest::Admit { tenant: "hot" },
                timeout
            )?,
            GatewayReply::RateLimited { tenant: "hot", .. }
        ));
        assert!(matches!(
            live_call(
                app,
                gateway,
                GatewayRequest::Admit { tenant: "cold" },
                timeout
            )?,
            GatewayReply::Admitted { tenant: "cold" }
        ));
        assert!(matches!(
            live_call(
                app,
                gateway,
                GatewayRequest::Admit { tenant: "third" },
                timeout
            )?,
            GatewayReply::TableFull { tenant: "third" }
        ));

        std::thread::sleep(Duration::from_millis(60));
        assert!(matches!(
            live_call(
                app,
                gateway,
                GatewayRequest::Admit { tenant: "hot" },
                timeout
            )?,
            GatewayReply::Admitted { tenant: "hot" }
        ));
        assert!(matches!(
            live_call(
                app,
                gateway,
                GatewayRequest::CloseAndProbe { tenant: "hot" },
                timeout
            )?,
            GatewayReply::Closed { tenant: "hot" }
        ));
        let snapshot = live_call(app, gateway, GatewayRequest::Snapshot, timeout)?;
        let GatewayReply::Snapshot(snapshot) = snapshot else {
            panic!("expected snapshot, got {snapshot:?}");
        };
        assert_eq!(snapshot.grants_admitted, 3);
        assert_eq!(snapshot.grants_settled, 3);
        assert_eq!(snapshot.rate_limited_count, 1);
        assert_eq!(snapshot.full_count, 1);
        Ok(())
    })
    .expect("clean workload and shutdown");
}

fn live_call(
    app: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
    gateway: RequestServiceHandle<GatewayRequest, GatewayReply>,
    request: GatewayRequest,
    timeout: Duration,
) -> anyhow::Result<GatewayReply> {
    match app.call_blocking_request(gateway, request, timeout)? {
        CallOutcome::Replied(reply) => Ok(reply),
        outcome => anyhow::bail!("unexpected live outcome: {outcome:?}"),
    }
}

#[test]
fn run_retains_exact_table_full_reply_inside_typed_terminal_error() {
    let error = run(RunConfig {
        max_tenants: 1,
        hot_requests: 1,
        cold_requests: 1,
        ..RunConfig::default()
    })
    .expect_err("cold tenant must find the one-slot table full");

    let RunError::Terminal(error) = error else {
        panic!("expected terminal workload error, got {error:?}");
    };
    match error.as_ref() {
        RunToShutdownError::Workload(report) => match report.get_ref() {
            WorkloadError::UnexpectedOutcome {
                phase: "cold",
                index: 0,
                outcome:
                    CallOutcome::Replied(GatewayReply::TableFull {
                        tenant: "tenant.cold",
                    }),
            } => {}
            other => panic!("wrong typed workload error: {other:?}"),
        },
        other => panic!("wrong terminal error: {other:?}"),
    }
}

#[derive(Debug)]
enum ProbeMessage {
    Call(GatewayRequest),
    Returned(CallOutcome<GatewayReply>),
}

struct Probe {
    gateway: RequestServiceHandle<GatewayRequest, GatewayReply>,
    replies: Rc<RefCell<Vec<GatewayReply>>>,
}

#[tina_runtime::isolate(message = ProbeMessage)]
impl Probe {
    fn handle(
        &mut self,
        message: ProbeMessage,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match message {
            ProbeMessage::Call(request) => {
                call_request(self.gateway, request, Duration::from_secs(1))
                    .then(ProbeMessage::Returned)
            }
            ProbeMessage::Returned(CallOutcome::Replied(reply)) => {
                self.replies.borrow_mut().push(reply);
                noop()
            }
            ProbeMessage::Returned(outcome) => panic!("sim request failed: {outcome:?}"),
        }
    }
}

fn sim_script(seed: u64) -> Vec<GatewayReply> {
    let replies = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(
        SingleShard,
        SimulatorConfig {
            seed,
            ..SimulatorConfig::default()
        },
    );
    let gateway =
        sim.register_request_service(Gateway::new(RateLimit::new("test.sim", 2, 10, 1)), 16);
    let probe = sim.register_with_mailbox_capacity::<Probe, ProbeMessage, Infallible>(
        Probe {
            gateway,
            replies: Rc::clone(&replies),
        },
        16,
    );

    for request in [
        GatewayRequest::Admit { tenant: "hot" },
        GatewayRequest::Admit { tenant: "hot" },
        GatewayRequest::Admit { tenant: "cold" },
        GatewayRequest::Admit { tenant: "third" },
    ] {
        sim.try_send(probe, ProbeMessage::Call(request)).unwrap();
        sim.run_until_quiescent();
    }
    sim.advance_time(Duration::from_millis(100));
    sim.try_send(
        probe,
        ProbeMessage::Call(GatewayRequest::Admit { tenant: "hot" }),
    )
    .unwrap();
    sim.run_until_quiescent();
    sim.try_send(
        probe,
        ProbeMessage::Call(GatewayRequest::CloseAndProbe { tenant: "hot" }),
    )
    .unwrap();
    sim.run_until_quiescent();

    replies.borrow().clone()
}

#[test]
fn request_service_replays_deterministically_under_simulator_owned_time() {
    let expected = vec![
        GatewayReply::Admitted { tenant: "hot" },
        GatewayReply::RateLimited {
            tenant: "hot",
            retry_after: Duration::from_millis(100),
        },
        GatewayReply::Admitted { tenant: "cold" },
        GatewayReply::TableFull { tenant: "third" },
        GatewayReply::Admitted { tenant: "hot" },
        GatewayReply::Closed { tenant: "hot" },
    ];
    assert_eq!(sim_script(7), expected);
    assert_eq!(sim_script(7), expected);
    assert_eq!(sim_script(999), expected);
}
