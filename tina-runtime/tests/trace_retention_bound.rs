//! Live-runtime trace retention is bounded by default.
//!
//! The framework's contract is bounded-everything. A live worker's diagnostic
//! trace must not grow with uptime. These tests pin the two live config owners
//! (`ThreadedRuntimeConfig`, `LocalSystemConfig`) to a bounded default and prove
//! the bound holds under load that far exceeds it.
//!
//! Load-bearing check: `live_full_trace_retention_grows_without_bound` runs the
//! same loop under explicit `TraceRetention::Full` and watches the trace grow
//! past the bound. Flip the default owners back to `Full` and
//! `live_default_trace_retention_stays_bounded_under_load` fails — the bound is
//! the only thing keeping it green.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use tina::{Mailbox, TrySendError, prelude::*};
use tina_runtime::{
    DEFAULT_LIVE_TRACE_RETENTION, LocalSystemConfig, MailboxFactory, RuntimeCall, ThreadedRuntime,
    ThreadedRuntimeConfig, TraceRetention,
};

#[derive(Debug, Default)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(71)
    }
}

struct TestMailbox<T> {
    capacity: usize,
    queue: std::cell::RefCell<std::collections::VecDeque<T>>,
    closed: std::cell::Cell<bool>,
}

impl<T> Mailbox<T> for TestMailbox<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }
    fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        if self.closed.get() {
            return Err(TrySendError::Closed(message));
        }
        let mut queue = self.queue.borrow_mut();
        if queue.len() >= self.capacity {
            return Err(TrySendError::Full(message));
        }
        queue.push_back(message);
        Ok(())
    }
    fn recv(&self) -> Option<T> {
        self.queue.borrow_mut().pop_front()
    }
    fn is_empty(&self) -> bool {
        self.queue.borrow().is_empty()
    }
    fn close(&self) {
        self.closed.set(true);
    }
}

#[derive(Debug, Clone, Copy)]
struct TestMailboxFactory;

impl MailboxFactory for TestMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(TestMailbox {
            capacity,
            queue: std::cell::RefCell::new(std::collections::VecDeque::new()),
            closed: std::cell::Cell::new(false),
        })
    }
}

/// Counts down by re-sending itself, so one seed message drives many handler
/// turns and thus many trace events without any host round-trips.
#[derive(Debug, Clone, Copy)]
enum CountMsg {
    Tick,
}

struct Counter {
    remaining: usize,
    done: Arc<AtomicBool>,
}

impl Isolate for Counter {
    tina::isolate_types! {
        message: CountMsg,
        reply: (),
        send: Outbound<CountMsg>,
        spawn: Infallible,
        io: RuntimeCall<CountMsg>,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            CountMsg::Tick => {
                if self.remaining == 0 {
                    self.done.store(true, Ordering::Release);
                    noop()
                } else {
                    self.remaining -= 1;
                    ctx.send_self(CountMsg::Tick)
                }
            }
        }
    }
}

fn wait_until<F: FnMut() -> bool>(timeout: Duration, label: &str, mut predicate: F) {
    let deadline = Instant::now() + timeout;
    while !predicate() {
        if Instant::now() > deadline {
            panic!("wait_until({label}): not satisfied within timeout");
        }
        thread::yield_now();
    }
}

// Enough turns that the event count comfortably clears the default bound even
// at one trace event per turn (each turn actually emits several).
const DRIVE_TURNS: usize = DEFAULT_LIVE_TRACE_RETENTION + 4_000;

fn drive_counter(config: ThreadedRuntimeConfig) -> (usize, u64) {
    let done = Arc::new(AtomicBool::new(false));
    let runtime = ThreadedRuntime::with_config(TestShard, TestMailboxFactory, config);
    let worker = runtime
        .register_with_capacity::<Counter, _>(
            Counter {
                remaining: DRIVE_TURNS,
                done: Arc::clone(&done),
            },
            8,
        )
        .expect("counter register accepts");
    runtime
        .try_send(worker, CountMsg::Tick)
        .expect("counter ingress accepts");
    wait_until(Duration::from_secs(30), "counter drained", || {
        done.load(Ordering::Acquire)
    });
    let trace = runtime.trace();
    let len = trace.events().len();
    let dropped = trace.dropped_events();
    let _ = runtime.shutdown().expect("runtime shutdown");
    (len, dropped)
}

#[test]
fn live_config_defaults_are_bounded_not_full() {
    // Both live owners must default to the generous ring, never `Full`.
    assert_eq!(
        ThreadedRuntimeConfig::default().trace_retention,
        TraceRetention::Bounded(DEFAULT_LIVE_TRACE_RETENTION),
        "ThreadedRuntimeConfig default must be bounded"
    );
    assert_eq!(
        LocalSystemConfig::default().trace_retention,
        TraceRetention::Bounded(DEFAULT_LIVE_TRACE_RETENTION),
        "LocalSystemConfig default must be bounded"
    );
    assert_ne!(
        ThreadedRuntimeConfig::default().trace_retention,
        TraceRetention::Full
    );
    assert_ne!(
        LocalSystemConfig::default().trace_retention,
        TraceRetention::Full
    );
}

#[test]
fn live_default_trace_retention_stays_bounded_under_load() {
    // Default config, driven through far more events than the bound.
    let (len, dropped) = drive_counter(ThreadedRuntimeConfig::default());
    assert!(
        len <= DEFAULT_LIVE_TRACE_RETENTION,
        "retained trace ({len}) exceeded the bound ({DEFAULT_LIVE_TRACE_RETENTION})"
    );
    // The ring filled and started dropping: the trace stopped growing.
    assert_eq!(
        len, DEFAULT_LIVE_TRACE_RETENTION,
        "a full ring should retain exactly the bound"
    );
    assert!(
        dropped > 0,
        "load beyond the bound must report dropped events"
    );
}

#[test]
fn live_full_trace_retention_grows_without_bound() {
    // Same loop, explicit Full — the memory-growth the default now prevents.
    let (len, dropped) = drive_counter(ThreadedRuntimeConfig {
        trace_retention: TraceRetention::Full,
        ..Default::default()
    });
    assert!(
        len > DEFAULT_LIVE_TRACE_RETENTION,
        "Full retention should grow past the bound ({DEFAULT_LIVE_TRACE_RETENTION}), got {len}"
    );
    assert_eq!(dropped, 0, "Full retention drops nothing");
}
