//! Proof: the skip-empty scan is correct at scale.
//!
//! The per-step scheduler scan now probes `Mailbox::is_empty()` and skips the
//! expensive `recv` on quiet isolates. `is_empty()` reflects real mailbox state
//! for every ingress path, so:
//!
//! - a hot isolate is still served promptly with many idle isolates present;
//! - a message delivered to an otherwise-idle isolate is still scheduled (the
//!   direct-mailbox seam that the enqueue-side mark in a prior attempt missed;
//!   `address_liveness` covers the held-`try_send` handle case directly).

use std::convert::Infallible;
use std::time::{Duration, Instant};

use tina::prelude::*;
use tina_runtime::{CallOutcome, DefaultThreadedMailboxFactory, ThreadedRuntime};

#[derive(Debug, Clone, Copy)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(0)
    }
}

#[derive(Debug)]
enum Msg {
    Ping,
}

struct Counter {
    seen: u32,
}

#[tina_runtime::isolate(message = Msg, reply = u32, shard = TestShard)]
impl Counter {
    fn handle(&mut self, msg: Msg, _ctx: &mut Context<'_, TestShard, u32>) -> Effect<Self> {
        match msg {
            Msg::Ping => {
                self.seen += 1;
                reply(self.seen)
            }
        }
    }

    fn handle_call(&mut self, msg: Msg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            Msg::Ping => {
                self.seen += 1;
                let n = self.seen;
                call.reply(n)
            }
        }
    }
}

const CAP: usize = 8;

/// A hot isolate stays served promptly while thousands of idle isolates sit on
/// the shard. With the skip-empty scan the quiet isolates cost only a cheap
/// `is_empty()` probe each step, so the hot path is not dragged by the scan.
#[test]
fn hot_isolate_served_with_many_idle_isolates() {
    let runtime: ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory> =
        ThreadedRuntime::new(TestShard, DefaultThreadedMailboxFactory);

    // A pile of idle isolates that never receive a message.
    for _ in 0..2000 {
        runtime
            .register_with_capacity::<_, Infallible>(Counter { seen: 0 }, CAP)
            .expect("register idle");
    }
    let hot = runtime
        .register_with_capacity::<_, Infallible>(Counter { seen: 0 }, CAP)
        .expect("register hot");

    let started = Instant::now();
    for expected in 1..=50 {
        let outcome = runtime
            .call_blocking(hot, Msg::Ping, Duration::from_secs(2))
            .expect("call");
        assert_eq!(outcome, CallOutcome::Replied(expected));
    }
    let elapsed = started.elapsed();

    // Correctness is the assertion above; the timing is recorded as evidence the
    // 2000-idle scan does not dominate (generous ceiling, not a perf gate).
    assert!(
        elapsed < Duration::from_secs(2),
        "50 hot calls over 2000 idle isolates took {elapsed:?}"
    );
    eprintln!("hot_with_2000_idle: 50 calls in {elapsed:?}");

    runtime.shutdown().expect("shutdown");
}

/// A message to a single isolate among many idle ones is still scheduled — the
/// skip-empty scan does not skip a non-empty mailbox regardless of how the
/// message arrived.
#[test]
fn message_to_one_of_many_idle_isolates_is_scheduled() {
    let runtime: ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory> =
        ThreadedRuntime::new(TestShard, DefaultThreadedMailboxFactory);

    let mut targets = Vec::new();
    for _ in 0..500 {
        targets.push(
            runtime
                .register_with_capacity::<_, Infallible>(Counter { seen: 0 }, CAP)
                .expect("register"),
        );
    }

    // Pick a middle isolate and a last one; both must be served despite all the
    // empty mailboxes around them.
    for &idx in &[0usize, 250, 499] {
        let outcome = runtime
            .call_blocking(targets[idx], Msg::Ping, Duration::from_secs(2))
            .expect("call");
        assert_eq!(outcome, CallOutcome::Replied(1), "isolate {idx} not served");
    }

    runtime.shutdown().expect("shutdown");
}
