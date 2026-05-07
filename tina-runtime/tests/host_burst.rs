//! Phase 062 Rock 3: `try_send_outcome` + `HostBurstOutcomes`.
//!
//! Proves the burst accumulator counts each typed outcome distinctly:
//! `admitted`, `mailbox_full`, `mailbox_closed`, `ingress_full`, and
//! `worker_stopped`. The accumulator wraps the existing
//! `try_send_and_observe_with` shape; nothing about that contract
//! changes.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use tina::prelude::*;
use tina_runtime::{
    DefaultThreadedMailboxFactory, HostBurstOutcomes, HostBurstWaitError, SendObservedUntilError,
    ThreadedRuntime, ThreadedRuntimeConfig,
};

#[derive(Debug, Clone, Copy)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(0)
    }
}

#[derive(Debug)]
#[allow(dead_code)]
enum SlowMsg {
    Job(u32),
    /// Stop the isolate cleanly so its mailbox closes.
    Stop,
}

struct Slow {
    processed: Arc<AtomicU32>,
}

#[tina_runtime::isolate(message = SlowMsg, shard = TestShard)]
impl Slow {
    fn handle(&mut self, msg: SlowMsg, _ctx: &mut Context<'_, TestShard>) -> Effect<Self> {
        match msg {
            SlowMsg::Job(_) => {
                self.processed.fetch_add(1, Ordering::Release);
                noop()
            }
            SlowMsg::Stop => stop(),
        }
    }
}

fn make_runtime() -> ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory> {
    ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 256,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    )
}

#[test]
fn try_send_outcome_admits_full_burst_when_mailbox_has_room() {
    let runtime = make_runtime();
    let processed = Arc::new(AtomicU32::new(0));
    let worker = runtime
        .register_with_capacity::<Slow, Infallible>(
            Slow {
                processed: Arc::clone(&processed),
            },
            32,
        )
        .expect("register slow");

    let outcomes = HostBurstOutcomes::new();
    for n in 0..16 {
        runtime
            .try_send_outcome(worker, SlowMsg::Job(n), &outcomes)
            .expect("burst admits");
    }
    outcomes
        .wait_complete(Duration::from_secs(2))
        .expect("observers fire");

    let snap = outcomes.snapshot();
    assert_eq!(snap.submitted, 16);
    assert_eq!(snap.observed, 16);
    assert_eq!(snap.admitted, 16);
    assert_eq!(snap.mailbox_full, 0);
    assert_eq!(snap.mailbox_closed, 0);
    assert_eq!(snap.ingress_full, 0);
    assert_eq!(snap.worker_stopped, 0);

    let _ = runtime.shutdown();
}

#[test]
fn try_send_outcome_overflow_marks_mailbox_full() {
    let runtime = make_runtime();
    let processed = Arc::new(AtomicU32::new(0));
    let worker = runtime
        .register_with_capacity::<Slow, Infallible>(
            Slow {
                processed: Arc::clone(&processed),
            },
            // Tiny mailbox so a tight burst of commands fills before
            // the worker can step the isolate. The threaded worker
            // drains the entire command queue (each Run command is
            // followed by `continue`) before stepping, so the mailbox
            // fills with admitted messages before any get processed.
            4,
        )
        .expect("register slow");

    let outcomes = HostBurstOutcomes::new();
    let burst = 32u32;
    for n in 0..burst {
        let _ = runtime.try_send_outcome(worker, SlowMsg::Job(n), &outcomes);
    }
    outcomes
        .wait_complete(Duration::from_secs(2))
        .expect("observers fire");

    let snap = outcomes.snapshot();
    assert_eq!(snap.submitted, burst);
    assert_eq!(snap.observed, burst);
    assert!(
        snap.mailbox_full > 0,
        "burst of {burst} into cap=4 must hit MailboxFull at least once; snapshot={snap:?}"
    );
    assert_eq!(
        snap.admitted
            + snap.mailbox_full
            + snap.mailbox_closed
            + snap.ingress_full
            + snap.worker_stopped,
        burst,
        "every send must show up in exactly one outcome bucket"
    );

    let _ = runtime.shutdown();
}

#[test]
fn try_send_outcome_marks_mailbox_closed_after_stop() {
    let runtime = make_runtime();
    let processed = Arc::new(AtomicU32::new(0));
    let worker = runtime
        .register_with_capacity::<Slow, Infallible>(
            Slow {
                processed: Arc::clone(&processed),
            },
            8,
        )
        .expect("register slow");

    let stopped = runtime.observe_isolate_complete(worker);
    runtime.try_send(worker, SlowMsg::Stop).expect("kick stop");
    stopped.wait(Duration::from_secs(3)).expect("worker stops");

    let outcomes = HostBurstOutcomes::new();
    for n in 0..4 {
        let _ = runtime.try_send_outcome(worker, SlowMsg::Job(n), &outcomes);
    }
    outcomes
        .wait_complete(Duration::from_secs(2))
        .expect("observers fire");

    let snap = outcomes.snapshot();
    assert_eq!(snap.submitted, 4);
    assert_eq!(snap.observed, 4);
    assert_eq!(snap.admitted, 0);
    assert_eq!(snap.mailbox_closed, 4);

    let _ = runtime.shutdown();
}

// ---------- send_observed_until (Rock 4) ----------

#[test]
fn send_observed_until_succeeds_immediately_when_mailbox_has_room() {
    let runtime = make_runtime();
    let processed = Arc::new(AtomicU32::new(0));
    let worker = runtime
        .register_with_capacity::<Slow, Infallible>(
            Slow {
                processed: Arc::clone(&processed),
            },
            8,
        )
        .expect("register slow");

    runtime
        .send_observed_until(
            worker,
            Instant::now() + Duration::from_secs(1),
            Duration::from_millis(2),
            || SlowMsg::Job(0),
        )
        .expect("admits on first try");

    let _ = runtime.shutdown();
}

#[test]
fn send_observed_until_returns_closed_when_target_stopped() {
    let runtime = make_runtime();
    let processed = Arc::new(AtomicU32::new(0));
    let worker = runtime
        .register_with_capacity::<Slow, Infallible>(
            Slow {
                processed: Arc::clone(&processed),
            },
            8,
        )
        .expect("register slow");

    let stopped = runtime.observe_isolate_complete(worker);
    runtime.try_send(worker, SlowMsg::Stop).expect("stop");
    stopped
        .wait(Duration::from_secs(3))
        .expect("worker stops");

    let outcome = runtime.send_observed_until(
        worker,
        Instant::now() + Duration::from_secs(1),
        Duration::from_millis(2),
        || SlowMsg::Job(0),
    );
    assert_eq!(outcome, Err(SendObservedUntilError::Closed));

    let _ = runtime.shutdown();
}

#[test]
fn send_observed_until_attempts_at_least_once_even_with_past_deadline() {
    let runtime = make_runtime();
    let processed = Arc::new(AtomicU32::new(0));
    let worker = runtime
        .register_with_capacity::<Slow, Infallible>(
            Slow {
                processed: Arc::clone(&processed),
            },
            8,
        )
        .expect("register slow");

    // Deadline already in the past. The mailbox has room, so the
    // first attempt must succeed.
    let result = runtime.send_observed_until(
        worker,
        Instant::now() - Duration::from_secs(1),
        Duration::from_millis(10),
        || SlowMsg::Job(42),
    );
    assert_eq!(result, Ok(()));

    let _ = runtime.shutdown();
}

#[test]
fn send_observed_until_does_not_retry_closed() {
    // The match arms in `send_observed_until` retry only `MailboxFull`
    // and `IngressFull`. Closed targets must surface immediately so
    // the caller doesn't burn the deadline on a dead address.
    let runtime = make_runtime();
    let processed = Arc::new(AtomicU32::new(0));
    let worker = runtime
        .register_with_capacity::<Slow, Infallible>(
            Slow {
                processed: Arc::clone(&processed),
            },
            8,
        )
        .expect("register slow");

    let stopped = runtime.observe_isolate_complete(worker);
    runtime.try_send(worker, SlowMsg::Stop).expect("kick stop");
    stopped
        .wait(Duration::from_secs(3))
        .expect("worker stops");

    // Generous deadline. If the helper looped on Closed, this would
    // sleep until the deadline. It must return Closed immediately.
    let start = Instant::now();
    let result = runtime.send_observed_until(
        worker,
        Instant::now() + Duration::from_secs(10),
        Duration::from_millis(50),
        || SlowMsg::Job(0),
    );
    let elapsed = start.elapsed();
    assert_eq!(result, Err(SendObservedUntilError::Closed));
    assert!(
        elapsed < Duration::from_millis(500),
        "Closed must surface immediately, took {elapsed:?}"
    );

    let _ = runtime.shutdown();
}

#[test]
fn host_burst_wait_error_display_names_the_timeout() {
    // Pure API surface: Timeout's Display message is part of the
    // public contract because it shows up in `{e}` formatting.
    assert_eq!(
        format!("{}", HostBurstWaitError::Timeout),
        "timed out before every host-burst observer fired"
    );
}

#[test]
fn send_observed_until_error_display_names_each_variant() {
    assert_eq!(
        format!("{}", SendObservedUntilError::Timeout),
        "deadline elapsed before mailbox accepted the message"
    );
    assert_eq!(
        format!("{}", SendObservedUntilError::Closed),
        "target isolate mailbox is closed or stale"
    );
    assert_eq!(
        format!("{}", SendObservedUntilError::WorkerStopped),
        "worker thread stopped before the send could be observed"
    );
}
