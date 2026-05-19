//! Integration proofs for the Phase 118 admission and rate policy layer.
//!
//! These tests stay independent of the rest of the runtime — admission
//! policies are plain state. They prove three load-bearing claims:
//!
//! 1. **Replay determinism.** Same `(config, now, key)` sequence → same
//!    decision sequence across freshly built policy instances.
//! 2. **Cold key/tenant progress under hot pressure.** A rate-limited hot
//!    key cannot block a different cold key from succeeding.
//! 3. **Fill / cancel / refill.** Releasing or retiring a permit must allow
//!    a fresh admission with no stale-charge leak.
//! 4. **Capacity report integration.** Each policy projects onto
//!    `CapacitySurfaceReport` so existing `CapacitySummary` assertions
//!    keep working.
//! 5. **Retry budget integration.** Pairing `RateLimit` with `FullHandling`
//!    gives a fully caller-owned retry path; exhaustion is visible in the
//!    typed outcome.

use std::time::{Duration, Instant};

use tina::Deadline;
use tina::capacity::{CapacityMode, CapacitySurfaceReport};
use tina::time::Backoff;
use tina_runtime::{
    AdmissionDecision, AdmissionFailure, CapacitySummary, ConcurrencyLimit, FullDecision,
    FullExhaustionReason, FullHandling, KeyedLimit, PressureAction, RateLimit,
    format_discovery_line,
};

#[test]
fn rate_limit_replay_byte_identical_under_same_inputs() {
    // Two freshly constructed limiters must produce the same decision
    // sequence given the same caller-supplied `now` stream. This is the
    // "sim replay proves time-based policy determinism" proof: simulated
    // time and live `Instant` inputs flow through the same integer math.
    let base = Instant::now();
    let inputs: Vec<(&'static str, Duration)> = vec![
        ("tenant.alpha", Duration::ZERO),
        ("tenant.alpha", Duration::from_millis(1)),
        ("tenant.beta", Duration::ZERO),
        ("tenant.alpha", Duration::from_millis(50)),
        ("tenant.alpha", Duration::from_millis(120)),
        ("tenant.beta", Duration::from_millis(200)),
        ("tenant.alpha", Duration::from_millis(300)),
        ("tenant.gamma", Duration::from_millis(300)),
        ("tenant.alpha", Duration::from_millis(350)),
    ];

    let run = || -> Vec<String> {
        let mut limit = RateLimit::<&'static str>::new("rate.replay.proof", 4, 5, 2);
        inputs
            .iter()
            .map(
                |(key, offset)| match limit.try_admit(*key, base + *offset) {
                    AdmissionDecision::Admitted(_) => format!("{key}@{}ms=ok", offset.as_millis()),
                    AdmissionDecision::RateLimited { retry_after, .. } => format!(
                        "{key}@{}ms=rate({}ms)",
                        offset.as_millis(),
                        retry_after.as_millis()
                    ),
                    AdmissionDecision::Full(_) => format!("{key}@{}ms=full", offset.as_millis()),
                    other => format!("{key}@{}ms=other({other:?})", offset.as_millis()),
                },
            )
            .collect()
    };

    let first = run();
    let second = run();
    let third = run();
    assert_eq!(first, second, "replay #1 must match the original");
    assert_eq!(first, third, "replay #2 must match the original");
    // Sanity: hot tenant must hit rate-limit at least once in this trace.
    assert!(
        first.iter().any(|line| line.contains("=rate(")),
        "trace did not exercise the rate-limit path: {first:?}"
    );
}

#[test]
fn cold_tenant_progresses_while_hot_tenant_is_limited() {
    let mut limit = RateLimit::<&'static str>::new("rate.fairness", 4, 5, 2);
    let now = Instant::now();
    // Hot tenant exhausts the bucket.
    for _ in 0..2 {
        let _g = limit
            .try_admit("hot", now)
            .into_admitted()
            .expect("hot burst");
    }
    // Hot is rate-limited.
    match limit.try_admit("hot", now) {
        AdmissionDecision::RateLimited { .. } => {}
        other => panic!("hot must be rate-limited, got {other:?}"),
    }
    // Cold tenant still succeeds many times within its own bucket.
    for _ in 0..2 {
        let _g = limit
            .try_admit("cold", now)
            .into_admitted()
            .expect("cold success despite hot pressure");
    }
    // And cold's bucket can refill independently of hot.
    let later = now + Duration::from_secs(1);
    for _ in 0..2 {
        let _g = limit
            .try_admit("cold", later)
            .into_admitted()
            .expect("cold refilled");
    }
}

#[test]
fn concurrency_fill_cancel_refill_admits_new_work() {
    let mut limit = ConcurrencyLimit::with_capacity("conc.refill", 2);
    let p1 = limit
        .try_admit()
        .into_admitted()
        .expect("first admit succeeds");
    let p2 = limit
        .try_admit()
        .into_admitted()
        .expect("second admit succeeds");
    // Full.
    match limit.try_admit() {
        AdmissionDecision::Full(_) => {}
        other => panic!("expected Full, got {other:?}"),
    }
    // Cancel (retire) one — it must reclaim the slot.
    limit.retire(p1).expect("retire");
    let p3 = limit
        .try_admit()
        .into_admitted()
        .expect("refill after retire");
    // Release the rest.
    limit.release(p2).expect("release p2");
    limit.release(p3).expect("release p3");
    let report = limit.report();
    assert_eq!(report.current, 0, "all permits released");
    assert!(report.full_count >= 1, "full was observed");
}

#[test]
fn keyed_limit_fill_cancel_refill_per_key_and_table() {
    let mut limit = KeyedLimit::<&'static str>::new("keyed.refill", 2, 2);
    let p_a1 = limit.try_admit("alpha").into_admitted().unwrap();
    let p_a2 = limit.try_admit("alpha").into_admitted().unwrap();
    // alpha at cap.
    let alpha_full = limit.try_admit("alpha").into_admitted().unwrap_err();
    assert!(matches!(alpha_full, AdmissionFailure::Full(_)));
    // beta occupies the second slot.
    let p_b1 = limit.try_admit("beta").into_admitted().unwrap();
    // gamma cannot find a slot.
    let gamma_full = limit.try_admit("gamma").into_admitted().unwrap_err();
    assert!(matches!(gamma_full, AdmissionFailure::Full(_)));
    // Release every alpha; slot frees; gamma now admits.
    limit.release(p_a1).expect("release a1");
    limit.release(p_a2).expect("release a2");
    let p_g1 = limit.try_admit("gamma").into_admitted().unwrap();
    limit.release(p_b1).expect("release b1");
    limit.release(p_g1).expect("release g1");
    assert_eq!(limit.report().current, 0);
}

#[test]
fn admission_report_projects_into_capacity_summary() {
    let mut conc = ConcurrencyLimit::with_capacity("svc.conc", 2);
    let _p = conc.try_admit().into_admitted().unwrap();
    let _q = conc.try_admit().into_admitted().unwrap();
    let _ = conc.try_admit(); // force Full

    let mut rate = RateLimit::<&'static str>::new("svc.rate", 4, 5, 1);
    let now = Instant::now();
    let _ = rate.try_admit("alpha", now).into_admitted().unwrap();
    let _ = rate.try_admit("alpha", now); // force RateLimited

    let mut summary = CapacitySummary::new();
    summary.push(conc.capacity_surface()).unwrap();
    summary.push(rate.capacity_surface()).unwrap();

    let conc_surface = summary.surface("svc.conc").report().unwrap();
    assert_eq!(conc_surface.max_messages, Some(2));
    assert!(conc_surface.full_count >= 1);
    let line = format_discovery_line(conc_surface);
    assert!(
        line.contains("surface=svc.conc") && line.contains("cur=2"),
        "discovery line missing fields: {line}"
    );
    let rate_surface = summary.surface("svc.rate").report().unwrap();
    assert_eq!(rate_surface.max_messages, Some(4));
    // Rate-limit rejections roll into the aggregate full_count for
    // any_full() honesty.
    assert!(rate_surface.full_count >= 1);
}

#[test]
fn rate_limit_with_caller_owned_retry_budget_is_visible() {
    // Compose RateLimit with FullHandling for caller-owned retry on
    // rate-limit. Retry exhaustion must surface as a typed Exhausted
    // outcome from FullHandling, never a hidden swallow.
    let mut rate = RateLimit::<&'static str>::new("rate.retry", 2, 1, 1);
    let mut budget = FullHandling::retry_backoff(
        Backoff::constant(Duration::from_millis(50), 2).expect("backoff"),
    );

    let now = Instant::now();
    // First admit succeeds.
    let _grant = rate.try_admit("tenant.a", now).into_admitted().unwrap();
    // Second is rate-limited.
    let outcome = rate.try_admit("tenant.a", now);
    let failure = outcome.into_admitted().unwrap_err();
    assert!(matches!(failure, AdmissionFailure::RateLimited { .. }));

    // Caller chooses to consult the retry budget.
    let first_retry = budget.on_full(now, None::<Deadline>);
    match first_retry {
        FullDecision::RetryAfter { delay, .. } => {
            assert_eq!(delay, Duration::from_millis(50));
        }
        other => panic!("expected first RetryAfter, got {other:?}"),
    }
    let second_retry = budget.on_full(now, None::<Deadline>);
    match second_retry {
        FullDecision::RetryAfter { delay, .. } => {
            assert_eq!(delay, Duration::from_millis(50));
        }
        other => panic!("expected second RetryAfter, got {other:?}"),
    }
    let exhausted = budget.on_full(now, None::<Deadline>);
    match exhausted {
        FullDecision::Exhausted { reason, report } => {
            assert_eq!(reason, FullExhaustionReason::BudgetExhausted);
            assert!(report.exhausted_count >= 1);
        }
        other => panic!("expected Exhausted, got {other:?}"),
    }
}

#[test]
fn concurrency_close_emits_typed_outcome_after_admission_stops() {
    let mut limit =
        ConcurrencyLimit::with_capacity("svc.shutdown", 2).on_pressure(PressureAction::Close);
    let p = limit.try_admit().into_admitted().unwrap();
    limit.close();
    match limit.try_admit() {
        AdmissionDecision::Closed(r) => {
            assert!(r.closed_count >= 1, "closed_count must increment");
        }
        other => panic!("expected Closed, got {other:?}"),
    }
    // Releasing the still-live permit is still legal; close is admission-only.
    limit.release(p).expect("release after close");
    let report = limit.report();
    assert_eq!(report.current, 0, "current must drain to zero after close");
}

#[test]
fn capacity_surface_round_trips_for_tuning_mode() {
    // Tuning mode must propagate through the report → surface projection
    // so discovery lines can name the mode honestly.
    let limit = ConcurrencyLimit::with_capacity("conc.tuning", 4).with_mode(CapacityMode::Tuning);
    let surface: CapacitySurfaceReport = limit.capacity_surface();
    assert_eq!(surface.mode, CapacityMode::Tuning);
    let line = format_discovery_line(&surface);
    assert!(line.contains("mode=tuning"), "expected tuning mode: {line}");
}
