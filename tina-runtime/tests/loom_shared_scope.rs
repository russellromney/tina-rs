#![cfg(feature = "loom")]

//! Loom model for `SharedCapacityScope`.
//!
//! `SharedCapacityScope` is public, `Clone`, and `Arc`-backed, so the type
//! can cross threads even though the intended pattern is shard-local. That
//! makes its reserve/release CAS loops part of the verified shared-memory
//! race surface inventoried in `.intent/race-surface-allowlist.txt`. These
//! models let loom permute every legal interleaving of two contending
//! admitters/releasers — at weight 1 and at weights > 1 — and check the safety
//! invariants the std stress test can only sample:
//!
//! - the cap is never exceeded (no lost CAS update / over-admission),
//! - weighted charges accumulate, not clobber,
//! - counters stay conserved (`current == admitted - released` at rest),
//! - every `Full` return is counted exactly once.
//!
//! What loom does *not* prove here: the Release/Acquire annotations. `current`
//! is a single atomic location, so coherence already orders the admitter's view
//! of a release; the orderings are defensive, and downgrading them to Relaxed
//! leaves these models green. The phantom-`Full` *reduction* under churn is a
//! timing property covered by the std stress test in `shared_scope.rs`.
//!
//! Run via `cargo test -p tina-runtime --features loom --test loom_shared_scope`.

use loom::thread;
use tina_runtime::SharedCapacityScope;

/// Bounded model checking. The reserve/release paths are `compare_exchange_weak`
/// spin loops, so the unbounded interleaving space is enormous and slow. A
/// preemption bound of 3 keeps the run to seconds while still permuting every
/// short interleaving where the real bugs live — the same trade the SPSC loom
/// suite makes.
fn bounded_model<F>(f: F)
where
    F: Fn() + Sync + Send + 'static,
{
    let mut builder = loom::model::Builder::new();
    builder.max_threads = 3;
    builder.preemption_bound = Some(3);
    builder.check(f);
}

/// Two admitters contend for the single slot and both hold their result.
/// Exactly one must win; the loser must record exactly one honest `Full`.
#[test]
fn two_admitters_never_exceed_cap_of_one() {
    bounded_model(|| {
        let scope = SharedCapacityScope::new("cap1", "u", 1);
        let a = scope.clone();
        let b = scope.clone();

        // Return (and thus hold) the lease so neither releases before the
        // other's attempt is decided.
        let ta = thread::spawn(move || a.try_admit(1));
        let tb = thread::spawn(move || b.try_admit(1));

        let ra = ta.join().expect("admitter a finishes cleanly");
        let rb = tb.join().expect("admitter b finishes cleanly");

        let winners = ra.is_ok() as usize + rb.is_ok() as usize;
        let snap = scope.snapshot();

        assert!(snap.current <= scope.max(), "cap exceeded: {snap:?}");
        assert_eq!(
            winners, 1,
            "exactly one admitter holds the only slot: {snap:?}"
        );
        assert_eq!(snap.current, 1, "the winner's charge is live: {snap:?}");
        assert_eq!(
            snap.full_count, 1,
            "the loser counted exactly one Full: {snap:?}"
        );
        assert_eq!(snap.high_water, 1, "{snap:?}");

        drop(ra);
        drop(rb);
    });
}

/// Two threads admit then immediately release. After both finish the scope
/// must drain to zero with conserved counters and no underflow/over-admit.
#[test]
fn concurrent_admit_release_conserves_counters() {
    bounded_model(|| {
        let scope = SharedCapacityScope::new("cap2", "u", 2);
        let a = scope.clone();
        let b = scope.clone();

        let ta = thread::spawn(move || {
            // cap=2, one unit each: admission cannot be Full here.
            let lease = a.try_admit(1).expect("room for unit a");
            drop(lease);
        });
        let tb = thread::spawn(move || {
            let lease = b.try_admit(1).expect("room for unit b");
            drop(lease);
        });

        ta.join().expect("releaser a finishes cleanly");
        tb.join().expect("releaser b finishes cleanly");

        let snap = scope.snapshot();
        assert_eq!(snap.current, 0, "scope drained: {snap:?}");
        assert_eq!(
            snap.admitted, snap.released,
            "every charge released: {snap:?}"
        );
        assert!(snap.high_water <= 2, "high-water within cap: {snap:?}");
    });
}

/// A release races an admit against a full cap. However the two interleave,
/// the cap must hold, counters must stay conserved, and `full_count` must
/// match the admitter's actual outcome (no phantom or uncounted Full).
#[test]
fn release_races_admit_without_corrupting_counters() {
    bounded_model(|| {
        let scope = SharedCapacityScope::new("cap1", "u", 1);
        let held = scope.try_admit(1).expect("first unit fits");
        let admitter = scope.clone();

        let release_thread = thread::spawn(move || drop(held));
        let admit_thread = thread::spawn(move || admitter.try_admit(1));

        release_thread.join().expect("releaser finishes cleanly");
        let admit_result = admit_thread.join().expect("admitter finishes cleanly");

        let admitted = admit_result.is_ok();
        let snap = scope.snapshot();

        assert!(snap.current <= scope.max(), "cap exceeded: {snap:?}");
        assert_eq!(
            snap.full_count,
            if admitted { 0 } else { 1 },
            "Full is counted iff the admit was rejected: {snap:?}",
        );
        assert_eq!(
            snap.current,
            if admitted { 1 } else { 0 },
            "live charge matches the held lease: {snap:?}",
        );
        assert_eq!(
            snap.admitted - snap.released,
            snap.current as u64,
            "current == admitted - released: {snap:?}",
        );

        drop(admit_result);
    });
}

/// Two different weights (>1) that sum to exactly the cap must both fit under
/// every interleaving — the weighted `saturating_add(weight)` CAS must
/// accumulate, not clobber.
#[test]
fn weighted_admits_accumulate_to_cap() {
    bounded_model(|| {
        let scope = SharedCapacityScope::new("cap3", "u", 3);
        let a = scope.clone();
        let b = scope.clone();

        let ta = thread::spawn(move || a.try_admit(1));
        let tb = thread::spawn(move || b.try_admit(2));

        let ra = ta.join().expect("admitter a finishes cleanly");
        let rb = tb.join().expect("admitter b finishes cleanly");

        let snap = scope.snapshot();
        assert!(
            ra.is_ok() && rb.is_ok(),
            "both weights fit (1+2=3): {snap:?}"
        );
        assert_eq!(
            snap.current, 3,
            "weights accumulated, not clobbered: {snap:?}"
        );
        assert_eq!(snap.admitted, 3, "{snap:?}");
        assert_eq!(snap.high_water, 3, "{snap:?}");
        assert_eq!(snap.full_count, 0, "{snap:?}");

        drop(ra);
        drop(rb);
    });
}

/// Two weighted admits that cannot both fit: exactly one wins, the cap holds,
/// and the loser counts exactly one honest weighted `Full`.
#[test]
fn weighted_admit_respects_cap_under_contention() {
    bounded_model(|| {
        let scope = SharedCapacityScope::new("cap2", "u", 2);
        let a = scope.clone();
        let b = scope.clone();

        let ta = thread::spawn(move || a.try_admit(2));
        let tb = thread::spawn(move || b.try_admit(2));

        let ra = ta.join().expect("admitter a finishes cleanly");
        let rb = tb.join().expect("admitter b finishes cleanly");

        let winners = ra.is_ok() as usize + rb.is_ok() as usize;
        let snap = scope.snapshot();

        assert!(snap.current <= scope.max(), "cap exceeded: {snap:?}");
        assert_eq!(winners, 1, "only one weight-2 admit fits cap=2: {snap:?}");
        assert_eq!(snap.current, 2, "the winner's weight is live: {snap:?}");
        assert_eq!(
            snap.full_count, 1,
            "the loser counted one weighted Full: {snap:?}"
        );

        drop(ra);
        drop(rb);
    });
}
