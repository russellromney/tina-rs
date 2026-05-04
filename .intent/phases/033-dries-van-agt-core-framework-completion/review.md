# 033 Dries van Agt Implementation Review

Verdict: strong first implementation slice. The core Dries direction is now
real in code: backend-honest live names, bounded trace retention, a narrow
Tokio/Tower/Axum bridge, and runnable proof that Tokio code can enter Tina
without async handlers or hidden unbounded queues.

What I reviewed directly:

- `BetelgeuseRuntime` / `BetelgeuseMultiShardRuntime` were renamed to
  `BetelgeuseBackedRuntime` / `BetelgeuseBackedMultiShardRuntime` across the
  public live runner surface and tests.
- `TraceRetention::{Full, Bounded, Off}` is wired into `Runtime` and
  `BetelgeuseBackedRuntimeConfig`; tests prove bounded retention on explicit
  runtime and live worker paths.
- `tina-tokio-bridge` is a separate crate. It keeps Tina handlers synchronous,
  uses bounded live-runtime ingress, and exposes `BridgeError::{Full, Closed,
  Timeout}`.
- Axum/Tower proof is real code, not logs. Tests assert a route enters Tina and
  receives a response.
- Bridge backpressure proof covers both worker-ingress `Full` and target
  mailbox `Full`.
- Bridge timeout proof covers the case where Tina accepts a request but keeps
  the responder open without replying.
- `llama_bridge` is a runnable example over Tokio current-thread plus a
  Betelgeuse-backed Tina worker.

Bug found and fixed during review:

- Initial bridge implementation used live `try_send`, which only proved
  bounded worker handoff. Target mailbox `Full` would have been ignored by the
  worker command and surfaced to Tokio as timeout. Fixed by adding
  `try_send_and_observe_with`, a nonblocking observed-handoff hook that reports
  mailbox `Full` / `Closed` later from the worker thread. Added regression
  proof that target mailbox `Full` returns `BridgeError::Full` before timeout.

Remaining honest limits:

- Bridge is narrow first form, not general async ecosystem integration.
- Tower `poll_ready` is permissive; boundedness is enforced at `call`.
- Trace bounded retention is chronological recent retention, not yet a
  zero-copy ring-buffer API.
- Bridge `Service` boxes one future per call; acceptable for this adoption
  bridge slice, not a hot-path performance claim.

Second pass after fixes:

- `make verify` passes after the bridge mailbox-full fix and rustdoc cleanup.
- No additional correctness bugs found in the touched public surface.

Third pass after bridge production-shape additions:

- Added `BridgeHost` so the common app path is one owner for runtime startup,
  service registration, bridge handle creation, and shutdown.
- Added explicit bridge health/close state, bounded retry/reject policy helpers,
  metrics counters, and a capability table that separates preserved Tina
  guarantees from weakened Tokio-edge behavior.
- Added compile-fail guardrails for non-`Send` bridge requests and wrong bridge
  response type, plus runtime tests for health, metrics, explicit retry budget,
  host shutdown ownership, caller timeout, late response rejection, ingress
  full, target mailbox full, and Axum entry.
- Added `TINA_DRIVER_RUNTIME_CONTRACT`, pinning Tina's substrate direction as
  completion-based I/O with bounded commands, explicit cancellation, owned
  shutdown, explicit progress, deterministic simulation, and no hidden executor
  tasks or general async-runtime claim.

Review notes:

- `poll_ready` remains intentionally modest: it reports bridge closed/open
  state only. Queue capacity is still a call-time fact because the underlying
  runtime does not expose a stable capacity probe.
- `BridgeBackpressure::Retry` is explicit and bounded. It sleeps between
  attempts; it does not create a hidden queue.
- Late response accounting is tracked only for bridge-created responders. Manual
  `BridgeRequest::new` remains available for tests/low-level use and does not
  attach shared metrics.
- During self-review, bridge metrics were moved from the caller wait path into
  the worker-observed callback for mailbox admission outcomes. Regression proof:
  if the caller times out while the worker is blocked, a later target
  mailbox-`Full` observation is still counted instead of disappearing.

Fourth pass after review findings:

- Bridge timeout/cancellation now has behavior, not just prose. Each bridge
  request carries a cancellation token. If Tokio drops or times out before the
  worker admits the request, worker-side preflight rejects it. If the request is
  already in a host-registered mailbox but has not reached user code, the
  `BridgeGuard` skips the user handler so state does not mutate.
- `BridgeHost::register_bridge` now supports larger service message enums via
  `From<BridgeRequest<M, R>> + BridgeMessage`, so a bridge-facing service can
  also use runtime calls such as `sleep(...)` in the same isolate.
- `BridgeHost::try_shutdown` keeps the host intact on `StillShared`, making
  "drop handles, retry shutdown" the natural lifecycle path.
- `BridgeBackpressure::Retry` now names `max_retries`; docs call the timeout
  parameter a per-attempt timeout.
