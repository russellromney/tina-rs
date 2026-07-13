//! Public narrow `RateLimitDecision` contract and widening parity.

use std::time::{Duration, Instant};

use tina::capacity::CapacityMode;
use tina_runtime::{
    AdmissionDecision, AdmissionReport, RateLimit, RateLimitDecision, ServicePolicy,
};

#[derive(Debug, PartialEq, Eq)]
struct Observed {
    kind: &'static str,
    retry_after: Option<Duration>,
    report: Option<AdmissionReport>,
}

fn observe_narrow<T>(decision: RateLimitDecision<T>) -> Observed {
    match decision {
        RateLimitDecision::Admitted(grant) => {
            drop(grant);
            Observed {
                kind: "admitted",
                retry_after: None,
                report: None,
            }
        }
        RateLimitDecision::RateLimited {
            retry_after,
            report,
        } => Observed {
            kind: "rate_limited",
            retry_after: Some(retry_after),
            report: Some(report),
        },
        RateLimitDecision::TableFull(report) => Observed {
            kind: "table_full",
            retry_after: None,
            report: Some(report),
        },
        RateLimitDecision::Closed(report) => Observed {
            kind: "closed",
            retry_after: None,
            report: Some(report),
        },
    }
}

fn observe_wide<T: std::fmt::Debug>(decision: AdmissionDecision<T>) -> Observed {
    match decision {
        AdmissionDecision::Admitted(grant) => {
            drop(grant);
            Observed {
                kind: "admitted",
                retry_after: None,
                report: None,
            }
        }
        AdmissionDecision::RateLimited {
            retry_after,
            report,
        } => Observed {
            kind: "rate_limited",
            retry_after: Some(retry_after),
            report: Some(report),
        },
        AdmissionDecision::Full(report) => Observed {
            kind: "table_full",
            retry_after: None,
            report: Some(report),
        },
        AdmissionDecision::Closed(report) => Observed {
            kind: "closed",
            retry_after: None,
            report: Some(report),
        },
        other => panic!("RateLimit widening produced an impossible decision: {other:?}"),
    }
}

fn kind<T>(decision: RateLimitDecision<T>) -> String {
    match decision {
        RateLimitDecision::Admitted(grant) => {
            drop(grant);
            "admitted".to_owned()
        }
        RateLimitDecision::RateLimited { retry_after, .. } => {
            format!("rate_limited:{}", retry_after.as_nanos())
        }
        RateLimitDecision::TableFull(_) => "table_full".to_owned(),
        RateLimitDecision::Closed(_) => "closed".to_owned(),
    }
}

#[test]
fn admit_drop_rate_limit_and_refill_preserve_exact_truth() {
    let now = Instant::now();
    let mut limit = RateLimit::new("rate.refill", 2, 10, 1);

    let grant = match limit.try_admit(&"alpha", now) {
        RateLimitDecision::Admitted(grant) => grant,
        other => panic!("first token must admit, got {other:?}"),
    };
    assert_eq!(limit.report().current, 1);
    drop(grant);
    assert_eq!(
        limit.report().current,
        1,
        "a rate grant proves a consumed token; dropping it is not a capacity release"
    );

    match limit.try_admit(&"alpha", now) {
        RateLimitDecision::RateLimited {
            retry_after,
            report,
        } => {
            assert_eq!(retry_after, Duration::from_millis(100));
            assert_eq!(report.rate_limited_count, 1);
            assert_eq!(report.full_count, 0);
        }
        other => panic!("empty bucket must preserve rate-limit truth, got {other:?}"),
    }

    assert!(matches!(
        limit.try_admit(&"alpha", now + Duration::from_millis(100)),
        RateLimitDecision::Admitted(_)
    ));
}

#[test]
fn table_full_is_distinct_and_admin_eviction_refills_capacity() {
    let now = Instant::now();
    let mut limit = RateLimit::new("rate.table", 1, 1, 1);
    assert!(matches!(
        limit.try_admit(&"alpha", now),
        RateLimitDecision::Admitted(_)
    ));

    for expected_full_count in 1..=2 {
        match limit.try_admit(&"beta", now) {
            RateLimitDecision::TableFull(report) => {
                assert_eq!(report.current, 1);
                assert_eq!(report.capacity, 1);
                assert_eq!(report.full_count, expected_full_count);
                assert_eq!(report.rate_limited_count, 0);
            }
            other => panic!("new key at table capacity must be table-full, got {other:?}"),
        }
    }

    assert!(limit.evict_key_for_capacity(&"alpha"));
    assert!(matches!(
        limit.try_admit(&"beta", now),
        RateLimitDecision::Admitted(_)
    ));
    let report = limit.report();
    assert_eq!(report.current, 1);
    assert_eq!(report.evicted_count, 1);
}

#[test]
fn explicit_close_is_terminal_and_does_not_spend_or_allocate() {
    let now = Instant::now();
    let mut limit = RateLimit::new("rate.closed", 1, 10, 2);
    limit.close();
    assert!(limit.is_closed());

    match limit.try_admit(&"alpha", now) {
        RateLimitDecision::Closed(report) => {
            assert_eq!(report.closed_count, 1);
            assert_eq!(report.current, 0);
            assert_eq!(report.rate_limited_count, 0);
            assert_eq!(report.full_count, 0);
        }
        other => panic!("closed policy must preserve closed truth, got {other:?}"),
    }
    assert!(matches!(
        limit.try_admit(&"alpha", now),
        RateLimitDecision::Closed(_)
    ));
    assert_eq!(limit.report().closed_count, 2);
}

fn replay(now: Instant) -> Vec<String> {
    let mut limit = RateLimit::new("rate.replay", 2, 10, 1);
    [
        ("alpha", Duration::ZERO),
        ("alpha", Duration::ZERO),
        ("beta", Duration::ZERO),
        ("gamma", Duration::ZERO),
        ("alpha", Duration::from_millis(100)),
    ]
    .into_iter()
    .map(|(key, elapsed)| kind(limit.try_admit(&key, now + elapsed)))
    .collect()
}

#[test]
fn decisions_replay_for_the_same_logical_time_and_history() {
    let anchor = Instant::now();
    let expected = vec![
        "admitted",
        "rate_limited:100000000",
        "admitted",
        "table_full",
        "admitted",
    ];
    assert_eq!(replay(anchor), expected);
    assert_eq!(replay(anchor), replay(anchor));
}

#[test]
fn inherent_narrow_decision_matches_service_policy_widening() {
    let t0 = Instant::now();
    let mut narrow = RateLimit::new("rate.parity", 1, 10, 1);
    let mut wide = RateLimit::new("rate.parity", 1, 10, 1);

    for (key, offset) in [
        ("alpha", Duration::ZERO),
        ("alpha", Duration::ZERO),
        ("alpha", Duration::from_millis(25)),
        ("beta", Duration::from_millis(25)),
        ("beta", Duration::from_millis(25)),
        ("alpha", Duration::from_millis(10)),
        ("alpha", Duration::from_millis(100)),
    ] {
        assert_eq!(
            observe_narrow(narrow.try_admit(&key, t0 + offset)),
            observe_wide(ServicePolicy::decide(&mut wide, &key, t0 + offset)),
        );
        assert_eq!(narrow.report(), wide.report());
    }

    assert_eq!(
        narrow.evict_key_for_capacity(&"alpha"),
        wide.evict_key_for_capacity(&"alpha")
    );
    narrow.close();
    wide.close();
    assert_eq!(
        observe_narrow(narrow.try_admit(&"beta", t0)),
        observe_wide(ServicePolicy::decide(&mut wide, &"beta", t0)),
    );
    assert_eq!(narrow.report(), wide.report());
}

#[test]
fn backwards_clock_preserves_last_seen_and_exact_retry() {
    let t0 = Instant::now();
    let mut limit = RateLimit::new("rate.backward", 1, 10, 1);
    assert!(matches!(
        limit.try_admit(&"alpha", t0),
        RateLimitDecision::Admitted(_)
    ));
    let forward = t0 + Duration::from_millis(200);
    assert!(matches!(
        limit.try_admit(&"alpha", forward),
        RateLimitDecision::Admitted(_)
    ));
    match limit.try_admit(&"alpha", t0) {
        RateLimitDecision::RateLimited { retry_after, .. } => {
            assert_eq!(retry_after, Duration::from_millis(100));
        }
        other => panic!("backward clock must not refill, got {other:?}"),
    }
    assert_eq!(limit.key_state(&"alpha").unwrap().last_seen, forward);
    match limit.try_admit(&"alpha", forward + Duration::from_millis(50)) {
        RateLimitDecision::RateLimited { retry_after, .. } => {
            assert_eq!(retry_after, Duration::from_millis(50));
        }
        other => panic!("half refill must remain limited, got {other:?}"),
    }
}

#[test]
fn configuration_capacity_and_decision_reports_are_exact() {
    let now = Instant::now();
    let mut limit = RateLimit::<u32>::new("rate.config", 7, 13, 3).with_mode(CapacityMode::Tuning);
    assert_eq!(limit.max_keys(), 7);
    assert_eq!(limit.rate_per_sec(), 13);
    assert_eq!(limit.burst(), 3);
    assert_eq!(limit.live_keys(), 0);
    assert_eq!(limit.evicted_count(), 0);

    let report = limit.report();
    assert_eq!(report.surface, "rate.config");
    assert_eq!(report.mode, CapacityMode::Tuning);
    assert_eq!(report.capacity, 7);
    assert_eq!(limit.capacity_surface(), report.capacity_surface());

    let admitted = limit.try_admit(&1, now);
    assert!(admitted.report().is_none());
    drop(admitted);
    for _ in 1..3 {
        assert!(matches!(
            limit.try_admit(&1, now),
            RateLimitDecision::Admitted(_)
        ));
    }
    let limited = limit.try_admit(&1, now);
    assert_eq!(
        limited.report().map(|report| report.rate_limited_count),
        Some(1)
    );
}
