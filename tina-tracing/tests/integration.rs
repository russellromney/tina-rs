//! Integration tests for `tina-tracing`.
//!
//! Uses a small in-test capturing subscriber so we can assert on the
//! exact (level, kind, fields) shape that `emit_event` produces.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use tina::{AddressGeneration, CallRejectedReason, IsolateId, RestartPolicy, ShardId};
use tina_runtime::{
    AffinityStatus, CallCompletionRejectedReason, CallError, CallId, CallKind,
    CallReplyRejectedReason, CauseId, DeferredReplyRejectedReason, DeferredSlotId, EffectKind,
    EventId, LiveShardState, RestartSkippedReason, RuntimeEvent, RuntimeEventKind,
    SendRejectedReason, SupervisionRejectedReason, TraceRetention,
};
use tina_tracing::RUNTIME_TRACE_TARGET;
use tina_tracing::events::{call_kind_name, emit_event, emit_events};
use tina_tracing::live::{affinity_status_name, shard_state_name};
use tracing::{
    Event, Level, Metadata, Subscriber,
    field::{Field, Visit},
    span::{Attributes, Id, Record},
};

#[derive(Debug, Clone)]
struct CapturedEvent {
    target: String,
    level: Level,
    fields: BTreeMap<String, String>,
}

impl CapturedEvent {
    fn field(&self, name: &str) -> Option<&str> {
        self.fields.get(name).map(String::as_str)
    }

    fn kind(&self) -> Option<&str> {
        self.field("kind")
    }
}

#[derive(Debug, Clone, Default)]
struct Capture {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

impl Capture {
    fn new() -> Self {
        Self::default()
    }

    fn events(&self) -> Vec<CapturedEvent> {
        self.events.lock().expect("capture lock poisoned").clone()
    }
}

impl Subscriber for Capture {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _attrs: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let metadata = event.metadata();
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        let captured = CapturedEvent {
            target: metadata.target().to_string(),
            level: *metadata.level(),
            fields: visitor.fields,
        };
        self.events
            .lock()
            .expect("capture lock poisoned")
            .push(captured);
    }

    fn enter(&self, _span: &Id) {}
    fn exit(&self, _span: &Id) {}
}

#[derive(Default)]
struct FieldVisitor {
    fields: BTreeMap<String, String>,
}

impl Visit for FieldVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.fields
            .insert(field.name().to_string(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }
}

fn with_capture<R>(f: impl FnOnce(&Capture) -> R) -> R {
    let capture = Capture::new();
    tracing::subscriber::with_default(capture.clone(), || f(&capture))
}

fn evt(id: u64, cause: Option<u64>, kind: RuntimeEventKind) -> RuntimeEvent {
    RuntimeEvent::new(
        EventId::new(id),
        cause.map(|c| CauseId::new(EventId::new(c))),
        ShardId::new(1),
        IsolateId::new(7),
        kind,
    )
}

// ---------------------------------------------------------------------------
// Lifecycle / handler / supervisor mapping (Rock 2)
// ---------------------------------------------------------------------------

#[test]
fn handler_lifecycle_emits_expected_levels_and_kinds() {
    let events = [
        evt(1, None, RuntimeEventKind::MailboxAccepted),
        evt(2, Some(1), RuntimeEventKind::HandlerStarted),
        evt(
            3,
            Some(2),
            RuntimeEventKind::HandlerFinished {
                effect: EffectKind::Reply,
            },
        ),
        evt(4, Some(3), RuntimeEventKind::IsolateStopped),
    ];
    let captured = with_capture(|cap| {
        emit_events(events.iter());
        cap.events()
    });
    assert_eq!(captured.len(), 4);

    let kinds: Vec<&str> = captured.iter().map(|e| e.kind().unwrap()).collect();
    assert_eq!(
        kinds,
        vec![
            "mailbox_accepted",
            "handler_started",
            "handler_finished",
            "isolate_stopped"
        ]
    );

    let levels: Vec<Level> = captured.iter().map(|e| e.level).collect();
    assert_eq!(
        levels,
        vec![Level::TRACE, Level::TRACE, Level::TRACE, Level::DEBUG]
    );

    let started = &captured[1];
    assert_eq!(started.target, RUNTIME_TRACE_TARGET);
    assert_eq!(started.field("event_id"), Some("2"));
    assert_eq!(started.field("shard"), Some("1"));
    assert_eq!(started.field("isolate"), Some("7"));
    assert_eq!(started.field("cause_id"), Some("1"));

    let finished = &captured[2];
    assert_eq!(finished.field("effect"), Some("reply"));

    let mailbox_accepted = &captured[0];
    assert_eq!(mailbox_accepted.field("cause_id"), Some("-"));
}

#[test]
fn handler_panic_is_error_level() {
    let captured = with_capture(|cap| {
        emit_event(&evt(1, None, RuntimeEventKind::HandlerPanicked));
        cap.events()
    });
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].level, Level::ERROR);
    assert_eq!(captured[0].kind(), Some("handler_panicked"));
}

#[test]
fn supervisor_restart_triggered_records_policy_and_child() {
    let captured = with_capture(|cap| {
        emit_event(&evt(
            5,
            Some(4),
            RuntimeEventKind::SupervisorRestartTriggered {
                policy: RestartPolicy::OneForAll,
                failed_child: IsolateId::new(99),
                failed_ordinal: 3,
            },
        ));
        cap.events()
    });
    assert_eq!(captured.len(), 1);
    let e = &captured[0];
    assert_eq!(e.level, Level::DEBUG);
    assert_eq!(e.kind(), Some("supervisor_restart_triggered"));
    assert_eq!(e.field("policy"), Some("OneForAll"));
    assert_eq!(e.field("failed_child"), Some("99"));
    assert_eq!(e.field("failed_ordinal"), Some("3"));
}

#[test]
fn supervisor_restart_rejected_distinguishes_budget_and_stopped() {
    let events = [
        evt(
            1,
            None,
            RuntimeEventKind::SupervisorRestartRejected {
                failed_child: IsolateId::new(2),
                failed_ordinal: 0,
                reason: SupervisionRejectedReason::BudgetExceeded {
                    attempted_restart: 4,
                    max_restarts: 3,
                },
            },
        ),
        evt(
            2,
            None,
            RuntimeEventKind::SupervisorRestartRejected {
                failed_child: IsolateId::new(2),
                failed_ordinal: 0,
                reason: SupervisionRejectedReason::SupervisorStopped,
            },
        ),
    ];
    let captured = with_capture(|cap| {
        emit_events(events.iter());
        cap.events()
    });
    assert_eq!(captured.len(), 2);
    assert_eq!(captured[0].level, Level::WARN);
    assert_eq!(captured[0].field("reason"), Some("BudgetExceeded"));
    assert_eq!(captured[0].field("attempted_restart"), Some("4"));
    assert_eq!(captured[0].field("max_restarts"), Some("3"));
    assert_eq!(captured[1].field("reason"), Some("SupervisorStopped"));
    assert_eq!(captured[1].fields.get("attempted_restart"), None);
}

#[test]
fn restart_child_lifecycle_carries_ordinal_and_generations() {
    let events = [
        evt(
            1,
            None,
            RuntimeEventKind::RestartChildAttempted {
                child_ordinal: 2,
                old_isolate: IsolateId::new(5),
                old_generation: AddressGeneration::new(1),
            },
        ),
        evt(
            2,
            Some(1),
            RuntimeEventKind::RestartChildSkipped {
                child_ordinal: 2,
                old_isolate: IsolateId::new(5),
                old_generation: AddressGeneration::new(1),
                reason: RestartSkippedReason::NotRestartable,
            },
        ),
        evt(
            3,
            Some(1),
            RuntimeEventKind::RestartChildCompleted {
                child_ordinal: 3,
                old_isolate: IsolateId::new(6),
                old_generation: AddressGeneration::new(1),
                new_isolate: IsolateId::new(7),
                new_generation: AddressGeneration::new(2),
            },
        ),
    ];
    let captured = with_capture(|cap| {
        emit_events(events.iter());
        cap.events()
    });
    assert_eq!(captured.len(), 3);
    assert_eq!(captured[0].kind(), Some("restart_child_attempted"));
    assert_eq!(captured[0].field("child_ordinal"), Some("2"));
    assert_eq!(captured[1].kind(), Some("restart_child_skipped"));
    assert_eq!(captured[1].field("reason"), Some("NotRestartable"));
    assert_eq!(captured[2].kind(), Some("restart_child_completed"));
    assert_eq!(captured[2].field("new_isolate"), Some("7"));
    assert_eq!(captured[2].field("new_generation"), Some("2"));
}

// ---------------------------------------------------------------------------
// Pressure mapping (Rock 3)
// ---------------------------------------------------------------------------

#[test]
fn send_rejected_full_and_closed_keep_distinct_reasons() {
    let events = [
        evt(
            1,
            None,
            RuntimeEventKind::SendRejected {
                target_shard: ShardId::new(2),
                target_isolate: IsolateId::new(3),
                target_generation: AddressGeneration::new(0),
                reason: SendRejectedReason::Full,
            },
        ),
        evt(
            2,
            None,
            RuntimeEventKind::SendRejected {
                target_shard: ShardId::new(2),
                target_isolate: IsolateId::new(3),
                target_generation: AddressGeneration::new(0),
                reason: SendRejectedReason::Closed,
            },
        ),
    ];
    let captured = with_capture(|cap| {
        emit_events(events.iter());
        cap.events()
    });
    assert_eq!(captured.len(), 2);
    assert!(captured.iter().all(|e| e.level == Level::WARN));
    assert_eq!(captured[0].kind(), Some("send_rejected"));
    assert_eq!(captured[0].field("reason"), Some("Full"));
    assert_eq!(captured[1].field("reason"), Some("Closed"));
    assert_eq!(captured[0].field("target_shard"), Some("2"));
    assert_eq!(captured[0].field("target_isolate"), Some("3"));
    assert_eq!(captured[0].field("target_generation"), Some("0"));
}

#[test]
fn call_completion_rejected_preserves_each_reason() {
    use CallCompletionRejectedReason::*;
    let cases = [
        (MailboxFull, "MailboxFull"),
        (RequesterClosed, "RequesterClosed"),
        (ResourceClosed, "ResourceClosed"),
    ];
    for (id, (reason, expected)) in cases.iter().enumerate() {
        let captured = with_capture(|cap| {
            emit_event(&evt(
                id as u64 + 1,
                None,
                RuntimeEventKind::CallCompletionRejected {
                    call_id: CallId::new(42),
                    call_kind: CallKind::Sleep,
                    reason: *reason,
                },
            ));
            cap.events()
        });
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].kind(), Some("call_completion_rejected"));
        assert_eq!(captured[0].level, Level::WARN);
        assert_eq!(captured[0].field("reason"), Some(*expected));
        assert_eq!(captured[0].field("call_id"), Some("42"));
        assert_eq!(captured[0].field("call_kind"), Some("sleep"));
    }
}

#[test]
fn call_reply_rejected_preserves_each_reason() {
    use CallReplyRejectedReason::*;
    let cases = [
        (NoPendingCall, "NoPendingCall"),
        (ReplyPathFull, "ReplyPathFull"),
        (RequesterShardClosed, "RequesterShardClosed"),
    ];
    for (id, (reason, expected)) in cases.iter().enumerate() {
        let captured = with_capture(|cap| {
            emit_event(&evt(
                id as u64 + 1,
                None,
                RuntimeEventKind::CallReplyRejected {
                    call_id: CallId::new(13),
                    reason: *reason,
                },
            ));
            cap.events()
        });
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].kind(), Some("call_reply_rejected"));
        assert_eq!(captured[0].level, Level::WARN);
        assert_eq!(captured[0].field("reason"), Some(*expected));
        assert_eq!(captured[0].field("call_id"), Some("13"));
    }
}

#[test]
fn deferred_reply_rejected_preserves_each_reason() {
    use DeferredReplyRejectedReason::*;
    let cases = [
        (CallerClosed, "CallerClosed"),
        (ReplyPathFull, "ReplyPathFull"),
        (RequesterShardClosed, "RequesterShardClosed"),
        (TypeMismatch, "TypeMismatch"),
    ];
    for (id, (reason, expected)) in cases.iter().enumerate() {
        let captured = with_capture(|cap| {
            emit_event(&evt(
                id as u64 + 1,
                None,
                RuntimeEventKind::DeferredReplyRejected {
                    slot_id: DeferredSlotId::new(7),
                    call_id: CallId::new(99),
                    reason: *reason,
                },
            ));
            cap.events()
        });
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].kind(), Some("deferred_reply_rejected"));
        assert_eq!(captured[0].level, Level::WARN);
        assert_eq!(captured[0].field("reason"), Some(*expected));
        assert_eq!(captured[0].field("slot_id"), Some("7"));
        assert_eq!(captured[0].field("call_id"), Some("99"));
    }
}

#[test]
fn deferred_reply_capture_send_drop_keep_correlation() {
    let events = [
        evt(
            1,
            None,
            RuntimeEventKind::DeferredReplyCaptured {
                slot_id: DeferredSlotId::new(3),
                call_id: CallId::new(5),
            },
        ),
        evt(
            2,
            Some(1),
            RuntimeEventKind::DeferredReplySent {
                slot_id: DeferredSlotId::new(3),
                call_id: CallId::new(5),
            },
        ),
        evt(
            3,
            Some(1),
            RuntimeEventKind::DeferredReplyDropped {
                slot_id: DeferredSlotId::new(4),
                call_id: CallId::new(6),
            },
        ),
    ];
    let captured = with_capture(|cap| {
        emit_events(events.iter());
        cap.events()
    });
    assert_eq!(captured.len(), 3);
    assert!(captured.iter().all(|e| e.level == Level::TRACE));
    let kinds: Vec<&str> = captured.iter().map(|e| e.kind().unwrap()).collect();
    assert_eq!(
        kinds,
        vec![
            "deferred_reply_captured",
            "deferred_reply_sent",
            "deferred_reply_dropped"
        ]
    );
    assert_eq!(captured[0].field("slot_id"), Some("3"));
    assert_eq!(captured[2].field("slot_id"), Some("4"));
}

// ---------------------------------------------------------------------------
// Call / resource correlation (Rock 4)
// ---------------------------------------------------------------------------

#[test]
fn call_failed_carries_call_kind_and_typed_error() {
    let captured = with_capture(|cap| {
        emit_event(&evt(
            1,
            None,
            RuntimeEventKind::CallFailed {
                call_id: CallId::new(11),
                call_kind: CallKind::TcpRead,
                reason: CallError::Timeout,
            },
        ));
        cap.events()
    });
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].level, Level::ERROR);
    assert_eq!(captured[0].kind(), Some("call_failed"));
    assert_eq!(captured[0].field("call_kind"), Some("tcp_read"));
    assert_eq!(captured[0].field("error"), Some("Timeout"));
    assert_eq!(captured[0].field("call_id"), Some("11"));
}

#[test]
fn every_call_kind_has_a_distinct_stable_name() {
    use CallKind::*;
    // List every CallKind variant. Adding a new variant in tina-runtime
    // forces this test to be updated, which is the contract.
    let all = [
        TcpBind,
        TcpAccept,
        TcpConnect,
        TcpRead,
        TcpWrite,
        TcpListenerClose,
        TcpStreamClose,
        UdpBind,
        UdpSendTo,
        UdpRecvFrom,
        UdpSocketClose,
        TlsConnect,
        TlsBind,
        TlsAccept,
        TlsListenerClose,
        TlsRead,
        TlsWrite,
        TlsClose,
        DnsLookup,
        SignalWait,
        ProcessRun,
        FileOpen,
        FileReadAt,
        FileWriteAt,
        FileFsync,
        FileSize,
        FileClose,
        Mkdir,
        PathMetadata,
        RenameReplace,
        RemoveFile,
        ReadDir,
        SyncParent,
        SnapshotCommit,
        SnapshotLoad,
        JournalAppend,
        JournalReplay,
        Sleep,
        ObservedSend,
        IsolateCall,
    ];
    let mut seen = std::collections::HashSet::new();
    for kind in all {
        let name = call_kind_name(kind);
        assert!(!name.is_empty(), "{kind:?} has empty name");
        assert!(seen.insert(name), "{name} appears twice");
    }
}

#[test]
fn journal_append_failed_carries_record_index_and_error() {
    let captured = with_capture(|cap| {
        emit_event(&evt(
            1,
            None,
            RuntimeEventKind::JournalAppendFailed {
                record_index: 42,
                reason: CallError::StorageFull,
            },
        ));
        cap.events()
    });
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].level, Level::WARN);
    assert_eq!(captured[0].kind(), Some("journal_append_failed"));
    assert_eq!(captured[0].field("record_index"), Some("42"));
    assert_eq!(captured[0].field("error"), Some("StorageFull"));
}

#[test]
fn snapshot_commit_failed_carries_typed_error() {
    let captured = with_capture(|cap| {
        emit_event(&evt(
            1,
            None,
            RuntimeEventKind::SnapshotCommitFailed {
                reason: CallError::CommitUncertain,
            },
        ));
        cap.events()
    });
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].field("error"), Some("CommitUncertain"));
}

#[test]
fn call_failed_rejected_error_preserves_inner_reason() {
    let captured = with_capture(|cap| {
        emit_event(&evt(
            1,
            None,
            RuntimeEventKind::CallFailed {
                call_id: CallId::new(7),
                call_kind: CallKind::IsolateCall,
                reason: CallError::Rejected(CallRejectedReason::ReplyAbandoned),
            },
        ));
        cap.events()
    });
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].kind(), Some("call_failed"));
    assert_eq!(captured[0].field("error"), Some("Rejected.ReplyAbandoned"));
}

#[test]
fn recovery_failed_is_error_level() {
    let captured = with_capture(|cap| {
        emit_event(&evt(
            1,
            None,
            RuntimeEventKind::RecoveryFailed {
                reason: CallError::CorruptRecord,
            },
        ));
        cap.events()
    });
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].level, Level::ERROR);
    assert_eq!(captured[0].field("error"), Some("CorruptRecord"));
}

#[test]
fn partial_marker_warns_with_missing_shards_listed() {
    let captured = with_capture(|cap| {
        tina_tracing::emit_partial_marker(&[ShardId::new(0), ShardId::new(2)]);
        cap.events()
    });
    assert_eq!(captured.len(), 1);
    let e = &captured[0];
    assert_eq!(e.target, RUNTIME_TRACE_TARGET);
    assert_eq!(e.level, Level::WARN);
    assert_eq!(e.kind(), Some("trace_snapshot_partial"));
    assert_eq!(e.field("missing_shards"), Some("[0, 2]"));
}

#[test]
fn partial_marker_with_empty_slice_still_emits_one_event() {
    // Document the edge: callers normally guard with `is_partial()`.
    // The marker itself does not silently no-op on an empty slice.
    let captured = with_capture(|cap| {
        tina_tracing::emit_partial_marker(&[]);
        cap.events()
    });
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].field("missing_shards"), Some("[]"));
}

#[test]
fn iteration_order_is_preserved() {
    let events: Vec<RuntimeEvent> = (1..=10)
        .map(|i| evt(i, None, RuntimeEventKind::MailboxAccepted))
        .collect();
    let captured = with_capture(|cap| {
        emit_events(events.iter());
        cap.events()
    });
    let ids: Vec<&str> = captured
        .iter()
        .map(|e| e.field("event_id").unwrap())
        .collect();
    assert_eq!(ids, vec!["1", "2", "3", "4", "5", "6", "7", "8", "9", "10"]);
}

#[test]
fn trace_retention_variants_are_referenced_for_doc_compile() {
    // Sanity check: the retention type is stable. The live emitter
    // formats it via Debug so the field is informational only.
    let _ = TraceRetention::Full;
    let _ = TraceRetention::Bounded(1024);
    let _ = TraceRetention::Off;
}

// ---------------------------------------------------------------------------
// Live snapshot helper-name mapping (Rock 5, lightweight)
// ---------------------------------------------------------------------------
//
// `LiveTopologyReport` constructors are crate-private in tina-runtime,
// so we build a real one via a running ThreadedRuntime and walk the
// emitter against it. The stable-name helpers also get unit-level
// coverage so renaming the runtime enums fails here loudly.

#[test]
fn shard_state_name_round_trips_each_state() {
    assert_eq!(shard_state_name(LiveShardState::Running), "Running");
    assert_eq!(shard_state_name(LiveShardState::Stopped), "Stopped");
    assert_eq!(shard_state_name(LiveShardState::Failed), "Failed");
}

#[test]
fn live_emit_snapshot_walks_a_real_running_topology() {
    use std::time::Duration;
    use tina::prelude::*;
    use tina_runtime::{DefaultThreadedMailboxFactory, ThreadedRuntime, ThreadedRuntimeConfig};

    let runtime = ThreadedRuntime::with_config(
        SingleShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig::default(),
    );
    // Give the worker a moment to register its thread id.
    std::thread::sleep(Duration::from_millis(20));
    let topology = runtime.topology();

    let captured = with_capture(|cap| {
        tina_tracing::live::emit_snapshot(&topology);
        cap.events()
    });
    let _ = runtime.shutdown();

    let shard_events: Vec<&CapturedEvent> = captured
        .iter()
        .filter(|e| e.kind() == Some("live_shard"))
        .collect();
    assert_eq!(
        shard_events.len(),
        1,
        "single-shard runtime emits one shard event"
    );
    let e = shard_events[0];
    assert_eq!(e.target, tina_tracing::LIVE_TOPOLOGY_TARGET);
    assert_eq!(e.level, Level::INFO);
    assert_eq!(e.field("state"), Some("Running"));
    assert_eq!(e.field("shard"), Some("0"));
    assert!(e.field("ingress_capacity").is_some());
    // Optional fields render via OptValue: bare value when present, "-" when absent.
    // configured_core was not set, so it must render as "-", not "None".
    assert_eq!(e.field("configured_core"), Some("-"));
    assert_eq!(e.field("observed_core"), Some("-"));
    // worker_name is set by ThreadedRuntime; render bare without quotes.
    assert!(
        e.field("worker_name")
            .map(|s| !s.starts_with("Some(") && s != "None")
            .unwrap_or(false),
        "worker_name renders bare, not as Some(...)/None — got {:?}",
        e.field("worker_name"),
    );
    // Single-shard runtime has no remote queues.
    assert!(
        captured
            .iter()
            .all(|e| e.kind() != Some("live_remote_queue")),
        "single-shard runtime has no remote queues",
    );
}

// ---------------------------------------------------------------------------
// Live observer wiring
// ---------------------------------------------------------------------------
//
// `tracing::subscriber::with_default` is thread-local. The
// ThreadedRuntime worker runs on its own OS thread, so the live
// observer's `tracing::Event`s fire there with no thread-local
// subscriber and fall back to the global default. The unit test
// installs a process-global capture once via OnceLock and asserts
// against it. Other tests in this file use thread-local `with_default`,
// which overrides the global on the test thread; they do not pollute
// the global capture.

static GLOBAL_CAPTURE: std::sync::OnceLock<Capture> = std::sync::OnceLock::new();

fn install_global_capture() -> &'static Capture {
    GLOBAL_CAPTURE.get_or_init(|| {
        let capture = Capture::new();
        // OK if another test binary already installed one; guard so
        // we do not panic on second initialisation in the same process.
        let _ = tracing::subscriber::set_global_default(capture.clone());
        capture
    })
}

#[test]
fn tracing_observer_emits_events_through_runtime_hook() {
    use std::convert::Infallible;
    use std::sync::Arc;
    use std::time::Duration;
    use tina::prelude::*;
    use tina_runtime::{
        DefaultThreadedMailboxFactory, ThreadedRuntime, ThreadedRuntimeConfig, sleep,
    };
    use tina_tracing::TracingObserver;

    #[derive(Debug, Clone)]
    enum Msg {
        Begin,
        Done,
    }

    struct Tiny;

    #[tina_runtime::isolate(message = Msg)]
    impl Tiny {
        fn handle(
            &mut self,
            msg: Msg,
            _ctx: &mut Context<'_, SingleShard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                Msg::Begin => sleep(Duration::ZERO).then(|_| Msg::Done),
                Msg::Done => stop(),
            }
        }
    }

    let capture = install_global_capture();
    let baseline = capture.events().len();

    let runtime = ThreadedRuntime::with_config_and_trace_observer(
        SingleShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig::default(),
        Arc::new(TracingObserver::new()),
    );
    let addr = runtime
        .register_with_capacity::<Tiny, Infallible>(Tiny, 4)
        .expect("register tiny");
    runtime.try_send(addr, Msg::Begin).expect("kick tiny");
    let _ = runtime.shutdown();

    let events = capture.events();
    let new_events = &events[baseline..];
    assert!(
        new_events
            .iter()
            .any(|e| e.kind() == Some("handler_started")),
        "live observer surfaced handler_started",
    );
    assert!(
        new_events
            .iter()
            .any(|e| e.kind() == Some("isolate_stopped")),
        "live observer surfaced isolate_stopped",
    );
}

#[test]
fn affinity_status_name_keeps_failed_distinct() {
    assert_eq!(
        affinity_status_name(&AffinityStatus::NotRequested),
        "NotRequested"
    );
    assert_eq!(affinity_status_name(&AffinityStatus::Applied), "Applied");
    assert_eq!(
        affinity_status_name(&AffinityStatus::Unsupported),
        "Unsupported"
    );
    assert_eq!(
        affinity_status_name(&AffinityStatus::Failed("nope".into())),
        "Failed"
    );
    assert_eq!(
        affinity_status_name(&AffinityStatus::AdvisoryOnly),
        "AdvisoryOnly"
    );
}
