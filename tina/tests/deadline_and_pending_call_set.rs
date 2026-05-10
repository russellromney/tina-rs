//! Public-surface tests for the value types added in 072: `Deadline`,
//! `Context::now`, `Context::deadline_after`, and `PendingCallSet`.
//!
//! These run inside the `tina` crate, so they cover everything the
//! types promise without a runtime dependency. Runtime-driven proofs
//! (fill -> cancel/timeout -> refill, runtime/sim parity) live in
//! `tina-runtime/tests/` and `tina-sim/tests/`.

use std::any::TypeId;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tina::{
    CallHandle, CallHandleShared, Context, Deadline, IsolateId, PendingCallSet,
    PendingCallSetInsertError, SingleShard, runtime_internal,
};

fn make_handle<R: 'static>() -> CallHandle<R> {
    let shared = Arc::new(CallHandleShared::new(TypeId::of::<R>()));
    runtime_internal::call_handle_from_shared::<R>(shared)
}

#[test]
fn deadline_from_instant_remaining_and_expired() {
    let now = Instant::now();
    let d = Deadline::from_instant(now, Duration::from_millis(500));
    assert_eq!(d.remaining(now), Some(Duration::from_millis(500)));
    assert_eq!(d.remaining_or_zero(now), Duration::from_millis(500));
    assert!(!d.expired(now));

    let later = now + Duration::from_secs(1);
    assert!(d.expired(later));
    assert_eq!(d.remaining_or_zero(later), Duration::ZERO);
    assert_eq!(d.remaining(later), None);
}

#[test]
fn deadline_saturates_on_overflow() {
    let now = Instant::now();
    let d = Deadline::from_instant(now, Duration::MAX);
    assert!(
        !d.expired(now),
        "Duration::MAX must not collapse to expired-now",
    );
    assert!(d.remaining_or_zero(now) > Duration::from_secs(60 * 60 * 24 * 365 * 50));
}

#[test]
fn context_deadline_after_uses_stamped_now() {
    let mut shard = SingleShard;
    let stamped = Instant::now();
    let ctx = Context::<_, ()>::new_typed(&mut shard, IsolateId::new(1)).with_now(stamped);

    let d = ctx.deadline_after(Duration::from_millis(250));
    assert_eq!(d.instant(), stamped + Duration::from_millis(250));
    assert_eq!(ctx.now(), stamped);
}

/// Regression: a `Context` constructed without `with_now` must panic on
/// `now()` rather than silently fall through to `Instant::now()`. The
/// 072 plan rule is "DST-claimed code cannot silently depend on
/// `std::time::Instant::now()`," and the panic is what enforces it.
#[test]
#[should_panic(expected = "Context::now() requires the runtime/simulator to have stamped")]
fn context_now_panics_when_not_stamped() {
    let mut shard = SingleShard;
    let ctx = Context::<_, ()>::new_typed(&mut shard, IsolateId::new(1));
    let _ = ctx.now();
}

#[test]
#[should_panic(expected = "Context::now() requires the runtime/simulator to have stamped")]
fn context_deadline_after_panics_when_now_not_stamped() {
    let mut shard = SingleShard;
    let ctx = Context::<_, ()>::new_typed(&mut shard, IsolateId::new(1));
    let _ = ctx.deadline_after(Duration::from_millis(10));
}

#[test]
fn pending_call_set_value_type_invariants() {
    let mut set: PendingCallSet<u64, ()> = PendingCallSet::with_capacity(3);
    assert_eq!(set.capacity(), 3);
    assert!(set.is_empty());
    assert!(!set.is_full());

    set.insert(1, make_handle()).map_err(|_| ()).unwrap();
    set.insert(2, make_handle()).map_err(|_| ()).unwrap();
    set.insert(3, make_handle()).map_err(|_| ()).unwrap();
    assert!(set.is_full());

    // Full surfaces typed.
    match set.insert(4, make_handle()) {
        Err(PendingCallSetInsertError::Full { key, handle: _ }) => assert_eq!(key, 4),
        _ => panic!("expected Full"),
    }
    // Duplicate key surfaces typed; existing entry preserved.
    match set.insert(2, make_handle()) {
        Err(PendingCallSetInsertError::DuplicateKey { key, handle: _ }) => assert_eq!(key, 2),
        _ => panic!("expected DuplicateKey"),
    }
    assert_eq!(set.len(), 3);

    // contains_key reads
    assert!(set.contains_key(&1));
    assert!(!set.contains_key(&99));

    // iter walks all entries.
    let keys: Vec<_> = set.iter().map(|(k, _)| *k).collect();
    assert_eq!(keys.len(), 3);

    // Remove on completion frees a slot.
    let _h = set.remove(&2).expect("present");
    assert!(!set.is_full());
    assert!(!set.contains_key(&2));

    // Drain commits even if the iterator is not consumed.
    let drained = set.drain();
    drop(drained);
    assert!(set.is_empty());
}

/// PendingCallSet must be `Send` when its key is `Send`. The handle's
/// shared cell is already `Send + Sync`, so the only restriction comes
/// from the user's `K`.
#[test]
fn pending_call_set_is_send_when_key_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<PendingCallSet<u64, ()>>();
    assert_send::<PendingCallSet<String, ()>>();
}
