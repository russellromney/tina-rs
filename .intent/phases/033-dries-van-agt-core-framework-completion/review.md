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
