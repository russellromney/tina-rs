//! Per-`RuntimeEvent` emission.
//!
//! [`emit_event`] turns one [`RuntimeEvent`] into one [`tracing::Event`].
//! [`emit_events`] iterates a slice. The match on
//! [`RuntimeEventKind`] is exhaustive: a new variant in `tina-runtime`
//! forces a compile error here, which is on purpose.
//!
//! Field shape and rule are documented on the crate root.

use std::fmt;

use tina::CallRejectedReason;
use tina_runtime::{
    CallCompletionRejectedReason, CallError, CallKind, CallReplyRejectedReason,
    DeferredReplyRejectedReason, EffectKind, RestartSkippedReason, RuntimeEvent, RuntimeEventKind,
    SendRejectedReason, SupervisionRejectedReason, TerminalCompletionAction, TraceSnapshot,
};
use tracing::{Level, event};

use crate::RUNTIME_TRACE_TARGET;

/// Renders an `Option<T>` as bare `T` (Display) when present, `-` when absent.
/// No zero-as-absent sentinel, no `Some(...)`/`None` Debug noise.
pub(crate) struct OptValue<T>(pub Option<T>);

impl<T: fmt::Display> fmt::Debug for OptValue<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Some(value) => write!(formatter, "{value}"),
            None => formatter.write_str("-"),
        }
    }
}

/// Emits one [`tracing::Event`] for `event`. Field set on the crate root.
/// IDs (event/cause/call/slot/isolate) are correlation fields, not metric labels.
pub fn emit_event(event: &RuntimeEvent) {
    let event_id = event.id().get();
    let cause_id = OptValue(event.cause().map(|c| c.event().get()));
    let shard = event.shard().get();
    let isolate = event.isolate().get();

    match event.kind() {
        RuntimeEventKind::MailboxAccepted => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::TRACE,
            kind = "mailbox_accepted",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
        ),
        RuntimeEventKind::HandlerStarted => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::TRACE,
            kind = "handler_started",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
        ),
        RuntimeEventKind::HandlerPanicked => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::ERROR,
            kind = "handler_panicked",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
        ),
        RuntimeEventKind::HandlerReportedFailure => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::ERROR,
            kind = "handler_reported_failure",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
        ),
        RuntimeEventKind::HandlerFinished { effect } => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::TRACE,
            kind = "handler_finished",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
            effect = effect_kind_name(effect),
        ),
        RuntimeEventKind::EffectObserved { effect } => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::TRACE,
            kind = "effect_observed",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
            effect = effect_kind_name(effect),
        ),
        RuntimeEventKind::SendDispatchAttempted {
            target_shard,
            target_isolate,
            target_generation,
        } => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::TRACE,
            kind = "send_dispatch_attempted",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
            target_shard = target_shard.get(),
            target_isolate = target_isolate.get(),
            target_generation = target_generation.get(),
        ),
        RuntimeEventKind::SendAccepted {
            target_shard,
            target_isolate,
            target_generation,
        } => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::TRACE,
            kind = "send_accepted",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
            target_shard = target_shard.get(),
            target_isolate = target_isolate.get(),
            target_generation = target_generation.get(),
        ),
        RuntimeEventKind::SendRejected {
            target_shard,
            target_isolate,
            target_generation,
            reason,
        } => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::WARN,
            kind = "send_rejected",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
            target_shard = target_shard.get(),
            target_isolate = target_isolate.get(),
            target_generation = target_generation.get(),
            reason = send_rejected_reason_name(reason),
        ),
        RuntimeEventKind::Spawned { child_isolate } => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::TRACE,
            kind = "spawned",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
            child_isolate = child_isolate.get(),
        ),
        RuntimeEventKind::SupervisorRestartTriggered {
            policy,
            failed_child,
            failed_ordinal,
        } => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::DEBUG,
            kind = "supervisor_restart_triggered",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
            policy = ?policy,
            failed_child = failed_child.get(),
            failed_ordinal = failed_ordinal as u64,
        ),
        RuntimeEventKind::SupervisorRestartRejected {
            failed_child,
            failed_ordinal,
            reason,
        } => match reason {
            SupervisionRejectedReason::BudgetExceeded {
                attempted_restart,
                max_restarts,
            } => event!(
                target: RUNTIME_TRACE_TARGET,
                Level::WARN,
                kind = "supervisor_restart_rejected",
                event_id,
                cause_id = ?cause_id,
                shard,
                isolate,
                failed_child = failed_child.get(),
                failed_ordinal = failed_ordinal as u64,
                reason = "BudgetExceeded",
                attempted_restart,
                max_restarts,
            ),
            SupervisionRejectedReason::SupervisorStopped => event!(
                target: RUNTIME_TRACE_TARGET,
                Level::WARN,
                kind = "supervisor_restart_rejected",
                event_id,
                cause_id = ?cause_id,
                shard,
                isolate,
                failed_child = failed_child.get(),
                failed_ordinal = failed_ordinal as u64,
                reason = "SupervisorStopped",
            ),
        },
        RuntimeEventKind::RestartChildAttempted {
            child_ordinal,
            old_isolate,
            old_generation,
        } => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::TRACE,
            kind = "restart_child_attempted",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
            child_ordinal = child_ordinal as u64,
            old_isolate = old_isolate.get(),
            old_generation = old_generation.get(),
        ),
        RuntimeEventKind::RestartChildSkipped {
            child_ordinal,
            old_isolate,
            old_generation,
            reason,
        } => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::DEBUG,
            kind = "restart_child_skipped",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
            child_ordinal = child_ordinal as u64,
            old_isolate = old_isolate.get(),
            old_generation = old_generation.get(),
            reason = restart_skipped_reason_name(reason),
        ),
        RuntimeEventKind::RestartChildCompleted {
            child_ordinal,
            old_isolate,
            old_generation,
            new_isolate,
            new_generation,
        } => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::TRACE,
            kind = "restart_child_completed",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
            child_ordinal = child_ordinal as u64,
            old_isolate = old_isolate.get(),
            old_generation = old_generation.get(),
            new_isolate = new_isolate.get(),
            new_generation = new_generation.get(),
        ),
        RuntimeEventKind::ChildStopped {
            child_ordinal,
            child_isolate,
            child_generation,
        } => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::DEBUG,
            kind = "child_stopped",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
            child_ordinal = child_ordinal as u64,
            child_isolate = child_isolate.get(),
            child_generation = child_generation.get(),
        ),
        RuntimeEventKind::ChildStarted {
            child_shard,
            child_isolate,
            child_generation,
        } => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::DEBUG,
            kind = "child_started",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
            child_shard = child_shard.get(),
            child_isolate = child_isolate.get(),
            child_generation = child_generation.get(),
        ),
        RuntimeEventKind::RemoteChildStopRequested {
            child_shard,
            child_ordinal,
            child_isolate,
            child_generation,
        } => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::TRACE,
            kind = "remote_child_stop_requested",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
            child_shard = child_shard.get(),
            child_ordinal = child_ordinal as u64,
            child_isolate = child_isolate.get(),
            child_generation = child_generation.get(),
        ),
        RuntimeEventKind::RemoteChildStopped {
            child_shard,
            child_ordinal,
            child_isolate,
            child_generation,
        } => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::DEBUG,
            kind = "remote_child_stopped",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
            child_shard = child_shard.get(),
            child_ordinal = child_ordinal as u64,
            child_isolate = child_isolate.get(),
            child_generation = child_generation.get(),
        ),
        RuntimeEventKind::RemoteChildControlRejected {
            target_shard,
            reason,
        } => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::WARN,
            kind = "remote_child_control_rejected",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
            target_shard = target_shard.get(),
            reason = send_rejected_reason_name(reason),
        ),
        RuntimeEventKind::RemoteChildControlPressure { capacity } => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::WARN,
            kind = "remote_child_control_pressure",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
            capacity = capacity as u64,
        ),
        RuntimeEventKind::IsolateStopped => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::DEBUG,
            kind = "isolate_stopped",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
        ),
        RuntimeEventKind::MessageAbandoned => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::TRACE,
            kind = "message_abandoned",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
        ),
        RuntimeEventKind::CallDispatchAttempted { call_id, call_kind } => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::TRACE,
            kind = "call_dispatch_attempted",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
            call_id = call_id.get(),
            call_kind = call_kind_name(call_kind),
        ),
        RuntimeEventKind::CallCompleted { call_id, call_kind } => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::TRACE,
            kind = "call_completed",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
            call_id = call_id.get(),
            call_kind = call_kind_name(call_kind),
        ),
        RuntimeEventKind::CallContinuationOverflowed { call_id, call_kind } => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::DEBUG,
            kind = "call_continuation_overflowed",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
            call_id = call_id.get(),
            call_kind = call_kind_name(call_kind),
        ),
        RuntimeEventKind::CallFailed {
            call_id,
            call_kind,
            reason,
        } => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::ERROR,
            kind = "call_failed",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
            call_id = call_id.get(),
            call_kind = call_kind_name(call_kind),
            error = call_error_name(reason),
        ),
        RuntimeEventKind::CallCompletionRejected {
            call_id,
            call_kind,
            reason,
        } => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::WARN,
            kind = "call_completion_rejected",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
            call_id = call_id.get(),
            call_kind = call_kind_name(call_kind),
            reason = call_completion_rejected_reason_name(reason),
        ),
        RuntimeEventKind::CallCompletionAction {
            call_id,
            call_kind,
            action,
        } => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::TRACE,
            kind = "call_completion_action",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
            call_id = call_id.get(),
            call_kind = call_kind_name(call_kind),
            action = terminal_completion_action_name(action),
        ),
        RuntimeEventKind::CallReplyRejected { call_id, reason } => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::WARN,
            kind = "call_reply_rejected",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
            call_id = call_id.get(),
            reason = call_reply_rejected_reason_name(reason),
        ),
        RuntimeEventKind::CallReplyAbandoned { call_id } => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::WARN,
            kind = "call_reply_abandoned",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
            call_id = call_id.get(),
        ),
        RuntimeEventKind::CallRejected { call_id, reason } => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::WARN,
            kind = "call_rejected",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
            call_id = call_id.get(),
            reason = call_rejected_reason_name(reason),
        ),
        RuntimeEventKind::SnapshotCommitted => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::TRACE,
            kind = "snapshot_committed",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
        ),
        RuntimeEventKind::SnapshotCommitFailed { reason } => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::WARN,
            kind = "snapshot_commit_failed",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
            error = call_error_name(reason),
        ),
        RuntimeEventKind::JournalAppended { record_index } => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::TRACE,
            kind = "journal_appended",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
            record_index,
        ),
        RuntimeEventKind::JournalAppendFailed {
            record_index,
            reason,
        } => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::WARN,
            kind = "journal_append_failed",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
            record_index,
            error = call_error_name(reason),
        ),
        RuntimeEventKind::RecoveryStarted => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::TRACE,
            kind = "recovery_started",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
        ),
        RuntimeEventKind::RecoveryFinished => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::TRACE,
            kind = "recovery_finished",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
        ),
        RuntimeEventKind::RecoveryFailed { reason } => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::ERROR,
            kind = "recovery_failed",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
            error = call_error_name(reason),
        ),
        RuntimeEventKind::DeferredReplyCaptured { slot_id, call_id } => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::TRACE,
            kind = "deferred_reply_captured",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
            slot_id = slot_id.get(),
            call_id = call_id.get(),
        ),
        RuntimeEventKind::DeferredReplySent { slot_id, call_id } => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::TRACE,
            kind = "deferred_reply_sent",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
            slot_id = slot_id.get(),
            call_id = call_id.get(),
        ),
        RuntimeEventKind::DeferredReplyRejected {
            slot_id,
            call_id,
            reason,
        } => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::WARN,
            kind = "deferred_reply_rejected",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
            slot_id = slot_id.get(),
            call_id = call_id.get(),
            reason = deferred_reply_rejected_reason_name(reason),
        ),
        RuntimeEventKind::DeferredReplyDropped { slot_id, call_id } => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::TRACE,
            kind = "deferred_reply_dropped",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
            slot_id = slot_id.get(),
            call_id = call_id.get(),
        ),
        RuntimeEventKind::CallCancelled { call_id, cause } => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::DEBUG,
            kind = "call_cancelled",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
            call_id = call_id.get(),
            cancel_cause = cancel_cause_name(cause),
        ),
        RuntimeEventKind::FactObserved { fact } => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::TRACE,
            kind = "fact_observed",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
            fact = ?fact,
        ),
        RuntimeEventKind::DriverCompletionQuarantined { call_id } => event!(
            target: RUNTIME_TRACE_TARGET,
            Level::WARN,
            kind = "driver_completion_quarantined",
            event_id,
            cause_id = ?cause_id,
            shard,
            isolate,
            call_id = call_id.get(),
        ),
    }
}

/// Stable string name for a [`tina::CancelCause`].
pub fn cancel_cause_name(cause: tina::CancelCause) -> &'static str {
    match cause {
        tina::CancelCause::CallerCancelled => "CallerCancelled",
        tina::CancelCause::CallerTimedOut => "CallerTimedOut",
        tina::CancelCause::OwnerStopped => "OwnerStopped",
        tina::CancelCause::RuntimeStopped => "RuntimeStopped",
    }
}

/// Emits one [`tracing::Event`] per item in `events`.
///
/// Iteration order is preserved; emission is synchronous.
pub fn emit_events<'a, I>(events: I)
where
    I: IntoIterator<Item = &'a RuntimeEvent>,
{
    for event in events {
        emit_event(event);
    }
}

/// Emits a [`TraceSnapshot`].
///
/// Partial snapshots emit one warn-level
/// `kind="trace_snapshot_partial" missing_shards=[…]` marker first,
/// then the events. The marker is a meta-fact; consumers filtering
/// by `kind` should treat it as "what follows is incomplete."
pub fn emit_trace_snapshot(snapshot: &TraceSnapshot) {
    if snapshot.is_partial() {
        emit_partial_marker(snapshot.missing_shards());
    }
    emit_events(snapshot.events().iter());
}

/// Emits the partial-snapshot marker only — no events walked.
///
/// For hosts that build composite snapshots and want the same warning
/// shape without going through [`TraceSnapshot`].
pub fn emit_partial_marker(missing_shards: &[tina::ShardId]) {
    let ids: Vec<u32> = missing_shards.iter().map(|s| s.get()).collect();
    event!(
        target: RUNTIME_TRACE_TARGET,
        Level::WARN,
        kind = "trace_snapshot_partial",
        missing_shards = ?ids,
    );
}

/// Stable name for an [`EffectKind`]. External observers use this to
/// build matching field values.
pub fn effect_kind_name(kind: EffectKind) -> &'static str {
    match kind {
        EffectKind::Noop => "noop",
        EffectKind::Reply => "reply",
        EffectKind::Send => "send",
        EffectKind::Spawn => "spawn",
        EffectKind::SpawnObserved => "spawn_observed",
        EffectKind::Stop => "stop",
        EffectKind::Fail => "fail",
        EffectKind::StopChildren => "stop_children",
        EffectKind::SpawnObservedOn => "spawn_observed_on",
        EffectKind::StopWith => "stop_with",
        EffectKind::RestartChildren => "restart_children",
        EffectKind::Io => "io",
        EffectKind::Batch => "batch",
        EffectKind::ReplyTo => "reply_to",
        EffectKind::Reject => "reject",
        EffectKind::Fact => "fact",
    }
}

/// Stable string name for a [`CallKind`].
pub fn call_kind_name(kind: CallKind) -> &'static str {
    match kind {
        CallKind::TcpBind => "tcp_bind",
        CallKind::TcpAccept => "tcp_accept",
        CallKind::TcpConnect => "tcp_connect",
        CallKind::TcpRead => "tcp_read",
        CallKind::TcpWrite => "tcp_write",
        CallKind::TcpWriteClose => "tcp_write_close",
        CallKind::TcpListenerClose => "tcp_listener_close",
        CallKind::TcpStreamClose => "tcp_stream_close",
        CallKind::UdpBind => "udp_bind",
        CallKind::UdpSendTo => "udp_send_to",
        CallKind::UdpRecvFrom => "udp_recv_from",
        CallKind::UdpSocketClose => "udp_socket_close",
        CallKind::TlsConnect => "tls_connect",
        CallKind::TlsBind => "tls_bind",
        CallKind::TlsAccept => "tls_accept",
        CallKind::TlsListenerClose => "tls_listener_close",
        CallKind::TlsRead => "tls_read",
        CallKind::TlsWrite => "tls_write",
        CallKind::TlsClose => "tls_close",
        CallKind::DnsLookup => "dns_lookup",
        CallKind::SignalWait => "signal_wait",
        CallKind::ProcessRun => "process_run",
        CallKind::FileOpen => "file_open",
        CallKind::FileReadAt => "file_read_at",
        CallKind::FileWriteAt => "file_write_at",
        CallKind::FileFsync => "file_fsync",
        CallKind::FileSize => "file_size",
        CallKind::FileClose => "file_close",
        CallKind::Mkdir => "mkdir",
        CallKind::PathMetadata => "path_metadata",
        CallKind::RenameReplace => "rename_replace",
        CallKind::RemoveFile => "remove_file",
        CallKind::ReadDir => "read_dir",
        CallKind::SyncParent => "sync_parent",
        CallKind::SnapshotCommit => "snapshot_commit",
        CallKind::SnapshotLoad => "snapshot_load",
        CallKind::JournalAppend => "journal_append",
        CallKind::JournalReplay => "journal_replay",
        CallKind::Sleep => "sleep",
        CallKind::ObservedSend => "observed_send",
        CallKind::IsolateCall => "isolate_call",
        CallKind::CancelCall => "cancel_call",
        CallKind::UnixBind => "unix_bind",
        CallKind::UnixAccept => "unix_accept",
        CallKind::UnixConnect => "unix_connect",
        CallKind::UnixRead => "unix_read",
        CallKind::UnixWrite => "unix_write",
        CallKind::UnixListenerClose => "unix_listener_close",
        CallKind::UnixStreamClose => "unix_stream_close",
    }
}

/// Stable string name for a [`SendRejectedReason`].
pub fn send_rejected_reason_name(reason: SendRejectedReason) -> &'static str {
    match reason {
        SendRejectedReason::Full => "Full",
        SendRejectedReason::Closed => "Closed",
    }
}

/// Stable string name for a [`CallCompletionRejectedReason`].
pub fn call_completion_rejected_reason_name(reason: CallCompletionRejectedReason) -> &'static str {
    match reason {
        CallCompletionRejectedReason::MailboxFull => "MailboxFull",
        CallCompletionRejectedReason::RequesterClosed => "RequesterClosed",
        CallCompletionRejectedReason::ResourceClosed => "ResourceClosed",
        CallCompletionRejectedReason::TerminalActionOnFailure => "TerminalActionOnFailure",
    }
}

/// Stable string name for a [`TerminalCompletionAction`].
pub fn terminal_completion_action_name(action: TerminalCompletionAction) -> &'static str {
    match action {
        TerminalCompletionAction::StopRequester => "StopRequester",
        TerminalCompletionAction::Noop => "Noop",
    }
}

/// Stable string name for a [`CallReplyRejectedReason`].
pub fn call_reply_rejected_reason_name(reason: CallReplyRejectedReason) -> &'static str {
    match reason {
        CallReplyRejectedReason::NoPendingCall => "NoPendingCall",
        CallReplyRejectedReason::ReplyPathFull => "ReplyPathFull",
        CallReplyRejectedReason::RequesterShardClosed => "RequesterShardClosed",
        CallReplyRejectedReason::CallerCancelled => "CallerCancelled",
        CallReplyRejectedReason::CallerTimedOut => "CallerTimedOut",
        CallReplyRejectedReason::OwnerStopped => "OwnerStopped",
        CallReplyRejectedReason::RuntimeStopped => "RuntimeStopped",
    }
}

/// Stable string name for a [`CallRejectedReason`].
pub fn call_rejected_reason_name(reason: CallRejectedReason) -> &'static str {
    match reason {
        CallRejectedReason::ReplyAbandoned => "ReplyAbandoned",
        CallRejectedReason::HandlerPanicked => "HandlerPanicked",
        CallRejectedReason::UnsupportedMessage => "UnsupportedMessage",
    }
}

/// Stable string name for a [`DeferredReplyRejectedReason`].
pub fn deferred_reply_rejected_reason_name(reason: DeferredReplyRejectedReason) -> &'static str {
    match reason {
        DeferredReplyRejectedReason::CallerClosed => "CallerClosed",
        DeferredReplyRejectedReason::ReplyPathFull => "ReplyPathFull",
        DeferredReplyRejectedReason::RequesterShardClosed => "RequesterShardClosed",
        DeferredReplyRejectedReason::TypeMismatch => "TypeMismatch",
        DeferredReplyRejectedReason::CallerCancelled => "CallerCancelled",
        DeferredReplyRejectedReason::CallerTimedOut => "CallerTimedOut",
        DeferredReplyRejectedReason::OwnerStopped => "OwnerStopped",
        DeferredReplyRejectedReason::RuntimeStopped => "RuntimeStopped",
    }
}

/// Stable string name for a [`RestartSkippedReason`].
pub fn restart_skipped_reason_name(reason: RestartSkippedReason) -> &'static str {
    match reason {
        RestartSkippedReason::NotRestartable => "NotRestartable",
        RestartSkippedReason::FactoryPanicked => "FactoryPanicked",
        RestartSkippedReason::RemoteNotRestartable => "RemoteNotRestartable",
    }
}

/// Stable string name for a [`CallError`].
pub fn call_error_name(error: CallError) -> &'static str {
    match error {
        CallError::InvariantViolation => "InvariantViolation",
        CallError::InvalidResource => "InvalidResource",
        CallError::NotFound => "NotFound",
        CallError::Io => "Io",
        CallError::Unsupported => "Unsupported",
        CallError::ResourceBusy => "ResourceBusy",
        CallError::CorruptRecord => "CorruptRecord",
        CallError::CommitUncertain => "CommitUncertain",
        CallError::StorageFull => "StorageFull",
        CallError::StorageClosed => "StorageClosed",
        CallError::TargetFull => "TargetFull",
        CallError::TargetClosed => "TargetClosed",
        CallError::Timeout => "Timeout",
        CallError::DnsFull => "DnsFull",
        CallError::DnsClosed => "DnsClosed",
        CallError::TlsFull => "TlsFull",
        CallError::TlsClosed => "TlsClosed",
        CallError::TlsCertificate => "TlsCertificate",
        CallError::TlsName => "TlsName",
        CallError::TlsHandshake => "TlsHandshake",
        CallError::TlsAlpnMismatch => "TlsAlpnMismatch",
        CallError::SignalFull => "SignalFull",
        CallError::SignalClosed => "SignalClosed",
        CallError::ProcessFull => "ProcessFull",
        CallError::ProcessClosed => "ProcessClosed",
        CallError::KillUncertain => "KillUncertain",
        CallError::TimerFull => "TimerFull",
        CallError::Rejected(CallRejectedReason::ReplyAbandoned) => "Rejected.ReplyAbandoned",
        CallError::Rejected(CallRejectedReason::HandlerPanicked) => "Rejected.HandlerPanicked",
        CallError::Rejected(CallRejectedReason::UnsupportedMessage) => {
            "Rejected.UnsupportedMessage"
        }
    }
}
