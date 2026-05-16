# Hostile Review - Phase 102

## Finding 1 [P2] Shutdown handle could hide service drain policy

The risky version of this phase would make `request_shutdown()` look like
"gracefully stop my application." That is not true unless the service has
already exposed its own Stop/Drain protocol. The plan now says the shutdown
handle controls runtime shutdown only; service drain remains explicit service
policy.

## Finding 2 [P2] Non-consuming shutdown can create double-join confusion

If two host threads wait for the same terminal report, silent second waiter
hangs would be awful, and single-claim `AlreadyJoined` would make host code
annoying. The plan now pins the better shape: cache terminal truth and let
multiple waiters receive equal `LocalSystemTerminalReport` values.

## Finding 3 [P2] `call_blocking_on` made the copied path worse than existing host APIs

The first draft required `call_blocking_on(shard, addr, ...)`. That is easy to
get wrong and inconsistent with `try_send` / `observe_result`, which route by
the address shard. The plan now ships the boring copied path:
`ThreadedMultiShardRuntime::call_blocking(addr, msg, timeout)`. No explicit
`*_on` variant ships until a real caller needs "host call from shard A into
target shard B."

## Finding 4 [P2] Timeout semantics need to match single-shard `call_blocking`

The helper must use the normal Tina call timeout and a host wait timeout, just
like the single-shard helper. It must not cancel accepted work or pretend that
timeout stopped the callee. The plan now calls out those outcomes and requires
a held-caller timeout test.

## Finding 5 [P3] Specimen migration could balloon

Real multi-shard placement can cause a specimen rewrite. Letting the worker
choose made the plan less executable. The plan now pins the migration:
`system_session_auth` proves real threaded multi-shard host calls, and
`system_metrics_shipper` proves the shutdown handle.

## Finding 6 [P3] Unknown-shard behavior must match local convention

Some current host APIs panic on unknown shard as programmer error. The plan
does not invent a new error vocabulary; it now pins multi-shard `call_blocking`
to panic on unknown address shard, matching existing multi-shard host API
convention.

## Finding 7 [P2] Non-consuming shutdown needs an ownership refactor

A cloneable shutdown handle cannot produce the existing terminal report unless
it can claim the worker join handle and retained trace/topology truth. The plan
now says this explicitly: refactor threaded runtime internals into one shared
shutdown state. The first waiter, consuming `shutdown_report(self)`, or runtime
`Drop` joins and caches terminal truth. Every later waiter or consuming
`shutdown_report(self)` returns that same cached report. This avoids a fake
polling-only shutdown handle that cannot return real terminal truth.

## Finding 8 [P3] Scope should not leak into the trait crate

This is host-control ergonomics, not a core service trait redesign. The plan now
pins API home to `tina-runtime`; `tina` trait-crate changes are out of scope.

## Finding 9 [P2] Shutdown request must not hang behind the command queue

The first draft made `request_shutdown()` infallible. That invites an
implementation that calls blocking `send(ThreadedCommand::Shutdown)` on a full
bounded command queue. Host-control shutdown should not create a new stuck-host
path. The plan now pins `request_shutdown() -> Result<(), ShutdownRequestError>`
with `CommandFull` / `WorkerStopped`, and requires a proof that full/stopped
request paths do not block forever.

## Finding 10 [P2] Wait-before-request behavior needed a rule

A cloneable wait handle can be used wrong. If `wait_report(timeout)` silently
requests shutdown, it hides control flow. If it blocks forever, it is a footgun.
The plan now says `wait_report` only waits; it does not request shutdown. While
the runtime is still live and no one has requested/dropped shutdown, it returns
`ShutdownWaitError::Timeout`.

## Finding 11 [P2] Runtime Drop must share the same terminal cache

If `Drop` keeps the old private shutdown path, handles that outlive the runtime
can never observe the terminal report. The plan now requires `Drop`,
`shutdown_report(self)`, and `ThreadedShutdownHandle::wait_report` to all use
the same shared join/report state.

## Finding 12 [P2] Host calls can still block before the call timeout starts

The copied `call_blocking` path has to register a temporary driver through the
worker command queue before it can issue the normal Tina call. If that command
admission uses blocking `send` on a full `sync_channel`, the host can hang
before the Tina call timeout even exists. The plan now requires bounded or
nonblocking command admission and a public `ThreadedRuntimeError::CommandFull`
outcome for both the new multi-shard host call and the existing single-shard
host call.
