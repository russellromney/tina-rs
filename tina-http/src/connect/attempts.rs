//! The bounded, Tina-shaped connect helper.
//!
//! [`ConnectAttempts`] is state and report vocabulary, not a scheduler. A
//! manager still builds and returns the visible Tina effects. The helper:
//!
//! - classifies one runtime DNS result into a [`DnsOutcome`] and admits a
//!   bounded candidate-address set through [`BoundedItems`] — a DNS result
//!   never constructs more candidates than the service-owned attempt cap;
//! - hands the manager one candidate at a time via [`take_candidate`], only
//!   while concurrency and total-attempt caps allow, so no connect effect is
//!   built before its attempt slot is admitted;
//! - owns a [`CallGroup`] for the first-success race: each attempt is one
//!   [`tina_runtime::call_cancelable`] branch keyed by address;
//! - on the first success, returns the losers as explicit cancel requests so
//!   the manager closes their caller-side waits and stops their clients;
//! - tombstones and counts any loser reply that arrives after the race was
//!   won — a loser can never become the user's success;
//! - assembles the typed [`ConnectReport`].
//!
//! There is no hidden reconnect loop and no unbounded attempt storage: at
//! most `max_total_attempts` slots are admitted over one connect, and at
//! most `max_concurrent_attempts` are live at once.
//!
//! [`take_candidate`]: ConnectAttempts::take_candidate

use std::collections::VecDeque;
use std::net::SocketAddr;

use tina::{CancelOutcome, Effect, Isolate};
use tina_runtime::{
    BoundedItems, CallError, CallGroup, CallGroupCancelRequest, CallGroupStartError,
    CallGroupToken, CallOutcome, CancelableCall, RuntimeCall,
};

use super::endpoint::{ConnectSecurity, EndpointGeneration, EndpointId};
use super::policy::ConnectPolicy;
use super::report::{
    ConnectAttemptOutcome, ConnectAttemptReport, ConnectReport, ConnectTlsTruth, DnsOutcome,
};

/// The address key one attempt is raced under.
pub type AttemptKey = SocketAddr;

/// Whether the DNS phase produced anything to connect to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DnsClassification {
    /// DNS produced at least one candidate address.
    Proceed,
    /// DNS succeeded but produced zero addresses.
    NoAddresses,
    /// DNS failed, timed out, or the lane was full/closed.
    Failed,
}

/// What recording one attempt reply or cancel outcome means for the race.
#[derive(Debug)]
pub enum ConnectStep<R> {
    /// The attempt failed; the race continues. The manager may start more
    /// candidates (via [`ConnectAttempts::take_candidate`]).
    Continue,
    /// First success. Each loser must have its caller-side wait cancelled
    /// (`cancel_call(handle)`) and its client stopped; feed each cancel
    /// outcome back through [`ConnectAttempts::record_cancel`].
    Won {
        /// Losers to cancel, with their call handles.
        losers: Vec<CallGroupCancelRequest<SocketAddr, R>>,
    },
    /// A loser completed after the race was won. Its result is tombstoned
    /// and counted, never a user success. When `connected`, the manager
    /// must stop that client to release the stream Tina now owns.
    LateCompletion {
        /// The late attempt's address.
        addr: SocketAddr,
        /// Whether the late attempt had connected.
        connected: bool,
    },
    /// Every attempt is terminal and none connected.
    Exhausted,
    /// The race has fully settled (winner found and every loser-cancel
    /// outcome recorded). The report is ready.
    Settled,
}

/// Why starting an attempt failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectAttemptsError {
    /// No attempt slot is free (concurrency cap reached).
    AttemptSlotsFull,
    /// An attempt is already live for this address.
    DuplicateAttempt,
}

/// One bounded connect in progress: DNS, candidate admission, the race, and
/// report assembly.
pub struct ConnectAttempts<R> {
    endpoint: EndpointId,
    generation: EndpointGeneration,
    host: String,
    port: u16,
    authority: String,
    tls: Option<ConnectTlsTruth>,
    policy: ConnectPolicy,
    group: CallGroup<SocketAddr, R>,
    candidates: VecDeque<SocketAddr>,
    resolved_addresses: Vec<SocketAddr>,
    dns: DnsOutcome,
    started_total: usize,
    attempts_log: Vec<ConnectAttemptReport>,
    winner: Option<SocketAddr>,
    cancelled_losers: usize,
    late_completions: usize,
    settled: bool,
}

impl<R> ConnectAttempts<R> {
    /// Build a fresh connect for one endpoint generation.
    ///
    /// `authority`, `tls`, `host`, and `port` are the endpoint truth carried
    /// straight into the report. `policy` must already validate.
    pub fn new(
        endpoint: EndpointId,
        generation: EndpointGeneration,
        host: impl Into<String>,
        port: u16,
        authority: impl Into<String>,
        security: &ConnectSecurity,
        policy: ConnectPolicy,
    ) -> Self {
        let tls = match security {
            ConnectSecurity::Plain => None,
            ConnectSecurity::Tls {
                server_name, alpn, ..
            } => Some(ConnectTlsTruth {
                server_name: server_name.clone(),
                alpn_h2: alpn.is_h2(),
            }),
        };
        Self {
            endpoint,
            generation,
            host: host.into(),
            port,
            authority: authority.into(),
            tls,
            group: CallGroup::with_capacity(policy.happy_eyeballs.max_concurrent_attempts.max(1)),
            candidates: VecDeque::new(),
            resolved_addresses: Vec::new(),
            dns: DnsOutcome::NotAttempted,
            started_total: 0,
            attempts_log: Vec::new(),
            winner: None,
            cancelled_losers: 0,
            late_completions: 0,
            settled: false,
            policy,
        }
    }

    /// Endpoint identity for this connect.
    pub fn endpoint(&self) -> EndpointId {
        self.endpoint
    }

    /// Endpoint generation for this connect.
    pub fn generation(&self) -> EndpointGeneration {
        self.generation
    }

    /// Classify one runtime DNS result and admit the bounded candidate set.
    ///
    /// The ordered, family-policy address list is capped to the
    /// service-owned attempt cap before any of it can become a connect
    /// effect — this is the `BoundedItems` admission the plan requires.
    pub fn record_dns(&mut self, result: Result<Vec<SocketAddr>, CallError>) -> DnsClassification {
        match result {
            Ok(addrs) => {
                self.dns = DnsOutcome::Resolved { count: addrs.len() };
                let mut ordered = self.policy.order_addresses(&addrs);
                ordered.truncate(self.policy.effective_attempt_cap());
                self.resolved_addresses = ordered.clone();
                // The admitted candidate set never exceeds the attempt cap.
                let admitted = BoundedItems::try_from_iter(
                    self.policy.effective_attempt_cap().max(1),
                    ordered,
                )
                .expect("ordered list is already truncated to the attempt cap");
                self.candidates = VecDeque::from(admitted.into_vec());
                if self.candidates.is_empty() {
                    DnsClassification::NoAddresses
                } else {
                    DnsClassification::Proceed
                }
            }
            Err(error) => {
                self.dns = match error {
                    CallError::DnsFull => DnsOutcome::Full,
                    CallError::DnsClosed => DnsOutcome::Closed,
                    CallError::Timeout => DnsOutcome::Timeout,
                    _ => DnsOutcome::Failed,
                };
                self.settled = true;
                DnsClassification::Failed
            }
        }
    }

    /// True when an attempt may start right now: a candidate remains and the
    /// concurrency and total caps allow it.
    pub fn can_start(&self) -> bool {
        self.winner.is_none()
            && !self.settled
            && !self.candidates.is_empty()
            && self.started_total < self.policy.max_total_attempts
            && self.group.len() < self.policy.happy_eyeballs.max_concurrent_attempts
            && !self.group.is_full()
    }

    /// Pop the next candidate to dial, if caps allow. The caller must follow
    /// with [`start`](Self::start) for the returned address.
    pub fn take_candidate(&mut self) -> Option<SocketAddr> {
        if !self.can_start() {
            return None;
        }
        self.candidates.pop_front()
    }

    /// Start one cancelable connect attempt for `addr`.
    ///
    /// The attempt slot is reserved inside [`CallGroup::start_cancelable`]
    /// before the effect is built, so no connect effect can exist before its
    /// slot is admitted.
    pub fn start<I, M, T, F>(
        &mut self,
        addr: SocketAddr,
        call: CancelableCall<T, R>,
        translate: F,
    ) -> Result<Effect<I>, ConnectAttemptsError>
    where
        I: Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(SocketAddr, CallGroupToken, CallOutcome<R>) -> M + 'static,
        M: 'static,
        T: Send + 'static,
        R: 'static,
    {
        match self.group.start_cancelable(addr, call, translate) {
            Ok(effect) => {
                self.started_total += 1;
                Ok(effect)
            }
            Err(CallGroupStartError::Full { .. }) => {
                self.candidates.push_front(addr);
                Err(ConnectAttemptsError::AttemptSlotsFull)
            }
            Err(CallGroupStartError::DuplicateKey { .. }) => {
                Err(ConnectAttemptsError::DuplicateAttempt)
            }
        }
    }

    /// Record one attempt reply.
    ///
    /// `classify` maps a delivered reply into a typed terminal outcome; it is
    /// called only for [`CallOutcome::Replied`]. A reply classified as
    /// connected is the first-success winner. Any reply that arrives after a
    /// winner exists is a tombstoned late completion.
    pub fn record_attempt<C>(
        &mut self,
        addr: SocketAddr,
        token: CallGroupToken,
        outcome: CallOutcome<R>,
        classify: C,
    ) -> ConnectStep<R>
    where
        R: 'static,
        C: FnOnce(&R) -> ConnectAttemptOutcome,
    {
        let typed = match &outcome {
            CallOutcome::Replied(reply) => classify(reply),
            CallOutcome::Timeout => ConnectAttemptOutcome::ConnectTimeout,
            CallOutcome::Full | CallOutcome::Closed | CallOutcome::Rejected(_) => {
                ConnectAttemptOutcome::ConnectIo
            }
        };
        let connected = typed.is_connected();

        // The race is already decided: this reply is a late completion. It is
        // tombstoned and counted, never converted into a user success.
        if self.winner.is_some() || self.settled {
            let _ = self.group.record_reply(addr, token, outcome, |_| false);
            self.late_completions += 1;
            self.record_terminal(addr, ConnectAttemptOutcome::LateCompletion);
            return ConnectStep::LateCompletion { addr, connected };
        }

        match self.group.record_reply(addr, token, outcome, |_| connected) {
            Ok(step) => {
                if connected {
                    self.winner = Some(addr);
                    self.record_terminal(addr, ConnectAttemptOutcome::Connected);
                    self.cancelled_losers = step.cancel_losers.len();
                    if step.cancel_losers.is_empty() {
                        self.settled = true;
                    }
                    ConnectStep::Won {
                        losers: step.cancel_losers,
                    }
                } else {
                    self.record_terminal(addr, typed);
                    if self.is_exhausted() {
                        self.settled = true;
                        ConnectStep::Exhausted
                    } else {
                        ConnectStep::Continue
                    }
                }
            }
            // A stale/unknown token or an unexpected post-win reply: tombstone
            // it. It must never remove a newer same-key branch or win.
            Err(_) => {
                self.late_completions += 1;
                ConnectStep::LateCompletion { addr, connected }
            }
        }
    }

    /// Record one loser-cancel outcome.
    ///
    /// Returns [`ConnectStep::Settled`] once every expected loser cancel is
    /// in, meaning the report is final.
    pub fn record_cancel(
        &mut self,
        addr: SocketAddr,
        token: CallGroupToken,
        outcome: CancelOutcome,
    ) -> ConnectStep<R> {
        let ready = self
            .group
            .record_cancel(addr, token, outcome)
            .unwrap_or(false);
        // A cancelled loser's terminal outcome is `Cancelled`, unless it has
        // already been recorded as a late completion.
        self.record_terminal(addr, ConnectAttemptOutcome::Cancelled);
        if ready {
            self.settled = true;
            ConnectStep::Settled
        } else {
            ConnectStep::Continue
        }
    }

    /// Drain every live attempt for explicit owner-stop cancellation (used on
    /// manager shutdown mid-connect). Feed each outcome back through
    /// [`record_cancel`](Self::record_cancel).
    pub fn drain_for_cancel(&mut self) -> Vec<CallGroupCancelRequest<SocketAddr, R>>
    where
        R: 'static,
    {
        self.candidates.clear();
        let drained = self.group.drain_pending_for_cancel();
        self.cancelled_losers += drained.len();
        if drained.is_empty() {
            self.settled = true;
        }
        drained
    }

    /// True when the connect has fully settled.
    pub fn is_settled(&self) -> bool {
        self.settled && self.group.report_ready()
    }

    /// The winning address, if any.
    pub fn winner(&self) -> Option<SocketAddr> {
        self.winner
    }

    /// The DNS outcome so far.
    pub fn dns_outcome(&self) -> &DnsOutcome {
        &self.dns
    }

    /// Number of candidate addresses not yet started.
    pub fn candidates_remaining(&self) -> usize {
        self.candidates.len()
    }

    /// Number of live in-flight attempts.
    pub fn in_flight(&self) -> usize {
        self.group.len()
    }

    /// Total attempts started over this connect.
    pub fn started_total(&self) -> usize {
        self.started_total
    }

    /// Snapshot the report so far (clone).
    pub fn report(&self) -> ConnectReport {
        ConnectReport {
            endpoint: self.endpoint,
            generation: self.generation,
            host: self.host.clone(),
            port: self.port,
            authority: self.authority.clone(),
            tls: self.tls.clone(),
            dns: self.dns.clone(),
            resolved_addresses: self.resolved_addresses.clone(),
            attempted: self.attempts_log.clone(),
            winner: self.winner,
            cancelled_losers: self.cancelled_losers,
            late_completions: self.late_completions,
        }
    }

    /// Consume into the final report.
    pub fn into_report(self) -> ConnectReport {
        ConnectReport {
            endpoint: self.endpoint,
            generation: self.generation,
            host: self.host,
            port: self.port,
            authority: self.authority,
            tls: self.tls,
            dns: self.dns,
            resolved_addresses: self.resolved_addresses,
            attempted: self.attempts_log,
            winner: self.winner,
            cancelled_losers: self.cancelled_losers,
            late_completions: self.late_completions,
        }
    }

    fn record_terminal(&mut self, addr: SocketAddr, outcome: ConnectAttemptOutcome) {
        if !self.attempts_log.iter().any(|a| a.addr == addr) {
            self.attempts_log
                .push(ConnectAttemptReport::new(addr, outcome));
        }
    }

    fn is_exhausted(&self) -> bool {
        self.winner.is_none()
            && self.group.is_empty()
            && (self.candidates.is_empty() || self.started_total >= self.policy.max_total_attempts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connect::endpoint::ConnectSecurity;
    use std::any::TypeId;
    use std::sync::Arc;
    use std::time::Duration;
    use tina::{CallHandle, CallHandleShared, runtime_internal};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct ConnReply {
        ok: bool,
    }

    fn make_handle() -> CallHandle<ConnReply> {
        let shared = Arc::new(CallHandleShared::new(TypeId::of::<ConnReply>()));
        runtime_internal::call_handle_from_shared::<ConnReply>(shared)
    }

    fn classify(reply: &ConnReply) -> ConnectAttemptOutcome {
        if reply.ok {
            ConnectAttemptOutcome::Connected
        } else {
            ConnectAttemptOutcome::ConnectIo
        }
    }

    fn v4(port: u16) -> SocketAddr {
        format!("127.0.0.{port}:80").parse().unwrap()
    }
    fn v6(port: u16) -> SocketAddr {
        format!("[::{port}]:80").parse().unwrap()
    }

    fn policy(concurrent: usize, total: usize, resolved: usize) -> ConnectPolicy {
        let mut p = ConnectPolicy::balanced();
        p.happy_eyeballs.max_concurrent_attempts = concurrent;
        p.max_total_attempts = total;
        p.max_resolved_addresses = resolved;
        p.address_family = super::super::policy::AddressFamilyPolicy::PreserveOrder;
        p.validate().unwrap();
        p
    }

    fn attempts(p: ConnectPolicy) -> ConnectAttempts<ConnReply> {
        ConnectAttempts::new(
            EndpointId::new(1),
            EndpointGeneration::first(),
            "api.local",
            80,
            "api.local",
            &ConnectSecurity::Plain,
            p,
        )
    }

    // Drive the group through start_cancelable via a real test isolate so the
    // handle is stored; we discard the effect and only exercise bookkeeping.
    #[allow(dead_code)]
    struct TestIso;
    impl Isolate for TestIso {
        tina::isolate_types! {
            message: TestMsg,
            reply: (),
            send: tina::Outbound<std::convert::Infallible>,
            spawn: std::convert::Infallible,
            call: RuntimeCall<TestMsg>,
            shard: tina::SingleShard,
        }
        fn handle(
            &mut self,
            _m: TestMsg,
            _c: &mut tina::Context<'_, tina::SingleShard>,
        ) -> Effect<Self> {
            tina::noop()
        }
    }
    #[derive(Debug)]
    #[allow(dead_code)]
    enum TestMsg {
        Ping,
        Reply(SocketAddr, CallGroupToken, CallOutcome<ConnReply>),
    }

    #[test]
    fn start_builds_a_cancelable_effect_and_counts_the_attempt() {
        let mut a = attempts(policy(2, 4, 4));
        a.record_dns(Ok(vec![v4(1)]));
        let addr = a.take_candidate().unwrap();
        let client = tina::Address::<TestMsg, ConnReply>::new_with_generation(
            tina::ShardId::new(0),
            tina::IsolateId::new(1),
            tina::AddressGeneration::new(0),
        );
        let call = tina_runtime::call_cancelable(client, TestMsg::Ping, Duration::from_secs(1));
        let _effect: Effect<TestIso> = a
            .start(addr, call, TestMsg::Reply)
            .expect("first attempt admits");
        assert_eq!(a.started_total(), 1);
        assert_eq!(a.in_flight(), 1);
    }

    fn start_attempt(a: &mut ConnectAttempts<ConnReply>, addr: SocketAddr) -> CallGroupToken {
        // Insert directly so the test owns the token deterministically.
        a.started_total += 1;
        a.group.insert(addr, make_handle()).unwrap()
    }

    #[test]
    fn dns_failure_is_classified_and_settles() {
        let mut a = attempts(policy(2, 4, 3));
        assert_eq!(
            a.record_dns(Err(CallError::DnsFull)),
            DnsClassification::Failed
        );
        assert_eq!(a.dns_outcome(), &DnsOutcome::Full);
        assert!(!a.can_start());

        let mut a = attempts(policy(2, 4, 3));
        assert_eq!(
            a.record_dns(Err(CallError::Timeout)),
            DnsClassification::Failed
        );
        assert_eq!(a.dns_outcome(), &DnsOutcome::Timeout);

        let mut a = attempts(policy(2, 4, 3));
        assert_eq!(a.record_dns(Err(CallError::Io)), DnsClassification::Failed);
        assert_eq!(a.dns_outcome(), &DnsOutcome::Failed);
    }

    #[test]
    fn dns_success_admits_bounded_candidates() {
        let mut a = attempts(policy(2, 3, 3));
        // Four resolved, cap 3 → three candidates.
        let got = a.record_dns(Ok(vec![v4(1), v4(2), v4(3), v4(4)]));
        assert_eq!(got, DnsClassification::Proceed);
        assert_eq!(a.candidates_remaining(), 3);
        assert!(matches!(a.dns_outcome(), DnsOutcome::Resolved { count: 4 }));
    }

    #[test]
    fn dns_empty_resolution_is_no_addresses() {
        let mut a = attempts(policy(2, 3, 3));
        assert_eq!(a.record_dns(Ok(vec![])), DnsClassification::NoAddresses);
    }

    #[test]
    fn concurrency_cap_limits_simultaneous_starts() {
        let mut a = attempts(policy(2, 4, 4));
        a.record_dns(Ok(vec![v4(1), v4(2), v4(3), v4(4)]));
        let a1 = a.take_candidate().unwrap();
        let _ = start_attempt(&mut a, a1);
        let a2 = a.take_candidate().unwrap();
        let _ = start_attempt(&mut a, a2);
        // Two in flight, concurrency cap 2 → no more until one frees.
        assert!(a.take_candidate().is_none());
        assert_eq!(a.in_flight(), 2);
    }

    #[test]
    fn first_success_wins_and_returns_losers_to_cancel() {
        let mut a = attempts(policy(2, 4, 4));
        a.record_dns(Ok(vec![v4(1), v4(2)]));
        let a1 = a.take_candidate().unwrap();
        let t1 = start_attempt(&mut a, a1);
        let a2 = a.take_candidate().unwrap();
        let _t2 = start_attempt(&mut a, a2);

        // a1 connects first → winner, a2 is a loser to cancel.
        let step = a.record_attempt(
            a1,
            t1,
            CallOutcome::Replied(ConnReply { ok: true }),
            classify,
        );
        match step {
            ConnectStep::Won { losers } => {
                assert_eq!(losers.len(), 1);
                assert_eq!(*losers[0].key(), a2);
            }
            other => panic!("expected Won, got {other:?}"),
        }
        assert_eq!(a.winner(), Some(a1));
    }

    #[test]
    fn loser_late_success_is_tombstoned_not_a_win() {
        let mut a = attempts(policy(2, 4, 4));
        a.record_dns(Ok(vec![v4(1), v4(2)]));
        let a1 = a.take_candidate().unwrap();
        let t1 = start_attempt(&mut a, a1);
        let a2 = a.take_candidate().unwrap();
        let t2 = start_attempt(&mut a, a2);

        let won = a.record_attempt(
            a1,
            t1,
            CallOutcome::Replied(ConnReply { ok: true }),
            classify,
        );
        let losers = match won {
            ConnectStep::Won { losers } => losers,
            other => panic!("expected Won, got {other:?}"),
        };
        // The loser's connect *succeeds late*, arriving as a message.
        let step = a.record_attempt(
            a2,
            t2,
            CallOutcome::Replied(ConnReply { ok: true }),
            classify,
        );
        assert!(matches!(
            step,
            ConnectStep::LateCompletion {
                connected: true,
                ..
            }
        ));
        // Cancel the loser; cancel reports AlreadyCompleted.
        for req in losers {
            let (addr, token, _handle) = req.into_parts();
            let _ = a.record_cancel(addr, token, CancelOutcome::AlreadyCompleted);
        }
        let report = a.into_report();
        // The winner is a1; a2 never became a success.
        assert_eq!(report.winner, Some(a1));
        assert_eq!(report.late_completions, 1);
        assert_eq!(report.cancelled_losers, 1);
        assert!(
            report
                .attempted
                .iter()
                .any(|x| x.addr == a2 && x.outcome == ConnectAttemptOutcome::LateCompletion)
        );
    }

    #[test]
    fn three_way_race_winner_cancels_two_losers_one_completes_late() {
        let mut a = attempts(policy(3, 4, 4));
        a.record_dns(Ok(vec![v4(1), v4(2), v4(3)]));
        let a1 = a.take_candidate().unwrap();
        let t1 = start_attempt(&mut a, a1);
        let a2 = a.take_candidate().unwrap();
        let t2 = start_attempt(&mut a, a2);
        let a3 = a.take_candidate().unwrap();
        let t3 = start_attempt(&mut a, a3);
        assert_eq!(a.in_flight(), 3);

        // a2 connects first → winner, a1 and a3 are losers to cancel.
        let losers = match a.record_attempt(
            a2,
            t2,
            CallOutcome::Replied(ConnReply { ok: true }),
            classify,
        ) {
            ConnectStep::Won { losers } => losers,
            other => panic!("expected Won, got {other:?}"),
        };
        assert_eq!(losers.len(), 2);

        // a1 fails late (arrives as a message), a3 is cancelled cleanly.
        let late = a.record_attempt(
            a1,
            t1,
            CallOutcome::Replied(ConnReply { ok: false }),
            classify,
        );
        assert!(matches!(
            late,
            ConnectStep::LateCompletion {
                connected: false,
                ..
            }
        ));
        // Even an unused token (t3) cancel must settle the race exactly once.
        for req in losers {
            let (addr, token, _handle) = req.into_parts();
            let _ = a.record_cancel(addr, token, CancelOutcome::Cancelled);
        }
        assert!(a.is_settled());
        let report = a.into_report();
        assert_eq!(report.winner, Some(a2));
        assert_eq!(report.cancelled_losers, 2, "both losers were cancelled");
        assert_eq!(report.late_completions, 1, "a1 completed late");
        // a1's terminal row is its late completion, not a clean cancel.
        assert!(
            report
                .attempted
                .iter()
                .any(|x| x.addr == a1 && x.outcome == ConnectAttemptOutcome::LateCompletion)
        );
        let _ = t3;
    }

    #[test]
    fn all_failures_exhaust_with_no_winner() {
        let mut a = attempts(policy(1, 2, 2));
        a.record_dns(Ok(vec![v4(1), v4(2)]));
        let a1 = a.take_candidate().unwrap();
        let t1 = start_attempt(&mut a, a1);
        let s1 = a.record_attempt(
            a1,
            t1,
            CallOutcome::Replied(ConnReply { ok: false }),
            classify,
        );
        assert!(matches!(s1, ConnectStep::Continue));
        let a2 = a.take_candidate().unwrap();
        let t2 = start_attempt(&mut a, a2);
        let s2 = a.record_attempt(a2, t2, CallOutcome::Timeout, classify);
        assert!(matches!(s2, ConnectStep::Exhausted));
        let report = a.into_report();
        assert!(report.winner.is_none());
        assert_eq!(report.attempted.len(), 2);
        assert_eq!(
            report.attempted[1].outcome,
            ConnectAttemptOutcome::ConnectTimeout
        );
    }

    #[test]
    fn total_attempt_cap_bounds_starts_even_with_more_candidates() {
        // resolved=4 but total=2: only two attempts ever start.
        let mut a = attempts(policy(1, 2, 4));
        a.record_dns(Ok(vec![v4(1), v4(2), v4(3), v4(4)]));
        // candidates truncated to effective cap = min(4,2)=2.
        assert_eq!(a.candidates_remaining(), 2);
        let a1 = a.take_candidate().unwrap();
        let t1 = start_attempt(&mut a, a1);
        a.record_attempt(
            a1,
            t1,
            CallOutcome::Replied(ConnReply { ok: false }),
            classify,
        );
        let a2 = a.take_candidate().unwrap();
        let t2 = start_attempt(&mut a, a2);
        a.record_attempt(
            a2,
            t2,
            CallOutcome::Replied(ConnReply { ok: false }),
            classify,
        );
        assert!(a.take_candidate().is_none());
        assert_eq!(a.started_total(), 2);
    }

    #[test]
    fn family_ordering_v6_first_is_reflected_in_resolved() {
        let mut p = policy(2, 4, 4);
        p.address_family = super::super::policy::AddressFamilyPolicy::Ipv6First;
        let mut a = attempts(p);
        a.record_dns(Ok(vec![v4(1), v6(1), v4(2)]));
        let first = a.take_candidate().unwrap();
        assert!(first.is_ipv6());
    }

    #[test]
    fn drain_for_cancel_stops_in_flight_and_clears_candidates() {
        let mut a = attempts(policy(2, 4, 4));
        a.record_dns(Ok(vec![v4(1), v4(2), v4(3)]));
        let a1 = a.take_candidate().unwrap();
        let _t1 = start_attempt(&mut a, a1);
        let drained = a.drain_for_cancel();
        assert_eq!(drained.len(), 1);
        assert_eq!(a.candidates_remaining(), 0);
    }
}
