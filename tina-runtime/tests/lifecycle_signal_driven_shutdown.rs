//! Phase 106 — signal-driven shutdown composes through
//! `ShutdownChoreography`.
//!
//! The plan requires "signal-driven shutdown works in live runtime." The
//! lifecycle helper is data; the actual signal capture lives in the
//! runtime's signal driver (Unix SIGINT/SIGTERM). What needs proving is
//! that the helper composes correctly when triggered from a separate
//! thread, which is the signal-handler shape.
//!
//! This test uses `ThreadedShutdownHandle` (the cloneable, cross-thread
//! shutdown trigger introduced by Phase 102) as the signal-handler
//! stand-in. A spawned thread sleeps briefly to simulate the signal
//! arrival, then calls `request_shutdown`. The main thread observes the
//! trigger, runs an ordered `ShutdownChoreography`, calls the runtime
//! shutdown, and asserts the typed terminal report is clean and the
//! steps are recorded in order.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use tina::prelude::*;
use tina_runtime::lifecycle::{
    CloseAdmission, Lifecycle, ResourceCloseReport, ResourceKind, ShutdownChoreography,
    ShutdownStep, StepOutcome,
};
use tina_runtime::{DefaultThreadedMailboxFactory, ThreadedRuntime};

struct Noop;

impl Isolate for Noop {
    tina::isolate_types! {
        message: (),
        reply: Infallible,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        io: Infallible,
        shard: SingleShard,
    }

    fn handle(
        &mut self,
        _msg: Self::Message,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        noop()
    }
}

#[test]
fn signal_driven_shutdown_composes_through_choreography() {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));
    runtime
        .register_with_capacity::<_, Infallible>(Noop, 1)
        .expect("register noop");

    let shutdown = runtime.shutdown_handle();
    let signal_arrived = Arc::new(AtomicBool::new(false));

    // Spawn the "signal handler" thread. It sleeps briefly, sets the
    // arrived flag, and requests runtime shutdown — the same shape a
    // real SIGINT handler would take using ThreadedShutdownHandle.
    let signal_clone = Arc::clone(&signal_arrived);
    let shutdown_clone = shutdown.clone();
    let handler = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        signal_clone.store(true, Ordering::SeqCst);
        shutdown_clone
            .request_shutdown()
            .expect("signal handler can request shutdown");
    });

    // Main thread waits for the signal, then runs the typed choreography
    // around the runtime shutdown. This is the production shape: a
    // service that wants ordered shutdown wraps the cross-thread signal
    // trigger in `ShutdownChoreography` so the terminal report names
    // every step.
    let wait_deadline = Instant::now() + Duration::from_secs(2);
    while !signal_arrived.load(Ordering::SeqCst) {
        if Instant::now() >= wait_deadline {
            panic!("signal handler thread did not fire within 2s");
        }
        thread::sleep(Duration::from_millis(5));
    }

    let mut choreo = ShutdownChoreography::new("signal_driven_proof");

    // Stage 1: stop ingress. Real services would close their listener
    // mailbox here; the proof keeps the step explicit.
    choreo.record(
        ShutdownStep::StopIngress,
        "signal_received",
        Duration::from_micros(1),
        StepOutcome::Clean,
    );

    // Stage 2: close the runtime as a named resource via the cross-thread
    // shutdown handle (already triggered by the signal thread; here we
    // wait for its terminal report).
    let t_close = Instant::now();
    let report = shutdown
        .wait_report(Duration::from_secs(5))
        .expect("runtime shuts down cleanly");
    let close_outcome = if report.error().is_none() {
        ResourceCloseReport::clean(
            "runtime",
            ResourceKind::Custom("runtime"),
            CloseAdmission::Drain,
            t_close.elapsed(),
        )
        .with_details("signal-driven runtime shutdown completed")
    } else {
        panic!("runtime shutdown returned an error: {:?}", report.error());
    };
    choreo.record_close(&close_outcome, "shutdown_runtime");

    // Stage 3: emit the terminal report.
    choreo.record(
        ShutdownStep::StopOwner,
        "owner_stopped",
        Duration::from_micros(1),
        StepOutcome::Clean,
    );

    handler.join().expect("signal handler joined");

    let terminal = choreo.finish();
    assert!(
        terminal.clean,
        "signal-driven choreography unclean: {terminal:#?}"
    );
    let step_kinds: Vec<ShutdownStep> = terminal.steps.iter().map(|s| s.step).collect();
    assert_eq!(
        step_kinds,
        vec![
            ShutdownStep::StopIngress,
            ShutdownStep::CloseResource,
            ShutdownStep::StopOwner,
        ],
    );
    assert_eq!(terminal.closes.len(), 1);
    assert_eq!(terminal.closes[0].name, "runtime");
    assert_eq!(terminal.closes[0].kind, ResourceKind::Custom("runtime"));

    // The Arc must drop cleanly: signal-driven shutdown does not leave
    // an orphaned runtime owner.
    drop(runtime);
}

#[test]
fn signal_driven_shutdown_records_late_signal_as_already_done() {
    // Negative-shape pair: if the signal arrives after the service is
    // already in `Lifecycle::Stopped`, the choreography records the
    // step as `AlreadyDone` rather than starting a second shutdown.
    let mut choreo = ShutdownChoreography::new("late-signal-svc");
    choreo.record(
        ShutdownStep::StopIngress,
        "first_close",
        Duration::from_micros(1),
        StepOutcome::Clean,
    );
    choreo.record(
        ShutdownStep::StopOwner,
        "first_stop",
        Duration::from_micros(1),
        StepOutcome::Clean,
    );
    // Simulated late signal lands after StopOwner. Recording StopIngress
    // again would violate ordering; the recorder should expose that.
    choreo.record(
        ShutdownStep::StopIngress,
        "late_signal",
        Duration::from_micros(1),
        StepOutcome::AlreadyDone,
    );
    let terminal = choreo.finish();
    assert!(!terminal.clean, "late re-signal must be visible");
    let last = terminal.steps.last().expect("last step");
    match last.outcome {
        StepOutcome::OrderingViolation { previous_step } => {
            assert_eq!(previous_step, ShutdownStep::StopOwner);
        }
        ref other => panic!("expected OrderingViolation, got {other:?}"),
    }
    assert!(!Lifecycle::Stopped.admits_new_work());
}
