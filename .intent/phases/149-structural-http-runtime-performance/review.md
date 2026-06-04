# Phase 149 Review

## Plan Review 1

Findings:

- [P2] The plan must not confuse trace-event count with turn count. Phase 148's
  `stage_count` is useful but too fuzzy. Plan now requires separate
  `event_stage_count`, `handler_turn_count`, backend call count, service-call
  count, completion count, and rejected-completion count.
- [P2] HTTP measurements currently include connect/accept cost. That is a real
  workload, but it is not steady-state request cost. Plan now requires both
  connect/accept rows and already-open keepalive rows.
- [P2] A fast continuation primitive can become fake async if it returns
  arbitrary effects or mutates isolate state without a handler. Plan was
  tightened to a narrow terminal completion action:
  `Message(M)` / `StopRequester` / `Noop`. That is enough for terminal HTTP
  close without inventing a broad hidden continuation surface.
- [P2] Allocation work could become public API churn. Plan keeps `HeaderMap`
  migration out unless the phase fully migrates/proves it, and allows
  `Static`/`Shared` body only with measured wins and body-pressure proof.
- [P2] Linux evidence could stay aspirational. Plan requires repeated Linux/x86
  rows or a named external blocker.
- [P3] Broader rows could become benchmark theater or get skipped. Plan now
  names the exact rows: steady-state keepalive small/fixed, concurrent
  keepalive clients, HTTP/2 unary, WebSocket echo, and mini-service `/health`.
  Each must carry process allocation/RSS/pressure/leak truth or an explicit
  missing baseline field.

Decision:

- Plan is large on purpose and implementation-ready. Rock 3 is deliberately
  narrow: terminal completion action, not arbitrary continuation effects. The
  phase is not done unless at least one structural turn path improves and at
  least one process-allocation row improves.
