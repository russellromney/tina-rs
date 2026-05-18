# Hostile Review — Phase 105 Request-Scoped Cancellation

## What landed

- `tina-runtime::scope` module: `RequestScopeId`, `RequestScope`,
  `RequestScopeSet`, `ScopedCallHandle`, `ScopeCancelCause`,
  `ScopeCancelReport`, `ScopeChildReport`, and the
  `DeferredScopedCall` admission helper with `CallContextScopeExt`
  blessed pattern `call_ctx.defer_scoped(&scope, label, work).try_admit(...)`.
- 5 deterministic proofs in `tina-sim/tests/request_scope.rs`:
  cancel-before-delivery, cancel-after-deferred-capture, fill-cancel-refill,
  multi-rail single cancel, owner-stop registration block.
- 1 live-runtime proof in `tina-runtime/tests/request_scope.rs`
  (two rails, one scope cancel, trace asserts both `CallCancelled`
  facts).
- Doc updates: `04-request-reply.md` (blessed pattern + section link),
  `14-lifecycle-and-shutdown.md` (rail truth table + bridge honesty
  pointer).

## Finding 1 [P1] Scope cancel only covers the `call_cancelable` rail

The plan's Rock 2 calls for wiring scope cancellation into
sleep/deadline, TCP read/write/accept, TLS read/write/accept, the body
stream source, and pool acquire.

What actually shipped: anything issued through `call_cancelable` is
scope-cancellable for free (which includes pool acquire, since the pool
already runs over `call_cancelable`). Sleep, raw TCP/TLS read/write, and
body sources are not scope-cancellable today because they have no
first-form `CallHandle`.

Resolution (in scope): documented the limitation in
`14-lifecycle-and-shutdown.md` as a literal section ("Any rail that does
not expose a `CallHandle` … is not yet scope-cancellable"). The
primitive is honest about what it can stop.

Resolution (follow-on): adding cancelable variants for `sleep_cancelable`
and `tcp_read_cancelable` (etc.) is the obvious next slice. They drop
into the scope without further work because the scope is generic over
`Arc<CallHandleShared>`.

## Finding 2 [P1] No HTTP/WebSocket/gRPC integration in this phase

The plan's Rock 4 wants client disconnect, WebSocket close, gRPC
`RST_STREAM`, and server shutdown drain to translate into scope cancel.

What shipped: the scope primitive can be cancelled by anyone holding a
clone (it is `Clone` over an `Arc<RequestScopeInner>`). The HTTP
listener does not yet hand out scopes or call `cancel_into_effects` on
disconnect.

Resolution: deferred. Wiring connection-state into a per-request scope
requires touching the connection isolate's state machine and the
service-side registration path. Better done in a follow-on phase that
also addresses the body-source side of Finding 1; doing it piecemeal
leaves two half-paths.

## Finding 3 [P1] Scope cancel uses `CancelCause::CallerCancelled`, not a new cause

When the scope issues `cancel_call_with_handle`, the underlying rail
records `CallCancelled { cause: CallerCancelled }` regardless of the
[`ScopeCancelCause`] the service chose. The scope cause is visible in
[`ScopeCancelReport`] (returned synchronously to the service) but does
not appear in the runtime trace.

Why this is acceptable: the trace fact stays consistent with every other
explicit cancel rail today; cross-shard cancel, owner-stop, etc. all
already have their own [`CancelCause`] variants. Adding a `ScopeCancelled`
variant in the trace would mean threading the [`ScopeCancelCause`] into
`cancel_call_with_handle` and into `RuntimeEventKind::CallCancelled`,
which is a broader change than one phase. The synchronous report covers
service-side observability; trace adds nothing the service cannot already
log.

Resolution: documented in the module preamble. Follow-on can add a
`ScopeCancelled(ScopeCancelCause)` variant if the trace need surfaces.

## Finding 4 [P2] Job queue specimen unchanged

The plan named `system_job_queue` as a user proof for fill-cancel-refill
via the scope path. But the queue has a one-rail-per-submit shape; the
scope vocabulary adds nothing the existing `PendingCancelableCallSet`
does not already do for one rail. Forcing a scope into it would be
ceremony for ceremony's sake.

Resolution: fill-cancel-refill is proven instead in
`tina-sim/tests/request_scope.rs::scope_set_fill_cancel_refill_reclaims_capacity`
on the bounded `RequestScopeSet`. The job queue's existing tests pass
unchanged. The multi-rail value proposition is proven in the two-rail
DST and the live-runtime test; the job queue is not the right witness.

Follow-on: if a future job-queue-style specimen has truly multi-rail
submits (a fan-out across workers per job), `RequestScopeSet` is the
right shape and the helper is already there.

## Finding 5 [P2] Mini SaaS API specimen unchanged

Same logic as Finding 4. The mini_saas_api specimen would be a good fit
for the canonical "request → DB + outbound + timer" multi-rail story,
but adopting it requires the HTTP listener disconnect plumbing from
Finding 2. Once that lands, the specimen update is mechanical:
allocate scope on request start, register children, cancel scope on
disconnect/timeout.

## Finding 6 [P3] `media_ingest_pipeline` did not exist

The plan named `system_media_ingest_pipeline` as a third user proof.
Searching the tree found no such directory; it is a forward reference,
not an existing specimen. Treat it as part of the follow-on phase that
builds the streaming-body cancellation rail (Finding 1).

## Finding 7 [P2] `RequestScope` is `Clone` → multiple cancellers can race

Two clones of the same scope can both call `cancel_into_effects`. The
state machine is honest about this: the first cancel locks the cause,
later cancels see `cancelled.is_some()`, drain zero children (because
the first call already drained them), and return an empty effects list
with the original cause.

A second cancel emits zero effects, so callers cannot accidentally
double-cancel the same rail. The cancel rail itself is also idempotent
(returns `AlreadyCancelled` on the second cancel) so even if the same
shared cell were cancelled twice by sibling code paths, no rail leaks.

Resolution: load-bearing in the implementation, documented inline in
`RequestScope::cancel_into_effects`.

## Finding 8 [P2] Closure clonability requirement for `cancel_into_effects`

`cancel_into_effects` requires `F: Fn(...) -> M + Clone + Send + 'static`.
Each registered child rail needs its own `FnOnce` continuation; the
runtime cancel call takes ownership of one. Most application closures
that capture only `Copy` state satisfy `Clone` automatically, and a
plain `fn` pointer always does. A closure capturing a non-clone `String`
will not compile; the diagnostic is the standard "missing `Clone` impl"
message.

Resolution: documented inline. An alternative — taking `Fn + Sync`
under an `Arc` — would let non-clone closures work, but adds an
allocation per cancel for what is usually a hot path on the disconnect
side. Keeping `Clone` until we have a real complaint.

## Finding 9 [P2] Failed admission returns the pending token; failed scope register returns the handle

The two paths use different recovery types because they capture
different points in the admission lifecycle:

- `RequestScope::register(handle)` returns the typed `CallHandle<R>` in
  the error so the caller can `cancel_call(handle)` or store it for
  retry — caller authority is somewhere else (still in the
  `CallContext`).
- `DeferredScopedCall::try_admit(...)` returns the
  `PendingCancelableCall<K, Q, R>` because that token already owns
  *both* the request context and the typed handle. Caller recovers
  authority through `into_request_context()`.

These names are deliberately different (`ScopeRegisterError` vs
`ScopedAdmitError`). The same word would suggest the same recovery
shape, which would mislead users.

## Finding 10 [P3] `scope.register(handle)` consumes the typed handle

The `register` method takes ownership of the typed `CallHandle<R>` and
drops it after extracting the shared cell. Service code that wants both
worker-return ownership (in `PendingCancelableCall`) and scope cancel
ownership should use `defer_scoped(...).try_admit(...)`, which clones
the shared cell internally and leaves the typed handle on the pending
token.

Direct `scope.register(handle)` is for code that never needs worker-
return ownership — fire-and-forget child calls where the scope is the
only canceller. The doc-string says this; the alternative
`register_shared` covers the case where you have already cloned the
shared cell elsewhere.

## Finding 11 [P3] `RequestScopeId::alloc` uses a single process-global counter

`RequestScopeId` ids are minted from a `static AtomicU64`. Two test
binaries running in parallel would share the counter, but each binary
sees a monotonic sequence inside itself. For deterministic
simulator runs, the counter does *not* reset between tests in the same
binary — a hostile reader might call this leaky. But scope ids are
not load-bearing for trace correlation (the runtime's `CallId` carries
that); they exist so the service can route the cancel ack continuation
on rail name + scope id.

Resolution: if a future need arises (deterministic snapshot of scope
ids inside one simulator run), provide a `RequestScopeId::new(raw)`
escape hatch — already shipped. Tests that need a fixed id call
`RequestScopeId::new(42)` instead of `alloc()`.

## Finding 12 [P2] Boundedness pillar: capacity report on `RequestScopeSet`

`RequestScopeSet::capacity_report()` returns `{ in_use, capacity }` so
a pressure report can include scope-set saturation. The set is also
honest about its own discipline: `try_insert` returns the rejected key
and scope on `Full` or `DuplicateKey`, never a silent drop.

What is not shipped: a `PressureReport` integration that automatically
surfaces scope-set saturation. The hook point is `capacity_report()`;
calling code currently has to compose it into its own pressure
summary. This is the same shape every other bounded structure in the
runtime uses today.

## Verdict

The primitive (`RequestScope` + `RequestScopeSet` + `defer_scoped` +
`cancel_into_effects`) is real, bounded, honest, and proven on two
runtimes (deterministic simulator + live threaded runtime). It is the
right shape: scopes do not span shards, do not pretend to kill external
work, do not retry, do not strand authority. Service writers can model
"this web request is gone; stop its work" without inventing a registry.

The scope of work the plan named is wider than one phase can ship
cleanly. The biggest missing pieces — sleep/TCP/body-source cancel
rails, HTTP listener disconnect → scope, and specimen-level adoption —
are tracked as follow-on Findings 1, 2, and 4–6 above. None of them
require redesigning the primitive; they extend it.
