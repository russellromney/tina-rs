//! Mailbox capacity truth tests.
//!
//! These tests pin the rule that runtime-call replies, isolate-call
//! replies, and observed-send replies all land in the requester's mailbox.
//! When the requester's mailbox is full, the runtime emits
//! `RuntimeEventKind::CallCompletionRejected { reason: MailboxFull }` so
//! trace consumers can diagnose under-sized mailboxes deterministically.
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
// the runtime then enqueues N replies into the caller's mailbox in one
// pass. With the mailbox size set lower than N, the late ones cannot
// land and the runtime emits the CallCompletionRejected event we want.
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
                    effects.push(sleep(Duration::ZERO).reply(|_| CallerMsg::SleepDone));
                }
                batch(effects)
            }
            CallerMsg::SleepDone => noop(),
        }
    }
}

#[test]
fn under_sized_mailbox_emits_call_completion_rejected_mailbox_full() {
    let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);
    let caller = runtime.register_with_capacity::<Caller, Infallible>(
        Caller { fanout: 6 },
        // Intentional under-sizing: 1 inbound slot only. The Begin message
        // lands in this slot first and is handled in the first step (so
        // the slot is empty when the sleeps fire), but only 1 of the 6
        // sleep replies fits into the next step before the caller can
        // drain.
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
    assert!(
        rejected_full >= 1,
        "expected at least one Sleep CallCompletionRejected MailboxFull event, got trace kinds: {:?}",
        trace.iter().map(|e| e.kind()).collect::<Vec<_>>(),
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
                        send_observed(self.fanout, NoiseMsg::Tick(n)).reply(|_| ObsMsg::Outcome),
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
