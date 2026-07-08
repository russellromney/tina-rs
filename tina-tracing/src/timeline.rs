//! Offline Chrome Trace JSON export for Tina runtime traces.
//!
//! The timeline is only a visual view. [`RuntimeEvent`]
//! remains the canonical trace truth, and all timestamps in this module are
//! logical event ids, not wall-clock time.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io;
use std::path::Path;

use serde_json::{Map, Value, json};
use tina::capacity::CapacitySurfaceReport;
use tina::{AddressGeneration, IsolateId, ShardId};
use tina_runtime::{
    CallId, CapacitySummary, FairnessReport, LocalSystemShutdownReport, PressureSummary,
    ProtocolFact, ProtocolFamily, RuntimeEvent, RuntimeEventKind, RuntimeFact,
    ServiceShutdownReport, SupervisionRejectedReason, TraceSnapshot,
};

use crate::events::{
    call_completion_rejected_reason_name, call_error_name, call_kind_name,
    call_rejected_reason_name, call_reply_rejected_reason_name, cancel_cause_name,
    deferred_reply_rejected_reason_name, effect_kind_name, restart_skipped_reason_name,
    send_rejected_reason_name, terminal_completion_action_name,
};

const RUNTIME_PID: u64 = 0;
const RUNTIME_TID: u64 = 0;

/// Completed timeline ready for Chrome Trace JSON export.
#[derive(Debug, Clone)]
pub struct TraceTimeline {
    name: Option<String>,
    events: Vec<RuntimeEvent>,
    missing_shards: Vec<ShardId>,
    capacity_summary: Option<CapacitySummary>,
    pressure_summary: Option<PressureSummary>,
    local_shutdown_report: Option<LocalSystemShutdownReport>,
    service_shutdown_report: Option<ServiceShutdownReport>,
    fairness_report: Option<FairnessReport>,
}

impl TraceTimeline {
    /// Starts a timeline builder from a [`TraceSnapshot`].
    pub fn from_snapshot(snapshot: &TraceSnapshot) -> TraceTimelineBuilder<'_> {
        TraceTimelineBuilder {
            input: TraceTimelineInput::Snapshot(snapshot),
            name: None,
            capacity_summary: None,
            pressure_summary: None,
            local_shutdown_report: None,
            service_shutdown_report: None,
            fairness_report: None,
        }
    }

    /// Starts a timeline builder from a complete in-memory event slice.
    pub fn from_events(events: &[RuntimeEvent]) -> TraceTimelineBuilder<'_> {
        TraceTimelineBuilder {
            input: TraceTimelineInput::Events(events),
            name: None,
            capacity_summary: None,
            pressure_summary: None,
            local_shutdown_report: None,
            service_shutdown_report: None,
            fairness_report: None,
        }
    }
}

/// Offline input accepted by [`TraceTimelineBuilder`].
#[derive(Debug, Clone, Copy)]
pub enum TraceTimelineInput<'a> {
    /// Events plus missing-shard truth from a snapshot.
    Snapshot(&'a TraceSnapshot),
    /// A complete event slice.
    Events(&'a [RuntimeEvent]),
}

/// Options used while exporting a timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceTimelineOptions {
    /// Display time unit written to the Chrome Trace JSON root object.
    pub display_time_unit: &'static str,
    /// Timestamp mode. This phase always uses logical event ids.
    pub time_kind: &'static str,
}

impl Default for TraceTimelineOptions {
    fn default() -> Self {
        Self {
            display_time_unit: "us",
            time_kind: "logical_event_id",
        }
    }
}

/// Builder for a [`TraceTimeline`].
#[derive(Debug, Clone)]
pub struct TraceTimelineBuilder<'a> {
    input: TraceTimelineInput<'a>,
    name: Option<String>,
    capacity_summary: Option<CapacitySummary>,
    pressure_summary: Option<PressureSummary>,
    local_shutdown_report: Option<LocalSystemShutdownReport>,
    service_shutdown_report: Option<ServiceShutdownReport>,
    fairness_report: Option<FairnessReport>,
}

impl<'a> TraceTimelineBuilder<'a> {
    /// Sets a human-readable trace name for metadata.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Adds an explicit capacity summary report.
    pub fn with_capacity_summary(mut self, summary: &CapacitySummary) -> Self {
        self.capacity_summary = Some(summary.clone());
        self
    }

    /// Adds an explicit pressure summary report.
    pub fn with_pressure_summary(mut self, summary: PressureSummary) -> Self {
        self.pressure_summary = Some(summary);
        self
    }

    /// Adds an explicit local-system shutdown report.
    pub fn with_shutdown_report(mut self, report: &LocalSystemShutdownReport) -> Self {
        self.local_shutdown_report = Some(report.clone());
        self
    }

    /// Adds an explicit service-lifecycle shutdown report.
    pub fn with_service_shutdown_report(mut self, report: &ServiceShutdownReport) -> Self {
        self.service_shutdown_report = Some(report.clone());
        self
    }

    /// Adds an explicit fairness report.
    pub fn with_fairness_report(mut self, report: &FairnessReport) -> Self {
        self.fairness_report = Some(report.clone());
        self
    }

    /// Finishes the builder.
    pub fn finish(self) -> TraceTimeline {
        let (events, missing_shards) = match self.input {
            TraceTimelineInput::Snapshot(snapshot) => (
                snapshot.events().to_vec(),
                snapshot.missing_shards().to_vec(),
            ),
            TraceTimelineInput::Events(events) => (events.to_vec(), Vec::new()),
        };
        TraceTimeline {
            name: self.name,
            events,
            missing_shards,
            capacity_summary: self.capacity_summary,
            pressure_summary: self.pressure_summary,
            local_shutdown_report: self.local_shutdown_report,
            service_shutdown_report: self.service_shutdown_report,
            fairness_report: self.fairness_report,
        }
    }
}

/// Error returned while exporting timeline JSON.
#[derive(Debug)]
pub enum TimelineExportError {
    /// Filesystem write failed.
    Io(io::Error),
    /// JSON serialization failed.
    Json(serde_json::Error),
}

impl std::fmt::Display for TimelineExportError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "timeline export I/O failed: {error}"),
            Self::Json(error) => write!(formatter, "timeline export JSON failed: {error}"),
        }
    }
}

impl std::error::Error for TimelineExportError {}

impl From<io::Error> for TimelineExportError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for TimelineExportError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

/// Writes `timeline` to `path` as Chrome Trace Event JSON.
pub fn write_chrome_trace_json(
    timeline: &TraceTimeline,
    path: impl AsRef<Path>,
) -> Result<(), TimelineExportError> {
    let file = File::create(path)?;
    serde_json::to_writer_pretty(file, &chrome_trace_value(timeline))?;
    Ok(())
}

/// Returns `timeline` as pretty Chrome Trace Event JSON.
pub fn to_chrome_trace_json_string(
    timeline: &TraceTimeline,
) -> Result<String, TimelineExportError> {
    Ok(serde_json::to_string_pretty(&chrome_trace_value(timeline))?)
}

fn chrome_trace_value(timeline: &TraceTimeline) -> Value {
    let options = TraceTimelineOptions::default();
    json!({
        "displayTimeUnit": options.display_time_unit,
        "traceEvents": build_trace_events(timeline, &options),
    })
}

#[derive(Debug, Clone)]
struct EmittedEvent {
    ts: u64,
    event_id: u64,
    order: u64,
    value: Value,
}

#[derive(Debug, Clone, Copy)]
struct SpanStart {
    event: RuntimeEvent,
    order: u64,
}

fn build_trace_events(timeline: &TraceTimeline, options: &TraceTimelineOptions) -> Vec<Value> {
    let mut out = Vec::new();
    let mut order = 0_u64;
    push_metadata(timeline, options, &mut out, &mut order);
    push_runtime_events(timeline, &mut out, &mut order);
    push_reports(timeline, &mut out, &mut order);
    out.sort_by_key(|event| {
        let shard = event.value["args"]["shard"].as_u64().unwrap_or(0);
        (event.ts, shard, event.event_id, event.order)
    });
    out.into_iter().map(|event| event.value).collect()
}

fn push_metadata(
    timeline: &TraceTimeline,
    options: &TraceTimelineOptions,
    out: &mut Vec<EmittedEvent>,
    order: &mut u64,
) {
    let name = timeline.name.as_deref().unwrap_or("tina runtime trace");
    push_raw(
        out,
        order,
        0,
        0,
        chrome_event(
            "process_name",
            "metadata",
            "M",
            0,
            RUNTIME_PID,
            RUNTIME_TID,
            json!({
                "name": name,
                "time_kind": options.time_kind,
                "canonical_truth": "RuntimeEvent",
                "timeline_role": "visual_view",
                "complete": timeline.missing_shards.is_empty(),
                "missing_shards": shard_ids(&timeline.missing_shards),
                "capacity_summary_supplied": timeline.capacity_summary.is_some(),
                "pressure_summary_supplied": timeline.pressure_summary.is_some(),
                "local_shutdown_report_supplied": timeline.local_shutdown_report.is_some(),
                "service_shutdown_report_supplied": timeline.service_shutdown_report.is_some(),
                "fairness_report_supplied": timeline.fairness_report.is_some(),
            }),
        ),
    );

    let mut shards: Vec<ShardId> = timeline.events.iter().map(|event| event.shard()).collect();
    shards.extend(timeline.missing_shards.iter().copied());
    shards.sort();
    shards.dedup();
    for shard in shards {
        push_raw(
            out,
            order,
            0,
            0,
            chrome_event(
                "thread_name",
                "metadata",
                "M",
                0,
                RUNTIME_PID,
                shard_tid(shard),
                json!({
                    "name": format!("shard {}", shard.get()),
                    "shard": shard.get(),
                    "missing": timeline.missing_shards.contains(&shard),
                }),
            ),
        );
    }

    if !timeline.missing_shards.is_empty() {
        push_raw(
            out,
            order,
            0,
            0,
            chrome_event(
                "trace_snapshot_partial",
                "metadata",
                "M",
                0,
                RUNTIME_PID,
                RUNTIME_TID,
                json!({
                    "complete": false,
                    "missing_shards": shard_ids(&timeline.missing_shards),
                }),
            ),
        );
    }
}

fn push_runtime_events(timeline: &TraceTimeline, out: &mut Vec<EmittedEvent>, order: &mut u64) {
    let mut handler_starts: HashMap<(ShardId, IsolateId), Vec<SpanStart>> = HashMap::new();
    let mut call_starts: BTreeMap<CallId, SpanStart> = BTreeMap::new();
    let mut deferred_starts: BTreeMap<(ShardId, u64), SpanStart> = BTreeMap::new();
    let mut events = timeline.events.clone();
    events.sort_by_key(|event| (event.shard(), event.id()));

    for event in events {
        match event.kind() {
            RuntimeEventKind::HandlerStarted => {
                handler_starts
                    .entry((event.shard(), event.isolate()))
                    .or_default()
                    .push(SpanStart {
                        event,
                        order: *order,
                    });
                *order += 1;
            }
            RuntimeEventKind::HandlerFinished { .. }
            | RuntimeEventKind::HandlerPanicked
            | RuntimeEventKind::HandlerReportedFailure => {
                let key = (event.shard(), event.isolate());
                if let Some(start) = handler_starts.get_mut(&key).and_then(Vec::pop) {
                    push_handler_span(start, event, out, order);
                } else {
                    push_instant(event, Some("missing_begin"), out, order);
                }
            }
            RuntimeEventKind::CallDispatchAttempted { call_id, .. } => {
                if let Some(previous) = call_starts.insert(
                    call_id,
                    SpanStart {
                        event,
                        order: *order,
                    },
                ) {
                    push_unmatched_begin(
                        previous,
                        "runtime_call",
                        Some(event.id().get()),
                        out,
                        order,
                    );
                }
                *order += 1;
            }
            RuntimeEventKind::CallCompleted { call_id, .. }
            | RuntimeEventKind::CallFailed { call_id, .. }
            | RuntimeEventKind::CallCompletionRejected { call_id, .. }
            | RuntimeEventKind::CallCompletionAction { call_id, .. }
            | RuntimeEventKind::CallCancelled { call_id, .. } => {
                if let Some(start) = call_starts.remove(&call_id) {
                    push_call_span(start, event, out, order);
                } else {
                    push_instant(event, Some("missing_begin"), out, order);
                }
            }
            RuntimeEventKind::DeferredReplyCaptured { slot_id, .. } => {
                if let Some(previous) = deferred_starts.insert(
                    (event.shard(), slot_id.get()),
                    SpanStart {
                        event,
                        order: *order,
                    },
                ) {
                    push_unmatched_begin(
                        previous,
                        "deferred_reply",
                        Some(event.id().get()),
                        out,
                        order,
                    );
                }
                *order += 1;
            }
            RuntimeEventKind::DeferredReplySent { slot_id, .. }
            | RuntimeEventKind::DeferredReplyRejected { slot_id, .. }
            | RuntimeEventKind::DeferredReplyDropped { slot_id, .. } => {
                if let Some(start) = deferred_starts.remove(&(event.shard(), slot_id.get())) {
                    push_deferred_span(start, event, out, order);
                } else {
                    push_instant(event, Some("missing_begin"), out, order);
                }
            }
            RuntimeEventKind::MailboxAccepted
            | RuntimeEventKind::EffectObserved { .. }
            | RuntimeEventKind::SendDispatchAttempted { .. }
            | RuntimeEventKind::SendAccepted { .. }
            | RuntimeEventKind::SendRejected { .. }
            | RuntimeEventKind::Spawned { .. }
            | RuntimeEventKind::ChildStarted { .. }
            | RuntimeEventKind::SupervisorRestartTriggered { .. }
            | RuntimeEventKind::SupervisorRestartRejected { .. }
            | RuntimeEventKind::RestartChildAttempted { .. }
            | RuntimeEventKind::RestartChildSkipped { .. }
            | RuntimeEventKind::RestartChildCompleted { .. }
            | RuntimeEventKind::ChildStopped { .. }
            | RuntimeEventKind::RemoteChildStopRequested { .. }
            | RuntimeEventKind::RemoteChildStopped { .. }
            | RuntimeEventKind::RemoteChildControlRejected { .. }
            | RuntimeEventKind::RemoteChildControlPressure { .. }
            | RuntimeEventKind::IsolateStopped
            | RuntimeEventKind::MessageAbandoned
            | RuntimeEventKind::CallReplyRejected { .. }
            | RuntimeEventKind::CallReplyAbandoned { .. }
            | RuntimeEventKind::CallRejected { .. }
            | RuntimeEventKind::SnapshotCommitted
            | RuntimeEventKind::SnapshotCommitFailed { .. }
            | RuntimeEventKind::JournalAppended { .. }
            | RuntimeEventKind::JournalAppendFailed { .. }
            | RuntimeEventKind::RecoveryStarted
            | RuntimeEventKind::RecoveryFinished
            | RuntimeEventKind::RecoveryFailed { .. }
            // Mid-flight backpressure, not a terminal: the call still
            // completes, so this does not close the call span.
            | RuntimeEventKind::CallContinuationOverflowed { .. }
            | RuntimeEventKind::FactObserved { .. }
            // Driver accounting bug, not tied to any call span: a standalone
            // instant marker.
            | RuntimeEventKind::DriverCompletionQuarantined { .. } => {
                push_instant(event, None, out, order)
            }
        }
    }

    for starts in handler_starts.values() {
        for start in starts {
            push_unmatched_begin(*start, "handler_turn", None, out, order);
        }
    }
    for start in call_starts.values() {
        push_unmatched_begin(*start, "runtime_call", None, out, order);
    }
    for start in deferred_starts.values() {
        push_unmatched_begin(*start, "deferred_reply", None, out, order);
    }
}

fn push_handler_span(
    start: SpanStart,
    end: RuntimeEvent,
    out: &mut Vec<EmittedEvent>,
    order: &mut u64,
) {
    let mut args = base_args(&start.event);
    insert_terminal_args(&mut args, &end);
    insert_end_args(&mut args, &end);
    push_raw(
        out,
        order,
        start.event.id().get(),
        start.event.id().get(),
        chrome_event(
            "handler_turn",
            "handler",
            "X",
            start.event.id().get(),
            RUNTIME_PID,
            shard_tid(start.event.shard()),
            Value::Object(args),
        )
        .with_dur(duration(start.event, end)),
    );
}

fn push_call_span(
    start: SpanStart,
    end: RuntimeEvent,
    out: &mut Vec<EmittedEvent>,
    order: &mut u64,
) {
    let mut args = base_args(&start.event);
    insert_terminal_args(&mut args, &end);
    insert_end_args(&mut args, &end);
    push_raw(
        out,
        order,
        start.event.id().get(),
        start.event.id().get(),
        chrome_event(
            "runtime_call",
            "call",
            "X",
            start.event.id().get(),
            RUNTIME_PID,
            shard_tid(start.event.shard()),
            Value::Object(args),
        )
        .with_dur(duration(start.event, end)),
    );
}

fn push_deferred_span(
    start: SpanStart,
    end: RuntimeEvent,
    out: &mut Vec<EmittedEvent>,
    order: &mut u64,
) {
    let mut args = base_args(&start.event);
    insert_terminal_args(&mut args, &end);
    insert_end_args(&mut args, &end);
    push_raw(
        out,
        order,
        start.event.id().get(),
        start.event.id().get(),
        chrome_event(
            "deferred_reply",
            "deferred",
            "X",
            start.event.id().get(),
            RUNTIME_PID,
            shard_tid(start.event.shard()),
            Value::Object(args),
        )
        .with_dur(duration(start.event, end)),
    );
}

fn push_unmatched_begin(
    start: SpanStart,
    span_name: &'static str,
    replaced_by_event_id: Option<u64>,
    out: &mut Vec<EmittedEvent>,
    order: &mut u64,
) {
    let mut args = base_args(&start.event);
    args.insert("span_name".into(), json!(span_name));
    args.insert("unmatched".into(), json!("missing_end"));
    if let Some(event_id) = replaced_by_event_id {
        args.insert("replaced_by_event_id".into(), json!(event_id));
    }
    push_raw(
        out,
        order,
        start.event.id().get(),
        start.order,
        chrome_event(
            event_name(start.event.kind()),
            event_category(start.event.kind()),
            "i",
            start.event.id().get(),
            RUNTIME_PID,
            shard_tid(start.event.shard()),
            Value::Object(args),
        ),
    );
}

fn push_instant(
    event: RuntimeEvent,
    unmatched: Option<&'static str>,
    out: &mut Vec<EmittedEvent>,
    order: &mut u64,
) {
    let mut args = base_args(&event);
    if let Some(unmatched) = unmatched {
        args.insert("unmatched".into(), json!(unmatched));
    }
    push_raw(
        out,
        order,
        event.id().get(),
        event.id().get(),
        chrome_event(
            event_name(event.kind()),
            event_category(event.kind()),
            "i",
            event.id().get(),
            RUNTIME_PID,
            shard_tid(event.shard()),
            Value::Object(args),
        ),
    );
}

fn push_reports(timeline: &TraceTimeline, out: &mut Vec<EmittedEvent>, order: &mut u64) {
    if let Some(summary) = &timeline.capacity_summary {
        for report in summary.reports() {
            push_capacity_counter(report, out, order);
        }
    }

    let trace_pressure = PressureSummary::from_events(timeline.events.iter());
    if trace_pressure.non_zero() {
        push_pressure_counter("pressure_summary", trace_pressure, out, order);
    }
    if let Some(summary) = timeline.pressure_summary {
        push_pressure_counter("pressure_summary_supplied", summary, out, order);
    }

    if let Some(report) = &timeline.local_shutdown_report {
        push_raw(
            out,
            order,
            0,
            u64::MAX - 2,
            chrome_event(
                "local_shutdown_report",
                "shutdown",
                "i",
                0,
                RUNTIME_PID,
                RUNTIME_TID,
                json!({
                    "final_state": format!("{:?}", report.final_state()),
                    "clean": report.clean(),
                    "canceled_count": report.canceled_count(),
                    "tombstoned_count": report.tombstoned_count(),
                    "rejected_after_drain_count": report.rejected_after_drain_count(),
                    "failed_shards": shard_ids(report.failed_shards()),
                    "remaining_owned_resource_count": report.remaining_owned_resource_count(),
                    "remaining_worker_held_resource_count": report.remaining_worker_held_resource_count(),
                    "remaining_pending_driver_call_count": report.remaining_pending_driver_call_count(),
                    "unclean_reasons": report.unclean_reasons().iter().map(|reason| format!("{reason:?}")).collect::<Vec<_>>(),
                }),
            ),
        );
    }
    if let Some(report) = &timeline.service_shutdown_report {
        push_raw(
            out,
            order,
            0,
            u64::MAX - 1,
            chrome_event(
                "service_shutdown_report",
                "shutdown",
                "i",
                0,
                RUNTIME_PID,
                RUNTIME_TID,
                json!({
                    "service": report.service,
                    "clean": report.clean,
                    "steps": report.steps.len(),
                    "closes": report.closes.len(),
                }),
            ),
        );
    }
    if let Some(report) = &timeline.fairness_report {
        for progress in &report.isolates {
            push_raw(
                out,
                order,
                0,
                u64::MAX,
                chrome_event(
                    "fairness_progress",
                    "fairness",
                    "C",
                    0,
                    RUNTIME_PID,
                    RUNTIME_TID,
                    json!({
                        "isolate": progress.isolate.get(),
                        "handler_turns": progress.handler_turns,
                        "sleep_completions": progress.sleep_completions,
                    }),
                ),
            );
        }
    }
}

fn push_capacity_counter(
    report: &CapacitySurfaceReport,
    out: &mut Vec<EmittedEvent>,
    order: &mut u64,
) {
    let mut args = Map::new();
    args.insert("surface".into(), json!(report.name));
    args.insert("mode".into(), json!(report.mode.label()));
    args.insert("current".into(), json!(report.current_messages));
    args.insert("high_water".into(), json!(report.high_water_messages));
    args.insert("full_count".into(), json!(report.full_count));
    insert_option_usize(&mut args, "max", report.max_messages);
    insert_option_usize(&mut args, "max_weight", report.max_weight);
    insert_option_usize(&mut args, "current_weight", report.current_weight);
    insert_option_usize(&mut args, "high_water_weight", report.high_water_weight);
    args.insert("weight_full_count".into(), json!(report.weight_full_count));
    if let Some(unit) = &report.weight_unit {
        args.insert("weight_unit".into(), json!(unit));
    }
    if let Some(scope) = &report.shared_scope {
        args.insert("shared_scope".into(), json!(scope));
    }
    insert_option_usize(&mut args, "shared_max_weight", report.shared_max_weight);
    insert_option_usize(
        &mut args,
        "shared_current_weight",
        report.shared_current_weight,
    );
    insert_option_usize(
        &mut args,
        "shared_high_water_weight",
        report.shared_high_water_weight,
    );
    args.insert(
        "shared_weight_full_count".into(),
        json!(report.shared_weight_full_count),
    );

    push_raw(
        out,
        order,
        0,
        u64::MAX - 3,
        chrome_event(
            "capacity_surface",
            "capacity",
            "C",
            0,
            RUNTIME_PID,
            RUNTIME_TID,
            Value::Object(args),
        ),
    );
}

fn push_pressure_counter(
    name: &'static str,
    summary: PressureSummary,
    out: &mut Vec<EmittedEvent>,
    order: &mut u64,
) {
    push_raw(
        out,
        order,
        0,
        u64::MAX - 4,
        chrome_event(
            name,
            "pressure",
            "C",
            0,
            RUNTIME_PID,
            RUNTIME_TID,
            json!({
                "completion_rejected_mailbox_full": summary.completion_rejected_mailbox_full,
                "completion_rejected_requester_closed": summary.completion_rejected_requester_closed,
                "completion_rejected_resource_closed": summary.completion_rejected_resource_closed,
                "reply_rejected_no_pending_call": summary.reply_rejected_no_pending_call,
                "reply_rejected_caller_cancelled": summary.reply_rejected_caller_cancelled,
                "reply_rejected_caller_timed_out": summary.reply_rejected_caller_timed_out,
                "reply_rejected_owner_stopped": summary.reply_rejected_owner_stopped,
                "reply_rejected_runtime_stopped": summary.reply_rejected_runtime_stopped,
                "reply_rejected_reply_path_full": summary.reply_rejected_reply_path_full,
                "reply_rejected_requester_shard_closed": summary.reply_rejected_requester_shard_closed,
                "send_rejected_full": summary.send_rejected_full,
                "send_rejected_closed": summary.send_rejected_closed,
            }),
        ),
    );
}

fn base_args(event: &RuntimeEvent) -> Map<String, Value> {
    let mut args = Map::new();
    args.insert("event_id".into(), json!(event.id().get()));
    if let Some(cause) = event.cause() {
        args.insert("cause_id".into(), json!(cause.event().get()));
    }
    args.insert("shard".into(), json!(event.shard().get()));
    args.insert("isolate".into(), json!(event.isolate().get()));
    insert_kind_args(&mut args, event.kind());
    args
}

fn insert_end_args(args: &mut Map<String, Value>, end: &RuntimeEvent) {
    args.insert("end_event_id".into(), json!(end.id().get()));
    args.insert("terminal_kind".into(), json!(event_name(end.kind())));
    if let Some(cause) = end.cause() {
        args.insert("end_cause_id".into(), json!(cause.event().get()));
    }
}

fn insert_terminal_args(args: &mut Map<String, Value>, event: &RuntimeEvent) {
    insert_kind_args(args, event.kind());
}

fn insert_kind_args(args: &mut Map<String, Value>, kind: RuntimeEventKind) {
    args.insert("kind".into(), json!(event_name(kind)));
    match kind {
        RuntimeEventKind::MailboxAccepted
        | RuntimeEventKind::HandlerStarted
        | RuntimeEventKind::HandlerPanicked
        | RuntimeEventKind::HandlerReportedFailure
        | RuntimeEventKind::IsolateStopped
        | RuntimeEventKind::MessageAbandoned
        | RuntimeEventKind::SnapshotCommitted
        | RuntimeEventKind::RecoveryStarted
        | RuntimeEventKind::RecoveryFinished => {}
        RuntimeEventKind::HandlerFinished { effect }
        | RuntimeEventKind::EffectObserved { effect } => {
            args.insert("effect".into(), json!(effect_kind_name(effect)));
        }
        RuntimeEventKind::SendDispatchAttempted {
            target_shard,
            target_isolate,
            target_generation,
        }
        | RuntimeEventKind::SendAccepted {
            target_shard,
            target_isolate,
            target_generation,
        } => insert_target_args(args, target_shard, target_isolate, target_generation),
        RuntimeEventKind::SendRejected {
            target_shard,
            target_isolate,
            target_generation,
            reason,
        } => {
            insert_target_args(args, target_shard, target_isolate, target_generation);
            args.insert("reason".into(), json!(send_rejected_reason_name(reason)));
        }
        RuntimeEventKind::Spawned { child_isolate } => {
            args.insert("child_isolate".into(), json!(child_isolate.get()));
        }
        RuntimeEventKind::ChildStarted {
            child_shard,
            child_isolate,
            child_generation,
        } => {
            args.insert("child_shard".into(), json!(child_shard.get()));
            args.insert("child_isolate".into(), json!(child_isolate.get()));
            args.insert("child_generation".into(), json!(child_generation.get()));
        }
        RuntimeEventKind::SupervisorRestartTriggered {
            policy,
            failed_child,
            failed_ordinal,
        } => {
            args.insert("policy".into(), json!(format!("{policy:?}")));
            args.insert("failed_child".into(), json!(failed_child.get()));
            args.insert("failed_ordinal".into(), json!(failed_ordinal));
        }
        RuntimeEventKind::SupervisorRestartRejected {
            failed_child,
            failed_ordinal,
            reason,
        } => {
            args.insert("failed_child".into(), json!(failed_child.get()));
            args.insert("failed_ordinal".into(), json!(failed_ordinal));
            match reason {
                SupervisionRejectedReason::BudgetExceeded {
                    attempted_restart,
                    max_restarts,
                } => {
                    args.insert("reason".into(), json!("BudgetExceeded"));
                    args.insert("attempted_restart".into(), json!(attempted_restart));
                    args.insert("max_restarts".into(), json!(max_restarts));
                }
                SupervisionRejectedReason::SupervisorStopped => {
                    args.insert("reason".into(), json!("SupervisorStopped"));
                }
            }
        }
        RuntimeEventKind::RestartChildAttempted {
            child_ordinal,
            old_isolate,
            old_generation,
        }
        | RuntimeEventKind::RestartChildSkipped {
            child_ordinal,
            old_isolate,
            old_generation,
            ..
        } => {
            args.insert("child_ordinal".into(), json!(child_ordinal));
            args.insert("old_isolate".into(), json!(old_isolate.get()));
            args.insert("old_generation".into(), json!(old_generation.get()));
            if let RuntimeEventKind::RestartChildSkipped { reason, .. } = kind {
                args.insert("reason".into(), json!(restart_skipped_reason_name(reason)));
            }
        }
        RuntimeEventKind::RestartChildCompleted {
            child_ordinal,
            old_isolate,
            old_generation,
            new_isolate,
            new_generation,
        } => {
            args.insert("child_ordinal".into(), json!(child_ordinal));
            args.insert("old_isolate".into(), json!(old_isolate.get()));
            args.insert("old_generation".into(), json!(old_generation.get()));
            args.insert("new_isolate".into(), json!(new_isolate.get()));
            args.insert("new_generation".into(), json!(new_generation.get()));
        }
        RuntimeEventKind::ChildStopped {
            child_ordinal,
            child_isolate,
            child_generation,
        } => {
            args.insert("child_ordinal".into(), json!(child_ordinal));
            args.insert("child_isolate".into(), json!(child_isolate.get()));
            args.insert("child_generation".into(), json!(child_generation.get()));
        }
        RuntimeEventKind::RemoteChildStopRequested {
            child_shard,
            child_ordinal,
            child_isolate,
            child_generation,
        }
        | RuntimeEventKind::RemoteChildStopped {
            child_shard,
            child_ordinal,
            child_isolate,
            child_generation,
        } => {
            args.insert("child_shard".into(), json!(child_shard.get()));
            args.insert("child_ordinal".into(), json!(child_ordinal));
            args.insert("child_isolate".into(), json!(child_isolate.get()));
            args.insert("child_generation".into(), json!(child_generation.get()));
        }
        RuntimeEventKind::RemoteChildControlRejected {
            target_shard,
            reason,
        } => {
            args.insert("target_shard".into(), json!(target_shard.get()));
            args.insert("reason".into(), json!(send_rejected_reason_name(reason)));
        }
        RuntimeEventKind::RemoteChildControlPressure { capacity } => {
            args.insert("capacity".into(), json!(capacity));
        }
        RuntimeEventKind::CallDispatchAttempted { call_id, call_kind }
        | RuntimeEventKind::CallCompleted { call_id, call_kind }
        | RuntimeEventKind::CallContinuationOverflowed { call_id, call_kind } => {
            insert_call_args(args, call_id, call_kind);
        }
        RuntimeEventKind::CallFailed {
            call_id,
            call_kind,
            reason,
        } => {
            insert_call_args(args, call_id, call_kind);
            args.insert("reason".into(), json!(call_error_name(reason)));
        }
        RuntimeEventKind::CallCompletionRejected {
            call_id,
            call_kind,
            reason,
        } => {
            insert_call_args(args, call_id, call_kind);
            args.insert(
                "reason".into(),
                json!(call_completion_rejected_reason_name(reason)),
            );
        }
        RuntimeEventKind::CallCompletionAction {
            call_id,
            call_kind,
            action,
        } => {
            insert_call_args(args, call_id, call_kind);
            args.insert(
                "action".into(),
                json!(terminal_completion_action_name(action)),
            );
        }
        RuntimeEventKind::CallReplyRejected { call_id, reason } => {
            args.insert("call_id".into(), json!(call_id.get()));
            args.insert(
                "reason".into(),
                json!(call_reply_rejected_reason_name(reason)),
            );
        }
        RuntimeEventKind::CallReplyAbandoned { call_id } => {
            args.insert("call_id".into(), json!(call_id.get()));
        }
        RuntimeEventKind::CallRejected { call_id, reason } => {
            args.insert("call_id".into(), json!(call_id.get()));
            args.insert("reason".into(), json!(call_rejected_reason_name(reason)));
        }
        RuntimeEventKind::CallCancelled { call_id, cause } => {
            args.insert("call_id".into(), json!(call_id.get()));
            args.insert("reason".into(), json!(cancel_cause_name(cause)));
        }
        RuntimeEventKind::SnapshotCommitFailed { reason }
        | RuntimeEventKind::RecoveryFailed { reason } => {
            args.insert("reason".into(), json!(call_error_name(reason)));
        }
        RuntimeEventKind::JournalAppended { record_index } => {
            args.insert("record_index".into(), json!(record_index));
        }
        RuntimeEventKind::JournalAppendFailed {
            record_index,
            reason,
        } => {
            args.insert("record_index".into(), json!(record_index));
            args.insert("reason".into(), json!(call_error_name(reason)));
        }
        RuntimeEventKind::DeferredReplyCaptured { slot_id, call_id }
        | RuntimeEventKind::DeferredReplySent { slot_id, call_id }
        | RuntimeEventKind::DeferredReplyDropped { slot_id, call_id } => {
            args.insert("slot_id".into(), json!(slot_id.get()));
            args.insert("call_id".into(), json!(call_id.get()));
        }
        RuntimeEventKind::DeferredReplyRejected {
            slot_id,
            call_id,
            reason,
        } => {
            args.insert("slot_id".into(), json!(slot_id.get()));
            args.insert("call_id".into(), json!(call_id.get()));
            args.insert(
                "reason".into(),
                json!(deferred_reply_rejected_reason_name(reason)),
            );
        }
        RuntimeEventKind::FactObserved { fact } => {
            args.insert("fact".into(), json!(format!("{fact:?}")));
            args.insert("fact_family".into(), json!(fact_family_name(fact)));
            args.insert("fact_name".into(), json!(fact_name(fact)));
            if let RuntimeFact::Protocol(protocol) = fact {
                args.insert(
                    "protocol_family".into(),
                    json!(protocol_family_name(protocol.family())),
                );
            }
        }
        RuntimeEventKind::DriverCompletionQuarantined { call_id } => {
            args.insert("call_id".into(), json!(call_id.get()));
        }
    }
}

fn event_name(kind: RuntimeEventKind) -> &'static str {
    match kind {
        RuntimeEventKind::MailboxAccepted => "mailbox_accepted",
        RuntimeEventKind::HandlerStarted => "handler_started",
        RuntimeEventKind::HandlerPanicked => "handler_panicked",
        RuntimeEventKind::HandlerReportedFailure => "handler_reported_failure",
        RuntimeEventKind::HandlerFinished { .. } => "handler_finished",
        RuntimeEventKind::EffectObserved { .. } => "effect_observed",
        RuntimeEventKind::SendDispatchAttempted { .. } => "send_dispatch_attempted",
        RuntimeEventKind::SendAccepted { .. } => "send_accepted",
        RuntimeEventKind::SendRejected { .. } => "send_rejected",
        RuntimeEventKind::Spawned { .. } => "spawned",
        RuntimeEventKind::ChildStarted { .. } => "child_started",
        RuntimeEventKind::SupervisorRestartTriggered { .. } => "supervisor_restart_triggered",
        RuntimeEventKind::SupervisorRestartRejected { .. } => "supervisor_restart_rejected",
        RuntimeEventKind::RestartChildAttempted { .. } => "restart_child_attempted",
        RuntimeEventKind::RestartChildSkipped { .. } => "restart_child_skipped",
        RuntimeEventKind::RestartChildCompleted { .. } => "restart_child_completed",
        RuntimeEventKind::ChildStopped { .. } => "child_stopped",
        RuntimeEventKind::RemoteChildStopRequested { .. } => "remote_child_stop_requested",
        RuntimeEventKind::RemoteChildStopped { .. } => "remote_child_stopped",
        RuntimeEventKind::RemoteChildControlRejected { .. } => "remote_child_control_rejected",
        RuntimeEventKind::RemoteChildControlPressure { .. } => "remote_child_control_pressure",
        RuntimeEventKind::IsolateStopped => "isolate_stopped",
        RuntimeEventKind::MessageAbandoned => "message_abandoned",
        RuntimeEventKind::CallDispatchAttempted { .. } => "call_dispatch_attempted",
        RuntimeEventKind::CallCompleted { .. } => "call_completed",
        RuntimeEventKind::CallContinuationOverflowed { .. } => "call_continuation_overflowed",
        RuntimeEventKind::CallFailed { .. } => "call_failed",
        RuntimeEventKind::CallCompletionRejected { .. } => "call_completion_rejected",
        RuntimeEventKind::CallCompletionAction { .. } => "call_completion_action",
        RuntimeEventKind::CallReplyRejected { .. } => "call_reply_rejected",
        RuntimeEventKind::CallReplyAbandoned { .. } => "call_reply_abandoned",
        RuntimeEventKind::CallRejected { .. } => "call_rejected",
        RuntimeEventKind::CallCancelled { .. } => "call_cancelled",
        RuntimeEventKind::SnapshotCommitted => "snapshot_committed",
        RuntimeEventKind::SnapshotCommitFailed { .. } => "snapshot_commit_failed",
        RuntimeEventKind::JournalAppended { .. } => "journal_appended",
        RuntimeEventKind::JournalAppendFailed { .. } => "journal_append_failed",
        RuntimeEventKind::RecoveryStarted => "recovery_started",
        RuntimeEventKind::RecoveryFinished => "recovery_finished",
        RuntimeEventKind::RecoveryFailed { .. } => "recovery_failed",
        RuntimeEventKind::DeferredReplyCaptured { .. } => "deferred_reply_captured",
        RuntimeEventKind::DeferredReplySent { .. } => "deferred_reply_sent",
        RuntimeEventKind::DeferredReplyRejected { .. } => "deferred_reply_rejected",
        RuntimeEventKind::DeferredReplyDropped { .. } => "deferred_reply_dropped",
        RuntimeEventKind::FactObserved { .. } => "fact_observed",
        RuntimeEventKind::DriverCompletionQuarantined { .. } => "driver_completion_quarantined",
    }
}

fn event_category(kind: RuntimeEventKind) -> &'static str {
    match kind {
        RuntimeEventKind::HandlerStarted
        | RuntimeEventKind::HandlerPanicked
        | RuntimeEventKind::HandlerReportedFailure
        | RuntimeEventKind::HandlerFinished { .. } => "handler",
        RuntimeEventKind::CallDispatchAttempted { .. }
        | RuntimeEventKind::CallCompleted { .. }
        | RuntimeEventKind::CallContinuationOverflowed { .. }
        | RuntimeEventKind::CallFailed { .. }
        | RuntimeEventKind::CallCompletionRejected { .. }
        | RuntimeEventKind::CallCompletionAction { .. }
        | RuntimeEventKind::CallReplyRejected { .. }
        | RuntimeEventKind::CallReplyAbandoned { .. }
        | RuntimeEventKind::CallRejected { .. }
        | RuntimeEventKind::CallCancelled { .. } => "call",
        RuntimeEventKind::SendDispatchAttempted { .. }
        | RuntimeEventKind::SendAccepted { .. }
        | RuntimeEventKind::SendRejected { .. } => "send",
        RuntimeEventKind::Spawned { .. }
        | RuntimeEventKind::ChildStarted { .. }
        | RuntimeEventKind::SupervisorRestartTriggered { .. }
        | RuntimeEventKind::SupervisorRestartRejected { .. }
        | RuntimeEventKind::RestartChildAttempted { .. }
        | RuntimeEventKind::RestartChildSkipped { .. }
        | RuntimeEventKind::RestartChildCompleted { .. }
        | RuntimeEventKind::ChildStopped { .. }
        | RuntimeEventKind::RemoteChildStopRequested { .. }
        | RuntimeEventKind::RemoteChildStopped { .. }
        | RuntimeEventKind::RemoteChildControlRejected { .. }
        | RuntimeEventKind::RemoteChildControlPressure { .. }
        | RuntimeEventKind::IsolateStopped
        | RuntimeEventKind::MessageAbandoned => "lifecycle",
        RuntimeEventKind::DeferredReplyCaptured { .. }
        | RuntimeEventKind::DeferredReplySent { .. }
        | RuntimeEventKind::DeferredReplyRejected { .. }
        | RuntimeEventKind::DeferredReplyDropped { .. } => "deferred",
        RuntimeEventKind::FactObserved { .. } => "protocol",
        RuntimeEventKind::SnapshotCommitted
        | RuntimeEventKind::SnapshotCommitFailed { .. }
        | RuntimeEventKind::JournalAppended { .. }
        | RuntimeEventKind::JournalAppendFailed { .. }
        | RuntimeEventKind::RecoveryStarted
        | RuntimeEventKind::RecoveryFinished
        | RuntimeEventKind::RecoveryFailed { .. } => "persistence",
        RuntimeEventKind::MailboxAccepted
        | RuntimeEventKind::EffectObserved { .. }
        | RuntimeEventKind::DriverCompletionQuarantined { .. } => "runtime",
    }
}

fn fact_family_name(fact: tina_runtime::RuntimeFact) -> &'static str {
    match fact {
        RuntimeFact::Protocol(_) => "protocol",
        _ => "unknown",
    }
}

fn fact_name(fact: RuntimeFact) -> &'static str {
    match fact {
        RuntimeFact::Protocol(protocol) => protocol_fact_name(protocol),
        _ => "unknown",
    }
}

fn protocol_fact_name(fact: ProtocolFact) -> &'static str {
    match fact {
        ProtocolFact::Http2StreamOpened { .. } => "http2_stream_opened",
        ProtocolFact::Http2StreamClosed { .. } => "http2_stream_closed",
        ProtocolFact::Http2StreamReset { .. } => "http2_stream_reset",
        ProtocolFact::Http2FlowControlFull { .. } => "http2_flow_control_full",
        ProtocolFact::HttpBodyHighWater { .. } => "http_body_high_water",
        ProtocolFact::WebSocketSlowPeerClosed { .. } => "websocket_slow_peer_closed",
        ProtocolFact::WebSocketSessionClosed { .. } => "websocket_session_closed",
        ProtocolFact::GrpcFinalStatusSent { .. } => "grpc_final_status_sent",
        ProtocolFact::GrpcFinalStatusReceived { .. } => "grpc_final_status_received",
        _ => "unknown_protocol_fact",
    }
}

fn protocol_family_name(family: ProtocolFamily) -> &'static str {
    match family {
        ProtocolFamily::Http2 => "http2",
        ProtocolFamily::HttpBody => "http_body",
        ProtocolFamily::WebSocket => "websocket",
        ProtocolFamily::Grpc => "grpc",
        _ => "unknown_protocol_family",
    }
}

fn insert_target_args(
    args: &mut Map<String, Value>,
    target_shard: ShardId,
    target_isolate: IsolateId,
    target_generation: AddressGeneration,
) {
    args.insert("target_shard".into(), json!(target_shard.get()));
    args.insert("target_isolate".into(), json!(target_isolate.get()));
    args.insert("target_generation".into(), json!(target_generation.get()));
}

fn insert_call_args(
    args: &mut Map<String, Value>,
    call_id: CallId,
    call_kind: tina_runtime::CallKind,
) {
    args.insert("call_id".into(), json!(call_id.get()));
    args.insert("call_kind".into(), json!(call_kind_name(call_kind)));
}

fn insert_option_usize(args: &mut Map<String, Value>, key: &'static str, value: Option<usize>) {
    if let Some(value) = value {
        args.insert(key.into(), json!(value));
    }
}

fn duration(start: RuntimeEvent, end: RuntimeEvent) -> u64 {
    end.id().get().saturating_sub(start.id().get()).max(1)
}

fn shard_tid(shard: ShardId) -> u64 {
    u64::from(shard.get())
}

fn shard_ids(shards: &[ShardId]) -> Vec<u32> {
    shards.iter().map(|shard| shard.get()).collect()
}

fn chrome_event(
    name: &str,
    cat: &str,
    ph: &str,
    ts: u64,
    pid: u64,
    tid: u64,
    args: Value,
) -> Value {
    let mut value = json!({
        "name": name,
        "cat": cat,
        "ph": ph,
        "ts": ts,
        "pid": pid,
        "tid": tid,
        "args": args,
    });
    if ph == "i" {
        if let Value::Object(map) = &mut value {
            map.insert("s".into(), json!("t"));
        }
    }
    value
}

trait ChromeDuration {
    fn with_dur(self, dur: u64) -> Self;
}

impl ChromeDuration for Value {
    fn with_dur(mut self, dur: u64) -> Self {
        if let Value::Object(map) = &mut self {
            map.insert("dur".into(), json!(dur));
        }
        self
    }
}

fn push_raw(out: &mut Vec<EmittedEvent>, order: &mut u64, ts: u64, event_id: u64, value: Value) {
    out.push(EmittedEvent {
        ts,
        event_id,
        order: *order,
        value,
    });
    *order += 1;
}
