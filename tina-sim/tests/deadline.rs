//! Deterministic simulator proofs for [`tina::Deadline`] and
//! [`tina::Context::now`].
//!
//! The point: the simulator stamps `Context::now()` from its virtual
//! clock anchor + `virtual_now`, so a `Deadline` built inside a
//! handler returns the same remaining-budget answer under DST/replay
//! that it would in live code — without ever calling
//! `std::time::Instant::now()` from the handler.
//!
//! Without this guarantee, examples that copy `ctx.deadline_after(d)`
//! into replay-claimed code would silently break replay determinism.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::sleep;
use tina_sim::{Simulator, SimulatorConfig};

#[derive(Debug, Default, Clone, Copy)]
struct Report {
    /// Difference between the two `Context::now()` reads, in
    /// milliseconds. Should equal exactly the simulator's advanced
    /// virtual time — deterministic, not approximate.
    delta_ms: u64,
    /// `deadline.remaining_or_zero(ctx.now())` at Begin.
    remaining_at_begin_ms: u64,
    /// Same after the simulated sleep elapsed. Should be zero if the
    /// budget was 80ms and the sleep was 150ms.
    remaining_at_second_ms: u64,
    /// `deadline.expired(now)` at the second tick.
    expired_at_second: bool,
    finished: bool,
}

#[derive(Debug)]
enum CheckerMsg {
    Begin,
    SecondTick(tina_runtime::SleepReply),
}

struct Checker {
    deadline: Option<Deadline>,
    first_now: Option<std::time::Instant>,
    report: Arc<Mutex<Report>>,
}

#[tina_runtime::isolate(message = CheckerMsg)]
impl Checker {
    fn handle(
        &mut self,
        msg: CheckerMsg,
        ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            CheckerMsg::Begin => {
                let now = ctx.now();
                self.first_now = Some(now);
                let deadline = ctx.deadline_after(Duration::from_millis(80));
                self.deadline = Some(deadline);
                {
                    let mut r = self.report.lock().expect("report lock");
                    r.remaining_at_begin_ms = deadline.remaining_or_zero(now).as_millis() as u64;
                }
                sleep(Duration::from_millis(150)).reply(CheckerMsg::SecondTick)
            }
            CheckerMsg::SecondTick(Ok(())) => {
                let now = ctx.now();
                let first = self.first_now.expect("first now");
                let delta = now.saturating_duration_since(first);
                let deadline = self.deadline.expect("deadline");
                {
                    let mut r = self.report.lock().expect("report lock");
                    r.delta_ms = delta.as_millis() as u64;
                    r.remaining_at_second_ms = deadline.remaining_or_zero(now).as_millis() as u64;
                    r.expired_at_second = deadline.expired(now);
                    r.finished = true;
                }
                stop()
            }
            CheckerMsg::SecondTick(Err(_)) => stop(),
        }
    }
}

#[test]
fn simulator_now_is_virtual_and_deadline_is_deterministic() {
    let mut sim = Simulator::new(SingleShard, SimulatorConfig::default());
    let report = Arc::new(Mutex::new(Report::default()));

    let checker = sim.register::<_, _, Infallible>(Checker {
        deadline: None,
        first_now: None,
        report: Arc::clone(&report),
    });

    sim.try_send(checker, CheckerMsg::Begin)
        .expect("send Begin");
    sim.run_until_quiescent();

    let snap = *report.lock().expect("report lock");
    assert!(snap.finished, "checker should have run to second tick");
    // Simulator virtual clock advances by exactly the sleep budget the
    // handler asked for; live runtimes can drift past it on slow CI
    // but the simulator never does. This is the load-bearing parity
    // claim.
    assert_eq!(
        snap.delta_ms, 150,
        "simulator delta should equal the requested sleep exactly",
    );
    assert_eq!(
        snap.remaining_at_begin_ms, 80,
        "fresh deadline reports the full budget under sim",
    );
    assert_eq!(
        snap.remaining_at_second_ms, 0,
        "deadline expired -> remaining_or_zero is zero",
    );
    assert!(
        snap.expired_at_second,
        "deadline must report expired at the second tick",
    );
}
