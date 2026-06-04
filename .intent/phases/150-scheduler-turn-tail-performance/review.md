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
