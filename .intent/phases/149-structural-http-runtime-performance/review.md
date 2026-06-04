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

## Plan Review 2

Findings:

- [P2] `StopRequester` could accidentally become a special stop path. Plan now
  requires it to use the same cleanup/trace path as normal `Effect::Stop`, with
  fairness/load tests still passing.
- [P2] `Noop` could hide failures. Plan now restricts `StopRequester`/`Noop` to
  successful completions. Backend failures must still record `CallFailed` and
  use the ordinary message path unless a typed terminal-failure action is added
  and proved.
- [P2] Terminal completion trace truth was underspecified. Plan now requires
  successful terminal actions to still record `CallCompleted`, plus an
  append-only terminal-action fact, and forbids renumbering stable tags.
- [P2] Steady-state keepalive measurement can be polluted by warmup/tail events.
  Plan now requires warmup/tail drain before the timed window.
- [P3] Process allocation rows can lie under concurrent-client probes. Plan now
  requires serial allocation measurement and treats concurrent rows as latency /
  pressure rows unless the report explains cross-thread accounting.
- [P3] Broader workload labels and baselines were too loose. Plan now pins row
  labels, requires HTTP/1 Axum/hyper baselines, and requires review text for any
  HTTP/2/WebSocket missing baseline.

Decision:

- Plan remains implementation-ready. The important shape is sharper now: reduce
  structural cost, but every fast path must preserve stop cleanup, completion
  truth, trace stability, pressure truth, and replay honesty.

## Implementation Review

Findings:

- [P2] The terminal completion action must not hide backend failure. The live
  and simulator dispatch paths now classify failed `CallOutput` first. If a
  translator returns `StopRequester`/`Noop` for a failed backend call, the
  runtime records `CallFailed` plus
  `CallCompletionRejected(TerminalActionOnFailure)` and enqueues no terminal
  action. `runtime_terminal_completion_action.rs` proves the bad-stream case,
  and `tina-sim/tests/timer_semantics.rs` proves terminal stop/noop actions in
  the simulator.
- [P2] `StopRequester` must use ordinary stop truth. The implementation routes
  through `stop_entry`, emits `CallCompleted` plus `CallCompletionAction`, and
  tests that the isolate reaches the ordinary `IsolateStopped` path.
- [P2] HTTP partial/failure write-close must stay on the old path. The HTTP/1
  fast path only returns `StopRequester` for `TcpWroteOwnedClose` when the
  backend wrote the full buffer and closed the stream. Partial writes and
  failures still become `WroteClose`, so body pressure/retry/error behavior
  stays in the handler.
- [P2] A tempting single-shard worker-loop "yield while in-flight" change was
  tested and rejected. It worsened local hotpath noise and pulled stale tail
  facts into the measured window. It was removed.
- [P2] The original implementation proved terminal actions, but did not prove
  the ordinary message fallback stayed ordinary. Added live proof that
  `RuntimeCallCompletion::Message` records `CallCompleted`, delivers through
  the requester mailbox, and emits no terminal-action fact.
- [P2] Simulator fallback failure pressure was under-proved. Added simulator
  proof that a message fallback hitting a full requester mailbox records
  `CallCompletionRejected(MailboxFull)` instead of dropping silently.
- [P2] Simulator terminal failure truth was under-proved. Added simulator
  proof that a malicious `Noop` translator on a failed backend call records
  `TerminalActionOnFailure` and no terminal action.
- [P3] `RuntimeCall::into_parts` became a trap for backend/test authors who
  need terminal completions. Added `into_completion_parts`, documented the
  message-only panic behavior of `into_parts`, and pinned preservation of
  `StopRequester`/`Noop` with a unit test.
- [P3] Phase 149 did not finish HTTP/2/WebSocket equivalent workload rows or
  Linux/x86 repeated evidence. Those remain next-performance-pass work, not
  hidden as done.

Evidence from local macOS/aarch64 release sample:

- `hotpath_http1_fixed_body_close`: 26 event stages, 4 handler turns, 3 runtime
  calls, 1 service call, 44 process allocations.
- `hotpath_http1_keepalive_steady_state_small`: 16 event stages, 3 handler
  turns, 1 runtime call, 1 service call, 44 process allocations.
- `http1_keepalive_steady_state_small`: Tina p50 0.36 ms vs Axum p50 0.39 ms;
  Tina p90/p99 remain much worse/noisier.
- `http1_keepalive_steady_state_fixed`: Tina p50 0.37 ms vs Axum p50 0.42 ms;
  Tina p90/p99 remain much worse/noisier.

Decision:

- This phase earns merge as a structural improvement and measurement cleanup,
  not as a production performance claim. Tina HTTP/1 p50 is now in the same
  neighborhood as Axum on these local rows, but tails are still bad enough
  that the next pass should attack remaining turn/scheduling wobble and add
  Linux/x86, HTTP/2, and WebSocket rows.
