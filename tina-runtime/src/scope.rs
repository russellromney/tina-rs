//! Request-scoped cancellation.
//!
//! A request is a tree. When the request dies, its children should stop
//! waiting. This module supplies the bookkeeping that makes that pattern a
//! first-class Tina primitive instead of every service inventing a registry.
//!
//! ## Vocabulary
//!
//! - [`RequestScopeId`] — runtime-stable identifier for one scope.
//! - [`RequestScope`] — bounded child registry plus a cancellation flag.
//! - [`ScopedCallHandle`] — typed handle that was registered into a scope.
//!   Storing it on the scope is what teaches the scope which rail to cancel.
//! - [`RequestScopeSet`] — bounded fixed-cap storage of in-flight scopes,
//!   used by services that admit many concurrent requests.
//! - [`ScopeCancelCause`] — why a scope was cancelled. Visible in the report.
//! - [`ScopeCancelReport`] — synchronous summary returned by
//!   [`RequestScope::cancel_into_effects`] (cause, child rows, capacity).
//!
//! ## Cancellation honesty
//!
//! Scope cancellation closes Tina-owned waits and reclaims caller capacity.
//! It does not stop external work that a bridge already accepted; a late
//! completion still becomes a visible trace fact
//! (`CallReplyRejected` / worker-terminal) instead of a ghost.
//!
//! ## Single-shard
//!
//! Scopes are owned by the isolate that runs the request. Cross-shard
//! cancel is rejected by the underlying cancel rail
//! ([`CancelOutcome::WrongShard`](tina::CancelOutcome)); the scope itself
//! never tries to span shards.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tina::{CallHandle, CallHandleShared, CallHandleState, CancelOutcome, Effect, Isolate};

use crate::call::{CallOutcome, RuntimeCall};

/// Stable identifier for one request scope.
///
/// Allocated via [`RequestScopeId::alloc`] (process-monotonic). Two scopes
/// in the same process never share an id; a fresh request always gets a
/// fresh id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestScopeId(u64);

static NEXT_SCOPE_ID: AtomicU64 = AtomicU64::new(1);

impl RequestScopeId {
    /// Returns a fresh, process-monotonic scope identifier.
    pub fn alloc() -> Self {
        Self(NEXT_SCOPE_ID.fetch_add(1, Ordering::Relaxed))
    }

    /// Constructs a scope id from a raw integer. Use only in tests or when
    /// adopting an externally-assigned identifier.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw scope identifier.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Why a request scope was cancelled. Mirrors the rails a service might wire
/// the scope into. Visible in [`ScopeCancelReport`] and in trace facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScopeCancelCause {
    /// HTTP client disconnect, WebSocket close, or peer reset mid-request.
    ClientDisconnect,
    /// Caller explicitly cancelled the request (e.g. user pressed stop).
    CallerCancelled,
    /// Per-request deadline elapsed.
    Timeout,
    /// Owning isolate stopped, or the server's drain deadline fired.
    OwnerStopped,
    /// Application-defined cancel reason; service code chose this.
    User,
}

/// Reason [`RequestScope::register`] declined to take ownership of a handle.
///
/// The error returns the typed handle so the caller can recover authority
/// (cancel it directly, store it elsewhere, or drop it deliberately).
#[derive(Debug)]
pub enum ScopeRegisterError<R> {
    /// The scope is already cancelled. The handle should be cancelled
    /// directly or replied to with an error so the caller is not stranded.
    Cancelled {
        /// Reason the scope was cancelled.
        cause: ScopeCancelCause,
        /// Handle the caller passed in. Recover authority and answer the
        /// original caller deliberately.
        handle: CallHandle<R>,
    },
    /// The scope's child cap is full. Usually means a service-level
    /// admission bug (request opened more child rails than were budgeted).
    Full {
        /// Configured child cap.
        cap: usize,
        /// Handle the caller passed in.
        handle: CallHandle<R>,
    },
}

/// Per-child row in [`ScopeCancelReport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeChildReport {
    /// Service-supplied label such as `"tcp_read"`, `"sleep"`, or
    /// `"pool_acquire"`. Carried into the cancel ack translator so the
    /// service can route on rail name without coupling to typed reply
    /// shapes.
    pub label: &'static str,
    /// Handle state observed at cancel time. Useful for distinguishing
    /// "already finished" rails from "still waiting" rails in trace.
    pub state_at_cancel: CallHandleState,
}

/// Synchronous summary returned by [`RequestScope::cancel_into_effects`].
///
/// The scope itself knows what was registered and which rails were still
/// pending. Late completions still flow back as ordinary trace facts
/// (`CallReplyRejected`, worker-terminal), not via this report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeCancelReport {
    /// The scope that was cancelled.
    pub scope_id: RequestScopeId,
    /// Why the scope was cancelled.
    pub cause: ScopeCancelCause,
    /// One row per registered child.
    pub children: Vec<ScopeChildReport>,
    /// Configured child cap, for capacity-report parity with
    /// [`RequestScopeSet`].
    pub child_cap: usize,
}

impl ScopeCancelReport {
    /// Children whose handle was still `Pending` at cancel time.
    pub fn cancelled_count(&self) -> usize {
        self.children
            .iter()
            .filter(|c| matches!(c.state_at_cancel, CallHandleState::Pending))
            .count()
    }

    /// Children whose handle had already settled (succeeded / failed /
    /// timed out) before the cancel reached them. These had no wait to
    /// close; their reply, if any, is delivered or rejected by normal
    /// runtime paths.
    pub fn already_settled_count(&self) -> usize {
        self.children
            .iter()
            .filter(|c| !matches!(c.state_at_cancel, CallHandleState::Pending))
            .count()
    }
}

/// One registered child rail. The scope keeps a clone of the call handle's
/// shared cell so it can issue an idempotent cancel later without taking
/// ownership of the typed handle.
#[derive(Debug)]
struct RegisteredChild {
    label: &'static str,
    shared: Arc<CallHandleShared>,
}

#[derive(Debug, Default)]
struct ScopeState {
    children: Vec<RegisteredChild>,
    cancelled: Option<ScopeCancelCause>,
}

#[derive(Debug)]
struct RequestScopeInner {
    id: RequestScopeId,
    child_cap: usize,
    state: std::sync::Mutex<ScopeState>,
}

/// A request scope.
///
/// Owned by the service isolate that runs the request. Hand out clones to
/// any code that needs to cancel the scope (HTTP listener on disconnect,
/// a deadline timer, a user-facing `Cancel` button).
///
/// ```text
/// // Service owns scope_set; per-request scope lives inside one entry.
/// let scope = RequestScope::with_child_cap(RequestScopeId::alloc(), 4);
/// scope_set.try_insert(scope.clone(), MyKey(req_id))?;
/// // ... admit children via defer_scoped ...
/// // On client disconnect:
/// let (report, effects) = scope.cancel_into_effects(
///     ScopeCancelCause::ClientDisconnect,
///     |id, label, outcome| Msg::ScopeChildCancelled { id, label, outcome },
/// );
/// ```
#[derive(Debug, Clone)]
pub struct RequestScope {
    inner: Arc<RequestScopeInner>,
}

impl RequestScope {
    /// Builds a fresh scope with the supplied child cap.
    ///
    /// `child_cap` is the maximum number of child rails the scope may
    /// register. A request that wants three child operations should pass
    /// `3`, not "some large number." Hitting the cap means the service
    /// admitted more concurrent child rails than it budgeted; the
    /// admission helper will refuse the extra one.
    ///
    /// Panics when `child_cap == 0`: a zero-cap scope refuses every
    /// register, which is never what the service wants.
    pub fn with_child_cap(id: RequestScopeId, child_cap: usize) -> Self {
        assert!(
            child_cap > 0,
            "RequestScope requires child_cap > 0; a zero-cap scope rejects every register",
        );
        Self {
            inner: Arc::new(RequestScopeInner {
                id,
                child_cap,
                state: std::sync::Mutex::new(ScopeState::default()),
            }),
        }
    }

    /// Returns this scope's identifier.
    pub fn id(&self) -> RequestScopeId {
        self.inner.id
    }

    /// Returns the configured child cap.
    pub fn child_cap(&self) -> usize {
        self.inner.child_cap
    }

    /// Returns the number of children currently registered.
    pub fn registered(&self) -> usize {
        self.inner.state.lock().unwrap().children.len()
    }

    /// Returns `true` once the scope has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.inner.state.lock().unwrap().cancelled.is_some()
    }

    /// Returns the cancel cause once the scope has been cancelled.
    pub fn cancel_cause(&self) -> Option<ScopeCancelCause> {
        self.inner.state.lock().unwrap().cancelled
    }

    /// Registers a child rail for scope-wide cancellation.
    ///
    /// The scope keeps a clone of the handle's shared cell so a later
    /// [`Self::cancel_into_effects`] can close the wait without taking
    /// ownership of the typed handle. The typed handle stays with the
    /// service (usually stored in a [`PendingCancelableCallSet`] via the
    /// returned token from `defer_cancelable`), so worker-return paths
    /// still answer the caller through the existing rail.
    ///
    /// On `Cancelled` or `Full`, the typed handle is returned in the
    /// error so the service can recover authority without stranding the
    /// caller.
    pub fn register<R: 'static>(
        &self,
        label: &'static str,
        handle: CallHandle<R>,
    ) -> Result<(), ScopeRegisterError<R>> {
        let mut state = self.inner.state.lock().unwrap();
        if let Some(cause) = state.cancelled {
            return Err(ScopeRegisterError::Cancelled { cause, handle });
        }
        if state.children.len() >= self.inner.child_cap {
            return Err(ScopeRegisterError::Full {
                cap: self.inner.child_cap,
                handle,
            });
        }
        let shared = tina::runtime_internal::call_handle_shared(&handle).clone();
        state.children.push(RegisteredChild { label, shared });
        // The typed handle is intentionally dropped here: the scope now
        // owns the cancellation capability through the shared cell, and
        // the typed handle's primary purpose (`cancel_call(handle)`) is
        // a subset of what the scope can do. Worker-return ownership
        // lives in `PendingCancelableCall::handle`, not here.
        drop(handle);
        Ok(())
    }

    /// Registers an already-erased shared cell. Used by helpers that
    /// already cloned the shared cell from a typed handle and want to
    /// keep the typed handle on a [`PendingCancelableCall`] token.
    ///
    /// Service code usually wires this through
    /// [`crate::DeferredCancelableCall::reply_into_scope`] instead of
    /// calling this directly.
    pub fn register_shared(
        &self,
        label: &'static str,
        shared: Arc<CallHandleShared>,
    ) -> Result<(), ScopeRegisterSharedError> {
        let mut state = self.inner.state.lock().unwrap();
        if let Some(cause) = state.cancelled {
            return Err(ScopeRegisterSharedError::Cancelled { cause });
        }
        if state.children.len() >= self.inner.child_cap {
            return Err(ScopeRegisterSharedError::Full {
                cap: self.inner.child_cap,
            });
        }
        state.children.push(RegisteredChild { label, shared });
        Ok(())
    }

    /// Marks the scope cancelled and turns every registered child into a
    /// cancel effect.
    ///
    /// The first cancel sets the cause. A second cancel is a no-op: the
    /// scope stays cancelled with the original cause, and an empty
    /// effects list is returned with a report whose `children` is empty.
    ///
    /// `translator` is required by the cancel rail; it must be `Clone`
    /// because each child rail needs its own [`FnOnce`] continuation.
    /// Most application closures that capture `Copy` state satisfy this
    /// automatically.
    pub fn cancel_into_effects<I, F, M>(
        &self,
        cause: ScopeCancelCause,
        translator: F,
    ) -> (ScopeCancelReport, Vec<Effect<I>>)
    where
        I: Isolate<Message = M, Call = RuntimeCall<M>>,
        F: Fn(RequestScopeId, &'static str, CancelOutcome) -> M + Clone + Send + 'static,
        M: 'static,
    {
        let mut state = self.inner.state.lock().unwrap();
        if state.cancelled.is_none() {
            state.cancelled = Some(cause);
        }
        let registered_cause = state.cancelled.expect("just set above if it was None");
        let scope_id = self.inner.id;
        let drained: Vec<RegisteredChild> = state.children.drain(..).collect();
        drop(state);

        let mut rows = Vec::with_capacity(drained.len());
        let mut effects = Vec::with_capacity(drained.len());
        for child in drained {
            let RegisteredChild { label, shared } = child;
            let state_at_cancel = shared.state();
            rows.push(ScopeChildReport {
                label,
                state_at_cancel,
            });
            let translator_for_child = translator.clone();
            effects.push(Effect::Call(RuntimeCall::cancel_call_with_handle(
                shared,
                move |outcome: CancelOutcome| translator_for_child(scope_id, label, outcome),
            )));
        }
        (
            ScopeCancelReport {
                scope_id,
                cause: registered_cause,
                children: rows,
                child_cap: self.inner.child_cap,
            },
            effects,
        )
    }

    /// Marks the scope cancelled without producing cancel effects.
    ///
    /// Use when the scope is being discarded along a path that already
    /// closes the underlying rails (for example, the service is stopping
    /// and the runtime is about to send `OwnerStopped` cancels by itself).
    /// Late completions still appear in trace as ordinary rejected facts.
    pub fn cancel_synchronously(&self, cause: ScopeCancelCause) -> ScopeCancelReport {
        let mut state = self.inner.state.lock().unwrap();
        if state.cancelled.is_none() {
            state.cancelled = Some(cause);
        }
        let registered_cause = state.cancelled.expect("just set above");
        let scope_id = self.inner.id;
        let rows: Vec<ScopeChildReport> = state
            .children
            .drain(..)
            .map(|c| ScopeChildReport {
                label: c.label,
                state_at_cancel: c.shared.state(),
            })
            .collect();
        ScopeCancelReport {
            scope_id,
            cause: registered_cause,
            children: rows,
            child_cap: self.inner.child_cap,
        }
    }
}

/// Reason [`RequestScope::register_shared`] declined.
///
/// Erased counterpart of [`ScopeRegisterError`] — there is no handle to
/// return because the caller already erased the type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeRegisterSharedError {
    /// The scope is already cancelled.
    Cancelled {
        /// Reason the scope was cancelled.
        cause: ScopeCancelCause,
    },
    /// The scope's child cap is full.
    Full {
        /// Configured child cap.
        cap: usize,
    },
}

/// Typed view of a child rail registered into a scope.
///
/// The scope already owns the cancel capability (through the shared
/// cell). `ScopedCallHandle` is the typed worker-return half: it preserves
/// the typed [`CallHandle`] so the service can still consume it inside a
/// [`PendingCancelableCall`] for the worker-return path.
///
/// Construct via [`scope_register`] (lowercase free function) or by hand
/// when wiring a custom rail.
#[must_use = "store the typed handle on the worker-return path (e.g., \
              PendingCancelableCall) — dropping it lets the call run to \
              completion outside the scope"]
#[derive(Debug)]
pub struct ScopedCallHandle<R> {
    scope_id: RequestScopeId,
    handle: CallHandle<R>,
}

impl<R: 'static> ScopedCallHandle<R> {
    /// Returns the scope this handle was registered into.
    pub fn scope_id(&self) -> RequestScopeId {
        self.scope_id
    }

    /// Returns the typed call handle.
    ///
    /// Use this to store the handle on a [`PendingCancelableCall`] for
    /// the worker-return path. The scope still owns a clone of the
    /// shared cell, so the scope can cancel later regardless.
    pub fn into_handle(self) -> CallHandle<R> {
        self.handle
    }

    /// Returns the typed call handle without consuming the wrapper.
    pub fn handle(&self) -> &CallHandle<R> {
        &self.handle
    }
}

/// Registers `handle` into `scope` and returns the typed wrapper that
/// preserves worker-return ownership.
///
/// Most service code uses
/// [`DeferredCancelableCall::reply_into_scope`](crate::DeferredCancelableCall::reply_into_scope)
/// instead, which combines admission into the scope with the
/// `PendingCancelableCall` plumbing.
pub fn scope_register<R: 'static>(
    scope: &RequestScope,
    label: &'static str,
    handle: CallHandle<R>,
) -> Result<ScopedCallHandle<R>, ScopeRegisterError<R>> {
    let scope_id = scope.id();
    let shared = tina::runtime_internal::call_handle_shared(&handle).clone();
    match scope.register_shared(label, shared) {
        Ok(()) => Ok(ScopedCallHandle { scope_id, handle }),
        Err(ScopeRegisterSharedError::Cancelled { cause }) => {
            Err(ScopeRegisterError::Cancelled { cause, handle })
        }
        Err(ScopeRegisterSharedError::Full { cap }) => {
            Err(ScopeRegisterError::Full { cap, handle })
        }
    }
}

// ---------- RequestScopeSet ----------

/// Bounded fixed-capacity storage for in-flight request scopes.
///
/// One slot per concurrent request. The key (`K`) is service-defined —
/// `RequestId(u64)`, `(ConnectionId, StreamId)`, a session token, etc.
/// Hitting capacity returns the scope to the caller for explicit handling
/// (reply `Busy`, queue, or shed). Late completions surface through
/// existing trace facts; this set holds no async state of its own.
///
/// This helper is deliberately only storage. It never builds cancel
/// effects on its own; the service hands an entry to
/// [`RequestScope::cancel_into_effects`] when the request dies.
#[derive(Debug)]
pub struct RequestScopeSet<K> {
    entries: Vec<RequestScopeEntry<K>>,
    capacity: usize,
}

#[derive(Debug)]
struct RequestScopeEntry<K> {
    key: K,
    scope: RequestScope,
}

/// Snapshot of [`RequestScopeSet`] capacity at one observation point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestScopeSetCapacityReport {
    /// Number of admitted scopes.
    pub in_use: usize,
    /// Total slot capacity.
    pub capacity: usize,
}

impl RequestScopeSetCapacityReport {
    /// Free slots remaining at observation time.
    pub fn available(self) -> usize {
        self.capacity.saturating_sub(self.in_use)
    }
}

/// Reasons [`RequestScopeSet::try_insert`] may reject admission.
#[derive(Debug)]
pub enum RequestScopeInsertError<K> {
    /// The set is at capacity. The rejected scope and key are returned
    /// so the service can recover authority and answer immediately
    /// (`Busy`, shed, queue, etc.).
    Full {
        /// Rejected key.
        key: K,
        /// Rejected scope.
        scope: RequestScope,
    },
    /// A scope is already stored under this key.
    DuplicateKey {
        /// Rejected key.
        key: K,
        /// Rejected scope.
        scope: RequestScope,
    },
}

/// Reasons [`RequestScopeSet::remove`] may not find an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestScopeRemoveError {
    /// No entry exists for the supplied key.
    MissingKey,
}

impl<K> RequestScopeSet<K>
where
    K: PartialEq,
{
    /// Builds an empty set with fixed `capacity`.
    ///
    /// Panics when `capacity == 0`: zero-capacity scope storage rejects
    /// every admission, which is never the intent.
    pub fn with_capacity(capacity: usize) -> Self {
        assert!(
            capacity > 0,
            "RequestScopeSet requires capacity > 0; a zero-capacity set rejects every insert",
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

    /// Returns the number of admitted scopes.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns whether the set holds no scopes.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns whether the next non-duplicate insert would be rejected.
    pub fn is_full(&self) -> bool {
        self.entries.len() >= self.capacity
    }

    /// Returns whether `key` is already present.
    pub fn contains_key(&self, key: &K) -> bool {
        self.entries.iter().any(|entry| &entry.key == key)
    }

    /// Returns a capacity snapshot suitable for pressure reports.
    pub fn capacity_report(&self) -> RequestScopeSetCapacityReport {
        RequestScopeSetCapacityReport {
            in_use: self.entries.len(),
            capacity: self.capacity,
        }
    }

    /// Returns a clone of the scope under `key`, if any.
    pub fn get(&self, key: &K) -> Option<RequestScope> {
        self.entries
            .iter()
            .find(|entry| &entry.key == key)
            .map(|entry| entry.scope.clone())
    }

    /// Attempts to admit `scope` under `key`.
    ///
    /// On `Full` or `DuplicateKey`, the rejected key and scope are
    /// returned so the service can answer immediately.
    pub fn try_insert(
        &mut self,
        key: K,
        scope: RequestScope,
    ) -> Result<(), RequestScopeInsertError<K>> {
        if self.contains_key(&key) {
            return Err(RequestScopeInsertError::DuplicateKey { key, scope });
        }
        if self.is_full() {
            return Err(RequestScopeInsertError::Full { key, scope });
        }
        self.entries.push(RequestScopeEntry { key, scope });
        Ok(())
    }

    /// Removes the scope stored under `key`, freeing one slot.
    ///
    /// Removal does not cancel the scope. Cancel first if the request
    /// died with children still in flight; remove after the request
    /// settles cleanly (no children pending).
    pub fn remove(&mut self, key: &K) -> Result<RequestScope, RequestScopeRemoveError> {
        let Some(pos) = self.entries.iter().position(|entry| &entry.key == key) else {
            return Err(RequestScopeRemoveError::MissingKey);
        };
        Ok(self.entries.swap_remove(pos).scope)
    }

    /// Drains every scope, freeing all capacity.
    ///
    /// Used by service shutdown after a drain deadline: the caller
    /// usually iterates the result and calls
    /// [`RequestScope::cancel_into_effects`] on each, with cause
    /// [`ScopeCancelCause::OwnerStopped`].
    pub fn drain(&mut self) -> impl Iterator<Item = (K, RequestScope)> + '_ {
        self.entries.drain(..).map(|entry| (entry.key, entry.scope))
    }
}

// ---------- defer_scoped helper ----------

use crate::call::{
    CancelableCall, DeferredCancelableCall, PendingCancelableCall, PendingCancelableCallSet,
    PendingCancelableInsertError, PendingCancelableTicket,
};

/// Builder returned by `CallContext::defer_scoped(scope, work)`.
///
/// Combines [`DeferredCancelableCall`] with a registration into a
/// [`RequestScope`]. The scope-side cancel handle is registered before
/// any child effect is returned, so a cancellation that races with
/// admission still produces a clean answer.
pub struct DeferredScopedCall<T, R, Q> {
    inner: DeferredCancelableCall<T, R, Q>,
    scope: RequestScope,
    label: &'static str,
}

/// Failure modes for [`DeferredScopedCall::try_admit`].
#[derive(Debug)]
pub enum ScopedAdmitError<K, Q, R> {
    /// The scope is already cancelled. The rejected pending token is
    /// returned so the caller can recover authority.
    ScopeCancelled {
        /// Reason the scope was cancelled.
        cause: ScopeCancelCause,
        /// Pending token. Recover authority via
        /// [`PendingCancelableCall::into_request_context`].
        token: PendingCancelableCall<K, Q, R>,
    },
    /// The scope's child cap is full.
    ScopeFull {
        /// Configured child cap.
        cap: usize,
        /// Pending token.
        token: PendingCancelableCall<K, Q, R>,
    },
    /// Underlying [`PendingCancelableCallSet`] admission failed.
    Pending(PendingCancelableInsertError<K, Q, R>),
}

impl<T, R, Q> DeferredScopedCall<T, R, Q>
where
    T: Send + 'static,
    R: 'static,
    Q: 'static,
{
    /// Builds the worker-return pending token and registers the rail
    /// into the scope so a scope-wide cancel will close the wait.
    ///
    /// Order of operations: register the rail's shared cell into the
    /// scope first, then build the pending token. Admission into
    /// [`PendingCancelableCallSet`] is the *gate* — only on success is
    /// the child effect returned. On gate failure, the pending token is
    /// returned in the error so the service can recover authority.
    ///
    /// The scope-side registration is best-effort: if the scope is
    /// already cancelled or full, the caller still owns the typed
    /// handle through the pending token and can settle the request on
    /// its own.
    pub fn try_admit<I, F, M, K>(
        self,
        pending: &mut PendingCancelableCallSet<K, Q, R>,
        key: K,
        translator: F,
    ) -> Result<Effect<I>, ScopedAdmitError<K, Q, R>>
    where
        I: Isolate<Message = M, Reply = Q, Call = RuntimeCall<M>>,
        F: FnOnce(K, PendingCancelableTicket, CallOutcome<R>) -> M + 'static,
        K: Clone + PartialEq + 'static,
        M: 'static,
    {
        let (token, effect) = self.inner.reply_with_ticket(key, translator);

        // Register the shared cell into the scope using a borrow of the
        // typed handle held by the token. The token still owns the typed
        // handle for the worker-return path.
        let shared = tina::runtime_internal::call_handle_shared(token_handle(&token)).clone();
        match self.scope.register_shared(self.label, shared) {
            Ok(()) => {}
            Err(ScopeRegisterSharedError::Cancelled { cause }) => {
                return Err(ScopedAdmitError::ScopeCancelled { cause, token });
            }
            Err(ScopeRegisterSharedError::Full { cap }) => {
                return Err(ScopedAdmitError::ScopeFull { cap, token });
            }
        }

        match pending.try_insert(token) {
            Ok(_ticket) => Ok(effect),
            Err(err) => Err(ScopedAdmitError::Pending(err)),
        }
    }
}

// Borrow the typed handle field of a PendingCancelableCall via the
// crate-private accessor (the field is private to `call.rs`).
fn token_handle<K: 'static, Q: 'static, R: 'static>(
    token: &PendingCancelableCall<K, Q, R>,
) -> &CallHandle<R> {
    token.handle_ref()
}

/// Trait helper so `CallContext::defer_scoped(scope, work)` reads naturally.
///
/// Implemented for the runtime's prepared cancelable work types.
pub trait DeferScopedThrough<I>
where
    I: Isolate,
{
    /// Builder type returned after caller authority and scope are captured.
    type DeferredScoped;

    /// Consumes caller authority and prepares scope-aware deferred work.
    fn defer_scoped_through(
        self,
        scope: &RequestScope,
        label: &'static str,
        call: tina::CallContext<'_, I>,
    ) -> Self::DeferredScoped;
}

impl<T, R, I> DeferScopedThrough<I> for CancelableCall<T, R>
where
    T: Send + 'static,
    R: 'static,
    I: Isolate,
    I::Reply: 'static,
{
    type DeferredScoped = DeferredScopedCall<T, R, I::Reply>;

    fn defer_scoped_through(
        self,
        scope: &RequestScope,
        label: &'static str,
        call: tina::CallContext<'_, I>,
    ) -> Self::DeferredScoped {
        DeferredScopedCall {
            inner: <Self as tina::DeferCancelableThrough<I>>::defer_cancelable_through(self, call),
            scope: scope.clone(),
            label,
        }
    }
}

/// Extension trait on [`tina::CallContext`] adding the blessed pattern
/// `call_ctx.defer_scoped(scope, work).try_admit(...)`.
///
/// Lives here (not on [`CallContext`] itself) because `tina` is the
/// trait crate and the scope vocabulary belongs to the runtime crate.
pub trait CallContextScopeExt<I>
where
    I: Isolate,
{
    /// Defers this caller reply through cancelable work registered into a
    /// request scope. See [`DeferredScopedCall::try_admit`] for the
    /// admission rules.
    fn defer_scoped<W>(
        self,
        scope: &RequestScope,
        label: &'static str,
        work: W,
    ) -> W::DeferredScoped
    where
        W: DeferScopedThrough<I>,
        I::Reply: 'static;
}

impl<'a, I> CallContextScopeExt<I> for tina::CallContext<'a, I>
where
    I: Isolate,
{
    fn defer_scoped<W>(
        self,
        scope: &RequestScope,
        label: &'static str,
        work: W,
    ) -> W::DeferredScoped
    where
        W: DeferScopedThrough<I>,
        I::Reply: 'static,
    {
        work.defer_scoped_through(scope, label, self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::any::TypeId;
    use std::sync::Arc;
    use tina::CallHandle;

    fn fake_handle<R: 'static>() -> CallHandle<R> {
        let shared = Arc::new(CallHandleShared::new(TypeId::of::<R>()));
        tina::runtime_internal::call_handle_from_shared::<R>(shared)
    }

    #[test]
    fn scope_register_records_children_until_cap() {
        let scope = RequestScope::with_child_cap(RequestScopeId::alloc(), 2);
        scope.register("a", fake_handle::<u32>()).expect("first");
        scope.register("b", fake_handle::<u32>()).expect("second");
        let err = scope
            .register("c", fake_handle::<u32>())
            .expect_err("third should overflow cap");
        match err {
            ScopeRegisterError::Full { cap, handle: _ } => assert_eq!(cap, 2),
            other => panic!("unexpected error: {other:?}"),
        }
        assert_eq!(scope.registered(), 2);
    }

    #[test]
    fn cancel_drains_children_and_marks_cause() {
        let scope = RequestScope::with_child_cap(RequestScopeId::alloc(), 4);
        scope.register("a", fake_handle::<u32>()).unwrap();
        scope.register("b", fake_handle::<u32>()).unwrap();
        let report = scope.cancel_synchronously(ScopeCancelCause::ClientDisconnect);
        assert_eq!(report.cause, ScopeCancelCause::ClientDisconnect);
        assert_eq!(report.children.len(), 2);
        assert_eq!(report.child_cap, 4);
        assert_eq!(scope.registered(), 0);
        assert_eq!(
            scope.cancel_cause(),
            Some(ScopeCancelCause::ClientDisconnect)
        );
        // Re-cancel is a no-op and keeps the original cause.
        let report2 = scope.cancel_synchronously(ScopeCancelCause::Timeout);
        assert_eq!(report2.cause, ScopeCancelCause::ClientDisconnect);
        assert!(report2.children.is_empty());
    }

    #[test]
    fn register_after_cancel_returns_handle() {
        let scope = RequestScope::with_child_cap(RequestScopeId::alloc(), 2);
        let _ = scope.cancel_synchronously(ScopeCancelCause::OwnerStopped);
        let err = scope
            .register("a", fake_handle::<u32>())
            .expect_err("register after cancel");
        match err {
            ScopeRegisterError::Cancelled { cause, handle: _ } => {
                assert_eq!(cause, ScopeCancelCause::OwnerStopped);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn scope_set_fill_cancel_refill() {
        let mut set = RequestScopeSet::<u32>::with_capacity(2);
        let s1 = RequestScope::with_child_cap(RequestScopeId::alloc(), 4);
        let s2 = RequestScope::with_child_cap(RequestScopeId::alloc(), 4);
        set.try_insert(1, s1.clone()).expect("admit 1");
        set.try_insert(2, s2.clone()).expect("admit 2");
        assert!(set.is_full());

        let s3 = RequestScope::with_child_cap(RequestScopeId::alloc(), 4);
        let err = set
            .try_insert(3, s3.clone())
            .expect_err("third should be Full");
        match err {
            RequestScopeInsertError::Full { key, scope: _ } => assert_eq!(key, 3),
            other => panic!("unexpected: {other:?}"),
        }

        // Cancel the scope tied to key 1 and remove it, freeing one slot.
        let _report = s1.cancel_synchronously(ScopeCancelCause::ClientDisconnect);
        set.remove(&1).expect("remove key 1");
        assert!(!set.is_full());
        assert_eq!(set.capacity_report().available(), 1);

        // Refill the freed slot.
        set.try_insert(3, s3).expect("admit 3 after refill");
        assert!(set.is_full());
    }

    #[test]
    fn duplicate_key_rejected_with_recovery() {
        let mut set = RequestScopeSet::<u32>::with_capacity(2);
        let s1 = RequestScope::with_child_cap(RequestScopeId::alloc(), 4);
        let s1b = RequestScope::with_child_cap(RequestScopeId::alloc(), 4);
        set.try_insert(1, s1).expect("admit");
        let err = set.try_insert(1, s1b).expect_err("duplicate key rejected");
        match err {
            RequestScopeInsertError::DuplicateKey { key, scope: _ } => assert_eq!(key, 1),
            other => panic!("unexpected: {other:?}"),
        }
    }
}
