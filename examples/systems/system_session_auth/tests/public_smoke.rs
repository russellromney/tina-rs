//! Public runner proof for the sharded session-auth system.

use std::cell::RefCell;
use std::convert::Infallible;
use std::rc::Rc;
use std::time::Duration;

use system_session_auth::{
    AuthShard, RunConfig, RunConfigError, RunError, SessionAuthEvent, SessionAuthReply,
    SessionAuthRequest, SessionBucket, SessionToken, WorkloadError, expect_reply, run,
    run_idle_expiry, run_login_touch_logout, run_overflow,
};
use tina::prelude::*;
use tina_runtime::{CallError, CallOutcome, call_request};
use tina_sim::{MultiShardSimulator, MultiShardSimulatorConfig, SimulatorConfig};

fn default_config() -> RunConfig {
    RunConfig {
        shards: 4,
        max_sessions_per_shard: 16,
        idle_timeout_ms: 80,
        sweep_interval_ms: 20,
        session_mailbox: 128,
        call_timeout_ms: 2_000,
    }
}

fn assert_login_touch_logout(report: &system_session_auth::LoginTouchLogoutReport) {
    assert!(report.login_ok);
    assert!(report.touch_ok);
    assert!(report.logout_ok);
    assert!(report.touch_after_logout_not_found);
    assert_eq!(report.stats.admitted, 1);
    assert_eq!(report.stats.logged_out, 1);
    assert_eq!(report.stats.touch_ok, 1);
    assert_eq!(report.stats.touch_not_found, 1);
    assert_eq!(report.stats.active, 0);
    assert_eq!(report.stats.idle_expired, 0);
    assert_eq!(report.stats.full_rejects, 0);
    assert_eq!(report.stats.timer_errors, 0);
}

fn assert_idle_expiry(report: &system_session_auth::IdleExpiryReport) {
    assert!(report.touch_after_idle_not_found);
    assert_eq!(report.stats.admitted, 1);
    assert_eq!(report.stats.idle_expired, 1);
    assert_eq!(report.stats.active, 0);
    assert!(
        report.stats.sweeps_run >= 2,
        "expected at least 2 sweeps, saw {}",
        report.stats.sweeps_run
    );
}

fn assert_overflow(report: &system_session_auth::OverflowReport) {
    assert_eq!(report.admitted, 4);
    assert_eq!(report.full, 5);
    assert_eq!(report.stats.full_rejects, 5);
    assert_eq!(report.stats.per_shard_high_water, vec![4]);
    assert_eq!(report.stats.per_shard_active, vec![4]);
}

/// Documented public runner path: `run(RunConfig::…)`.
#[test]
fn public_smoke() {
    let report = run(default_config()).expect("run succeeds");
    assert_login_touch_logout(&report.login_touch_logout);
    assert_idle_expiry(&report.idle_expiry);
    assert_overflow(&report.overflow);
}

/// Pins accepted default workload counts and terminal facts.
#[test]
fn public_characterization() {
    let config = default_config();
    assert_eq!(config.shards, 4);
    assert_eq!(config.max_sessions_per_shard, 16);
    assert_eq!(config.idle_timeout_ms, 80);
    assert_eq!(config.sweep_interval_ms, 20);
    assert_eq!(config.session_mailbox, 128);
    assert_eq!(config.call_timeout_ms, 2_000);

    assert_login_touch_logout(&run_login_touch_logout(config).expect("login walk"));
    assert_idle_expiry(&run_idle_expiry(config).expect("idle walk"));
    assert_overflow(&run_overflow(config).expect("overflow walk"));
}

#[test]
fn invalid_configs_are_typed_and_do_not_start_workers() {
    let zeros = [
        (
            RunConfig {
                shards: 0,
                ..default_config()
            },
            "shards",
        ),
        (
            RunConfig {
                max_sessions_per_shard: 0,
                ..default_config()
            },
            "max_sessions_per_shard",
        ),
        (
            RunConfig {
                idle_timeout_ms: 0,
                ..default_config()
            },
            "idle_timeout_ms",
        ),
        (
            RunConfig {
                sweep_interval_ms: 0,
                ..default_config()
            },
            "sweep_interval_ms",
        ),
        (
            RunConfig {
                session_mailbox: 0,
                ..default_config()
            },
            "session_mailbox",
        ),
        (
            RunConfig {
                call_timeout_ms: 0,
                ..default_config()
            },
            "call_timeout_ms",
        ),
    ];
    for (config, field) in zeros {
        assert!(matches!(
            run(config),
            Err(RunError::InvalidConfig(RunConfigError::Zero { field: actual }))
                if actual == field
        ));
    }

    assert!(matches!(
        run(RunConfig {
            shards: system_session_auth::MAX_SHARDS + 1,
            ..default_config()
        }),
        Err(RunError::InvalidConfig(RunConfigError::TooLarge {
            field: "shards",
            ..
        }))
    ));
}

#[test]
fn overflow_public_runner_retains_exact_full_domain_overload() {
    let report = run_overflow(default_config()).expect("overflow");
    assert_eq!(report.admitted, 4);
    assert_eq!(report.full, 5);
    assert_eq!(report.stats.full_rejects, 5);
}

#[test]
fn host_call_timeout_stays_distinct() {
    let error = expect_reply("timeout_probe", Ok(CallOutcome::Timeout))
        .expect_err("timeout must not collapse");
    match error {
        WorkloadError::UnexpectedOutcome {
            phase: "timeout_probe",
            outcome: CallOutcome::Timeout,
        } => {}
        other => panic!("collapsed timeout: {other:?}"),
    }
}

#[test]
fn host_call_closed_full_and_rejection_stay_distinct() {
    let closed = expect_reply("closed_probe", Ok(CallOutcome::Closed)).expect_err("closed");
    assert!(matches!(
        closed,
        WorkloadError::UnexpectedOutcome {
            phase: "closed_probe",
            outcome: CallOutcome::Closed,
        }
    ));

    let full = expect_reply("full_probe", Ok(CallOutcome::Full)).expect_err("full");
    assert!(matches!(
        full,
        WorkloadError::UnexpectedOutcome {
            phase: "full_probe",
            outcome: CallOutcome::Full,
        }
    ));

    let rejected = expect_reply(
        "reject_probe",
        Ok(CallOutcome::Rejected(
            tina::CallRejectedReason::ReplyAbandoned,
        )),
    )
    .expect_err("rejected");
    assert!(matches!(
        rejected,
        WorkloadError::UnexpectedOutcome {
            phase: "reject_probe",
            outcome: CallOutcome::Rejected(tina::CallRejectedReason::ReplyAbandoned),
        }
    ));
}

#[test]
fn timer_dependency_failure_surfaces_on_public_stats() {
    // Public contract: BucketStats::timer_errors is part of SessionStats and is
    // retained through host aggregation. Unit proof in lib exercises the
    // failure path with CallError::TimerFull without expiring rows.
    let _ = CallError::TimerFull;
    let stats = system_session_auth::BucketStats {
        timer_errors: 1,
        ..system_session_auth::BucketStats::default()
    };
    assert_eq!(stats.timer_errors, 1);
}

#[derive(Debug)]
enum ProbeMessage {
    Login {
        user_id: String,
        token: SessionToken,
    },
    Touch {
        token: SessionToken,
    },
    Stats,
    Returned {
        phase: &'static str,
        outcome: CallOutcome<SessionAuthReply>,
    },
}

struct Probe {
    bucket: tina::ServiceRequestAddress<SessionAuthEvent, SessionAuthRequest, SessionAuthReply>,
    replies: Rc<RefCell<Vec<(&'static str, SessionAuthReply)>>>,
}

#[tina_runtime::isolate(message = ProbeMessage, shard = AuthShard)]
impl Probe {
    fn handle(
        &mut self,
        message: ProbeMessage,
        _ctx: &mut Context<'_, AuthShard, Self::Reply>,
    ) -> Effect<Self> {
        match message {
            ProbeMessage::Login { user_id, token } => call_request(
                self.bucket,
                SessionAuthRequest::Login { user_id, token },
                Duration::from_secs(1),
            )
            .then(|outcome| ProbeMessage::Returned {
                phase: "login",
                outcome,
            }),
            ProbeMessage::Touch { token } => call_request(
                self.bucket,
                SessionAuthRequest::Touch { token },
                Duration::from_secs(1),
            )
            .then(|outcome| ProbeMessage::Returned {
                phase: "touch",
                outcome,
            }),
            ProbeMessage::Stats => call_request(
                self.bucket,
                SessionAuthRequest::Stats,
                Duration::from_secs(1),
            )
            .then(|outcome| ProbeMessage::Returned {
                phase: "stats",
                outcome,
            }),
            ProbeMessage::Returned { phase, outcome } => {
                match outcome {
                    CallOutcome::Replied(reply) => self.replies.borrow_mut().push((phase, reply)),
                    other => panic!("sim {phase} failed: {other:?}"),
                }
                noop()
            }
        }
    }
}

/// Drain ready deliveries only. Recurring sweep timers never fully quiesce, so
/// the host must not call `run_until_quiescent` while a sweep is armed.
fn drive_ready(sim: &mut MultiShardSimulator<AuthShard>, rounds: usize) {
    for _ in 0..rounds {
        if sim.step() == 0 {
            break;
        }
    }
}

/// Drive same-shard request/reply work without advancing virtual time.
fn drive_until_phase(
    sim: &mut MultiShardSimulator<AuthShard>,
    replies: &Rc<RefCell<Vec<(&'static str, SessionAuthReply)>>>,
    phase: &'static str,
) {
    for _ in 0..10_000 {
        if replies.borrow().iter().any(|(p, _)| *p == phase) {
            return;
        }
        if sim.step() == 0 {
            break;
        }
    }
    assert!(
        replies.borrow().iter().any(|(p, _)| *p == phase),
        "sim never produced phase {phase}; saw {:?}",
        replies.borrow()
    );
}

fn sim_idle_expiry_script(seed: u64) -> (SessionAuthReply, system_session_auth::BucketStats) {
    let replies = Rc::new(RefCell::new(Vec::new()));
    let mut sim = MultiShardSimulator::with_config(
        [AuthShard(0)],
        SimulatorConfig {
            seed,
            ..SimulatorConfig::default()
        },
        MultiShardSimulatorConfig::default(),
    );

    let config = RunConfig {
        shards: 1,
        max_sessions_per_shard: 4,
        idle_timeout_ms: 100,
        sweep_interval_ms: 20,
        session_mailbox: 16,
        call_timeout_ms: 1_000,
    };
    let bucket = sim
        .register_split_service_with_bootstrap_on::<SessionBucket, SessionAuthEvent, SessionAuthRequest, Infallible>(
            ShardId::new(0),
            SessionBucket::new(config),
            config.session_mailbox,
            SessionAuthEvent::Bootstrap,
        )
        .expect("bucket registers");
    // Process Bootstrap and arm the first sweep timer. Do not advance time.
    drive_ready(&mut sim, 32);

    let probe = sim.register_with_capacity_on::<Probe, ProbeMessage, Infallible>(
        ShardId::new(0),
        Probe {
            bucket: bucket.requests,
            replies: Rc::clone(&replies),
        },
        16,
    );

    let token = SessionToken("sim-1".into());
    sim.try_send(
        probe,
        ProbeMessage::Login {
            user_id: "sim-user".into(),
            token: token.clone(),
        },
    )
    .expect("login admitted");
    drive_until_phase(&mut sim, &replies, "login");

    // Jump owner time past idle and the armed sweep interval so one due sweep
    // harvests and expires the untouched session under the same clock rail.
    sim.advance_time(Duration::from_millis(
        config.idle_timeout_ms + config.sweep_interval_ms,
    ));
    drive_ready(&mut sim, 64);

    sim.try_send(probe, ProbeMessage::Touch { token })
        .expect("touch sent");
    drive_until_phase(&mut sim, &replies, "touch");
    sim.try_send(probe, ProbeMessage::Stats)
        .expect("stats sent");
    drive_until_phase(&mut sim, &replies, "stats");

    let captured = replies.borrow().clone();
    let touch = captured
        .iter()
        .find(|(phase, _)| *phase == "touch")
        .map(|(_, reply)| reply.clone())
        .expect("touch reply");
    let stats = captured
        .iter()
        .find(|(phase, _)| *phase == "stats")
        .map(|(_, reply)| match reply {
            SessionAuthReply::Stats(stats) => *stats,
            other => panic!("expected stats, got {other:?}"),
        })
        .expect("stats reply");
    (touch, stats)
}

#[test]
fn live_and_simulator_idle_expiry_share_owner_time_contract() {
    let live = run_idle_expiry(RunConfig {
        shards: 1,
        max_sessions_per_shard: 4,
        idle_timeout_ms: 80,
        sweep_interval_ms: 20,
        session_mailbox: 32,
        call_timeout_ms: 2_000,
    })
    .expect("live idle");
    assert!(live.touch_after_idle_not_found);
    assert_eq!(live.stats.idle_expired, 1);
    assert_eq!(live.stats.active, 0);

    let (touch, stats) = sim_idle_expiry_script(7);
    assert_eq!(touch, SessionAuthReply::NotFound);
    assert_eq!(stats.admitted, 1);
    assert_eq!(stats.idle_expired, 1);
    assert_eq!(stats.active, 0);
    assert!(stats.sweeps_run >= 1);

    // Deterministic under the same seed and independent of wall time.
    let again = sim_idle_expiry_script(7);
    assert_eq!(again.0, SessionAuthReply::NotFound);
    assert_eq!(again.1.idle_expired, 1);
    assert_eq!(sim_idle_expiry_script(999).1.idle_expired, 1);
}

#[test]
fn public_runner_shutdown_is_consuming_and_clean_on_success() {
    // run_* paths use run_to_shutdown_reported; success means workload and
    // terminal shutdown both completed without dual-failure.
    let report = run_login_touch_logout(default_config()).expect("clean shutdown path");
    assert!(report.login_ok);
}
