use std::time::{Duration, Instant};

use tina_runtime::{ShedRateLimit, ShedRateLimitDecision};

fn kind<T>(decision: ShedRateLimitDecision<T>) -> String {
    match decision {
        ShedRateLimitDecision::Admitted(grant) => {
            drop(grant);
            "admitted".to_owned()
        }
        ShedRateLimitDecision::RateLimited { retry_after, .. } => {
            format!("rate_limited:{}", retry_after.as_nanos())
        }
        ShedRateLimitDecision::TableFull(_) => "table_full".to_owned(),
        ShedRateLimitDecision::Closed(_) => "closed".to_owned(),
    }
}

#[test]
fn admit_drop_rate_limit_and_refill_preserve_exact_truth() {
    let now = Instant::now();
    let mut limit = ShedRateLimit::new("shed.refill", 2, 10, 1);

    let grant = match limit.try_admit(&"alpha", now) {
        ShedRateLimitDecision::Admitted(grant) => grant,
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
        ShedRateLimitDecision::RateLimited {
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
        ShedRateLimitDecision::Admitted(_)
    ));
}

#[test]
fn table_full_is_distinct_and_admin_eviction_refills_capacity() {
    let now = Instant::now();
    let mut limit = ShedRateLimit::new("shed.table", 1, 1, 1);
    assert!(matches!(
        limit.try_admit(&"alpha", now),
        ShedRateLimitDecision::Admitted(_)
    ));

    match limit.try_admit(&"beta", now) {
        ShedRateLimitDecision::TableFull(report) => {
            assert_eq!(report.current, 1);
            assert_eq!(report.capacity, 1);
            assert_eq!(report.full_count, 1);
            assert_eq!(report.rate_limited_count, 0);
        }
        other => panic!("new key at table capacity must be table-full, got {other:?}"),
    }

    assert!(limit.evict_key_for_capacity(&"alpha"));
    assert!(matches!(
        limit.try_admit(&"beta", now),
        ShedRateLimitDecision::Admitted(_)
    ));
    let report = limit.report();
    assert_eq!(report.current, 1);
    assert_eq!(report.evicted_count, 1);
}

#[test]
fn explicit_close_is_terminal_and_does_not_spend_or_allocate() {
    let now = Instant::now();
    let mut limit = ShedRateLimit::new("shed.closed", 1, 10, 2);
    limit.close();
    assert!(limit.is_closed());

    match limit.try_admit(&"alpha", now) {
        ShedRateLimitDecision::Closed(report) => {
            assert_eq!(report.closed_count, 1);
            assert_eq!(report.current, 0);
            assert_eq!(report.rate_limited_count, 0);
            assert_eq!(report.full_count, 0);
        }
        other => panic!("closed policy must preserve closed truth, got {other:?}"),
    }
    assert!(matches!(
        limit.try_admit(&"alpha", now),
        ShedRateLimitDecision::Closed(_)
    ));
    assert_eq!(limit.report().closed_count, 2);
}

fn replay(now: Instant) -> Vec<String> {
    let mut limit = ShedRateLimit::new("shed.replay", 2, 10, 1);
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
