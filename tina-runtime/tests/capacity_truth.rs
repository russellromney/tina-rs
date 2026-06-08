//! Mailbox capacity truth tests.
//!
//! Two rules live here. Runtime-call continuations (Sleep, I/O loops) keep a
//! held resource alive, so they are never dropped: when the mailbox is full
//! the runtime parks them in a priority overflow (`CallContinuationOverflowed`)
//! and still completes the call. Best-effort outcomes that do not gate a
//! held resource — observed-send replies — still land in the bounded mailbox
//! and surface `CallCompletionRejected { reason: MailboxFull }` when it is
//! full, so trace consumers can diagnose under-sized mailboxes.
//!
//! See `docs/mailbox-capacity.md` for the user-facing doc that this test
//! suite anchors.

use std::convert::Infallible;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallCompletionRejectedReason, CallKind, CallOutcome, DefaultMailboxFactory, Runtime,
    RuntimeEventKind, send_observed, sleep,
};

// Caller that fan-issues N simultaneous-completion sleep calls. All sleeps
// share the same duration so they all complete on the same driver poll;
// the runtime then enqueues N continuations into the caller in one pass.
// With the mailbox under-sized, the late ones cannot fit — the runtime
// parks them in the overflow rather than dropping them, so every sleep
// still completes.
#[derive(Debug, Clone)]
enum CallerMsg {
    Begin,
    SleepDone,
}

#[derive(Debug)]
struct Caller {
    fanout: u32,
}

#[tina_runtime::isolate(message = CallerMsg)]
impl Caller {
    fn handle(
        &mut self,
        msg: CallerMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            CallerMsg::Begin => {
                let mut effects: Vec<Effect<Self>> = Vec::with_capacity(self.fanout as usize);
                for _ in 0..self.fanout {
                    effects.push(sleep(Duration::ZERO).then(|_| CallerMsg::SleepDone));
                }
                batch(effects)
            }
            CallerMsg::SleepDone => noop(),
        }
    }
}

#[test]
fn under_sized_mailbox_overflows_runtime_call_continuations_without_dropping() {
    let fanout = 6;
    let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);
    let caller = runtime.register_with_capacity::<Caller, Infallible>(
        Caller { fanout },
        // Intentional under-sizing: 1 inbound slot only. The Begin message
        // lands in this slot first and is handled in the first step (so the
        // slot is empty when the sleeps fire), but only 1 of the 6 sleep
        // continuations fits the mailbox before the caller can drain. The
        // other 5 must overflow rather than drop.
        1,
    );
    runtime
        .try_send(caller, CallerMsg::Begin)
        .expect("kick caller");

    // Step until quiescent.
    for _ in 0..256 {
        runtime.step();
        if !runtime.has_in_flight_calls() {
            break;
        }
    }

    let trace = runtime.trace();
    let kinds: Vec<_> = trace.iter().map(|e| e.kind()).collect();

    // A held-resource continuation must never be dropped: no Sleep completion
    // is ever rejected for a full mailbox.
    let rejected_full = trace
        .iter()
        .filter(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallCompletionRejected {
                    reason: CallCompletionRejectedReason::MailboxFull,
                    call_kind: CallKind::Sleep,
                    ..
                }
            )
        })
        .count();
    assert_eq!(
        rejected_full, 0,
        "a runtime-call continuation must never be dropped on a full mailbox; trace: {kinds:?}",
    );

    // The over-capacity continuations are parked in the overflow.
    let overflowed = trace
        .iter()
        .filter(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallContinuationOverflowed {
                    call_kind: CallKind::Sleep,
                    ..
                }
            )
        })
        .count();
    assert!(
        overflowed >= 1,
        "expected at least one Sleep continuation to overflow under a size-1 mailbox; trace: {kinds:?}",
    );

    // Every sleep still completes — none lost.
    let completed = trace
        .iter()
        .filter(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallCompleted {
                    call_kind: CallKind::Sleep,
                    ..
                }
            )
        })
        .count();
    assert_eq!(
        completed, fanout as usize,
        "every sleep continuation must be delivered exactly once; trace: {kinds:?}",
    );
}

// A callee with reply, just to keep the cross-rule worked example below
// compiling — we don't currently use it, but keeping the type ensures the
// docs example in `mailbox-capacity.md` references real public surface.
#[derive(Debug, Clone)]
#[allow(dead_code)]
enum CalleeMsg {
    Echo(u32),
}

#[derive(Debug, Default)]
#[allow(dead_code)]
struct Callee;

#[tina_runtime::isolate(message = CalleeMsg, reply = u32)]
impl Callee {
    fn handle(
        &mut self,
        msg: CalleeMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            CalleeMsg::Echo(value) => reply(value),
        }
    }
}

#[allow(dead_code)]
fn _consume_call_outcome(_outcome: CallOutcome<u32>) {}

// A small isolate that observes its own send outcomes, used to prove the
// observed-send rule for capacity.
#[derive(Debug, Clone)]
enum ObsMsg {
    Begin,
    Outcome,
}

#[derive(Debug)]
struct ObsCaller {
    fanout: Address<NoiseMsg>,
    burst: u32,
}

#[tina_runtime::isolate(message = ObsMsg)]
impl ObsCaller {
    fn handle(
        &mut self,
        msg: ObsMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ObsMsg::Begin => {
                let mut effects: Vec<Effect<Self>> = Vec::with_capacity(self.burst as usize);
                for n in 0..self.burst {
                    effects.push(
                        send_observed(self.fanout, NoiseMsg::Tick(n)).then(|_| ObsMsg::Outcome),
                    );
                }
                batch(effects)
            }
            ObsMsg::Outcome => noop(),
        }
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
enum NoiseMsg {
    Tick(u32),
}

#[derive(Debug, Default)]
struct Noise;

#[tina_runtime::isolate(message = NoiseMsg)]
impl Noise {
    fn handle(
        &mut self,
        _msg: NoiseMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        noop()
    }
}

#[test]
fn observed_send_replies_count_against_caller_mailbox() {
    let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);
    let noise = runtime.register_with_capacity::<Noise, Infallible>(Noise, 16);
    let observer = runtime.register_with_capacity::<ObsCaller, Infallible>(
        ObsCaller {
            fanout: noise,
            burst: 5,
        },
        // Small enough that observed-send outcomes can't all fit.
        2,
    );
    runtime
        .try_send(observer, ObsMsg::Begin)
        .expect("kick observer");
    for _ in 0..128 {
        runtime.step();
        if !runtime.has_in_flight_calls() {
            break;
        }
    }
    let trace = runtime.trace();
    let rejected_full = trace
        .iter()
        .filter(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallCompletionRejected {
                    reason: CallCompletionRejectedReason::MailboxFull,
                    call_kind: CallKind::ObservedSend,
                    ..
                }
            )
        })
        .count();
    assert!(
        rejected_full >= 1,
        "expected at least one ObservedSend MailboxFull rejection, got trace kinds: {:?}",
        trace.iter().map(|e| e.kind()).collect::<Vec<_>>(),
    );
}
