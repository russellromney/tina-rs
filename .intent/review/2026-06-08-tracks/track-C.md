# Track C — Runtime calls, cross-shard delivery, and fairness (2026-06-08)

Scope: `tina-runtime` (`dispatch.rs`, `multi_shard.rs`, `threaded_multi_shard.rs`,
`remote.rs`, mailbox/queue code, scheduler). HEAD `49c3580`. Read-only.

Cross-checked against the prior adversarial review (`adversarial-review-2026-05-20.md`).
The terminal/ordinary reverse-queue split landed since then (separate
`terminal_senders`/`terminal_receivers` in threaded, `terminal_remote_queues` in
explicit-step and sim). That split reduces, but does **not** remove, the C1 drop;
it also created the C3 budget-subtraction shape that lets ordinary sends get zero
budget. Both prior findings remain live. One new compounding observation (C-N1)
on timeout misattribution from drain starvation.

---

## C1 — Cross-shard terminal reply dropped on a full reverse queue → caller degrades to `Timeout` (typed terminal cause lost)

- **Severity:** High
- **Confidence:** High
- **Files/lines:**
  - `tina-runtime/src/dispatch.rs:695-712` (in-handler `Effect::Reply`, remote routing)
  - `tina-runtime/src/dispatch.rs:811-823` (`reject_call_context`, remote)
  - `tina-runtime/src/dispatch.rs:929-959` (`execute_reply_to`, deferred remote reply)
  - `tina-runtime/src/threaded_multi_shard.rs:1022-1025` (`drain_remote_inbound`: `let _ = route_remote(outbound)`)
  - `tina-runtime/src/multi_shard.rs:324-348` (`let _ = enqueue_remote_envelope(...)` for harvested terminal reroutes)
  - `tina-sim/src/multi_shard.rs:339-360` (same drop, mirrored)
  - `tina-runtime/src/lib.rs:330,614` (`pending_isolate_calls: Vec`, uncapped)

- **Invariant violated:** "Every call settles exactly once with a typed terminal
  cause." "Full/Closed/Rejected/Timeout never silently converted into each other."
  "Bounded capacity bounds the real thing, not just a handle."

- **Concrete bug:** A terminal `QueuedRemoteEnvelope::CallReply` (a real
  `Replied`, or a `Full`/`Closed` produced by `harvest_remote_send` at
  `remote.rs:327,340` when the callee mailbox is full/closed) travels back over
  the bounded reverse **terminal** queue (`shard_pair_capacity`, default 64 live /
  64 explicit). When that queue is full, `route_remote`/`enqueue_remote_envelope`
  returns `Err(SendRejectedReason::Full)` and the reply is discarded:
  - In-handler reply (`dispatch.rs:695`): a `CallReplyRejected{ReplyPathFull}`
    event is recorded, but the reply payload is dropped and **the requester's
    `pending_isolate_calls` entry is never settled**. It later times out via
    `harvest_isolate_call_timeouts`, surfacing `CallOutcome::Timeout` instead of
    the true `Replied`/`Full`/`Closed`.
  - Harvest reroute (`threaded_multi_shard.rs:1025`, `multi_shard.rs:326/339`,
    sim `multi_shard.rs:341/354`): `let _ =` drops the terminal reply with no
    event at all on the caller's shard. Pure silent loss → caller times out.

  The caller-side admission is unbounded (`pending_isolate_calls` is a plain
  `Vec`), so the number of outstanding cross-shard calls can vastly exceed the
  bounded reverse queue. There is no coupling between reverse-queue space and how
  many requests the callee processes (and replies to) per `step`.

- **Why it happens in real use:** A fan-in callee shard B that holds > 64
  outstanding cross-shard calls from shard A, and replies to a batch of them in
  one `step`/drain window, overflows A←B's terminal reverse queue in that window.
  Scatter/gather coordinators (one A isolate calling many B isolates) or any
  service shard under burst produce exactly this shape. The caller observes a
  spurious `Timeout` while B's trace shows `CallReplyRejected{ReplyPathFull}` (or
  nothing, for the harvest-reroute path) and no caller settlement.

- **Repro / failing test:** Two shards, `shard_pair_capacity: 1`. Register a
  responder on B that immediately `Reply`s. From one A isolate, dispatch N>1
  cross-shard calls so B replies to all in one window. Assert every caller
  settles with `Replied`, never `Timeout`. Today at least N−1 callers time out.
  For the harvest path: make B's target mailbox full so `harvest_remote_send`
  emits `RemoteCallOutcome::Full`, and saturate the A←B terminal queue; assert
  the caller sees `CallOutcome::Full`, not `Timeout`/silent-loss.

- **Fix (small, idiomatic):** Never `let _ =` a terminal `CallReply`. Two honest
  options:
  1. Reserve one reverse-terminal slot per admitted cross-shard call at dispatch
    time (admission-coupled boundedness): bound `pending_isolate_calls` to the
    reverse-queue capacity per pair so a reply slot is guaranteed. Reject the
    *call* at admission with `CallOutcome::Full` instead of dropping the *reply*.
  2. Keep a per-pair overflow `VecDeque` for terminal replies that
    `route_remote` could not place, retried each pass before draining new
    inbound. Boundedness then lives on the call-admission side, not the reply
    side. A dropped reply is never acceptable: a settled call must reach the
    caller with its true cause.

- **LLM-pattern?** Yes. `let _ = route_remote(...)` / `let _ = enqueue_...` is the
  classic "ignore the Result on a fallible settle path" pattern — looks tidy,
  silently loses a terminal outcome. The terminal/ordinary split is also a
  plausible-but-incomplete fix: it addresses contention, not the drop.

---

## C3 — Terminal-reply delivery subtracted from the whole ordinary budget → ordinary cross-shard sends can get zero budget

- **Severity:** Medium
- **Confidence:** High
- **File/lines:** `tina-runtime/src/threaded_multi_shard.rs:939-954`

- **Invariant violated:** "One traffic class cannot starve another."

- **Concrete bug:**
  ```rust
  let terminal_delivered = drain_remote_inbound(.., terminal_receivers, .., budget);
  let ordinary_budget = budget.saturating_sub(terminal_delivered);   // <-- can be 0
  let remote_delivered = terminal_delivered
      + drain_remote_inbound(.., receivers, .., ordinary_budget);
  ```
  A sustained inbound **terminal-reply** flood (e.g. a reply-heavy responder
  shard) consumes the entire `remote_inbound_drain_budget` every pass, leaving
  `ordinary_budget == 0`. Ordinary cross-shard `Send` envelopes then drain at
  rate zero and back up indefinitely in `receivers`. The reverse is also true at
  C1 boundary: prioritising terminals is defensible for settlement, but giving
  ordinary sends a hard zero floor is a starvation hazard, not a priority policy.

- **Why it happens in real use:** Any topology where one shard is mostly a reply
  source (request/reply service) and another is sending it fresh work: the reply
  stream from the busy responder can pin the consumer's whole drain budget.

- **Repro / failing test:** Three roles on two shards. Saturate the terminal
  reverse queue into shard X with a steady reply stream while a separate source
  sends ordinary `Send`s to X. Assert the ordinary sends make progress within a
  bounded number of passes. Today they can stall for the duration of the reply
  flood.

- **Fix:** Give each lane a guaranteed floor of the budget rather than a strict
  subtractive cascade, e.g. split `budget` into `terminal_budget` and
  `ordinary_budget` (e.g. half/half, or a configured ratio) and drain each with
  its own cap. Terminal-first within its own half preserves fast settlement
  without zeroing the ordinary lane.

- **LLM-pattern?** Yes. `saturating_sub` to "share" a budget reads safe but
  encodes strict priority with a zero floor — a starvation policy disguised as
  fairness arithmetic.

---

## I4/C-drain — `drain_remote_inbound` starves higher-id sources (fixed source order, shared budget, no rotation)

- **Severity:** Medium
- **Confidence:** High
- **File/lines:** `tina-runtime/src/threaded_multi_shard.rs:1000-1034`
  (receiver vec order set at `:218-225` / sources sorted ascending at `:162`)

- **Invariant violated:** "One traffic class cannot starve another" (here:
  one *source shard* starves another).

- **Concrete bug:** `drain_remote_inbound` iterates `remote_receivers` in fixed
  vector order and fully drains each receiver (inner `loop`) until empty or the
  **shared** `budget` is exhausted, then moves to the next. The vec is built from
  a `BTreeMap` keyed by `source.id()` (shards sorted ascending at construction),
  so it is ascending-source-id order. A sustained flood from the lowest-id source
  keeps its receiver non-empty, so the inner loop hits `delivered >= budget` and
  `return`s before ever reaching higher-id sources' receivers. Higher-id sources'
  envelopes — ordinary **and** terminal replies — starve. This compounds C1: a
  higher-id source's terminal reply can sit undelivered while the caller's
  deadline passes (see C-N1).

- **Why it happens in real use:** Any cross-shard topology with one hot low-id
  source and a quieter high-id source feeding the same destination. The quiet
  source's traffic (including its call replies) is starved by the hot one.

- **Repro / failing test:** Three shards. Shards A (low id) and C (high id) both
  send to B; A floods, C sends one terminal reply per pass. Assert C's reply is
  delivered within a bounded number of passes. Today it can starve for the
  duration of A's flood.

- **Fix:** Round-robin the drain start index across passes (store a per-worker
  cursor), or give each source a per-source budget floor
  (`budget / n_sources`, min 1) instead of a single shared budget consumed in
  fixed order.

- **LLM-pattern?** Partial. Fixed-order nested drain with one shared counter is a
  natural-but-unfair greedy loop; the unfairness is invisible on a 2-shard or
  single-source test.

---

## C-N1 — Timeout misattribution from drain starvation (C1 × I4 interaction)

- **Severity:** Medium
- **Confidence:** Medium
- **Files:** `threaded_multi_shard.rs:939-954,1000-1034` + `dispatch.rs:1495-1535`
  (`harvest_isolate_call_timeouts`).

- **Invariant violated:** "Timeout settles caller authority but does not lie
  about external work." Here the callee *did* reply, on time, but the reply was
  starved in the inbound drain (I4) or dropped on a full terminal queue (C1)
  while the deadline passed; the next `step_with_remote` fires
  `harvest_isolate_call_timeouts` and settles `CallOutcome::Timeout`. The caller
  is told "no reply" when a real terminal reply existed.

- **Per-pass order:** the worker drains inbound (`drain_remote_inbound`, may
  settle replies) *before* `step_with_remote` (which runs timeout harvest). So in
  the common case a queued reply settles before its timeout. The misattribution
  window opens only when the reply is (a) starved behind a lower-id flood (I4) or
  (b) dropped on a full terminal queue (C1) for long enough that the deadline
  elapses. Fixing C1 + I4 closes this; recorded here so the timeout-cause symptom
  is not chased separately.

- **Repro:** Build the I4 starvation case with a short call timeout on the
  high-id source's call; assert the caller observes the callee's real reply, not
  `Timeout`.

- **Fix:** Subsumed by C1 (don't drop terminals) and I4 (don't starve
  higher-id sources). No separate change needed once those land.

---

## Disproven / not-a-bug (recorded with proof)

- **Double-settle on reply-races-timeout:** NOT a bug. Exactly-once is enforced
  structurally by `pending_isolate_calls` removal in `complete_isolate_call`
  (`dispatch.rs:1626-1656`): the first terminal removes the entry; a later
  attempt hits a missing `call_id` and routes to the cause-aware
  `recently_cancelled_cause` ring instead of re-settling
  (`remote.rs:345-400`, `dispatch.rs:672-681`). Verified by reading the harvest
  and timeout paths: both go through `complete_isolate_call`, which returns
  `false` once the entry is gone.

- **Local-command starvation by remote flood (A12):** Already fixed and tested.
  The worker reads `receiver.try_recv()` after every bounded remote-drain pass
  (`threaded_multi_shard.rs:960-971`), not only when the drain delivered zero.
  Covered by `tina-runtime/tests/multishard_fairness.rs`:
  `remote_flood_does_not_starve_local_run_command`,
  `shutdown_under_remote_flood_completes_bounded`,
  `ordinary_remote_throughput_still_progresses`.

- **Cross-shard cancel silently no-ops into AlreadyCompleted:** NOT a bug. A
  cancel issued from the wrong shard is rejected with the typed
  `CancelOutcome::WrongShard` (`dispatch.rs:1383-1389`), not folded into
  `AlreadyCompleted`. Shard identity is verified via the stamped `shard_id`.

- **`let _ = reply_tx.send(...)` host paths** (`threaded.rs:177,785,914,1363`,
  `threaded_multi_shard.rs:792,868`): NOT a settle drop. These are host-side
  `std::mpsc` sends where the receiver is the host thread; if the host dropped
  its waiter the call authority is already gone, so dropping the surplus value is
  correct. Distinct from the cross-shard `route_remote` drops in C1.

- **Live/sim drift on the reply drop:** NONE. Sim (`tina-sim/src/multi_shard.rs`)
  mirrors the same terminal/ordinary split and the same `let _ = enqueue_...`
  drop, so sim agrees with live on the (buggy) behavior. Sim's explicit-step
  drain has no per-pass budget, so the I4/C3 *starvation* shapes are live-only;
  the C1 *drop* is shared. A DST property asserting exactly-once-terminal would
  catch C1 in sim.

---

## Coverage note

Traced every cross-shard call/send path to a terminal outcome across both
substrates (threaded live + explicit-step) and the sim mirror. Read in full:
`dispatch.rs` (all settle/reject/timeout/cancel paths), `remote.rs` (harvest +
late-reply classification), `multi_shard.rs`, `threaded_multi_shard.rs` (worker
loop, route_remote, drain), and the sim `multi_shard.rs` reply queues. Verified
the exactly-once map-removal invariant and the cause-aware late-reply ring.

Not deeply reviewed (out of track or time): the driver-lane completion paths
(`driver/`, Track D), persistence completion events, and the O(N) entry-scan
perf collapse (Track I, prior #3/I1 — confirmed still `entries.iter().position`
at `dispatch.rs:1188-1191,1432,1688,1852` but that is a perf-as-correctness item
for Track I, not a settlement bug).
</content>
</invoke>
