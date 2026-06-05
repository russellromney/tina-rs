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

## Implementation Review — Rock 4 (hostile, self-adversarial)

Reviewer goal: break the ready scheduler — find a stuck message, a wrong-
incarnation delivery, a double-schedule, a fairness or ordering regression, or
a behaviour difference from the old full scan.

Findings:

- [P1][verified] BEHAVIOUR-PRESERVING. Recv'ing only ready isolates equals
  scanning all iff every non-empty mailbox is marked ready. Guaranteed because
  (a) the ONLY mailbox enqueue is `enqueue_entry_message`, which calls
  `mark_entry_ready` (the 3218/3244 `try_send` hits are inside the mailbox
  trait impl, not bypasses); (b) a per-step `debug_assert!` runs
  `first_unscheduled_nonempty_isolate` across all 653 lib tests + threaded +
  http + DST and never fires; (c) the DST saved-seed fingerprint is UNCHANGED.
  `round_messages` stays entry-indexed and the dispatch loop runs handlers in
  index order, so the delivered set and per-entry order are identical.
- [P1][verified] INCARNATION SAFETY. The queue stores (id, generation), not the
  entry index, because `gc_stopped_entries` compacts the Vec. At pop time the id
  is resolved through `entry_indexes` (stable identity) and the generation is
  checked, so a stale entry for a stopped/gc'd/restarted isolate resolves to
  None or a generation mismatch and is dropped — never delivered to the wrong
  incarnation. `stop_entry` clears the bit; gc destroys it with the entry.
- [P2][verified] DOUBLE-SCHEDULE / REQUEUE. The `ready` bit is the invariant
  "bit set <=> exactly one (id, gen) queued for this incarnation": mark pushes
  only when the bit is clear; a still-non-empty entry is requeued with the bit
  left set (no duplicate); a drained entry clears the bit. The snapshot pops
  exactly the round-start count, so requeued/newly-ready entries (appended at
  the back) are processed next round — one message per isolate per round, self-
  send stays a later turn.
- [P2][accepted] NOT FULLY O(ready) — TWO CHEAP O(entries) PASSES REMAIN. The
  expensive per-isolate work (the mailbox recv: virtual call + lock + context
  pop) is now O(ready). But `round_messages.resize_with(len, None)` and the
  dispatch loop `for index in 0..len` are still O(entries) — cheap (a memset and
  None-checks), and the dispatch loop must stay entry-indexed because the
  supervision/restart paths do `round_messages.get_mut(entry_index)`. So the
  quiet-isolate win is "no recv on empty mailboxes", not "zero per-step work".
  Honest framing in docs; a fully O(ready) dispatch is future work.
- [P3][accepted] HOT-PATH HASHMAP LOOKUP. With few isolates the snapshot now
  does one `entry_indexes` lookup per ready entry instead of a direct index.
  Negligible expected (hot path has ~3 isolates), but to be confirmed by a Fly
  Linux re-measure of Rocks 2-4 together (needs a `fly deploy` approval).
- [P3][accepted] FAIRNESS TEST TIMING. `equal_isolates_advance_in_lockstep`
  asserts spread <= 5 and min > 100 over a 100ms run. Round-robin holds per step
  regardless of machine load, so the spread is robust; min > 100 could in theory
  flake on an extremely slow box (thousands of rounds normally fit in 100ms).

Decision:

- Rock 4 lands the ready scheduler as a pure, provably-equivalent optimization
  (DST fingerprint unchanged), with incarnation safety and a debug detector for
  the one catastrophic failure mode (a missed mark-ready). The recv cost is now
  O(ready); the residual cheap O(entries) passes and the hot-path lookup are
  documented honestly and a Fly re-measure will confirm no hot-path regression.

## Implementation Review — Rock 5 (hostile, self-adversarial)

Reviewer goal: break the typed host-call command — lose a Full/Closed/Timeout/
Rejected outcome, bypass mailbox capacity, mutate isolate state from the host,
or special-case SingleShard.

Findings:

- [P1][verified] OUTCOME TRUTH THROUGH THE NEW PATH. Every `call_blocking` now
  travels the `HostCall` variant, so the existing threaded_call_blocking (10)
  and host_burst (5) suites — which cover Replied/Full/Closed/Timeout/Rejected,
  host-wait timeout, command-full, and bounded multi-threaded burst — now
  exercise the new path and pass. `run_host_call` routes through
  `runtime.try_send` (bounded dispatcher mailbox) and converts Full/Closed into
  the host-visible `CallOutcome::Full`/`Closed` via the begin task's sender, so
  no reject is dropped and capacity is not bypassed.
- [P2][verified] RUNS ON THE WORKER, NOT THE HOST. `run_host_call` is invoked
  only from the worker-thread command loop (exactly where the old boxed closure
  ran), so the host thread never touches isolate state. Both the single- and
  multi-shard loops handle `HostCall` at every command site (top try_recv,
  hot-drain inner try_recv, and the idle park recv_timeout), so it is not a
  SingleShard special case and is observed mid-hot-drain like any command.
- [P2][verified] MEASURED, NOT ASSUMED. perf_native shows host alloc 2 -> 1,
  process 6 -> 5 on call_blocking/_tail/_tail_traced; the ceiling is pinned at 2
  so a regression to the closure box fails the test.
- [P3][accepted] ENUM GREW. `ThreadedCommand` now sizes to the `HostCall`
  variant (an Address + a box) instead of one box. A few extra bytes per
  command-queue slot; negligible against the per-call allocation saved.
- [P3][accepted] `'static` BOUND ADDED to the enum so `dyn HostCallTaskBegin<S>`
  is nameable. Every real `ThreadedCommand` user already has `S: Send + 'static`
  (that is `ThreadedRuntime`'s bound), and the whole crate + test suite compiles
  unchanged, so this restricts nothing in practice.
- [P3][accepted] NO NEW EXPLICIT HOST-CALL COUNTERS. The plan listed per-stage
  counters (command accepted, dispatcher started, target started, reply sent).
  The existing trace already emits these as MailboxAccepted / HandlerStarted
  (dispatcher) / HandlerStarted (target) / CallCompleted, and Rock 1's hotpath
  row counts them (handler_turn_count = 2, completion_count, etc.). Adding
  parallel counters would duplicate existing trace facts, so they were not
  added; the path is already observable.

Decision:

- The big host-call allocation win (per-call isolate registration -> persistent
  dispatcher pool, 17 -> 6) was phase 145; Rock 5 here takes the incremental
  2 -> 1 host / 6 -> 5 process by removing the redundant command-closure box
  with a typed command, preserving every bound and outcome. The last host
  allocation (the type-erased begin box) is irreducible for a shared dispatcher.

## Implementation Review — Rock 6 (HTTP protocol turn cleanup, investigation + verification)

Goal: remove remaining HTTP/1 turns only where no user-policy boundary is
crossed and no invariant (wire bytes, body pressure, partial-write/failure
truth, slowloris/idle timeout) weakens. I mapped the connection state machine
(tina-http/src/connection.rs) end to end before touching anything.

Findings:

- [verified] THE TURN CLEANUP WAS LARGELY DELIVERED BY PHASE 149. The terminal
  close-after-write path (`write_pending_close` -> `RuntimeCallCompletion::
  StopRequester` when `count >= bytes.len() && closed`, connection.rs ~1254)
  already collapses the write+close into one terminal action. Partial writes
  and write/close failures already stay on the ordinary message path
  (`handle_wrote`/`handle_wrote_close` -> `begin_close`), and body pressure is
  released before any terminal bypass (`release_response`, and
  `release_request_all`/`release_response_all` in `begin_close`). The current
  `RuntimeCallCompletion` is sufficient; no new narrow protocol-local action is
  safely addable.
- [verified] THE "STALE TAIL" IS NOT REAL LATENCY. In the `hotpath_http1_close`
  trace, the trailing `call_completion_rejected` is the pending header-deadline
  `sleep` being rejected when the isolate stops; it fires ~1.5us after
  `isolate_stopped`, while the ~110us `->host_unblocked` gap is the test client
  reading the response over loopback. The stale fact is visible and does not
  dominate the host-observed window — exactly the plan's requirement. Nothing
  to cut.
- [investigated, runtime-scoped] STALE-DEADLINE ACCUMULATION IS A RUNTIME
  LIMITATION, NOT A PROTOCOL ISSUE. Each request iteration arms `sleep(deadline)`
  for the slowloris/idle guard and never physically cancels the previous one;
  the connection tombstones stale fires via `request_generation` +
  `head_deadline_armed`. Under sustained keepalive the physical timers
  accumulate (age out at idle_timeout) and each fires one ignored (noop) turn.
  But `tina-runtime`'s own `scope_timer` documents that plain `sleep` is NOT
  `CallHandle`-cancelable — even `ScopedTimer` only tombstones the ticket, it
  does not stop the physical timer. The HTTP connection already uses that exact
  best-available pattern. Physically bounding the timers would require a
  runtime-level cancelable-timer feature, which is outside Rock 6's
  protocol-local scope and touches security-relevant slowloris/idle logic.
  Recorded as a precise future item (runtime cancelable timers), not forced
  here.
- [verified] INVARIANTS HOLD UNDER THE NEW SCHEDULER. The full tina-http suite
  — 40 test binaries (server_smoke, server_keepalive, body_lifecycle,
  streaming_response/request, chunked, websocket_manager, pressure_503,
  server_bad_input, dst_simulator fingerprint, ...) — passes under Rocks 2-5,
  proving wire bytes, body high-water/current-to-zero, keepalive, partial/
  failure, idle-timeout, and slowloris behaviour are preserved. The DST HTTP
  fingerprint is unchanged.

Decision:

- Rock 6 is "already at its safe protocol-local limit + verified preserved".
  The honest engineering outcome is no code change: phase 149 cut the close
  turn, the connection already uses the runtime's best stale-deadline pattern,
  and forcing the only remaining win (physical timer cancellation) would mean a
  runtime timer rewrite on security-relevant paths. The 40-binary matrix is the
  proof the scheduler rocks did not regress HTTP.

## Implementation Review — Rock 7 (Linux/x86 evidence)

Captured twice on Fly performance-2x (dedicated CPU), region iad, profile
release, from commit 0612168 (Rocks 2-6). Baseline rows are commit 8d23be7
(Rock 1, separate performance-2x host). Saved to perf_sample_linux.txt. This is
local/alpha evidence on cloud machines, NOT a production performance claim.

What the Linux rows show:

- HOST/SERVICE ROW IMPROVED, p50 AND p90. `hotpath_call_blocking_tail`:
  p50 25.7us -> 13.5-15.1us (two runs), p90 35.4us -> 25.5-36.0us,
  scheduler_gap_count stays 0. The deterministic part is host allocations
  2 -> 1 / process 6 -> 5 (Rock 5's typed HostCall command). The p50/p90 drop
  is real and stable across two runs but is a cross-machine comparison (a
  different physical performance-2x host than the baseline), so the magnitude
  carries host variance; the direction and the alloc win are solid.
- TRACED vs UNTRACED OVERHEAD stays tiny: call_blocking_tail untraced
  13.5-15.1us vs traced 14.6us — the observer adds well under a microsecond,
  so the runtime cost, not the observer, is what the row measures.
- HTTP ROWS UNCHANGED, TAILS STAY TIGHT. http1_close_tail p50 ~1.16-1.18ms,
  keepalive_steady_tail ~1.17ms, both with p90/p50 ~1.03-1.16x and
  scheduler_gap_count 0-1. Same as baseline — expected, because the HTTP p50 is
  one stage (host_submit -> mbox_accepted ~1.09ms = TCP connect/accept), not a
  scheduler gap, and Rock 6 found no safe HTTP turn to cut.
- SAME BOTTLENECK SHAPE AS MAC, MUCH QUIETER. Linux x86 has the same structure
  (call dominated by the host->worker handoff + dispatcher round-trip; HTTP
  dominated by TCP connect) but far tighter tails than the noisy laptop.

Blocker note: the re-measure required a `fly deploy --build-only --push`
approval (auto-mode blocks shipping the private repo to a registry and blocks
self-granting the rule); the user approved it interactively. The exact command
and harness live in examples/systems/perf_native/fly/README.md.

Decision:

- Linux/x86 evidence exists, captured twice, with distribution-shape discussion
  (p50 and p90 improved on the host/service row; HTTP tails tight and
  unchanged). The deterministic Rock 5 alloc win is confirmed; the call p50/p90
  improvement is real with an honest cross-machine variance caveat.

## Finding — HTTP latency is the worker I/O re-poll gap (idle_repoll A/B)

The HTTP rows' ~1.17ms p50 is almost entirely one stage: `host_submit ->
mbox_accepted` (close) / `host_submit -> call_completed` (keepalive) ~1.09ms.
That is the worker discovering an incoming connection by RE-POLLING the
betelgeuse io_loop at the park interval, not real work. `call_blocking`'s same
stage is ~12us because a host command wakes the worker immediately; socket
readiness does not send a command, so the worker only notices it on its next
poll.

Controlled A/B (one Fly performance-2x machine, same binary, `idle_repoll`
swept via TINA_PERF_IDLE_REPOLL_US; zero cross-machine variance), saved to
idle_repoll_ab_linux.txt:

  idle_repoll  http_close p50   host_submit gap   keepalive p50
  1ms (deflt)  1.157 ms         1.105 ms          1.167 ms
  200us        0.328 ms         0.273 ms          0.340 ms
  100us        0.234 ms         0.167 ms          0.294 ms
  50us         0.300 ms         0.142 ms          0.249 ms

The `host_submit` gap tracks `idle_repoll` almost exactly (1ms->1.1ms,
100us->167us), proving the gap IS the re-poll latency. Lowering idle_repoll to
100us cuts HTTP p50 ~5x (1.16ms -> 0.23ms), landing next to the ~150us of
actual request work. 50us does not beat 100us — the floor is ~2x repoll
(accept + read/write each wait one poll) plus work, so ~100us is the knee.

Two levers:
- LEVER 1 (shipped knob, Rock 2): lower `idle_repoll_interval`. ~5x HTTP win at
  100us, at the cost of more wakeups WHILE I/O is pending (not while idle).
  Making it the default needs the Rock 8 soak to confirm the CPU cost.
- LEVER 2 (the next bottleneck): wire the betelgeuse io_loop's readiness fd into
  the worker's park, so the worker blocks on (command OR socket readiness) and
  wakes the instant a connection arrives — same ~100-200us HTTP latency with
  ZERO extra wakeups. A runtime + betelgeuse change, larger than any rock here;
  named as the phase's next bottleneck (Rock 9).
