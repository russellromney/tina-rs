//! Deterministic simulator replay proof for `tina_runtime::RateLimit`.
//!
//! `RateLimit` decisions are pure functions of `(rate, burst, now, key
//! history)`. The risk this test guards against is that `now` enters
//! through `ctx.now()` — and `ctx.now()` must be the simulator's *virtual*
//! clock, never `Instant::now()`. The simulator stamps `Context::now()`
//! from `virtual_anchor + virtual_now`, so an isolate that drives a
//! `RateLimit` off `ctx.now()` produces a byte-identical decision sequence
//! under replay.
//!
//! This is the "sim replay proves time-based policy determinism" proof:
//! the same scripted message-and-time sequence yields the same decisions
//! across runs, and — because the decisions never touch wall time — across
//! *different seeds* too.

use std::cell::RefCell;
use std::convert::Infallible;
use std::rc::Rc;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{RateLimit, RateLimitDecision, RuntimeEvent, stable_trace_hash};
use tina_sim::{Simulator, SimulatorConfig};

type DecisionLog = Rc<RefCell<Vec<String>>>;

#[derive(Debug, Clone)]
enum GateMsg {
    /// Ask the limiter to admit `key` at the current virtual time.
    Admit(&'static str),
}

struct Gate {
    limiter: RateLimit<&'static str>,
    log: DecisionLog,
}

#[tina_runtime::isolate(message = GateMsg)]
impl Gate {
    fn handle(
        &mut self,
        msg: GateMsg,
        ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            GateMsg::Admit(key) => {
                // The load-bearing line: `now` comes from the simulator's
                // virtual clock, not the wall clock.
                let now = ctx.now();
                let decision = match self.limiter.try_admit(&key, now) {
                    RateLimitDecision::Admitted(_) => format!("{key}=ok"),
                    RateLimitDecision::RateLimited { retry_after, .. } => {
                        format!("{key}=rate({}ms)", retry_after.as_millis())
                    }
                    RateLimitDecision::TableFull(_) => format!("{key}=table_full"),
                    RateLimitDecision::Closed(_) => format!("{key}=closed"),
                };
                self.log.borrow_mut().push(decision);
                noop()
            }
        }
    }
}

/// One `(key, virtual-time-ms)` admission attempt.
const SCRIPT: &[(&str, u64)] = &[
    ("a", 0),
    ("a", 0),
    ("a", 0),   // burst 2 → third at t=0 is rate-limited
    ("b", 0),   // a different key still admits at t=0
    ("a", 50),  // 50ms < refill window → still limited
    ("a", 120), // enough time for one token → ok
    ("b", 200),
    ("c", 200), // a third distinct key (max_keys = 2) → table full
    ("a", 260),
];

/// Drive the gate through `SCRIPT` under a fresh simulator with `seed`,
/// advancing virtual time between attempts. Returns the decision log and
/// the deterministic event trace.
fn run(seed: u64) -> (Vec<String>, Vec<RuntimeEvent>) {
    let log: DecisionLog = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(
        SingleShard,
        SimulatorConfig {
            seed,
            ..Default::default()
        },
    );
    let gate = sim.register::<_, _, Infallible>(Gate {
        // 10 tokens/sec, burst 2, up to 2 keys.
        limiter: RateLimit::new("sim.rate", 2, 10, 2),
        log: Rc::clone(&log),
    });

    let mut last_ms = 0u64;
    for (key, at_ms) in SCRIPT {
        if *at_ms > last_ms {
            sim.advance_time(Duration::from_millis(at_ms - last_ms));
            last_ms = *at_ms;
        }
        sim.try_send(gate, GateMsg::Admit(key)).expect("send admit");
        // Step delivers exactly this message, handled at the current
        // virtual time.
        sim.step();
    }
    sim.run_until_quiescent();
    (log.borrow().clone(), sim.trace().to_vec())
}

#[test]
fn rate_limit_decisions_replay_byte_identical_under_sim_time() {
    let (decisions_a, trace_a) = run(7);
    let (decisions_b, trace_b) = run(7);

    assert_eq!(
        decisions_a, decisions_b,
        "same script + seed must produce identical admission decisions",
    );
    assert_eq!(
        stable_trace_hash(trace_a.iter()),
        stable_trace_hash(trace_b.iter()),
        "the event trace must be byte-identical under replay",
    );

    // Sanity: the script must actually exercise the rate-limit path and an
    // eventual recovery, or the test proves nothing.
    assert!(
        decisions_a.iter().any(|d| d.contains("rate(")),
        "script did not hit the rate-limit path: {decisions_a:?}",
    );
    assert!(
        decisions_a.iter().any(|d| d.ends_with("=table_full")),
        "script did not hit key-table pressure: {decisions_a:?}",
    );
    assert!(
        decisions_a.iter().filter(|d| d.ends_with("=ok")).count() >= 3,
        "script did not exercise enough admits: {decisions_a:?}",
    );
}

#[test]
fn rate_limit_decisions_are_independent_of_seed() {
    // The decisions are a pure function of virtual time + key history, so
    // changing the simulator seed (which only steers fault injection, off
    // here) must not change a single decision. This is the strongest form
    // of "no live-only / nondeterministic rate timing".
    let (decisions_1, trace_1) = run(1);
    let (decisions_2, trace_2) = run(999_999);
    assert_eq!(
        decisions_1, decisions_2,
        "RateLimit decisions must not depend on the simulator seed",
    );
    assert_eq!(
        stable_trace_hash(trace_1.iter()),
        stable_trace_hash(trace_2.iter()),
        "with fault injection disabled, seed changes must not alter the trace",
    );
}

#[test]
fn retry_after_under_sim_is_the_exact_token_window() {
    // With 10 tokens/sec, one token takes 100ms. After burst is spent, the
    // reported retry_after must be exactly the deterministic token window —
    // no wall-clock jitter under sim.
    let log: DecisionLog = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(SingleShard, SimulatorConfig::default());
    let gate = sim.register::<_, _, Infallible>(Gate {
        limiter: RateLimit::new("sim.exact", 2, 10, 1),
        log: Rc::clone(&log),
    });
    // Burst 1: first admit OK, second at the same instant is rate-limited
    // with retry_after = 100ms exactly.
    sim.try_send(gate, GateMsg::Admit("k")).unwrap();
    sim.step();
    sim.try_send(gate, GateMsg::Admit("k")).unwrap();
    sim.step();
    let decisions = log.borrow().clone();
    assert_eq!(
        decisions,
        vec!["k=ok".to_string(), "k=rate(100ms)".to_string()]
    );
}
