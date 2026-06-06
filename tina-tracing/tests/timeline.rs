use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::rc::Rc;
use std::time::Duration;

use serde_json::Value;
use tina::capacity::{CapacityMode, CapacitySurfaceReport};
use tina::prelude::*;
use tina::{
    AddressGeneration, CallContext, CallRejectedReason, IsolateId, Mailbox, ShardId, TrySendError,
};
use tina_runtime::{
    CallCompletionRejectedReason, CallError, CallId, CallKind, CallOutcome,
    CallReplyRejectedReason, CapacitySummary, CauseId, DeferredReplyRejectedReason, DeferredSlotId,
    EventId, GrpcStatusCode, GrpcStreamId, Http2CloseReason, Http2FlowControlSide,
    Http2ResetReason, Http2StreamId, MailboxFactory, PressureSummary, ProtocolConnectionId,
    ProtocolDirection, ProtocolFact, Runtime, RuntimeCall, RuntimeEvent, RuntimeEventKind,
    RuntimeFact, SendRejectedReason, SupervisionRejectedReason, ThreadedRuntime,
    ThreadedRuntimeConfig, WebSocketCloseReason, WebSocketSessionId, call,
};
use tina_tracing::{TraceTimeline, to_chrome_trace_json_string, write_chrome_trace_json};

fn evt(id: u64, kind: RuntimeEventKind) -> RuntimeEvent {
    RuntimeEvent::new(
        EventId::new(id),
        id.checked_sub(1)
            .filter(|previous| *previous > 0)
            .map(|previous| CauseId::new(EventId::new(previous))),
        ShardId::new(1),
        IsolateId::new(7),
        kind,
    )
}

fn export(events: &[RuntimeEvent]) -> Value {
    let timeline = TraceTimeline::from_events(events).finish();
    serde_json::from_str(&to_chrome_trace_json_string(&timeline).unwrap()).unwrap()
}

fn trace_events(root: &Value) -> &[Value] {
    root["traceEvents"].as_array().unwrap()
}

fn names(root: &Value) -> Vec<&str> {
    trace_events(root)
        .iter()
        .map(|event| event["name"].as_str().unwrap())
        .collect()
}

fn first_named<'a>(root: &'a Value, name: &str) -> &'a Value {
    trace_events(root)
        .iter()
        .find(|event| event["name"] == name)
        .unwrap_or_else(|| panic!("missing event named {name}"))
}

fn assert_chrome_shape(root: &Value) {
    assert_eq!(root["displayTimeUnit"], "us");
    let events = trace_events(root);
    assert!(root.get("traceEvents").is_some());
    for event in events {
        for field in ["ph", "name", "cat", "ts", "pid", "tid", "args"] {
            assert!(event.get(field).is_some(), "missing {field} in {event:?}");
        }
    }
}

#[test]
fn empty_trace_exports_valid_chrome_json_with_metadata() {
    let root = export(&[]);
    assert_chrome_shape(&root);
    assert!(names(&root).contains(&"process_name"));
    assert_eq!(
        first_named(&root, "process_name")["args"]["time_kind"],
        "logical_event_id"
    );
    assert_eq!(
        first_named(&root, "process_name")["args"]["canonical_truth"],
        "RuntimeEvent"
    );
}

#[test]
fn partial_trace_snapshot_names_missing_shards() {
    #[derive(Debug, Clone, Copy)]
    struct EmptyShard;
    impl Shard for EmptyShard {
        fn id(&self) -> ShardId {
            ShardId::new(9)
        }
    }

    let runtime = ThreadedRuntime::with_config(
        EmptyShard,
        TestMailboxFactory,
        ThreadedRuntimeConfig::default(),
    );
    let handle = runtime.shutdown_handle();
    handle.request_shutdown().unwrap();
    let _ = handle.wait_report(Duration::from_secs(2)).unwrap();
    let snapshot = runtime.trace();

    assert!(snapshot.is_partial());
    let timeline = TraceTimeline::from_snapshot(&snapshot).finish();
    let root: Value =
        serde_json::from_str(&to_chrome_trace_json_string(&timeline).unwrap()).unwrap();
    assert_eq!(
        first_named(&root, "trace_snapshot_partial")["args"]["missing_shards"],
        serde_json::json!([9])
    );
}

#[test]
fn handler_start_finish_becomes_one_duration_slice() {
    let root = export(&[
        evt(1, RuntimeEventKind::HandlerStarted),
        evt(
            4,
            RuntimeEventKind::HandlerFinished {
                effect: tina_runtime::EffectKind::Reply,
            },
        ),
    ]);
    let span = first_named(&root, "handler_turn");
    assert_eq!(span["ph"], "X");
    assert_eq!(span["ts"], 1);
    assert_eq!(span["dur"], 3);
    assert_eq!(span["args"]["effect"], "reply");
    assert_eq!(span["args"]["terminal_kind"], "handler_finished");
}

#[test]
fn handler_panic_becomes_duration_with_terminal_truth() {
    let root = export(&[
        evt(10, RuntimeEventKind::HandlerStarted),
        evt(12, RuntimeEventKind::HandlerPanicked),
    ]);
    let span = first_named(&root, "handler_turn");
    assert_eq!(span["ph"], "X");
    assert_eq!(span["args"]["terminal_kind"], "handler_panicked");
}

#[test]
fn unmatched_handler_begin_and_end_are_visible_instants() {
    let root = export(&[
        evt(1, RuntimeEventKind::HandlerPanicked),
        evt(2, RuntimeEventKind::HandlerStarted),
    ]);
    let panic = first_named(&root, "handler_panicked");
    assert_eq!(panic["ph"], "i");
    assert_eq!(panic["s"], "t");
    assert_eq!(panic["args"]["unmatched"], "missing_begin");
    let start = first_named(&root, "handler_started");
    assert_eq!(start["args"]["unmatched"], "missing_end");
}

#[test]
fn call_dispatch_completed_becomes_duration_with_kind_and_id() {
    let root = export(&[
        evt(
            2,
            RuntimeEventKind::CallDispatchAttempted {
                call_id: CallId::new(42),
                call_kind: CallKind::Sleep,
            },
        ),
        evt(
            8,
            RuntimeEventKind::CallCompleted {
                call_id: CallId::new(42),
                call_kind: CallKind::Sleep,
            },
        ),
    ]);
    let span = first_named(&root, "runtime_call");
    assert_eq!(span["ph"], "X");
    assert_eq!(span["dur"], 6);
    assert_eq!(span["args"]["call_id"], 42);
    assert_eq!(span["args"]["call_kind"], "sleep");
    assert_eq!(span["args"]["terminal_kind"], "call_completed");
}

#[test]
fn typed_call_reasons_stay_distinct() {
    let root = export(&[
        evt(
            1,
            RuntimeEventKind::CallDispatchAttempted {
                call_id: CallId::new(1),
                call_kind: CallKind::Sleep,
            },
        ),
        evt(
            2,
            RuntimeEventKind::CallFailed {
                call_id: CallId::new(1),
                call_kind: CallKind::Sleep,
                reason: CallError::Timeout,
            },
        ),
        evt(
            3,
            RuntimeEventKind::CallDispatchAttempted {
                call_id: CallId::new(2),
                call_kind: CallKind::IsolateCall,
            },
        ),
        evt(
            4,
            RuntimeEventKind::CallCancelled {
                call_id: CallId::new(2),
                cause: tina::CancelCause::CallerCancelled,
            },
        ),
        evt(
            5,
            RuntimeEventKind::CallDispatchAttempted {
                call_id: CallId::new(3),
                call_kind: CallKind::TcpRead,
            },
        ),
        evt(
            6,
            RuntimeEventKind::CallCompletionRejected {
                call_id: CallId::new(3),
                call_kind: CallKind::TcpRead,
                reason: CallCompletionRejectedReason::MailboxFull,
            },
        ),
        evt(
            7,
            RuntimeEventKind::CallRejected {
                call_id: CallId::new(4),
                reason: CallRejectedReason::UnsupportedMessage,
            },
        ),
        evt(
            8,
            RuntimeEventKind::CallReplyRejected {
                call_id: CallId::new(5),
                reason: CallReplyRejectedReason::CallerTimedOut,
            },
        ),
    ]);

    let reasons: Vec<&str> = trace_events(&root)
        .iter()
        .filter_map(|event| event["args"]["reason"].as_str())
        .collect();
    assert!(reasons.contains(&"Timeout"));
    assert!(reasons.contains(&"CallerCancelled"));
    assert!(reasons.contains(&"MailboxFull"));
    assert!(reasons.contains(&"UnsupportedMessage"));
    assert!(reasons.contains(&"CallerTimedOut"));
    assert!(!reasons.contains(&"error"));
}

#[test]
fn deferred_capture_terminals_keep_slot_and_call_ids() {
    let root = export(&[
        evt(
            1,
            RuntimeEventKind::DeferredReplyCaptured {
                slot_id: DeferredSlotId::new(7),
                call_id: CallId::new(70),
            },
        ),
        evt(
            3,
            RuntimeEventKind::DeferredReplyRejected {
                slot_id: DeferredSlotId::new(7),
                call_id: CallId::new(70),
                reason: DeferredReplyRejectedReason::CallerCancelled,
            },
        ),
        evt(
            4,
            RuntimeEventKind::DeferredReplySent {
                slot_id: DeferredSlotId::new(8),
                call_id: CallId::new(80),
            },
        ),
        evt(
            5,
            RuntimeEventKind::DeferredReplyDropped {
                slot_id: DeferredSlotId::new(9),
                call_id: CallId::new(90),
            },
        ),
    ]);
    let span = first_named(&root, "deferred_reply");
    assert_eq!(span["args"]["slot_id"], 7);
    assert_eq!(span["args"]["call_id"], 70);
    assert_eq!(span["args"]["reason"], "CallerCancelled");
    assert_eq!(
        first_named(&root, "deferred_reply_sent")["args"]["unmatched"],
        "missing_begin"
    );
    assert_eq!(
        first_named(&root, "deferred_reply_dropped")["args"]["unmatched"],
        "missing_begin"
    );
}

#[test]
fn child_lifecycle_and_restart_events_appear_with_child_fields() {
    let root = export(&[
        evt(
            1,
            RuntimeEventKind::ChildStarted {
                child_shard: ShardId::new(2),
                child_isolate: IsolateId::new(10),
                child_generation: AddressGeneration::new(1),
            },
        ),
        evt(
            2,
            RuntimeEventKind::RestartChildCompleted {
                child_ordinal: 3,
                old_isolate: IsolateId::new(10),
                old_generation: AddressGeneration::new(1),
                new_isolate: IsolateId::new(11),
                new_generation: AddressGeneration::new(2),
            },
        ),
        evt(
            3,
            RuntimeEventKind::ChildStopped {
                child_ordinal: 3,
                child_isolate: IsolateId::new(11),
                child_generation: AddressGeneration::new(2),
            },
        ),
        evt(
            4,
            RuntimeEventKind::SupervisorRestartRejected {
                failed_child: IsolateId::new(11),
                failed_ordinal: 3,
                reason: SupervisionRejectedReason::BudgetExceeded {
                    attempted_restart: 4,
                    max_restarts: 3,
                },
            },
        ),
    ]);
    assert_eq!(
        first_named(&root, "child_started")["args"]["child_shard"],
        2
    );
    assert_eq!(
        first_named(&root, "restart_child_completed")["args"]["new_generation"],
        2
    );
    assert_eq!(
        first_named(&root, "child_stopped")["args"]["child_ordinal"],
        3
    );
    assert_eq!(
        first_named(&root, "supervisor_restart_rejected")["args"]["reason"],
        "BudgetExceeded"
    );
    assert_eq!(
        first_named(&root, "supervisor_restart_rejected")["args"]["max_restarts"],
        3
    );
}

#[test]
fn remote_child_lifecycle_events_appear_with_remote_fields() {
    let root = export(&[
        evt(
            1,
            RuntimeEventKind::RemoteChildStopRequested {
                child_shard: ShardId::new(4),
                child_ordinal: 2,
                child_isolate: IsolateId::new(20),
                child_generation: AddressGeneration::new(3),
            },
        ),
        evt(
            2,
            RuntimeEventKind::RemoteChildStopped {
                child_shard: ShardId::new(4),
                child_ordinal: 2,
                child_isolate: IsolateId::new(20),
                child_generation: AddressGeneration::new(3),
            },
        ),
        evt(
            3,
            RuntimeEventKind::RemoteChildControlRejected {
                target_shard: ShardId::new(4),
                reason: SendRejectedReason::Full,
            },
        ),
        evt(
            4,
            RuntimeEventKind::RemoteChildControlPressure { capacity: 8 },
        ),
    ]);

    let requested = first_named(&root, "remote_child_stop_requested");
    assert_eq!(requested["cat"], "lifecycle");
    assert_eq!(requested["args"]["child_shard"], 4);
    assert_eq!(requested["args"]["child_ordinal"], 2);
    assert_eq!(requested["args"]["child_isolate"], 20);
    assert_eq!(requested["args"]["child_generation"], 3);

    let stopped = first_named(&root, "remote_child_stopped");
    assert_eq!(stopped["args"]["child_shard"], 4);
    assert_eq!(stopped["args"]["child_ordinal"], 2);
    assert_eq!(stopped["args"]["child_isolate"], 20);
    assert_eq!(stopped["args"]["child_generation"], 3);

    let rejected = first_named(&root, "remote_child_control_rejected");
    assert_eq!(rejected["args"]["target_shard"], 4);
    assert_eq!(rejected["args"]["reason"], "Full");

    let pressure = first_named(&root, "remote_child_control_pressure");
    assert_eq!(pressure["args"]["capacity"], 8);
}

#[test]
fn protocol_facts_appear_as_typed_instants_with_stable_tokens() {
    let cases = [
        (
            ProtocolFact::Http2StreamOpened {
                connection: ProtocolConnectionId::new(3),
                stream: Http2StreamId::new(1),
                direction: ProtocolDirection::Outbound,
            },
            "http2_stream_opened",
            "http2",
        ),
        (
            ProtocolFact::Http2StreamClosed {
                connection: ProtocolConnectionId::new(3),
                stream: Http2StreamId::new(1),
                reason: Http2CloseReason::EndStream,
            },
            "http2_stream_closed",
            "http2",
        ),
        (
            ProtocolFact::Http2StreamReset {
                connection: ProtocolConnectionId::new(3),
                stream: Http2StreamId::new(1),
                direction: ProtocolDirection::Inbound,
                reason: Http2ResetReason::Cancel,
            },
            "http2_stream_reset",
            "http2",
        ),
        (
            ProtocolFact::Http2FlowControlFull {
                connection: ProtocolConnectionId::new(3),
                stream: Http2StreamId::new(0),
                side: Http2FlowControlSide::ConnectionSend,
            },
            "http2_flow_control_full",
            "http2",
        ),
        (
            ProtocolFact::HttpBodyHighWater {
                connection: ProtocolConnectionId::new(3),
                body_id: 4,
                direction: ProtocolDirection::Inbound,
                buffered_bytes: 1024,
                threshold_bytes: 512,
            },
            "http_body_high_water",
            "http_body",
        ),
        (
            ProtocolFact::WebSocketSlowPeerClosed {
                session: WebSocketSessionId::new(5),
                queued_frames: 6,
                queued_bytes: 2048,
            },
            "websocket_slow_peer_closed",
            "websocket",
        ),
        (
            ProtocolFact::WebSocketSessionClosed {
                session: WebSocketSessionId::new(5),
                reason: WebSocketCloseReason::Normal,
                code: Some(1000),
            },
            "websocket_session_closed",
            "websocket",
        ),
        (
            ProtocolFact::GrpcFinalStatusSent {
                connection: ProtocolConnectionId::new(3),
                stream: GrpcStreamId::new(7),
                status: GrpcStatusCode::Ok,
            },
            "grpc_final_status_sent",
            "grpc",
        ),
        (
            ProtocolFact::GrpcFinalStatusReceived {
                connection: ProtocolConnectionId::new(3),
                stream: GrpcStreamId::new(7),
                status: GrpcStatusCode::Cancelled,
            },
            "grpc_final_status_received",
            "grpc",
        ),
    ];

    for (index, (protocol_fact, fact_name, protocol_family)) in cases.into_iter().enumerate() {
        let root = export(&[evt(
            index as u64 + 1,
            RuntimeEventKind::FactObserved {
                fact: RuntimeFact::Protocol(protocol_fact),
            },
        )]);
        let fact = first_named(&root, "fact_observed");
        assert_eq!(fact["ph"], "i");
        assert_eq!(fact["cat"], "protocol");
        assert_eq!(fact["args"]["fact_family"], "protocol");
        assert_eq!(fact["args"]["fact_name"], fact_name);
        assert_eq!(fact["args"]["protocol_family"], protocol_family);
        assert!(!fact["args"]["fact"].as_str().unwrap().is_empty());
    }
}

#[test]
fn duplicate_call_and_deferred_begins_do_not_drop_partial_truth() {
    let root = export(&[
        evt(
            1,
            RuntimeEventKind::CallDispatchAttempted {
                call_id: CallId::new(42),
                call_kind: CallKind::Sleep,
            },
        ),
        evt(
            2,
            RuntimeEventKind::CallDispatchAttempted {
                call_id: CallId::new(42),
                call_kind: CallKind::Sleep,
            },
        ),
        evt(
            3,
            RuntimeEventKind::CallCompleted {
                call_id: CallId::new(42),
                call_kind: CallKind::Sleep,
            },
        ),
        evt(
            4,
            RuntimeEventKind::DeferredReplyCaptured {
                slot_id: DeferredSlotId::new(7),
                call_id: CallId::new(70),
            },
        ),
        evt(
            5,
            RuntimeEventKind::DeferredReplyCaptured {
                slot_id: DeferredSlotId::new(7),
                call_id: CallId::new(70),
            },
        ),
        evt(
            6,
            RuntimeEventKind::DeferredReplySent {
                slot_id: DeferredSlotId::new(7),
                call_id: CallId::new(70),
            },
        ),
    ]);
    let duplicate_call_begin = trace_events(&root)
        .iter()
        .find(|event| {
            event["name"] == "call_dispatch_attempted"
                && event["args"]["unmatched"] == "missing_end"
        })
        .expect("first duplicate call begin remains visible");
    assert_eq!(duplicate_call_begin["args"]["event_id"], 1);
    assert_eq!(duplicate_call_begin["args"]["replaced_by_event_id"], 2);
    let call_spans: Vec<&Value> = trace_events(&root)
        .iter()
        .filter(|event| event["name"] == "runtime_call")
        .collect();
    assert_eq!(call_spans.len(), 1);
    assert_eq!(call_spans[0]["args"]["event_id"], 2);

    let duplicate_deferred_begin = trace_events(&root)
        .iter()
        .find(|event| {
            event["name"] == "deferred_reply_captured"
                && event["args"]["unmatched"] == "missing_end"
        })
        .expect("first duplicate deferred begin remains visible");
    assert_eq!(duplicate_deferred_begin["args"]["event_id"], 4);
    assert_eq!(duplicate_deferred_begin["args"]["replaced_by_event_id"], 5);
    let deferred_spans: Vec<&Value> = trace_events(&root)
        .iter()
        .filter(|event| event["name"] == "deferred_reply")
        .collect();
    assert_eq!(deferred_spans.len(), 1);
    assert_eq!(deferred_spans[0]["args"]["event_id"], 5);
}

#[test]
fn output_ordering_is_deterministic_and_sorted_by_logical_time() {
    let events = [
        evt(5, RuntimeEventKind::MailboxAccepted),
        evt(1, RuntimeEventKind::MessageAbandoned),
        evt(3, RuntimeEventKind::IsolateStopped),
    ];
    let first = export(&events);
    let second = export(&events);
    assert_eq!(first, second);
    let ids: Vec<u64> = trace_events(&first)
        .iter()
        .filter_map(|event| event["args"]["event_id"].as_u64())
        .collect();
    assert_eq!(ids, vec![1, 3, 5]);
}

#[test]
fn capacity_and_pressure_counters_export_only_supplied_or_observed_truth() {
    let mut capacity = CapacitySummary::new();
    capacity
        .push(CapacitySurfaceReport::count(
            "runtime.inbox",
            CapacityMode::Fixed,
            4,
            1,
            4,
            2,
        ))
        .unwrap();
    let events = [evt(
        1,
        RuntimeEventKind::SendRejected {
            target_shard: ShardId::new(1),
            target_isolate: IsolateId::new(9),
            target_generation: AddressGeneration::new(0),
            reason: SendRejectedReason::Full,
        },
    )];
    let timeline = TraceTimeline::from_events(&events)
        .with_capacity_summary(&capacity)
        .with_pressure_summary(PressureSummary {
            send_rejected_closed: 1,
            ..PressureSummary::default()
        })
        .finish();
    let root: Value =
        serde_json::from_str(&to_chrome_trace_json_string(&timeline).unwrap()).unwrap();
    assert_eq!(
        first_named(&root, "capacity_surface")["args"]["full_count"],
        2
    );
    assert_eq!(
        first_named(&root, "pressure_summary")["args"]["send_rejected_full"],
        1
    );
    assert_eq!(
        first_named(&root, "pressure_summary_supplied")["args"]["send_rejected_closed"],
        1
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(3)
    }
}

struct TestMailbox<T> {
    capacity: usize,
    queue: Rc<RefCell<VecDeque<T>>>,
    closed: Rc<Cell<bool>>,
}

impl<T> TestMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: Rc::new(RefCell::new(VecDeque::new())),
            closed: Rc::new(Cell::new(false)),
        }
    }
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
        Box::new(TestMailbox::new(capacity))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerMsg {
    ReplyNow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WorkerReply;

#[derive(Debug)]
struct Worker;

impl Isolate for Worker {
    tina::isolate_types! {
        message: WorkerMsg,
        reply: WorkerReply,
        send: Outbound<Infallible>,
        spawn: Infallible,
        call: Infallible,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        _msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        noop()
    }

    fn handle_call(&mut self, _msg: Self::Message, call: CallContext<'_, Self>) -> Effect<Self> {
        call.reply(WorkerReply)
    }
}

#[derive(Debug)]
enum DriverMsg {
    Start(Address<WorkerMsg, WorkerReply>),
    Returned(CallOutcome<WorkerReply>),
}

#[derive(Debug)]
struct Driver {
    outcomes: Rc<RefCell<Vec<CallOutcome<WorkerReply>>>>,
}

impl Isolate for Driver {
    tina::isolate_types! {
        message: DriverMsg,
        reply: (),
        send: Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<DriverMsg>,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            DriverMsg::Start(worker) => {
                call(worker, WorkerMsg::ReplyNow, Duration::from_millis(50))
                    .then(DriverMsg::Returned)
            }
            DriverMsg::Returned(outcome) => {
                self.outcomes.borrow_mut().push(outcome);
                noop()
            }
        }
    }
}

#[test]
fn user_shaped_runtime_trace_exports_names_users_need() {
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let worker = runtime.register(Worker, TestMailbox::new(8));
    let outcomes = Rc::new(RefCell::new(Vec::new()));
    let driver = runtime.register(
        Driver {
            outcomes: Rc::clone(&outcomes),
        },
        TestMailbox::new(8),
    );

    runtime.try_send(driver, DriverMsg::Start(worker)).unwrap();
    while outcomes.borrow().is_empty() {
        assert_eq!(runtime.step(), 1);
    }

    let mut events = runtime.trace().to_vec();
    events.push(evt(
        1_000,
        RuntimeEventKind::SendRejected {
            target_shard: ShardId::new(3),
            target_isolate: IsolateId::new(99),
            target_generation: AddressGeneration::new(0),
            reason: SendRejectedReason::Full,
        },
    ));
    events.push(evt(
        1_001,
        RuntimeEventKind::RestartChildCompleted {
            child_ordinal: 1,
            old_isolate: IsolateId::new(20),
            old_generation: AddressGeneration::new(0),
            new_isolate: IsolateId::new(21),
            new_generation: AddressGeneration::new(1),
        },
    ));

    let timeline = TraceTimeline::from_events(&events)
        .with_name("small tina workload")
        .finish();
    let path = std::env::temp_dir().join(format!(
        "tina-tracing-user-shaped-{}.trace.json",
        std::process::id()
    ));
    write_chrome_trace_json(&timeline, &path).unwrap();
    let root: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let names = names(&root);
    assert!(names.contains(&"handler_turn"));
    assert!(names.contains(&"runtime_call"));
    assert!(names.contains(&"send_rejected"));
    assert!(names.contains(&"pressure_summary"));
    assert!(names.contains(&"restart_child_completed"));
}
