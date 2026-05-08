//! Capacity diagnostics.
//!
//! The trace already records every reply-delivery rejection, every
//! send rejection, and every call-completion rejection. This module
//! ships a small helper that walks a slice of [`RuntimeEvent`]s and
//! produces a counted summary so tests, specimens, and host code can
//! ask "did we hit reply-mailbox-full pressure?" without writing the
//! match-and-count loop themselves.
//!
//! The runtime contract is unchanged: this module is a pure trace
//! reader.

use std::fmt;

use crate::trace::{
    CallCompletionRejectedReason, CallReplyRejectedReason, RuntimeEvent, RuntimeEventKind,
    SendRejectedReason,
};

/// Counted summary of pressure-related trace events.
///
/// `non_zero()` returns whether any pressure event happened at all,
/// useful as a one-line "did we have backpressure?" check in tests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PressureSummary {
    /// Reply to a runtime call could not be enqueued because the
    /// requester mailbox was full. The trace records this as
    /// `CallCompletionRejected { reason: MailboxFull }`.
    pub completion_rejected_mailbox_full: u64,
    /// The requester had stopped (or its incarnation was replaced)
    /// by the time the runtime call completed.
    pub completion_rejected_requester_closed: u64,
    /// User code closed the resource while the call was pending.
    pub completion_rejected_resource_closed: u64,
    /// An isolate-call reply could not be matched against an
    /// outstanding pending call (typically caller already timed out).
    pub reply_rejected_no_pending_call: u64,
    /// An isolate-call reply arrived after the caller explicitly
    /// cancelled the wait via `cancel_call(handle)`.
    pub reply_rejected_caller_cancelled: u64,
    /// The bounded reply transport between callee and caller was
    /// full. This is the cross-shard reply mailbox; it has its own
    /// budget separate from the caller's inbox.
    pub reply_rejected_reply_path_full: u64,
    /// The requester shard was closed or failed.
    pub reply_rejected_requester_shard_closed: u64,
    /// A direct local send was rejected because the target mailbox
    /// was full.
    pub send_rejected_full: u64,
    /// A direct local send was rejected because the target mailbox
    /// was closed.
    pub send_rejected_closed: u64,
}

impl PressureSummary {
    /// Walks `events` and tallies pressure-related variants. The
    /// remaining [`RuntimeEventKind`] variants are ignored.
    pub fn from_events<'a, I>(events: I) -> Self
    where
        I: IntoIterator<Item = &'a RuntimeEvent>,
    {
        let mut summary = Self::default();
        for event in events {
            match event.kind() {
                RuntimeEventKind::CallCompletionRejected { reason, .. } => match reason {
                    CallCompletionRejectedReason::MailboxFull => {
                        summary.completion_rejected_mailbox_full += 1;
                    }
                    CallCompletionRejectedReason::RequesterClosed => {
                        summary.completion_rejected_requester_closed += 1;
                    }
                    CallCompletionRejectedReason::ResourceClosed => {
                        summary.completion_rejected_resource_closed += 1;
                    }
                },
                RuntimeEventKind::CallReplyRejected { reason, .. } => match reason {
                    CallReplyRejectedReason::NoPendingCall => {
                        summary.reply_rejected_no_pending_call += 1;
                    }
                    CallReplyRejectedReason::ReplyPathFull => {
                        summary.reply_rejected_reply_path_full += 1;
                    }
                    CallReplyRejectedReason::RequesterShardClosed => {
                        summary.reply_rejected_requester_shard_closed += 1;
                    }
                    CallReplyRejectedReason::CallerCancelled => {
                        summary.reply_rejected_caller_cancelled += 1;
                    }
                },
                RuntimeEventKind::SendRejected { reason, .. } => match reason {
                    SendRejectedReason::Full => summary.send_rejected_full += 1,
                    SendRejectedReason::Closed => summary.send_rejected_closed += 1,
                },
                _ => {}
            }
        }
        summary
    }

    /// True if any pressure-related event was counted.
    pub fn non_zero(&self) -> bool {
        self.completion_rejected_mailbox_full > 0
            || self.completion_rejected_requester_closed > 0
            || self.completion_rejected_resource_closed > 0
            || self.reply_rejected_no_pending_call > 0
            || self.reply_rejected_caller_cancelled > 0
            || self.reply_rejected_reply_path_full > 0
            || self.reply_rejected_requester_shard_closed > 0
            || self.send_rejected_full > 0
            || self.send_rejected_closed > 0
    }

    /// True if any *Full*-shaped rejection was counted. The
    /// closed/no-pending variants reflect lifecycle, not capacity.
    pub fn any_full(&self) -> bool {
        self.completion_rejected_mailbox_full > 0
            || self.reply_rejected_reply_path_full > 0
            || self.send_rejected_full > 0
    }
}

impl fmt::Display for PressureSummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "completion[mbox_full={} requester_closed={} resource_closed={}] \
             reply[no_pending={} cancelled={} path_full={} shard_closed={}] \
             send[full={} closed={}]",
            self.completion_rejected_mailbox_full,
            self.completion_rejected_requester_closed,
            self.completion_rejected_resource_closed,
            self.reply_rejected_no_pending_call,
            self.reply_rejected_caller_cancelled,
            self.reply_rejected_reply_path_full,
            self.reply_rejected_requester_shard_closed,
            self.send_rejected_full,
            self.send_rejected_closed,
        )
    }
}

/// Shared *convention* (not driver) for one-line pressure reports
/// printed by pressure-capable specimens.
///
/// The line format is intentionally boring `key=value` pairs so the
/// outer pressure runners (e.g. `eiffel_cpu_run`, `eiffel_mem_run`)
/// can grep for the leading `pressure ` prefix and surface what each
/// specimen produced. Specimens that do not opt in print whatever
/// they want and the runner passes through.
///
/// The shape is:
///
/// ```text
/// pressure side=<name> accepted=N full=N closed=N timeouts=N other=N rss_peak_kb=N exit=<clean|fail|...>
/// ```
///
/// Use [`format_pressure_line`] from a specimen `run()` to print one
/// canonical line at the end of a pressure run.
#[derive(Debug, Clone)]
pub struct PressureReport<'a> {
    /// Free-text label for the side ("tokio", "tina", or whatever
    /// the specimen calls it).
    pub side: &'a str,
    /// Calls/messages accepted into the system.
    pub accepted: u64,
    /// Rejections that surfaced as `Full`.
    pub full: u64,
    /// Rejections that surfaced as `Closed`.
    pub closed: u64,
    /// Timeouts observed by the caller.
    pub timeouts: u64,
    /// Catch-all bucket for "everything else" the specimen counts.
    pub other: u64,
    /// Optional peak RSS in KiB. `None` skips the field.
    pub rss_peak_kb: Option<u64>,
    /// Exit status: typically `"clean"` or `"fail"`.
    pub exit: &'a str,
}

/// Formats a [`PressureReport`] into the canonical one-line shape.
///
/// `pressure side=<name> accepted=N full=N closed=N timeouts=N other=N rss_peak_kb=N exit=<status>`
///
/// The `rss_peak_kb` field is omitted when `report.rss_peak_kb` is
/// `None`. The trailing newline is the caller's choice.
pub fn format_pressure_line(report: &PressureReport<'_>) -> String {
    let mut line = format!(
        "pressure side={} accepted={} full={} closed={} timeouts={} other={}",
        report.side, report.accepted, report.full, report.closed, report.timeouts, report.other,
    );
    if let Some(kb) = report.rss_peak_kb {
        line.push_str(&format!(" rss_peak_kb={kb}"));
    }
    line.push_str(&format!(" exit={}", report.exit));
    line
}

/// Mailbox capacity broken into incoming + reply-slot budgets.
///
/// A Tina mailbox holds one queue used both for messages *into* this
/// isolate and for *replies* to runtime calls the isolate has
/// dispatched. The math is:
///
/// > total = incoming + replies
///
/// Worth reading once: if an isolate runs N runtime calls in flight,
/// each one will eventually deliver one reply message. If incoming
/// traffic and replies share the same mailbox, the cap must absorb
/// both peaks. `MailboxBudget` lets the spawn site name the two
/// numbers separately and document the intent.
///
/// Use [`Self::total`] when calling
/// [`Runtime::register_with_capacity`](crate::Runtime) or
/// [`ThreadedRuntime::register_with_capacity`](crate::ThreadedRuntime).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MailboxBudget {
    /// Slots reserved for inbound messages.
    pub incoming: usize,
    /// Slots reserved for runtime-call replies.
    pub replies: usize,
}

impl MailboxBudget {
    /// Build a budget with `incoming` slots and zero reply slots.
    pub const fn new(incoming: usize) -> Self {
        Self {
            incoming,
            replies: 0,
        }
    }

    /// Reserve `replies` slots for runtime-call replies.
    pub const fn with_replies(self, replies: usize) -> Self {
        Self {
            incoming: self.incoming,
            replies,
        }
    }

    /// Total mailbox capacity. Pass this to `register_with_capacity`.
    pub const fn total(&self) -> usize {
        self.incoming + self.replies
    }

    /// **Listener preset:** one message slot for the supervisor's
    /// `Stop`, plus one reply slot per concurrent `tcp_accept`.
    pub const fn listener(max_in_flight_accepts: usize) -> Self {
        Self::new(1).with_replies(max_in_flight_accepts)
    }

    /// **Session preset:** one slot per inbound peer message, plus
    /// reply slots for any read/write/close in flight.
    pub const fn session(in_flight_peer_messages: usize, in_flight_runtime_calls: usize) -> Self {
        Self::new(in_flight_peer_messages).with_replies(in_flight_runtime_calls)
    }

    /// **Service preset:** one slot per concurrent inbound request,
    /// plus reply slots for any runtime calls the service issues.
    pub const fn service(concurrent_requests: usize, in_flight_runtime_calls: usize) -> Self {
        Self::new(concurrent_requests).with_replies(in_flight_runtime_calls)
    }

    /// **Fanout preset:** the frontend gives one inbound slot per
    /// upstream caller, plus reply slots equal to the worker count
    /// (one reply per worker per round).
    pub const fn fanout(upstream_callers: usize, worker_count: usize) -> Self {
        Self::new(upstream_callers).with_replies(worker_count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::{CauseId, EffectKind, EventId, RuntimeEvent, RuntimeEventKind};
    use tina::{IsolateId, ShardId};

    fn event(id: u64, kind: RuntimeEventKind) -> RuntimeEvent {
        RuntimeEvent::new(
            EventId::new(id),
            Some(CauseId::from(EventId::new(0))),
            ShardId::new(1),
            IsolateId::new(1),
            kind,
        )
    }

    fn handler_started(id: u64) -> RuntimeEvent {
        event(id, RuntimeEventKind::HandlerStarted)
    }

    #[test]
    fn pressure_summary_zero_on_clean_trace() {
        let events = [
            handler_started(1),
            event(
                2,
                RuntimeEventKind::HandlerFinished {
                    effect: EffectKind::Noop,
                },
            ),
        ];
        let summary = PressureSummary::from_events(events.iter());
        assert!(!summary.non_zero());
        assert!(!summary.any_full());
    }

    #[test]
    fn pressure_summary_counts_call_completion_mailbox_full() {
        use crate::call::CallId;
        use crate::trace::{CallCompletionRejectedReason, CallKind};

        let events = [event(
            1,
            RuntimeEventKind::CallCompletionRejected {
                call_id: CallId::new(1),
                call_kind: CallKind::Sleep,
                reason: CallCompletionRejectedReason::MailboxFull,
            },
        )];
        let summary = PressureSummary::from_events(events.iter());
        assert_eq!(summary.completion_rejected_mailbox_full, 1);
        assert!(summary.any_full());
    }

    #[test]
    fn pressure_summary_counts_send_rejected_full() {
        use tina::AddressGeneration;

        let events = [event(
            1,
            RuntimeEventKind::SendRejected {
                target_shard: ShardId::new(1),
                target_isolate: IsolateId::new(2),
                target_generation: AddressGeneration::new(0),
                reason: SendRejectedReason::Full,
            },
        )];
        let summary = PressureSummary::from_events(events.iter());
        assert_eq!(summary.send_rejected_full, 1);
        assert!(summary.any_full());
    }

    #[test]
    fn mailbox_budget_total_is_sum() {
        let budget = MailboxBudget::new(4).with_replies(2);
        assert_eq!(budget.total(), 6);
    }

    #[test]
    fn mailbox_budget_presets_round_to_expected() {
        assert_eq!(MailboxBudget::listener(8).total(), 9);
        assert_eq!(MailboxBudget::session(4, 4).total(), 8);
        assert_eq!(MailboxBudget::service(8, 8).total(), 16);
        assert_eq!(MailboxBudget::fanout(2, 8).total(), 10);
    }

    #[test]
    fn pressure_line_includes_rss_when_present() {
        let line = format_pressure_line(&PressureReport {
            side: "tina",
            accepted: 10,
            full: 2,
            closed: 0,
            timeouts: 1,
            other: 0,
            rss_peak_kb: Some(42_000),
            exit: "clean",
        });
        assert_eq!(
            line,
            "pressure side=tina accepted=10 full=2 closed=0 timeouts=1 other=0 \
             rss_peak_kb=42000 exit=clean"
        );
    }

    #[test]
    fn pressure_line_omits_rss_when_absent() {
        let line = format_pressure_line(&PressureReport {
            side: "tokio",
            accepted: 5,
            full: 0,
            closed: 0,
            timeouts: 0,
            other: 0,
            rss_peak_kb: None,
            exit: "clean",
        });
        assert_eq!(
            line,
            "pressure side=tokio accepted=5 full=0 closed=0 timeouts=0 other=0 exit=clean"
        );
    }
}
