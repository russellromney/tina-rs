# Phase 150 Review

## Plan Review 1

Findings:

- [P2] The plan could let "bounded hot drain" become unbounded in practice if
  only max rounds is checked. It now requires both max rounds and max elapsed,
  with command/shutdown polling between batches.
- [P2] Backend completion batching could hide failure order or terminal cause.
  Plan now requires per-completion trace facts, deterministic order, and
  explicit failed-completion proof.
- [P2] Ready-queue scheduling could accidentally change Tina's fairness model.
  Plan now keeps one-message-per-isolate fairness as the default semantic rule
  and allows ready-queue work only with deterministic/fairness proof.
- [P2] Host-call fast lane could bypass mailbox capacity if treated as a raw
  command path. Plan now forbids direct isolate-state mutation and requires
  dispatcher capacity/full proof.
- [P2] HTTP turn cleanup could become a hidden callback surface. Plan now
  restricts protocol-local fast paths to non-user-policy boundaries and keeps
  partial/failure writes on ordinary messages.
- [P3] Timing-only proof would be weak. Plan now requires p90/p99 stage fields,
  gap counters, fairness/load proof, idle CPU sanity, and soak proof.
- [P3] Linux could again be aspirational. Plan now requires repeated Linux/x86
  rows or exact blocker text in review.

Decision:

- Plan is intentionally big. The main work is scheduler/turn/tail cost, with
  allocation cleanup only where it is on that path. Do not call the phase done
  for one pretty p50.

## Plan Review 2

Findings:

- [P2] Rock 4 still had "if evidence shows" language, which would let the
  implementer avoid the main large-service scheduler problem. Plan now requires
  the ready scheduler explicitly: FIFO ready queue, per-entry ready bit, same
  mark-ready path for local/remote/deferred/child/bootstrap messages.
- [P2] Ready scheduling could accidentally run self-sends recursively. Plan now
  states self-send remains an ordinary later turn and requires proof.
- [P2] Ready scheduling could silently miss a mailbox path. Plan now requires
  bootstrap, send, call continuation, deferred reply, observed send, remote
  inbound, child lifecycle, and terminal fallback paths to share the ready mark,
  plus a debug/fallback proof.
- [P2] Idle CPU proof was too soft. Plan now requires a pending TCP-read sanity
  probe, not only "if practical" I/O.
- [P3] Verification named a placeholder test file. Plan now pins
  `scheduler_turn_tail` and `ready_scheduler` test files.

Decision:

- Phase 150 is now a true big scheduler phase: tail-aware measurement, bounded
  drain, completion batching, ready scheduler, host-call tightening, HTTP turn
  cleanup, Linux evidence, and soak/CPU sanity.

## Plan Review 3

Findings:

- [P2] The plan still allowed one pretty p50 to count as progress. It now
  requires p50 and p90 improvement on host/service rows, at least one HTTP
  p50+p90 improvement, and p90/p50 tightening on at least two key rows.
- [P2] Trace observer overhead could be mistaken for runtime cost. Plan now
  requires traced and untraced variants for key rows plus review text naming
  observer overhead.
- [P2] p99 could regress silently as long as p50 improved. Plan now requires
  no p99 regression without exact stage evidence, and p99/max gap fields in the
  report.
- [P3] Linux evidence could show only median improvement. Plan now requires
  Linux distribution-shape discussion, not only p50.

Decision:

- Phase 150 success is distribution tightening: faster p50, faster p90, smaller
  p90/p50 ratio, fewer scheduler gaps, no idle spin, no fairness regression.

## Implementation Review — Rock 2 (hostile, self-adversarial)

Reviewer goal: try to break the bounded hot-drain + pending-work-aware park,
and attack the test rigor. Findings below; [fixed] ones are addressed in a
follow-up commit, [accepted] ones carry a rationale.

Findings:

- [P2][fixed] HOT-PATH CLOCK COST. The single-shard inner drain called
  `drain_start.elapsed()` every round — one `Instant::now()` syscall per
  runtime step on the hot path. A ~15-step `call_blocking` paid ~15 extra
  clock reads (~0.3-0.5us on a 25us call, a ~1-2% tax). Fix: capture
  `drain_start` once per burst and consult `elapsed()` only every
  `HOT_DRAIN_ELAPSED_CHECK_ROUNDS` (64) rounds. A short call never reaches 64
  rounds, so it pays zero per-round clock reads; the coarse 50ms elapsed cap
  is unaffected.
- [P2][fixed] TEST GAP: command/shutdown during hot drain unproven. The plan
  requires "command submitted during hot drain is observed within budget" and
  "shutdown during hot drain is observed", but the shipped tests only proved
  correctness under a tiny budget. The inner-loop `try_recv` between rounds was
  structurally present but behaviorally untested. Fix: added
  `command_serviced_during_self_send_storm` (a `send_self` spinner keeps the
  worker hot; an interleaved `call_blocking` to a different isolate still
  replies under a bounded latency) and `shutdown_observed_during_self_send_storm`
  (shutdown returns promptly despite the storm).
- [P3][accepted] TIMER TEST IS TIMING-BASED. `pending_timer_serviced_at_idle_
  repoll_not_idle_wait` uses a 300ms ceiling vs a 2s idle_wait to distinguish
  the two park paths. Under extreme machine load the 15ms timer + jitter could
  in principle approach 300ms, but the margin is 20x the target and 1/6 of the
  idle_wait, and the failure mode (idle_wait path) would be ~2s — far outside
  300ms. Accepted as a robust-enough behavioral proof; the wake-count CPU proof
  is Rock 8's job.
- [P3][accepted] PENDING-IO LATENCY KNOB IS A TRADEOFF, NOT A WIN. With a
  pending TCP read (keepalive idle), the worker parks on the command channel,
  not the io_loop, so byte-arrival latency is bounded by `idle_repoll_interval`
  (default = idle_wait = 1ms, unchanged). Lowering idle_repoll cuts latency at
  the cost of more idle wakeups. This is the documented tradeoff, not a bug;
  the "right" zero-wakeup fix (unify the betelgeuse io_loop fd with the command
  park) is out of scope and noted as a future bottleneck.
- [P3][accepted] MULTI-SHARD HAS NO EXPLICIT ROUNDS/ELAPSED CAP. The
  multi-shard loop polls the command queue once per single `step_with_remote`
  (every step), so it already has per-step command fairness; an explicit burst
  cap would be redundant. Its in-flight `yield` is preserved deliberately (a
  pending cross-shard reply arrives via remote inbound and the step blocks in
  the io_loop, so a bounded park there would only delay the reply). Only the
  idle-branch park was made pending-work-aware, and default behavior is
  unchanged because idle_repoll defaults to idle_wait.
- [P3][verified] VALIDATION COVERAGE. All single-shard constructors funnel
  through `with_config_observer_and_io_loop_factory`; the multi-shard
  constructor has its own checks. Zero-budget should_panic tests cover the
  single-shard path; multi-shard verified by reading both constructors.
- [P3][verified] NO STARVATION REGRESSION. A `send_self` storm makes
  `step() > 0` indefinitely; both old and new code keep the worker on that
  isolate, but one-message-per-isolate-per-round fairness still gives the
  dispatcher/other isolates a turn each round, and the new per-round command
  poll bounds command latency strictly better than the old unbounded drain.

Decision:

- Rock 2 is structural (bounds + idle/timer-latency knobs, behavior-preserving
  defaults), not a warmed-p50 mover — the clean Linux rows show the warmed
  call/HTTP probes are not scheduler-gap-bound. Two fixes applied (hot-path
  clock cost, command/shutdown-during-drain tests); the rest accepted with
  rationale. Headline p50/p90 wins are expected from Rocks 3-5.

## Implementation Review — Rock 3 (hostile, self-adversarial)

Reviewer goal: break the bounded FIFO completion carry-over — find a way to
drop a completion, reorder one, panic, busy-spin, or corrupt accounting.

Findings:

- [P1][fixed during impl] CARRIED-COMPLETION PANIC RACE. `deliver_completion`
  panics ("driver produced completion for unknown call") if the in-flight call
  is gone. Carry-over holds a completion across steps, so a removal between
  harvest and delivery would panic. Audited every in-flight removal path:
  (1) `harvest_isolate_call_timeouts` operates on `pending_isolate_calls` /
  `pending_isolate_call_deadlines`, a SEPARATE map from the backend
  `in_flight_calls` `deliver_completion` uses — no timeout race; (2) a lane
  resolves each call exactly once (close wins over a pending op), so a
  completed call is not also close-cancelled; (3) shutdown rejects in-flight
  calls and clears translators. Fixes: `cancel_in_flight_calls_for_shutdown`
  now clears `pending_completions`; the close-cancel loop purges matching
  carried completions. The panic stays a true invariant.
- [P2][verified] CARRY-OVER IS BOUNDED. `pending_completions` can hold at most
  one entry per in-flight backend call, and in-flight backend calls are bounded
  by lane capacities + mailbox admission. So the carry-over is bounded by the
  same admission control as everything else — not a new unbounded queue.
- [P2][verified] NO BUSY-SPIN, NO STARVED DRAIN. advance_driver runs before the
  per-step message snapshot, so completion messages it enqueues are handled the
  same step and `step() > 0` keeps the bounded hot-drain productive until the
  carry-over empties. If a completion is rejected (mailbox full) and no handler
  runs, `step()` may return 0, but `has_pending_runtime_work` reports the
  carry-over so the worker re-polls at idle_repoll (a park, not a spin) and
  drains on subsequent advances. Bounded either way.
- [P2][verified] ACCOUNTING UNCHANGED. A call stays in `in_flight_calls` until
  `deliver_completion` removes it, so a carried (undelivered) completion's call
  is still counted in the resource report and body-pressure accounting — the
  call genuinely is not complete until delivered. No drift; the
  "do not weaken body-pressure accounting" rule holds.
- [P2][verified] DETERMINISTIC ORDER. Carried (older) entries are delivered
  before fresh ones (FIFO), and driver.advance produces a deterministic order
  per call, so completion order is stable across the budget boundary. The DST
  saved-seed fingerprint is unchanged (default budget 64 >> the sim's
  completions-per-advance, so the sim never splits a batch).
- [P3][accepted] NO DIRECT FAILURE-UNDER-CARRY-OVER TEST. `deliver_completion`
  is byte-for-byte unchanged, so its CallFailed/terminal-cause recording is
  invariant to *when* carry-over calls it; the FIFO test covers
  delivery/order/no-drop, and the existing terminal-completion and DST tests
  (which exercise failure/rejection paths) pass. A direct "failing completion
  carried then delivered still records CallFailed" test needs a
  failure-producing driver in the manual runtime; deferred to the Rock 9
  proof-fast / DST sweep rather than adding bespoke failure-injection here.

Decision:

- Rock 3 bounds a previously unbounded drain while preserving order, failure
  truth, and accounting. Like Rock 2 it is structural — the warmed probes have
  1-3 completions per advance, far under the budget, so it does not move warmed
  p50; its value is the bound (a completion burst can no longer make one step
  unbounded) and the carry-over plumbing the ready scheduler (Rock 4) builds on.
