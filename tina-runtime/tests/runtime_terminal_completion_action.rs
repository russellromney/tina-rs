//! Terminal runtime-call completion actions are a narrow hot-path escape hatch.
//!
//! They must only remove a handler turn after successful runtime-owned work.
//! Backend failures must stay visible and must not be silently swallowed by a
//! `Noop`/`StopRequester` translator.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tina::prelude::*;
use tina_runtime::{
    CallCompletionRejectedReason, CallInput, CallKind, CallOutput, DefaultThreadedMailboxFactory,
    RuntimeCall, RuntimeCallCompletion, RuntimeEventKind, StreamId, TerminalCompletionAction,
    ThreadedRuntime,
};

#[derive(Debug)]
enum TerminalMsg {
    StopOnTimer,
    StopBeforeTimer,
    NoopOnTimer,
    MessageOnTimer,
    BadStreamNoop,
    Done,
}

struct TerminalProbe {
    delivered: Arc<AtomicUsize>,
}

#[tina_runtime::isolate(message = TerminalMsg, reply = ())]
impl TerminalProbe {
    fn handle(
        &mut self,
        msg: TerminalMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            TerminalMsg::StopOnTimer => Effect::Io(RuntimeCall::new_with_completion(
                CallInput::Sleep {
                    after: Duration::from_millis(5),
                },
                |output| match output {
                    CallOutput::TimerFired => RuntimeCallCompletion::StopRequester,
                    other => RuntimeCallCompletion::Message(unexpected_completion(other)),
                },
            )),
            TerminalMsg::StopBeforeTimer => batch(vec![
                Effect::Io(RuntimeCall::new_with_completion(
                    CallInput::Sleep {
                        after: Duration::from_millis(20),
                    },
                    |output| match output {
                        CallOutput::TimerFired => RuntimeCallCompletion::StopRequester,
                        other => RuntimeCallCompletion::Message(unexpected_completion(other)),
                    },
                )),
                stop(),
            ]),
            TerminalMsg::NoopOnTimer => Effect::Io(RuntimeCall::new_with_completion(
                CallInput::Sleep {
                    after: Duration::from_millis(5),
                },
                |output| match output {
                    CallOutput::TimerFired => RuntimeCallCompletion::Noop,
                    other => RuntimeCallCompletion::Message(unexpected_completion(other)),
                },
            )),
            TerminalMsg::MessageOnTimer => Effect::Io(RuntimeCall::new_with_completion(
                CallInput::Sleep {
                    after: Duration::from_millis(5),
                },
                |output| match output {
                    CallOutput::TimerFired => RuntimeCallCompletion::Message(TerminalMsg::Done),
                    other => RuntimeCallCompletion::Message(unexpected_completion(other)),
                },
            )),
            TerminalMsg::BadStreamNoop => Effect::Io(RuntimeCall::new_with_completion(
                CallInput::TcpStreamClose {
                    stream: StreamId::new(999_999),
                },
                |_| RuntimeCallCompletion::Noop,
            )),
            TerminalMsg::Done => {
                self.delivered.fetch_add(1, Ordering::Relaxed);
                noop()
            }
        }
    }
}

fn unexpected_completion(output: CallOutput) -> TerminalMsg {
    panic!("unexpected call completion: {output:?}")
}

fn runtime() -> ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory> {
    ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory)
}

fn wait_until(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
    f: impl Fn() -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !f() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting; trace = {:?}",
            runtime.trace().events()
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn has_event(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
    pred: impl Fn(RuntimeEventKind) -> bool,
) -> bool {
    runtime
        .trace()
        .events()
        .iter()
        .any(|event| pred(event.kind()))
}

fn count_events(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
    pred: impl Fn(RuntimeEventKind) -> bool,
) -> usize {
    runtime
        .trace()
        .events()
        .iter()
        .filter(|event| pred(event.kind()))
        .count()
}

#[test]
fn stop_requester_completion_stops_without_later_message() {
    let runtime = runtime();
    let delivered = Arc::new(AtomicUsize::new(0));
    let probe = runtime
        .register_with_capacity::<_, Infallible>(
            TerminalProbe {
                delivered: Arc::clone(&delivered),
            },
            8,
        )
        .expect("register probe");

    runtime
        .try_send(probe, TerminalMsg::StopOnTimer)
        .expect("start terminal stop");
    wait_until(&runtime, || {
        has_event(&runtime, |kind| {
            matches!(
                kind,
                RuntimeEventKind::CallCompletionAction {
                    action: TerminalCompletionAction::StopRequester,
                    ..
                }
            )
        })
    });

    assert_eq!(
        delivered.load(Ordering::Relaxed),
        0,
        "terminal stop must not enqueue a synthetic Done message"
    );
    assert!(
        has_event(&runtime, |kind| matches!(
            kind,
            RuntimeEventKind::IsolateStopped
        )),
        "StopRequester must use the ordinary stop lifecycle"
    );

    let _ = runtime.shutdown();
}

#[test]
fn noop_completion_records_success_and_keeps_isolate_alive() {
    let runtime = runtime();
    let delivered = Arc::new(AtomicUsize::new(0));
    let probe = runtime
        .register_with_capacity::<_, Infallible>(
            TerminalProbe {
                delivered: Arc::clone(&delivered),
            },
            8,
        )
        .expect("register probe");

    runtime
        .try_send(probe, TerminalMsg::NoopOnTimer)
        .expect("start terminal noop");
    wait_until(&runtime, || {
        has_event(&runtime, |kind| {
            matches!(
                kind,
                RuntimeEventKind::CallCompletionAction {
                    action: TerminalCompletionAction::Noop,
                    ..
                }
            )
        })
    });
    assert_eq!(delivered.load(Ordering::Relaxed), 0);

    runtime
        .try_send(probe, TerminalMsg::MessageOnTimer)
        .expect("probe remains alive after Noop");
    wait_until(&runtime, || delivered.load(Ordering::Relaxed) == 1);

    let _ = runtime.shutdown();
}

#[test]
fn message_completion_uses_normal_delivery_path() {
    let runtime = runtime();
    let delivered = Arc::new(AtomicUsize::new(0));
    let probe = runtime
        .register_with_capacity::<_, Infallible>(
            TerminalProbe {
                delivered: Arc::clone(&delivered),
            },
            8,
        )
        .expect("register probe");

    runtime
        .try_send(probe, TerminalMsg::MessageOnTimer)
        .expect("start message completion");
    wait_until(&runtime, || delivered.load(Ordering::Relaxed) == 1);

    assert_eq!(
        count_events(&runtime, |kind| matches!(
            kind,
            RuntimeEventKind::CallCompleted {
                call_kind: CallKind::Sleep,
                ..
            }
        )),
        1,
        "fallback message completion should still record the backend completion"
    );
    assert!(
        !has_event(&runtime, |kind| matches!(
            kind,
            RuntimeEventKind::CallCompletionAction { .. }
        )),
        "fallback message completion must not claim a terminal action"
    );

    let _ = runtime.shutdown();
}

#[test]
fn backend_failure_cannot_be_hidden_by_terminal_noop() {
    let runtime = runtime();
    let delivered = Arc::new(AtomicUsize::new(0));
    let probe = runtime
        .register_with_capacity::<_, Infallible>(
            TerminalProbe {
                delivered: Arc::clone(&delivered),
            },
            8,
        )
        .expect("register probe");

    runtime
        .try_send(probe, TerminalMsg::BadStreamNoop)
        .expect("start failed terminal action");
    wait_until(&runtime, || {
        has_event(&runtime, |kind| {
            matches!(
                kind,
                RuntimeEventKind::CallCompletionRejected {
                    reason: CallCompletionRejectedReason::TerminalActionOnFailure,
                    ..
                }
            )
        })
    });

    assert!(
        !has_event(&runtime, |kind| matches!(
            kind,
            RuntimeEventKind::CallCompletionAction { .. }
        )),
        "failed backend work must not produce a terminal action event"
    );
    runtime
        .try_send(probe, TerminalMsg::MessageOnTimer)
        .expect("failed terminal action must not stop the isolate");
    wait_until(&runtime, || delivered.load(Ordering::Relaxed) == 1);

    let _ = runtime.shutdown();
}

#[test]
fn terminal_action_after_requester_stop_records_closed_truth() {
    let runtime = runtime();
    let delivered = Arc::new(AtomicUsize::new(0));
    let probe = runtime
        .register_with_capacity::<_, Infallible>(
            TerminalProbe {
                delivered: Arc::clone(&delivered),
            },
            8,
        )
        .expect("register probe");

    runtime
        .try_send(probe, TerminalMsg::StopBeforeTimer)
        .expect("start stop-before-completion");
    wait_until(&runtime, || {
        has_event(&runtime, |kind| {
            matches!(
                kind,
                RuntimeEventKind::CallCompletionRejected {
                    reason: CallCompletionRejectedReason::RequesterClosed,
                    ..
                }
            )
        })
    });

    assert!(
        !has_event(&runtime, |kind| matches!(
            kind,
            RuntimeEventKind::CallCompletionAction { .. }
        )),
        "a terminal action must not run after the requester has stopped"
    );
    assert_eq!(delivered.load(Ordering::Relaxed), 0);

    let _ = runtime.shutdown();
}
