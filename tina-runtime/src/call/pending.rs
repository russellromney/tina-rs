//! Cancelable-call admission types: `CancelableCall` and the
//! `PendingCancelableCallSet` family.
//!
//! Owns the move-only ticketed pending storage used by services that
//! need to admit one runtime-owned call per key and cancel it later.

use std::sync::Arc;
use std::time::Duration;

use tina::{Address, CallHandleShared};

use super::cancel_call;
use super::{CallOutcome, CancelOutcome, RuntimeCall};

/// Prepared isolate-call helper that also produces a caller-owned
/// [`CallHandle`] for cancellation, returned by [`call_cancelable`].
#[doc(hidden)]
pub struct CancelableCall<T, R> {
    destination: Address<T, R>,
    message: T,
    timeout: Duration,
    marker: std::marker::PhantomData<fn() -> R>,
}

/// Compatibility alias for the old cancelable-call builder name.
#[deprecated(since = "0.1.0", note = "use CancelableCall")]
pub type IsolateCallWithHandle<T, R> = CancelableCall<T, R>;

/// Prepared cancelable continuation after caller authority was captured.
#[doc(hidden)]
pub struct DeferredCancelableCall<T, R, Q> {
    inner: CancelableCall<T, R>,
    request: tina::RequestContext<Q>,
}

/// Request-effect wrapper for [`DeferredCancelableCall`].
#[doc(hidden)]
pub struct RequestDeferredCancelableCall<T, R, Q> {
    inner: DeferredCancelableCall<T, R, Q>,
}

/// Visible pending caller obligation for cancelable deferred work.
///
/// Store this in isolate state until the worker returns or the operation is
/// cancelled. The token owns both the caller request and the cancel handle so
/// neither branch loses the authority needed to answer the original caller.
#[must_use = "store this pending token, then complete or cancel it"]
#[derive(Debug)]
pub struct PendingCancelableCall<K, Q, R> {
    pub(super) key: K,
    pub(super) ticket: PendingCancelableTicket,
    pub(super) request: tina::RequestContext<Q>,
    pub(super) handle: tina::CallHandle<R>,
}

/// Per-token witness for a cancelable pending call.
///
/// The user key names the domain operation (`job_id`, `worker_slot`,
/// `request_id`). The ticket names this exact admitted instance of that key.
/// Pair both values when removing a stored [`PendingCancelableCall`]. This
/// prevents an old worker-return or cancel path from removing a newer call
/// that reused the same key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PendingCancelableTicket(pub(super) u64);

/// One active cancelable request per key.
///
/// Use this when the user's job is "this key has at most one in-flight
/// request right now." A second attempt with the same key is a
/// `DuplicateKey` rejection — caller authority is returned unchanged so
/// the service can reply `Busy` or `AlreadyRunning` immediately. For the
/// "many concurrent attempts per key" shape, reach for
/// [`crate::CancelableWork`] instead.
///
/// Bounded fixed-capacity storage for [`PendingCancelableCall`] tokens.
///
/// This helper is deliberately only storage. It never dispatches the child
/// effect and never cancels child work by itself. Prefer
/// [`DeferredCancelableCall::try_admit`] so the child effect is returned only
/// after this set accepts the token. If insertion fails, the error returns the
/// token so the caller can recover authority with
/// [`PendingCancelableCall::into_request_context`] and answer immediately.
///
/// Visible truth that stays on the surface:
///
/// - admission is `try_insert(token)` and returns `Full` or `DuplicateKey`
///   typed errors; caller authority comes back inside the rejected token.
/// - completion is `remove(key, ticket)` — the ticket is the exact instance,
///   so a stale worker-return cannot remove a newer call that reused the key.
/// - drain replies to every parked caller on stop; no implicit cancellation.
#[derive(Debug)]
pub struct PendingCancelableCallSet<K, Q, R> {
    entries: Vec<PendingCancelableEntry<K, Q, R>>,
    capacity: usize,
}

#[derive(Debug)]
struct PendingCancelableEntry<K, Q, R> {
    ticket: PendingCancelableTicket,
    token: PendingCancelableCall<K, Q, R>,
}

/// Reasons [`PendingCancelableCallSet::try_insert`] may reject admission.
#[derive(Debug)]
pub enum PendingCancelableInsertError<K, Q, R> {
    /// The set is at its configured capacity.
    Full {
        /// Rejected token. Recover caller authority from this value.
        token: PendingCancelableCall<K, Q, R>,
    },
    /// A pending call is already stored under this key.
    DuplicateKey {
        /// Rejected token. Recover caller authority from this value.
        token: PendingCancelableCall<K, Q, R>,
    },
}

/// Reasons [`PendingCancelableCallSet::remove`] may not find an exact entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingCancelableRemoveError {
    /// No entry exists for the supplied key.
    MissingKey,
    /// The key exists, but the ticket belongs to an older or newer call.
    StaleTicket,
}

impl<K, Q, R> PendingCancelableCallSet<K, Q, R>
where
    K: PartialEq,
{
    /// Builds an empty bounded set.
    ///
    /// Panics when `capacity == 0`: a zero-capacity pending table would reject
    /// every cancelable call and usually means the service was misconfigured.
    pub fn with_capacity(capacity: usize) -> Self {
        assert!(
            capacity > 0,
            "PendingCancelableCallSet requires capacity > 0; a zero-capacity set rejects every insert",
        );
        Self {
            entries: Vec::with_capacity(capacity),
            capacity,
        }
    }

    /// Returns the configured capacity.
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Returns the number of admitted pending calls.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns whether the set holds no pending calls.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns whether the next non-duplicate insert would be rejected as full.
    pub fn is_full(&self) -> bool {
        self.entries.len() >= self.capacity
    }

    /// Returns whether `key` is already present.
    pub fn contains_key(&self, key: &K) -> bool {
        self.entries.iter().any(|entry| &entry.token.key == key)
    }

    /// Returns the current ticket for `key`, if present.
    ///
    /// Use this for current owner decisions such as "cancel the pending
    /// operation under this key." Completion continuations should carry the
    /// ticket they received from [`DeferredCancelableCall::try_admit`] instead
    /// of looking up the current ticket, so stale completions cannot remove a
    /// newer entry.
    pub fn ticket(&self, key: &K) -> Option<PendingCancelableTicket> {
        self.entries
            .iter()
            .find(|entry| &entry.token.key == key)
            .map(|entry| entry.ticket)
    }

    /// Attempts to admit `token` under its own key.
    ///
    /// On success the returned ticket must be included in the completion or
    /// cancel continuation. On `Full` or `DuplicateKey`, the set is unchanged
    /// and the rejected token is returned for immediate caller settlement.
    pub fn try_insert(
        &mut self,
        token: PendingCancelableCall<K, Q, R>,
    ) -> Result<PendingCancelableTicket, PendingCancelableInsertError<K, Q, R>> {
        if self.contains_key(&token.key) {
            return Err(PendingCancelableInsertError::DuplicateKey { token });
        }
        if self.is_full() {
            return Err(PendingCancelableInsertError::Full { token });
        }

        let ticket = token.ticket;
        self.entries.push(PendingCancelableEntry { ticket, token });
        Ok(ticket)
    }

    /// Removes and returns the exact pending token for `(key, ticket)`.
    pub fn remove(
        &mut self,
        key: &K,
        ticket: PendingCancelableTicket,
    ) -> Result<PendingCancelableCall<K, Q, R>, PendingCancelableRemoveError> {
        let Some(pos) = self
            .entries
            .iter()
            .position(|entry| &entry.token.key == key)
        else {
            return Err(PendingCancelableRemoveError::MissingKey);
        };
        if self.entries[pos].ticket != ticket {
            return Err(PendingCancelableRemoveError::StaleTicket);
        }
        Ok(self.entries.swap_remove(pos).token)
    }

    /// Drains every stored token, freeing all capacity.
    pub fn drain(&mut self) -> impl Iterator<Item = PendingCancelableCall<K, Q, R>> + '_ {
        self.entries.drain(..).map(|entry| entry.token)
    }
}

impl<T, R> CancelableCall<T, R>
where
    T: Send + 'static,
    R: 'static,
{
    pub(super) fn new(destination: Address<T, R>, message: T, timeout: Duration) -> Self {
        Self {
            destination,
            message,
            timeout,
            marker: std::marker::PhantomData,
        }
    }

    /// Returns `(effect, handle)`. Store the handle in isolate state
    /// and return the effect; runtime stamps `CallId` on dispatch.
    #[deprecated(
        since = "0.1.0",
        note = "use `.then(...)` for ordinary continuations; use `call_ctx.defer(work).reply(...)` in handle_call when preserving caller authority"
    )]
    pub fn reply<I, F, M>(self, translator: F) -> (tina::Effect<I>, tina::CallHandle<R>)
    where
        I: tina::Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(CallOutcome<R>) -> M + 'static,
        M: 'static,
    {
        self.then(translator)
    }

    /// Returns `(effect, handle)` for one ordinary continuation. Store the
    /// handle in isolate state and return the effect; runtime stamps `CallId`
    /// on dispatch.
    pub fn then<I, F, M>(self, translator: F) -> (tina::Effect<I>, tina::CallHandle<R>)
    where
        I: tina::Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(CallOutcome<R>) -> M + 'static,
        M: 'static,
    {
        let shared = Arc::new(CallHandleShared::new(std::any::TypeId::of::<R>()));
        let effect = tina::Effect::Call(RuntimeCall::isolate_call_with_handle(
            self.destination,
            self.message,
            self.timeout,
            translator,
            shared.clone(),
        ));
        let handle = tina::runtime_internal::call_handle_from_shared::<R>(shared);
        (effect, handle)
    }

    /// Like [`reply`](Self::reply), but also carries the caller's
    /// [`RequestContext`] into the continuation message.
    #[deprecated(
        since = "0.1.0",
        note = "use `call_ctx.defer_cancelable(call_cancelable(...)).try_admit(...)` when preserving caller authority; hiding RequestContext in a cancelable continuation can strand the caller"
    )]
    pub fn reply_with_request<I, F, M, Q>(
        self,
        req: tina::RequestContext<Q>,
        translator: F,
    ) -> (tina::Effect<I>, tina::CallHandle<R>)
    where
        I: tina::Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(tina::RequestContext<Q>, CallOutcome<R>) -> M + 'static,
        M: 'static,
        Q: 'static,
    {
        self.then_with_request_inner(req, translator)
    }

    /// Carries a request context into the worker-return continuation.
    ///
    /// Prefer [`CallContext::defer_cancelable`](tina::CallContext::defer_cancelable)
    /// when starting from `handle_call`. A cancelable call can settle via a
    /// cancel acknowledgement instead of the worker-return continuation; the
    /// deferred helper keeps the request context in a visible pending token so
    /// either path can answer the caller.
    #[deprecated(
        since = "0.1.0",
        note = "use `call_ctx.defer_cancelable(call_cancelable(...)).try_admit(...)` when preserving caller authority; hiding RequestContext in a cancelable continuation can strand the caller"
    )]
    pub fn then_with_request<I, F, M, Q>(
        self,
        req: tina::RequestContext<Q>,
        translator: F,
    ) -> (tina::Effect<I>, tina::CallHandle<R>)
    where
        I: tina::Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(tina::RequestContext<Q>, CallOutcome<R>) -> M + 'static,
        M: 'static,
        Q: 'static,
    {
        self.then_with_request_inner(req, translator)
    }

    fn then_with_request_inner<I, F, M, Q>(
        self,
        req: tina::RequestContext<Q>,
        translator: F,
    ) -> (tina::Effect<I>, tina::CallHandle<R>)
    where
        I: tina::Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(tina::RequestContext<Q>, CallOutcome<R>) -> M + 'static,
        M: 'static,
        Q: 'static,
    {
        let shared = Arc::new(CallHandleShared::new(std::any::TypeId::of::<R>()));
        let effect = tina::Effect::Call(RuntimeCall::isolate_call_with_handle(
            self.destination,
            self.message,
            self.timeout,
            move |outcome| translator(req, outcome),
            shared.clone(),
        ));
        let handle = tina::runtime_internal::call_handle_from_shared::<R>(shared);
        (effect, handle)
    }
}

impl<T, R, Q> DeferredCancelableCall<T, R, Q>
where
    T: Send + 'static,
    R: 'static,
    Q: 'static,
{
    /// Builds the worker-return continuation and the pending token that must be
    /// stored by the service while the child call is in flight. The
    /// continuation receives the token ticket so stale completions can be
    /// rejected with [`PendingCancelableCallSet::remove`].
    ///
    /// Prefer [`DeferredCancelableCall::try_admit`] when using
    /// [`PendingCancelableCallSet`]. This lower-level form returns the child
    /// effect before admission so callers must store the pending token before
    /// returning the effect.
    pub fn reply<I, F, M, K>(
        self,
        key: K,
        translator: F,
    ) -> (PendingCancelableCall<K, Q, R>, tina::Effect<I>)
    where
        I: tina::Isolate<Message = M, Reply = Q, Call = RuntimeCall<M>>,
        F: FnOnce(K, PendingCancelableTicket, CallOutcome<R>) -> M + 'static,
        K: Clone + 'static,
        M: 'static,
    {
        self.reply_with_ticket(key, translator)
    }

    /// Builds the worker-return continuation and includes the pending token's
    /// ticket in that continuation.
    ///
    /// Prefer [`DeferredCancelableCall::try_admit`] when using
    /// [`PendingCancelableCallSet`]. This lower-level form is useful when
    /// admission is intentionally hand-written, but callers must store the
    /// pending token before returning the effect.
    pub fn reply_with_ticket<I, F, M, K>(
        self,
        key: K,
        translator: F,
    ) -> (PendingCancelableCall<K, Q, R>, tina::Effect<I>)
    where
        I: tina::Isolate<Message = M, Reply = Q, Call = RuntimeCall<M>>,
        F: FnOnce(K, PendingCancelableTicket, CallOutcome<R>) -> M + 'static,
        K: Clone + 'static,
        M: 'static,
    {
        let ticket = PendingCancelableTicket(self.request.slot_id());
        let continuation_key = key.clone();
        let (effect, handle) = self
            .inner
            .then(move |outcome| translator(continuation_key, ticket, outcome));
        let pending = PendingCancelableCall {
            key,
            ticket,
            request: self.request,
            handle,
        };
        (pending, effect)
    }

    /// Admits the pending token into bounded storage and returns the child
    /// effect only after admission succeeds.
    ///
    /// This is the preferred spelling when using
    /// [`PendingCancelableCallSet`]. It keeps the storage decision explicit
    /// while removing the easy-to-copy bug where user code returns the child
    /// effect before storing the pending token. On `Full` or `DuplicateKey`,
    /// the error owns the pending token so the caller can recover authority
    /// with [`PendingCancelableCall::into_request_context`] and answer now.
    pub fn try_admit<I, F, M, K>(
        self,
        pending: &mut PendingCancelableCallSet<K, Q, R>,
        key: K,
        translator: F,
    ) -> Result<tina::Effect<I>, PendingCancelableInsertError<K, Q, R>>
    where
        I: tina::Isolate<Message = M, Reply = Q, Call = RuntimeCall<M>>,
        F: FnOnce(K, PendingCancelableTicket, CallOutcome<R>) -> M + 'static,
        K: Clone + PartialEq + 'static,
        M: 'static,
    {
        let (token, effect) = self.reply_with_ticket(key, translator);
        pending.try_insert(token).map(|_| effect)
    }
}

impl<T, R, Q> RequestDeferredCancelableCall<T, R, Q>
where
    T: Send + 'static,
    R: 'static,
    Q: 'static,
{
    /// Builds the worker-return continuation and the pending token.
    ///
    /// Prefer [`Self::try_admit`] so the child effect is returned only after
    /// bounded pending storage accepts the token.
    pub fn reply<I, F, M, K>(
        self,
        key: K,
        translator: F,
    ) -> (PendingCancelableCall<K, Q, R>, tina::RequestEffect<I>)
    where
        I: tina::Isolate<Message = M, Reply = Q, Call = RuntimeCall<M>>,
        F: FnOnce(K, PendingCancelableTicket, CallOutcome<R>) -> M + 'static,
        K: Clone + 'static,
        M: 'static,
    {
        let (token, effect) = self.inner.reply(key, translator);
        (
            token,
            crate::call::request_effect_from_consumed_effect(effect),
        )
    }

    /// Admits the pending token into bounded storage and returns the child
    /// request effect only after admission succeeds.
    pub fn try_admit<I, F, M, K>(
        self,
        pending: &mut PendingCancelableCallSet<K, Q, R>,
        key: K,
        translator: F,
    ) -> Result<tina::RequestEffect<I>, PendingCancelableInsertError<K, Q, R>>
    where
        I: tina::Isolate<Message = M, Reply = Q, Call = RuntimeCall<M>>,
        F: FnOnce(K, PendingCancelableTicket, CallOutcome<R>) -> M + 'static,
        K: Clone + PartialEq + 'static,
        M: 'static,
    {
        self.inner
            .try_admit(pending, key, translator)
            .map(crate::call::request_effect_from_consumed_effect)
    }
}

impl<T, R, I> tina::DeferCancelableThrough<I> for CancelableCall<T, R>
where
    T: Send + 'static,
    R: 'static,
    I: tina::Isolate,
    I::Reply: 'static,
{
    type DeferredCancelable = DeferredCancelableCall<T, R, I::Reply>;

    fn defer_cancelable_through(self, call: tina::CallContext<'_, I>) -> Self::DeferredCancelable {
        DeferredCancelableCall {
            inner: self,
            request: call.into_request_context(),
        }
    }
}

impl<T, R, I> tina::RequestDeferCancelableThrough<I> for CancelableCall<T, R>
where
    T: Send + 'static,
    R: 'static,
    I: tina::Isolate,
    I::Reply: 'static,
{
    type RequestDeferredCancelable = RequestDeferredCancelableCall<T, R, I::Reply>;

    fn defer_cancelable_request_through(
        self,
        call: tina::RequestCall<'_, I>,
    ) -> Self::RequestDeferredCancelable {
        RequestDeferredCancelableCall {
            inner: <Self as tina::DeferCancelableThrough<I>>::defer_cancelable_through(
                self,
                call.into_call_context(),
            ),
        }
    }
}

impl<K, Q, R> PendingCancelableCall<K, Q, R>
where
    K: 'static,
    Q: 'static,
    R: 'static,
{
    /// Returns the user key associated with this pending operation.
    pub fn key(&self) -> &K {
        &self.key
    }

    /// Returns the per-token ticket that must accompany completion removal.
    pub fn ticket(&self) -> PendingCancelableTicket {
        self.ticket
    }

    /// Consumes the pending token and returns its request context.
    pub fn into_request_context(self) -> tina::RequestContext<Q> {
        self.request
    }

    /// Crate-private accessor for the typed call handle inside the token.
    /// Used by [`crate::scope::DeferredScopedCall::try_admit`] to clone the
    /// shared cell before registering the rail into a [`RequestScope`].
    pub(crate) fn handle_ref(&self) -> &tina::CallHandle<R> {
        &self.handle
    }

    /// Cancels the child wait and carries the request context into the cancel
    /// continuation so the service can explicitly answer its caller.
    pub fn cancel<I, F, M>(self, translator: F) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(K, tina::RequestContext<Q>, CancelOutcome) -> M + 'static,
        M: 'static,
    {
        cancel_call(self.handle).then(move |outcome| translator(self.key, self.request, outcome))
    }
}
