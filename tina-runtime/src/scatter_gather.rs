//! Bounded typed scatter/gather execution shared by every runtime backend.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use tina::{CancelOutcome, Effect, Isolate, RequestContext};

use crate::call::{CancelableCall, RuntimeCall};
use crate::call_group::{
    CallGroupToken, CallJoinSet, CallSetRecordCancelError, CallSetRecordReplyError,
    CallSetStartError,
};
use crate::sharded::{
    ScatterGatherConfig, ScatterGatherConfigError, ScatterGatherReport, ScatterGatherTargetOutcome,
};
use crate::{BoundedEffects, BoundedItems, CallOutcome, cancel_call, sleep};

#[derive(Debug)]
struct TargetSlot<K, R> {
    key: K,
    override_outcome: Option<ScatterGatherTargetOutcome<R>>,
}

/// Opaque identity for one scatter/gather operation.
///
/// Carry this token in every child-reply, aggregate-timer, and cancellation
/// event. It routes concurrent operations without application qids and, since
/// runtime sleeps are physically non-cancelable, prevents a completed
/// operation's late timer from expiring a newer operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[must_use = "carry the token into the aggregate-timeout continuation"]
pub struct ScatterGatherToken(u64);

static NEXT_SCATTER_GATHER_TOKEN: AtomicU64 = AtomicU64::new(1);

impl ScatterGatherToken {
    fn alloc() -> Self {
        Self(NEXT_SCATTER_GATHER_TOKEN.fetch_add(1, Ordering::Relaxed))
    }
}

/// One bounded scatter/gather operation, including original caller authority.
///
/// Child cancellation authority lives in a [`CallJoinSet`]. Caller target
/// order is independent of completion order, and aggregate expiry cannot
/// return the [`RequestContext`] until every pending child has acknowledged
/// cancellation.
#[derive(Debug)]
pub struct ScatterGather<K, R, Q> {
    token: ScatterGatherToken,
    config: ScatterGatherConfig,
    caller: Option<RequestContext<Q>>,
    targets: Vec<TargetSlot<K, R>>,
    calls: Option<CallJoinSet<K, R>>,
    aggregate_expired: bool,
}

/// Successful terminal ownership transfer from [`ScatterGather`].
#[derive(Debug)]
pub struct ScatterGatherCompleted<K, R, Q> {
    /// Original caller authority, returned exactly once.
    pub request: RequestContext<Q>,
    /// Exhaustive per-target report in caller-supplied order.
    pub report: ScatterGatherReport<R, K>,
}

/// Unified continuation vocabulary for a bounded scatter/gather owner.
#[derive(Debug)]
pub enum ScatterGatherEvent<K, R> {
    /// One child call reached a terminal outcome.
    Reply(ScatterGatherToken, K, CallGroupToken, CallOutcome<R>),
    /// The aggregate deadline fired.
    AggregateTimeout(ScatterGatherToken),
    /// One aggregate-timeout cancellation settled.
    Cancelled(ScatterGatherToken, K, CallGroupToken, CancelOutcome),
}

impl<K, R> ScatterGatherEvent<K, R> {
    /// Operation identity carried by this continuation.
    pub const fn operation(&self) -> ScatterGatherToken {
        match self {
            Self::Reply(operation, ..)
            | Self::AggregateTimeout(operation)
            | Self::Cancelled(operation, ..) => *operation,
        }
    }
}

/// Fixed-capacity owner for concurrent scatter/gather operations.
#[derive(Debug)]
pub struct ScatterGatherOperations<K, R, Q> {
    capacity: usize,
    operations: Vec<ScatterGather<K, R, Q>>,
}

/// Start result after [`ScatterGatherOperations`] has retained running state.
pub enum ScatterGatherOperationsStart<I, K, R, Q>
where
    I: Isolate,
{
    /// The operation completed without child effects.
    Ready(ScatterGatherCompleted<K, R, Q>),
    /// Running state was installed and these bounded effects may execute.
    Running(Effect<I>),
}

/// Typed continuation-routing failure that returns the unconsumed event.
#[derive(Debug)]
pub enum ScatterGatherOperationsError<K, R> {
    /// A reply or cancel continuation named no live operation.
    UnknownOperation(ScatterGatherEvent<K, R>),
    /// The operation rejected a duplicate, stale, or mismatched branch token.
    Record(ScatterGatherRecordError<K, R>),
}

/// Result of routing one unified event through a bounded operation owner.
pub type ScatterGatherOperationsAdvanceResult<I, K, R, Q> =
    Result<Option<ScatterGatherAdvance<I, K, R, Q>>, ScatterGatherOperationsError<K, R>>;

/// Result of starting one scatter/gather operation.
pub enum ScatterGatherStart<I, K, R, Q>
where
    I: Isolate,
{
    /// Every target was missing, so the operation completed without effects.
    Ready(ScatterGatherCompleted<K, R, Q>),
    /// At least one typed child call is live.
    Running {
        /// State that owns caller and child-call authority.
        operation: ScatterGather<K, R, Q>,
        /// Bounded child calls plus one aggregate timer.
        effect: Effect<I>,
    },
}

/// Aggregate-expiry work and any immediate completion.
pub struct ScatterGatherAdvance<I, K, R, Q>
where
    I: Isolate,
{
    /// Bounded cancellation batch, possibly empty.
    pub effect: Effect<I>,
    /// Present only when no cancellation acknowledgement remains outstanding.
    pub completed: Option<ScatterGatherCompleted<K, R, Q>>,
}

/// Why [`ScatterGather::start`] rejected before producing effects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScatterGatherStartError<K> {
    /// The bounded concurrent-operation owner is full.
    OperationsFull {
        /// Configured maximum simultaneous operations.
        max: usize,
    },
    /// Invalid scatter/gather configuration.
    Config(ScatterGatherConfigError),
    /// The bounded target input still exceeds this operation's configured cap.
    TooManyTargets {
        /// Configured target cap.
        max: usize,
        /// Supplied target count.
        observed: usize,
    },
    /// Caller target keys must be unique within one operation.
    DuplicateTarget(K),
    /// `max_targets + 1` cannot be represented for the aggregate timer.
    EffectCapacityOverflow,
}

/// Failed start with original caller authority returned unchanged.
#[derive(Debug)]
pub struct ScatterGatherStartFailure<K, Q> {
    /// Original caller authority; reply, reject, or retry explicitly.
    pub request: RequestContext<Q>,
    /// Validation failure that prevented any effect construction.
    pub error: ScatterGatherStartError<K>,
}

impl<K: fmt::Debug> fmt::Display for ScatterGatherStartError<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OperationsFull { max } => {
                write!(f, "scatter operation capacity {max} is full")
            }
            Self::Config(error) => write!(f, "invalid scatter/gather config: {error}"),
            Self::TooManyTargets { max, observed } => {
                write!(
                    f,
                    "observed {observed} scatter targets, exceeding max {max}"
                )
            }
            Self::DuplicateTarget(key) => write!(f, "duplicate scatter target {key:?}"),
            Self::EffectCapacityOverflow => f.write_str("scatter effect capacity overflow"),
        }
    }
}

impl<K, R, Q> ScatterGatherOperations<K, R, Q>
where
    K: Clone + PartialEq + 'static,
    R: 'static,
    Q: 'static,
{
    /// Creates an empty fixed-capacity concurrent operation owner.
    pub fn with_capacity(capacity: usize) -> Self {
        assert!(capacity > 0, "ScatterGatherOperations capacity must be > 0");
        Self {
            capacity,
            operations: Vec::with_capacity(capacity),
        }
    }

    /// Number of live operations.
    pub fn len(&self) -> usize {
        self.operations.len()
    }

    /// Whether no operation is live.
    pub fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }

    /// Whether a new operation would be rejected before effect construction.
    pub fn is_full(&self) -> bool {
        self.operations.len() == self.capacity
    }

    /// Starts and atomically retains one split-service scatter operation.
    pub fn start_service<I, Target, Payload, Event, Request, Build, Wrap>(
        &mut self,
        request: RequestContext<Q>,
        config: ScatterGatherConfig,
        targets: BoundedItems<(K, Option<Target>)>,
        build_call: Build,
        wrap: Wrap,
    ) -> Result<ScatterGatherOperationsStart<I, K, R, Q>, ScatterGatherStartFailure<K, Q>>
    where
        I: Isolate<
                Message = tina::ServiceMessage<Event, Request>,
                Io = RuntimeCall<tina::ServiceMessage<Event, Request>>,
            >,
        Payload: Send + 'static,
        Event: 'static,
        Request: 'static,
        Build: FnMut(Target, std::time::Duration) -> CancelableCall<Payload, R>,
        Wrap: Fn(ScatterGatherEvent<K, R>) -> Event + Clone + 'static,
    {
        if self.is_full() {
            return Err(ScatterGatherStartFailure {
                request,
                error: ScatterGatherStartError::OperationsFull { max: self.capacity },
            });
        }
        let reply_wrap = wrap.clone();
        let timeout_wrap = wrap;
        match ScatterGather::start_service(
            request,
            config,
            targets,
            build_call,
            move |operation, key, branch, outcome| {
                reply_wrap(ScatterGatherEvent::Reply(operation, key, branch, outcome))
            },
            move |operation| timeout_wrap(ScatterGatherEvent::AggregateTimeout(operation)),
        )? {
            ScatterGatherStart::Ready(completed) => {
                Ok(ScatterGatherOperationsStart::Ready(completed))
            }
            ScatterGatherStart::Running { operation, effect } => {
                self.operations.push(operation);
                Ok(ScatterGatherOperationsStart::Running(effect))
            }
        }
    }

    /// Advances one unified split-service continuation.
    pub fn advance_service<I, Event, Request, Wrap>(
        &mut self,
        event: ScatterGatherEvent<K, R>,
        wrap: Wrap,
    ) -> ScatterGatherOperationsAdvanceResult<I, K, R, Q>
    where
        I: Isolate<
                Message = tina::ServiceMessage<Event, Request>,
                Io = RuntimeCall<tina::ServiceMessage<Event, Request>>,
            >,
        Event: 'static,
        Request: 'static,
        Wrap: Fn(ScatterGatherEvent<K, R>) -> Event + Clone + 'static,
    {
        let operation_token = event.operation();
        let Some(index) = self
            .operations
            .iter()
            .position(|operation| operation.token() == operation_token)
        else {
            return match event {
                ScatterGatherEvent::AggregateTimeout(_) => Ok(None),
                other => Err(ScatterGatherOperationsError::UnknownOperation(other)),
            };
        };

        let (effect, completed) = match event {
            ScatterGatherEvent::Reply(_, key, branch, outcome) => (
                tina::noop(),
                self.operations[index]
                    .record_reply(key, branch, outcome)
                    .map_err(ScatterGatherOperationsError::Record)?,
            ),
            ScatterGatherEvent::AggregateTimeout(operation) => {
                let cancel_wrap = wrap;
                let Some(advance) = self.operations[index]
                    .aggregate_timeout_service::<I, Event, Request, _>(
                        operation,
                        move |operation, key, branch, outcome| {
                            cancel_wrap(ScatterGatherEvent::Cancelled(
                                operation, key, branch, outcome,
                            ))
                        },
                    )
                    .map_err(ScatterGatherOperationsError::Record)?
                else {
                    return Ok(None);
                };
                (advance.effect, advance.completed)
            }
            ScatterGatherEvent::Cancelled(_, key, branch, outcome) => (
                tina::noop(),
                self.operations[index]
                    .record_cancel(key, branch, outcome)
                    .map_err(ScatterGatherOperationsError::Record)?,
            ),
        };
        if completed.is_some() {
            self.operations.swap_remove(index);
        }
        Ok(Some(ScatterGatherAdvance { effect, completed }))
    }
}

impl<K: fmt::Debug> std::error::Error for ScatterGatherStartError<K> {}

/// Why a continuation could not update a scatter/gather operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScatterGatherRecordError<K, R> {
    /// Child reply did not name live or cancellation-pending authority.
    Reply(CallSetRecordReplyError<K, R>),
    /// Cancellation acknowledgement did not name expected authority.
    Cancel(CallSetRecordCancelError<K>),
    /// Aggregate expiry was delivered more than once.
    DuplicateAggregateTimeout,
    /// Caller authority has already transferred into a completion.
    AlreadyCompleted,
}

/// Result of recording one child reply or cancellation acknowledgement.
pub type ScatterGatherRecordResult<K, R, Q> =
    Result<Option<ScatterGatherCompleted<K, R, Q>>, ScatterGatherRecordError<K, R>>;

/// Result of observing an aggregate timer.
///
/// `Ok(None)` means the timer belonged to an older completed operation and was
/// intentionally ignored.
pub type ScatterGatherAdvanceResult<I, K, R, Q> =
    Result<Option<ScatterGatherAdvance<I, K, R, Q>>, ScatterGatherRecordError<K, R>>;

impl<K, R, Q> ScatterGather<K, R, Q>
where
    K: Clone + PartialEq + 'static,
    R: 'static,
    Q: 'static,
{
    /// Start bounded cancelable target calls and one aggregate timer.
    ///
    /// Each `None` target is reported as `MissingShard`. The complete bounded
    /// target list is checked for size and duplicate keys before any call is
    /// converted into an effect batch.
    pub fn start<I, Target, Payload, M, Build, ReplyEvent, TimeoutEvent>(
        request: RequestContext<Q>,
        config: ScatterGatherConfig,
        targets: BoundedItems<(K, Option<Target>)>,
        mut build_call: Build,
        reply_event: ReplyEvent,
        aggregate_timeout_event: TimeoutEvent,
    ) -> Result<ScatterGatherStart<I, K, R, Q>, ScatterGatherStartFailure<K, Q>>
    where
        I: Isolate<Message = M, Io = RuntimeCall<M>>,
        Payload: Send + 'static,
        M: 'static,
        Build: FnMut(Target, std::time::Duration) -> CancelableCall<Payload, R>,
        ReplyEvent:
            Fn(ScatterGatherToken, K, CallGroupToken, CallOutcome<R>) -> M + Clone + 'static,
        TimeoutEvent: FnOnce(ScatterGatherToken) -> M + 'static,
    {
        if let Err(error) = config.validate() {
            return Err(ScatterGatherStartFailure {
                request,
                error: ScatterGatherStartError::Config(error),
            });
        }
        if targets.len() > config.max_targets {
            return Err(ScatterGatherStartFailure {
                request,
                error: ScatterGatherStartError::TooManyTargets {
                    max: config.max_targets,
                    observed: targets.len(),
                },
            });
        }
        let targets = targets.into_vec();
        for (index, (key, _)) in targets.iter().enumerate() {
            if targets[..index].iter().any(|(seen, _)| seen == key) {
                return Err(ScatterGatherStartFailure {
                    request,
                    error: ScatterGatherStartError::DuplicateTarget(key.clone()),
                });
            }
        }

        let Some(effect_cap) = config.max_targets.checked_add(1) else {
            return Err(ScatterGatherStartFailure {
                request,
                error: ScatterGatherStartError::EffectCapacityOverflow,
            });
        };
        let token = ScatterGatherToken::alloc();
        let mut calls = CallJoinSet::with_capacity(config.max_targets);
        let mut rows = Vec::with_capacity(config.max_targets);
        let mut effects = Vec::with_capacity(effect_cap);
        for (key, target) in targets {
            match target {
                Some(target) => {
                    let call = build_call(target, config.per_target_timeout);
                    let operation_token = token;
                    let translator = reply_event.clone();
                    let effect = calls
                        .start_cancelable::<I, M, Payload, _>(
                            key.clone(),
                            call,
                            move |key, branch_token, outcome| {
                                translator(operation_token, key, branch_token, outcome)
                            },
                        )
                        .unwrap_or_else(|error| match error {
                            CallSetStartError::DuplicateKey { .. } => {
                                unreachable!(
                                    "duplicate keys were validated before call construction"
                                )
                            }
                            CallSetStartError::Full { .. } => {
                                unreachable!("validated target count fits CallJoinSet capacity")
                            }
                        });
                    effects.push(effect);
                    rows.push(TargetSlot {
                        key,
                        override_outcome: None,
                    });
                }
                None => rows.push(TargetSlot {
                    key,
                    override_outcome: Some(ScatterGatherTargetOutcome::MissingShard),
                }),
            }
        }

        let mut operation = Self {
            token,
            config,
            caller: Some(request),
            targets: rows,
            calls: Some(calls),
            aggregate_expired: false,
        };
        if operation.calls().is_empty() {
            return Ok(ScatterGatherStart::Ready(
                operation
                    .take_completed()
                    .expect("missing-only scatter is complete"),
            ));
        }
        effects.push(sleep(config.aggregate_timeout).then(move |_| aggregate_timeout_event(token)));
        let effects = BoundedEffects::try_from_iter(effect_cap, effects)
            .expect("scatter effects fit max_targets plus aggregate timer");
        Ok(ScatterGatherStart::Running {
            operation,
            effect: effects.into_batch(),
        })
    }

    /// Split-service sibling of [`start`](Self::start) that accepts domain
    /// events and supplies the private service envelope.
    pub fn start_service<I, Target, Payload, Event, Request, Build, ReplyEvent, TimeoutEvent>(
        request: RequestContext<Q>,
        config: ScatterGatherConfig,
        targets: BoundedItems<(K, Option<Target>)>,
        build_call: Build,
        reply_event: ReplyEvent,
        aggregate_timeout_event: TimeoutEvent,
    ) -> Result<ScatterGatherStart<I, K, R, Q>, ScatterGatherStartFailure<K, Q>>
    where
        I: Isolate<
                Message = tina::ServiceMessage<Event, Request>,
                Io = RuntimeCall<tina::ServiceMessage<Event, Request>>,
            >,
        Payload: Send + 'static,
        Event: 'static,
        Request: 'static,
        Build: FnMut(Target, std::time::Duration) -> CancelableCall<Payload, R>,
        ReplyEvent:
            Fn(ScatterGatherToken, K, CallGroupToken, CallOutcome<R>) -> Event + Clone + 'static,
        TimeoutEvent: FnOnce(ScatterGatherToken) -> Event + 'static,
    {
        Self::start(
            request,
            config,
            targets,
            build_call,
            move |operation, key, token, outcome| {
                tina::ServiceMessage::Event(reply_event(operation, key, token, outcome))
            },
            move |token| tina::ServiceMessage::Event(aggregate_timeout_event(token)),
        )
    }

    /// Identity to carry in the aggregate timer continuation.
    pub const fn token(&self) -> ScatterGatherToken {
        self.token
    }

    /// Record one child terminal outcome.
    ///
    /// Generation-stamped tokens reject duplicate and late continuations. A
    /// reply racing aggregate expiry is retained for cancellation settlement
    /// but cannot overwrite the already-terminal `AggregateTimeout` report row.
    pub fn record_reply(
        &mut self,
        key: K,
        token: CallGroupToken,
        outcome: CallOutcome<R>,
    ) -> ScatterGatherRecordResult<K, R, Q> {
        self.calls_mut()?
            .record_reply(key, token, outcome)
            .map_err(ScatterGatherRecordError::Reply)?;
        Ok(self.take_completed())
    }

    /// Mark pending targets `AggregateTimeout` and cancel their child waits.
    ///
    /// The caller is not returned until every emitted cancellation has
    /// acknowledged, even when a late child reply races the cancellation.
    pub fn aggregate_timeout<I, M, CancelEvent>(
        &mut self,
        token: ScatterGatherToken,
        cancel_event: CancelEvent,
    ) -> ScatterGatherAdvanceResult<I, K, R, Q>
    where
        I: Isolate<Message = M, Io = RuntimeCall<M>>,
        M: 'static,
        CancelEvent:
            Fn(ScatterGatherToken, K, CallGroupToken, CancelOutcome) -> M + Clone + 'static,
    {
        if token != self.token {
            return Ok(None);
        }
        if self.aggregate_expired {
            return Err(ScatterGatherRecordError::DuplicateAggregateTimeout);
        }
        self.aggregate_expired = true;
        let calls = self
            .calls
            .as_ref()
            .ok_or(ScatterGatherRecordError::AlreadyCompleted)?;
        for row in &mut self.targets {
            if row.override_outcome.is_none() && !calls.has_recorded_reply(&row.key) {
                row.override_outcome = Some(ScatterGatherTargetOutcome::AggregateTimeout);
            }
        }
        let cancels = self.calls_mut()?.drain_pending_for_cancel();
        let operation_token = self.token;
        let effects = cancels.into_iter().map(|request| {
            let (key, token, handle) = request.into_parts();
            let event_key = key.clone();
            let translator = cancel_event.clone();
            cancel_call(handle)
                .then(move |outcome| translator(operation_token, event_key, token, outcome))
        });
        let effects = BoundedEffects::try_from_iter(self.config.max_targets, effects)
            .expect("cancel count cannot exceed max_targets");
        Ok(Some(ScatterGatherAdvance {
            effect: effects.into_batch(),
            completed: self.take_completed(),
        }))
    }

    /// Split-service sibling of [`aggregate_timeout`](Self::aggregate_timeout)
    /// that accepts a domain-event translator.
    pub fn aggregate_timeout_service<I, Event, Request, CancelEvent>(
        &mut self,
        token: ScatterGatherToken,
        cancel_event: CancelEvent,
    ) -> ScatterGatherAdvanceResult<I, K, R, Q>
    where
        I: Isolate<
                Message = tina::ServiceMessage<Event, Request>,
                Io = RuntimeCall<tina::ServiceMessage<Event, Request>>,
            >,
        Event: 'static,
        Request: 'static,
        CancelEvent:
            Fn(ScatterGatherToken, K, CallGroupToken, CancelOutcome) -> Event + Clone + 'static,
    {
        self.aggregate_timeout(token, move |operation, key, token, outcome| {
            tina::ServiceMessage::Event(cancel_event(operation, key, token, outcome))
        })
    }

    /// Record one cancellation acknowledgement emitted by aggregate expiry.
    pub fn record_cancel(
        &mut self,
        key: K,
        token: CallGroupToken,
        outcome: CancelOutcome,
    ) -> ScatterGatherRecordResult<K, R, Q> {
        self.calls_mut()?
            .record_cancel(key, token, outcome)
            .map_err(ScatterGatherRecordError::Cancel)?;
        Ok(self.take_completed())
    }

    fn calls(&self) -> &CallJoinSet<K, R> {
        self.calls
            .as_ref()
            .expect("completed scatter/gather has no call set")
    }

    fn calls_mut(&mut self) -> Result<&mut CallJoinSet<K, R>, ScatterGatherRecordError<K, R>> {
        self.calls
            .as_mut()
            .ok_or(ScatterGatherRecordError::AlreadyCompleted)
    }

    fn take_completed(&mut self) -> Option<ScatterGatherCompleted<K, R, Q>> {
        if !self.calls().report_ready() {
            return None;
        }
        let join = self
            .calls
            .take()
            .expect("ready scatter owns call set")
            .into_report();
        for branch in join.branch_outcomes {
            let row = self
                .targets
                .iter_mut()
                .find(|row| row.key == branch.key)
                .expect("join key came from target rows");
            if row.override_outcome.is_none() {
                row.override_outcome = Some(match branch.outcome {
                    CallOutcome::Replied(reply) => ScatterGatherTargetOutcome::Replied(reply),
                    CallOutcome::Full => ScatterGatherTargetOutcome::Full,
                    CallOutcome::Closed => ScatterGatherTargetOutcome::Closed,
                    CallOutcome::Timeout => ScatterGatherTargetOutcome::Timeout,
                    CallOutcome::Rejected(reason) => ScatterGatherTargetOutcome::Rejected(reason),
                });
            }
        }
        let outcomes = self
            .targets
            .drain(..)
            .map(|row| {
                (
                    row.key,
                    row.override_outcome
                        .expect("ready scatter has one outcome per target"),
                )
            })
            .collect();
        Some(ScatterGatherCompleted {
            request: self
                .caller
                .take()
                .expect("caller authority transfers exactly once"),
            report: ScatterGatherReport {
                config: self.config,
                outcomes,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use std::any::TypeId;
    use std::cell::Cell;
    use std::convert::Infallible;
    use std::sync::Arc;
    use std::time::Duration;

    use tina::{
        Address, AddressGeneration, CallHandleShared, DeferredSlotShared, IsolateId, Outbound,
        ShardId, SingleShard, runtime_internal,
    };

    use super::*;
    use crate::call_cancelable;

    #[derive(Debug)]
    enum TestMessage {
        Reply(ScatterGatherToken, u8, CallGroupToken, CallOutcome<u32>),
        Aggregate,
        Cancel(ScatterGatherToken, u8, CallGroupToken, CancelOutcome),
    }

    #[derive(Debug)]
    struct TestIsolate;

    impl Isolate for TestIsolate {
        tina::isolate_types! {
            message: TestMessage,
            reply: (),
            send: Outbound<Infallible>,
            spawn: Infallible,
            io: RuntimeCall<TestMessage>,
            shard: SingleShard,
        }

        fn handle(
            &mut self,
            message: Self::Message,
            _ctx: &mut tina::Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            match message {
                TestMessage::Reply(operation, key, token, outcome) => {
                    let _ = (operation, key, token, outcome);
                }
                TestMessage::Aggregate => {}
                TestMessage::Cancel(operation, key, token, outcome) => {
                    let _ = (operation, key, token, outcome);
                }
            }
            tina::noop()
        }
    }

    #[derive(Debug)]
    enum TestServiceEvent {
        Scatter(ScatterGatherEvent<u8, u32>),
    }

    struct TestService;

    #[tina_macros::isolate(
        event = TestServiceEvent,
        request = (),
        reply = (),
        io = crate::RuntimeCall<tina::ServiceMessage<TestServiceEvent, ()>>
    )]
    impl TestService {
        fn handle_event(
            &mut self,
            event: TestServiceEvent,
            _ctx: &mut tina::Context<'_, SingleShard, Self::Reply>,
        ) -> Effect<Self> {
            let TestServiceEvent::Scatter(event) = event;
            let _ = event;
            tina::noop()
        }

        fn handle_request(
            &mut self,
            _request: (),
            call: tina::RequestCall<'_, Self>,
        ) -> tina::RequestEffect<Self> {
            call.reply(())
        }
    }

    fn request() -> RequestContext<()> {
        let shared = Arc::new(DeferredSlotShared::new(1, TypeId::of::<()>()));
        runtime_internal::request_context_from_deferred(runtime_internal::deferred_from_handle(
            runtime_internal::handle_from_shared(shared),
        ))
    }

    fn handle() -> tina::CallHandle<u32> {
        runtime_internal::call_handle_from_shared(Arc::new(CallHandleShared::new(
            TypeId::of::<u32>(),
        )))
    }

    fn operation(keys: &[u8]) -> (ScatterGather<u8, u32, ()>, Vec<(u8, CallGroupToken)>) {
        let config = config(keys.len().max(1));
        let mut calls = CallJoinSet::with_capacity(config.max_targets);
        let mut tokens = Vec::new();
        for key in keys.iter().copied() {
            tokens.push((key, calls.insert(key, handle()).expect("unique key")));
        }
        (
            ScatterGather {
                token: ScatterGatherToken::alloc(),
                config,
                caller: Some(request()),
                targets: keys
                    .iter()
                    .copied()
                    .map(|key| TargetSlot {
                        key,
                        override_outcome: None,
                    })
                    .collect(),
                calls: Some(calls),
                aggregate_expired: false,
            },
            tokens,
        )
    }

    fn config(max_targets: usize) -> ScatterGatherConfig {
        ScatterGatherConfig {
            max_targets,
            collector_capacity: max_targets,
            per_target_timeout: Duration::from_millis(10),
            aggregate_timeout: Duration::from_millis(20),
        }
    }

    #[test]
    fn ordered_report_preserves_every_call_outcome() {
        let (mut operation, tokens) = operation(&[5, 1, 4, 2, 3]);
        let outcomes = [
            CallOutcome::Replied(50),
            CallOutcome::Full,
            CallOutcome::Closed,
            CallOutcome::Timeout,
            CallOutcome::Rejected(tina::CallRejectedReason::UnsupportedMessage),
        ];
        let mut completed = None;
        for ((key, token), outcome) in tokens.into_iter().zip(outcomes) {
            completed = operation
                .record_reply(key, token, outcome)
                .expect("valid branch token");
        }
        let report = completed.expect("all branches complete").report;
        assert_eq!(
            report.outcomes,
            vec![
                (5, ScatterGatherTargetOutcome::Replied(50)),
                (1, ScatterGatherTargetOutcome::Full),
                (4, ScatterGatherTargetOutcome::Closed),
                (2, ScatterGatherTargetOutcome::Timeout),
                (
                    3,
                    ScatterGatherTargetOutcome::Rejected(
                        tina::CallRejectedReason::UnsupportedMessage
                    )
                ),
            ]
        );
    }

    #[test]
    fn aggregate_timeout_preserves_prior_timeout_and_waits_for_exact_cancel_settlement() {
        let (mut operation, tokens) = operation(&[1, 2, 3]);
        assert!(
            operation
                .record_reply(1, tokens[0].1, CallOutcome::Timeout)
                .unwrap()
                .is_none()
        );
        let advance = operation
            .aggregate_timeout::<TestIsolate, _, _>(operation.token(), TestMessage::Cancel)
            .unwrap()
            .expect("current aggregate token advances operation");
        match advance.effect {
            Effect::Batch(effects) => assert_eq!(effects.len(), 2),
            other => panic!("expected bounded cancel batch, got {other:?}"),
        }
        assert!(advance.completed.is_none());

        // A late reply is retained by CallJoinSet for settlement but cannot
        // overwrite the aggregate terminal classification.
        assert!(
            operation
                .record_reply(2, tokens[1].1, CallOutcome::Replied(20))
                .unwrap()
                .is_none()
        );
        assert!(
            operation
                .record_cancel(2, tokens[1].1, CancelOutcome::AlreadyCompleted)
                .unwrap()
                .is_none()
        );
        let completed = operation
            .record_cancel(3, tokens[2].1, CancelOutcome::Cancelled)
            .unwrap()
            .expect("every cancel acknowledged");
        assert_eq!(
            completed.report.outcomes,
            vec![
                (1, ScatterGatherTargetOutcome::Timeout),
                (2, ScatterGatherTargetOutcome::AggregateTimeout),
                (3, ScatterGatherTargetOutcome::AggregateTimeout),
            ]
        );
    }

    #[test]
    fn duplicate_reply_cannot_overwrite_terminal_outcome() {
        let (mut operation, tokens) = operation(&[1, 2]);
        operation
            .record_reply(1, tokens[0].1, CallOutcome::Replied(10))
            .unwrap();
        let duplicate = operation.record_reply(1, tokens[0].1, CallOutcome::Replied(99));
        assert!(matches!(
            duplicate,
            Err(ScatterGatherRecordError::Reply(
                CallSetRecordReplyError::UnknownToken { .. }
            ))
        ));
    }

    #[test]
    fn late_aggregate_timer_cannot_expire_a_newer_operation() {
        let (old, _) = operation(&[1]);
        let old_token = old.token();
        let (mut current, tokens) = operation(&[1]);

        let stale = current
            .aggregate_timeout::<TestIsolate, _, _>(old_token, TestMessage::Cancel)
            .unwrap();
        assert!(stale.is_none());

        let completed = current
            .record_reply(1, tokens[0].1, CallOutcome::Replied(42))
            .unwrap()
            .expect("stale timer left current operation live");
        assert_eq!(
            completed.report.outcomes,
            vec![(1, ScatterGatherTargetOutcome::Replied(42))]
        );
    }

    #[test]
    fn over_cap_and_duplicate_targets_fail_before_effect_batch() {
        let address = Address::new_with_generation(
            ShardId::new(0),
            IsolateId::new(1),
            AddressGeneration::new(0),
        );
        let targets =
            BoundedItems::try_from_iter(2, [(1, Some(address)), (2, Some(address))]).unwrap();
        let builds = Cell::new(0);
        let result = ScatterGather::<u8, u32, ()>::start::<TestIsolate, _, _, _, _, _, _>(
            request(),
            config(1),
            targets,
            |address, timeout| {
                builds.set(builds.get() + 1);
                call_cancelable(address, (), timeout)
            },
            TestMessage::Reply,
            |_| TestMessage::Aggregate,
        );
        let failure = match result {
            Err(failure) => failure,
            Ok(_) => panic!("over-cap scatter unexpectedly started"),
        };
        assert!(failure.request.is_open());
        assert_eq!(builds.get(), 0);
        assert!(matches!(
            failure.error,
            ScatterGatherStartError::TooManyTargets {
                max: 1,
                observed: 2
            }
        ));

        let duplicate_address = Address::new_with_generation(
            ShardId::new(0),
            IsolateId::new(2),
            AddressGeneration::new(0),
        );
        let duplicate_targets = BoundedItems::try_from_iter(
            2,
            [(7, Some(duplicate_address)), (7, Some(duplicate_address))],
        )
        .unwrap();
        let duplicate_builds = Cell::new(0);
        let duplicate = ScatterGather::<u8, u32, ()>::start::<TestIsolate, _, _, _, _, _, _>(
            request(),
            config(2),
            duplicate_targets,
            |address, timeout| {
                duplicate_builds.set(duplicate_builds.get() + 1);
                call_cancelable(address, (), timeout)
            },
            TestMessage::Reply,
            |_| TestMessage::Aggregate,
        );
        let failure = match duplicate {
            Err(failure) => failure,
            Ok(_) => panic!("duplicate scatter unexpectedly started"),
        };
        assert!(failure.request.is_open());
        assert_eq!(duplicate_builds.get(), 0);
        assert!(matches!(
            failure.error,
            ScatterGatherStartError::DuplicateTarget(7)
        ));
    }

    #[test]
    fn config_and_effect_capacity_failures_return_untouched_caller() {
        let empty = BoundedItems::try_from_iter(1, std::iter::empty::<(u8, Option<Address<()>>)>())
            .unwrap();
        let invalid = match ScatterGather::<u8, u32, ()>::start::<TestIsolate, _, (), _, _, _, _>(
            request(),
            config(0),
            empty,
            |_address, _timeout| unreachable!("invalid config cannot build calls"),
            TestMessage::Reply,
            |_| TestMessage::Aggregate,
        ) {
            Err(failure) => failure,
            Ok(_) => panic!("zero max targets unexpectedly started"),
        };
        assert!(invalid.request.is_open());
        assert!(matches!(
            invalid.error,
            ScatterGatherStartError::Config(ScatterGatherConfigError::MaxTargetsZero)
        ));

        let overflow_config = ScatterGatherConfig {
            max_targets: usize::MAX,
            collector_capacity: usize::MAX,
            per_target_timeout: Duration::from_millis(1),
            aggregate_timeout: Duration::from_millis(1),
        };
        let empty = BoundedItems::try_from_iter(1, std::iter::empty::<(u8, Option<Address<()>>)>())
            .unwrap();
        let overflow = match ScatterGather::<u8, u32, ()>::start::<TestIsolate, _, (), _, _, _, _>(
            request(),
            overflow_config,
            empty,
            |_address, _timeout| unreachable!("capacity overflow cannot build calls"),
            TestMessage::Reply,
            |_| TestMessage::Aggregate,
        ) {
            Err(failure) => failure,
            Ok(_) => panic!("overflowing effect capacity unexpectedly started"),
        };
        assert!(overflow.request.is_open());
        assert!(matches!(
            overflow.error,
            ScatterGatherStartError::EffectCapacityOverflow
        ));

        let (operation, _) = operation(&[1]);
        let mut operations = ScatterGatherOperations::with_capacity(1);
        operations.operations.push(operation);
        let empty = BoundedItems::try_from_iter(1, std::iter::empty::<(u8, Option<Address<()>>)>())
            .unwrap();
        let full = match operations.start_service::<TestService, _, (), _, _, _, _>(
            request(),
            config(1),
            empty,
            |_address, _timeout| unreachable!("full owner cannot build calls"),
            TestServiceEvent::Scatter,
        ) {
            Err(failure) => failure,
            Ok(_) => panic!("full operation owner unexpectedly started"),
        };
        assert!(full.request.is_open());
        assert!(matches!(
            full.error,
            ScatterGatherStartError::OperationsFull { max: 1 }
        ));
    }

    #[test]
    fn bounded_owner_returns_late_events_and_ignores_only_stale_timer() {
        let (operation, tokens) = operation(&[1]);
        let operation_token = operation.token();
        let branch_token = tokens[0].1;
        let mut operations = ScatterGatherOperations::with_capacity(1);
        operations.operations.push(operation);

        let completed = operations
            .advance_service::<TestService, _, _, _>(
                ScatterGatherEvent::Reply(
                    operation_token,
                    1,
                    branch_token,
                    CallOutcome::Replied(7),
                ),
                TestServiceEvent::Scatter,
            )
            .unwrap()
            .expect("known operation advances")
            .completed
            .expect("only branch completed");
        assert_eq!(
            completed.report.outcomes,
            vec![(1, ScatterGatherTargetOutcome::Replied(7))]
        );
        assert!(operations.is_empty());

        let late = operations.advance_service::<TestService, _, _, _>(
            ScatterGatherEvent::Reply(operation_token, 1, branch_token, CallOutcome::Replied(9)),
            TestServiceEvent::Scatter,
        );
        assert!(matches!(
            late,
            Err(ScatterGatherOperationsError::UnknownOperation(
                ScatterGatherEvent::Reply(_, 1, _, CallOutcome::Replied(9))
            ))
        ));
        assert!(
            operations
                .advance_service::<TestService, _, _, _>(
                    ScatterGatherEvent::AggregateTimeout(operation_token),
                    TestServiceEvent::Scatter,
                )
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn bounded_owner_withholds_caller_until_every_cancel_ack() {
        let (operation, tokens) = operation(&[1, 2]);
        let operation_token = operation.token();
        let mut operations = ScatterGatherOperations::with_capacity(1);
        operations.operations.push(operation);

        let advance = operations
            .advance_service::<TestService, _, _, _>(
                ScatterGatherEvent::AggregateTimeout(operation_token),
                TestServiceEvent::Scatter,
            )
            .unwrap()
            .expect("current timer advances");
        assert!(advance.completed.is_none());
        assert!(matches!(advance.effect, Effect::Batch(ref effects) if effects.len() == 2));

        let first = operations
            .advance_service::<TestService, _, _, _>(
                ScatterGatherEvent::Cancelled(
                    operation_token,
                    1,
                    tokens[0].1,
                    CancelOutcome::Cancelled,
                ),
                TestServiceEvent::Scatter,
            )
            .unwrap()
            .expect("first ack advances");
        assert!(first.completed.is_none());
        assert_eq!(operations.len(), 1);

        let completed = operations
            .advance_service::<TestService, _, _, _>(
                ScatterGatherEvent::Cancelled(
                    operation_token,
                    2,
                    tokens[1].1,
                    CancelOutcome::Cancelled,
                ),
                TestServiceEvent::Scatter,
            )
            .unwrap()
            .expect("second ack advances")
            .completed
            .expect("all cancel truth returns caller");
        assert!(completed.request.is_open());
        assert!(operations.is_empty());
    }
}
