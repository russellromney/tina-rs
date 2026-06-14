# Track C — Runtime calls, cross-shard delivery, and fairness (2026-06-09)

Scope: `tina-runtime` dispatch/scheduler/delivery (`dispatch.rs`, `threaded.rs`,
`threaded_multi_shard.rs`, `multi_shard.rs`, `remote.rs`, `registration.rs`,
`mailbox.rs`, `deferred.rs`, `driver/mod.rs`) plus the vendored Betelgeuse
io_loop park (`vendor-betelgeuse/io/{darwin,linux}.rs`, `lib.rs`).
HEAD `0cd6a31` (= origin/main). Read-only.

Priority per the brief: fresh bugs in the post-2026-05-20 rework — 1ms-sleep
removal (`23f2bd9`), readiness park + cross-lane harvest fix (`755f531`),
scheduler tail (`1e64c6b`), stopped-entry GC rework (`f5694f1`), C1
terminal-overflow fix (`084b25e`). Prior-review C1/C3/I4 fixes were re-verified
against current code before hunting (see Disproven section).

Note on verification method: an empirical CPU probe for C-1 (small bin crate
against the workspace) was blocked by the session sandbox, so C-1 rests on
code-path proof across both io backends rather than a measurement. The code
path is unambiguous; a measurement test is part of the suggested fix.

---

## C-1 — Multi-shard worker hot-spins a full core whenever any in-flight work is pending and nothing is deliverable

- **Severity:** High
- **Confidence:** High (code-proven on both backends; no empirical run — see note above)
- **File/lines:**
  - `tina-runtime/src/threaded_multi_shard.rs:1113-1140` (the in-flight `else { thread::yield_now(); }` branch and the comment above it)
  - `tina-runtime/src/threaded_multi_shard.rs:1100-1103` (`!terminal_overflow.is_empty()` → `continue` with no park at all)
  - Proof that nothing blocks: `tina-runtime/src/driver/mod.rs:532-560` (`advance` = per-lane non-blocking substrate steps), `vendor-betelgeuse/io/darwin.rs:881-888` (`step()` polls kevent with a zero timespec), `vendor-betelgeuse/io/linux.rs:1037-1058` (`step()` ends in `harvest(false)`, non-blocking; `blocking_socket_io` only strips `MSG_DONTWAIT` from the uring op, it never blocks the submitter).

- **Invariant violated:** Performance as correctness ("caps whose names imply
  less work than exists"; an idle runtime must not burn CPU) and doc truth: the
  in-tree comment claims "the runtime step blocks inside the betelgeuse io_loop
  while a timer or lane op is pending, so this yield does not hot-spin
  (verified: ... does not burn a core)". That claim is false on the current
  code: `step_with_remote → advance_driver → lane.advance → io_loop.step()` is
  non-blocking on both macOS and Linux, and isolate-call deadlines are harvested
  from the clock (`harvest_isolate_call_timeouts`), not from a blocking wait.

- **Concrete bug:** The worker loop ends each pass with: if
  `delivered == 0 && remote_delivered == 0 && terminal_overflow.is_empty()`,
  park on `recv_timeout` **only when `!runtime.has_in_flight_calls()`**.
  `has_in_flight_calls()` (`host_call.rs:47-51`) is
  `!in_flight_calls.is_empty() || driver.has_pending() || !pending_isolate_calls.is_empty()`.
  So with *any* of the following pending and nothing deliverable, the loop
  degenerates to `drain remote (empty) → try_recv (empty) → step (0) →
  yield_now` at full speed:
  - a pending sleep/timer (`sleep_then(...)`) — the shard burns 100% CPU for
    the entire sleep;
  - a pending TCP/Unix accept or read — i.e. **an idle multi-shard server with
    a listener burns 100% CPU per listening shard, forever**
    (`driver/tcp.rs:594-598` counts pending accepts in `has_pending`);
  - an in-flight cross-shard call — the caller's shard burns CPU until the
    reply or timeout (a slow/held callee pins the caller's core for seconds);
  - pending TLS/DNS/process lane work.
  Separately, when `terminal_overflow` is non-empty and the target pair queue
  stays full, line 1100-1103 `continue`s without even a `yield_now` — a second
  spin source under reverse-queue pressure.

- **Why it happens in real use:** Any multi-shard deployment that sleeps,
  listens on a socket, or makes cross-shard calls — i.e. all of them. This is a
  regression shape introduced when the unconditional 1ms sleep was removed
  (`23f2bd9`): single-shard got the readiness park (`755f531`); multi-shard
  kept "command-queue park when fully idle" and a busy-yield otherwise. Phase
  151's PR text documents multi-shard as deferred, but the code comment claims
  the yield is not a spin, and no test pins multi-shard idle CPU
  (`tina-runtime/tests/readiness_park.rs` is single-shard/simulated only).

- **Repro / failing test:** Two shards, one isolate on shard A does
  `sleep_then(500ms, Done)`. Measure process CPU time (or a wakeup/iteration
  counter on the worker) across the sleep window: today it is ~0.5s of user CPU
  for one core; a parked worker would be ~zero. Same with an idle
  `tcp::bind`+`accept` pending: CPU stays pinned forever. A
  `park_wakeups`-style metric assert (as in
  `readiness_park.rs::simulated_threaded_backend_no_spin_and_command_wake`)
  ported to `ThreadedMultiShardRuntime` would fail today.

- **Fix (small, idiomatic):** In the in-flight branch, park on
  `receiver.recv_timeout(d)` with
  `d = next_park_deadline().map(|t| t - now).unwrap_or(idle_repoll_interval).min(idle_repoll_interval)`
  instead of `yield_now()` — identical shape to the single-shard
  pending-work-aware park. Cost: up to `idle_repoll_interval` (default 1ms)
  added latency on a cross-shard reply into a *quiet* shard — the same bound
  the fully-idle park already accepts for remote inbound today. The zero-cost
  variant is to give multi-shard workers the existing io_loop doorbell
  (`CommandSender` already supports `waker`) and have remote-pair senders ring
  it, then park in `park_io` exactly like single-shard. Either way, fix the
  comment, and add the multi-shard no-spin test. Also make the
  `terminal_overflow`-blocked case (line 1100) fall through to the park instead
  of `continue` when neither local nor remote delivered anything.

- **LLM-pattern?** Yes — a confident "verified" comment describing behavior the
  code does not have, surviving two refactors (`23f2bd9`, `1e64c6b`) because the
  comment, not the code, was re-read.

---

## C-2 — Registration-time bootstrap bypasses `call_contexts`; a call arriving while the bootstrap is queued binds the wrong call context

- **Severity:** Medium (consequence severe — wrong caller binding / misrouted
  settlement; window narrow)
- **Confidence:** High on the structural mismatch; Medium on real-world hit rate
- **File/lines:**
  - `tina-runtime/src/registration.rs:306` and `:797` — both
    `register_*_with_capacity_and_bootstrap` paths push the bootstrap message
    through `adapter.try_send_boxed(boxed)` *before* the entry exists, so no
    paired entry is pushed into `call_contexts`.
  - `tina-runtime/src/registration.rs:475-491` (`enqueue_entry_message`
    pushes one `call_contexts` entry per mailbox message — positional pairing)
    and `:493-515` (`recv_entry_message` pops one context per mailbox recv).

- **Invariant violated:** "Every call settles exactly once with a typed
  terminal cause" — and settles against *its own* callee turn. The
  context-per-message pairing is positional; one unpaired message shifts every
  later context one message earlier.

- **Concrete bug:** After `register_with_capacity_and_bootstrap*`, the mailbox
  holds `[bootstrap]` and `call_contexts` is empty. If a context-carrying
  message is enqueued before the next step's snapshot recv — on the threaded
  multi-shard worker, a cross-shard *call* harvested by `drain_remote_inbound`
  (`remote.rs:632`, `harvest_remote_send` passes `queued.call_context` into
  `enqueue_entry_message`) — the queues become `mailbox=[bootstrap, call]`,
  `contexts=[Some(remote_ctx)]`. The next `recv_entry_message` pairs the
  **bootstrap** with the remote call's context: the bootstrap is dispatched via
  `handle_call_boxed` with a `MessageCaller` for the wrong call (wrong expected
  reply type), and the real call message is delivered with `None` — its caller
  settles as `ReplyAbandoned` (or with a reply produced from handling the
  bootstrap), never with the true reply.

- **Why it can happen in real use:** Worker-loop ordering opens the window:
  each iteration drains remote inbound *before* polling the command queue
  (`threaded_multi_shard.rs:1026-1096`), and a `Run` command ends with
  `continue` — so during a host command burst ("register A with bootstrap;
  register B; ...") the registered-but-not-yet-stepped bootstrap sits queued
  across one or more remote drain passes. A peer shard that already holds the
  address (announce-then-call patterns; re-registration of well-known ids)
  can land a call in that window. Single-shard and explicit-step sequencing
  are safe (snapshot recv always consumes the bootstrap with an empty context
  queue before any handler effect can enqueue a contextful message), which is
  why no existing test catches it.

- **Repro / failing test:** Deterministic shape: register isolate T with
  bootstrap on shard A of a `ThreadedMultiShardRuntime`, and in a tight loop
  from shard B dispatch a cross-shard call to T while the host keeps shard A's
  command queue busy with no-op `Run`s; assert every call settles `Replied`
  with T's reply to *that* call and the bootstrap is handled without a caller.
  Unit-level: construct `Runtime`, call
  `register_with_capacity_and_bootstrap`, then `enqueue_entry_message(idx, m,
  Some(ctx))` before stepping; assert the bootstrap recv pairs `None`.

- **Fix:** Make bootstrap delivery go through the paired path: register the
  entry first, then `enqueue_entry_message(entry_index, bootstrap, None)`
  (check `mailbox_capacity > 0` up front to keep the typed
  `RegisterBootstrapError::Full` path), mirroring `enqueue_bootstrap_message`
  (`registration.rs:450-473`) which already does this correctly for
  restart/spawn bootstraps. Longer-term: carry the context *with* the boxed
  message instead of a parallel positional queue — the parallel queue is the
  latent hazard.

- **LLM-pattern?** Partial. Two sibling registration paths hand-roll the raw
  mailbox send while the third (restart) uses the safe helper — the classic
  "same operation, three implementations, one diverges" shape.

---

## C-3 — `child_records` never shrink: stopped unsupervised children (and their parents) are permanently un-GC-able, and the stopped-entry GC scan latches on forever

- **Severity:** Medium-High (unbounded memory under spawn churn + permanent
  per-step scan tax that resurrects the I8 cost shape)
- **Confidence:** High (no removal site exists; semantics partially intentional
  — see honesty note)
- **File/lines:**
  - No `child_records` removal anywhere in `tina-runtime/src/` (grep:
    only `push` / in-place updates; `dispatch.rs:2869-2873` restart updates,
    `remote.rs:554`).
  - `tina-runtime/src/dispatch.rs:3241-3278` (`can_gc_stopped_entry` refuses
    while `record.child == address` or `record.parent == entry.id &&
    remote_owner.is_none()`).
  - `tina-runtime/src/dispatch.rs:3209-3239` (`gc_stopped_entries`:
    `has_stopped_entries` re-derives true while any stopped-but-blocked entry
    remains, so the "steady-state live shards pay nothing" fast path never
    re-arms).
  - `tina-runtime/src/dispatch.rs:2392-2469` (`stop_entry_full` does not touch
    `child_records`).

- **Invariant violated:** "Bounded capacity means the real thing is bounded"
  (entries and child records grow without bound under spawn/stop churn) and
  the I8 fix's stated contract ("steady-state live shards pay nothing here",
  `dispatch.rs:3210-3212`).

- **Concrete bug:** A spawned child that stops and is never restarted (no
  supervisor, or `RestartSkipped`) leaves: (a) its `ChildRecord` forever (no
  removal code), (b) therefore its stopped `RegisteredEntry` forever
  (`can_gc_stopped_entry` → false via `record.child == address` — handler
  state + mailbox allocation retained), (c) therefore `has_stopped_entries ==
  true` forever, so **every** subsequent `step` pays the full GC pass: O(N
  entries) iteration with an O(child_records + supervisors + in_flight +
  pending_calls) blocked-check per stopped entry. With K stopped children that
  is O(K × records) per step, growing as churn continues — the same
  superlinear shape I8 (#230) fixed for the burst case, now permanent. Parents
  holding local child records are equally un-GC-able after they stop.

- **Why it can happen in real use:** Any one-shot-child pattern: spawn a
  worker per request/job, child does its work and `stop()`s. Long-running
  multi-shard services accumulate entries and per-step scan cost without
  bound.

- **Honesty note (why this is not fully by-design):**
  `ChildLifecycleReport::from_runtime` (`child_lifecycle.rs:98-121`) derives
  `Stopped` state by looking up the *live entry*'s stopped flag, so the GC
  blocker preserves report truth. But that only requires remembering "this
  child stopped", not retaining the whole entry (handler + mailbox) and not
  blocking GC forever.

- **Repro / failing test:** Register a parent; spawn N children that each stop
  on first message; step until quiet. Assert (a) `entries.len()` returns to
  ~baseline, (b) `has_stopped_entries == false` at steady state, (c) a
  step-time probe does not regress linearly in N. All three fail today.

- **Fix:** Record terminal state on the `ChildRecord` (e.g. `stopped:
  Option<AddressGeneration>` or a terminal flag) when the child entry stops
  (local: in `stop_entry_full` via a reverse lookup; remote: on
  `ChildStopped` harvest), have the lifecycle report read that instead of the
  live entry, drop the `record.child == address` GC blocker for
  terminal-flagged records, and `swap_remove` records for non-restartable
  children once reported (or when the parent stops). Keep restartable
  records, which already repoint on restart.

- **LLM-pattern?** No — accretion gap: each lifecycle feature added reads of
  `child_records`, nobody owned its retirement path.

---

## C-4 — ChildStop / SpawnCancel control envelopes are droppable under cross-shard pressure → permanently orphaned remote children

- **Severity:** Medium
- **Confidence:** High (drop is explicit; orphan permanence follows from
  no-retry + C-3's permanent records)
- **File/lines:**
  - Fully silent flavor: `tina-runtime/src/threaded_multi_shard.rs:1244-1247`
    — `let _ = route_remote(outbound)` for harvest-produced outbound; the
    preserved-terminal set (`:964-970`, `:1166-1173`) covers only `CallReply`,
    `SpawnReply`, `ChildStopped`, `ChildRestarted`. The one non-terminal
    harvest product is `ChildStop` from `harvest_remote_spawn_reply` when the
    owner stopped while the spawn was in flight (`remote.rs:470-482`). On
    `Full` it is dropped with **no event on either shard**. Explicit-step
    mirror: `multi_shard.rs:377-394` (`let _ =`), same class split at
    `:457-463`.
  - Trace-only flavor: `dispatch.rs:2651-2663` (`SpawnCancel`) and
    `:2710-2727` (`ChildStop` in `request_remote_child_stop`) — on `Full` the
    owner records `RemoteChildControlRejected` and never retries.

- **Invariant violated:** Cleanup/ownership must eventually settle; "bounded
  queue pressure may delay, not erase, a control obligation." Dropping the
  only stop request orphans the child exactly once and forever.

- **Concrete bug:** When a parent stops/panics while the source→child-shard
  pair queue is full (e.g. the parent's own death is part of a cross-shard
  burst), the `ChildStop`/`SpawnCancel` is dropped. The remote child keeps
  running with a dead owner; its destination-shard `ChildRecord`
  (`remote_owner = Some(dead)`) persists (C-3), and the child itself never
  stops. The `remote_spawn_cancel_tombstones` ring cannot help: it is only
  populated by a *delivered* cancel.

- **Why it can happen in real use:** Owner crash during fan-out is precisely
  when pair queues are full. The C1 fix (`084b25e`) made terminal *replies*
  lossless but classified control *requests* as droppable.

- **Repro / failing test:** Two shards, `shard_pair_capacity: 1`. Parent on A
  owns a child on B; saturate A→B's ordinary queue, then panic the parent.
  Assert the child on B eventually stops (today: it never does; the only
  truth is one `RemoteChildControlRejected` event on A, or nothing at all on
  the spawn-reply path).

- **Fix:** Add `ChildStop`, `SpawnCancel`, `ChildRestart` to the lossless
  overflow class (they are small, and bounded by live child/spawn counts —
  same boundedness argument as terminal replies), or keep a per-owner retry
  list swept each pass. At minimum, the harvest-path drop
  (`threaded_multi_shard.rs:1246`, `multi_shard.rs:379`) must record an event.

- **LLM-pattern?** Yes — the C1 fix's terminal whitelist was enumerated by
  "what settles a caller" and nobody re-asked "what else must never be
  dropped".

---

## C-5 — Cross-shard send/harvest still does O(N) entry scans per envelope

- **Severity:** Low (perf straggler from the I1/I2 wave)
- **Confidence:** High
- **File/lines:** `tina-runtime/src/remote.rs:599-602`
  (`harvest_remote_send`: `entries.iter().position(...)`), `:327-330`
  (`dispatch_local_send_with_context`, on the local send path),
  `tina-runtime/src/registration.rs:567-569` (`entry_by_isolate` is itself a
  linear scan, used by cleanup and spawn-reply harvest).
- **Concrete bug / fix:** `entry_indexes: HashMap<IsolateId, usize>` already
  exists and is maintained (`registration.rs:561-579`); these call sites
  predate it. Replace `iter().position(|e| e.id == target)` with
  `entry_indexes.get(&target)` + generation check (exactly what
  `entry_index()` does). With thousands of registered isolates, every
  cross-shard envelope and every local send pays O(N) today.
- **LLM-pattern?** The "fix the hot scan where the profiler pointed, leave the
  twins" pattern; same shape the prior review called out for I1/I2.

---

## Disproven / re-verified (recorded with proof)

- **Single-shard readiness-park lost wakeup (the Phase 151 prime suspect):**
  NOT a bug. The doorbell is genuinely level-triggered: `wake()` stores
  `pending=true` (Release) *before* `NOTE_TRIGGER` (`io/darwin.rs:79-104`);
  `step_blocking` swaps `pending` first and downgrades to a non-blocking poll
  if set; a wake landing between the swap and `kevent` is covered by the
  kernel user-event; the doorbell is registered `EV_ADD|EV_CLEAR` and a
  non-blocking `step()` that consumes the kernel event never clears `pending`
  (`io/darwin.rs:890-911` + doorbell doc comment). Worker-side ordering
  (try_recv → hot drain → compute deadline → park) is safe because every
  admission rings after enqueue (`threaded.rs:198-267`).
- **Cross-lane harvest theft:** fixed and bounded — `advance` re-harvests TCP
  and Unix once after all lanes drive the shared loop
  (`driver/mod.rs:542-559`); channel-delivered lanes (DNS/process/TLS/storage/
  signals) force a capped 1ms re-poll via `has_unsignaled_pending`
  (`driver/mod.rs:618-632`, `dispatch.rs:2016-2030`).
- **Carried driver completions lost or mis-parked (Phase 150 budget carry):**
  NOT a bug. `pending_completions` is FIFO-carried (`dispatch.rs:2032-2070`),
  counted by both `has_pending_runtime_work` (`:1969-1973`) and
  `park_needs_repoll` (`:2018`), and purged-on-shutdown deliberately after
  in-flight calls are rejected (`dispatch.rs:~95` comment). The
  cancelled-by-close `retain` keeps `deliver_completion`'s unknown-call panic
  a true invariant.
- **Stopped-entry GC swap_remove index corruption (I8 fix):** NOT a bug. The
  pass uses no id→index lookups while compacting, re-checks the swapped-in
  tail, and rebuilds `entry_indexes` once at the end
  (`dispatch.rs:3220-3238`); it runs after the round loop so `round_messages`
  indexes are never stale. Same for `remove_pending_isolate_call` /
  `remove_in_flight_call` / `remove_translator_entry` — each `swap_remove`
  re-points the moved element's index (`dispatch.rs:1553-1567`, `:186-193`,
  `:207-215`), and deadline keys `(deadline, insertion_order)` are unique.
  `PromotedSlots::sweep_dropped` (I9 fix) is sound: `strong_count <= 1` cannot
  race a clone (cloning requires an existing other ref).
- **Exactly-once settlement (reply vs timeout vs cancel vs late remote
  reply):** still structural — first terminal removes the pending entry;
  late replies classify through `recently_cancelled_cause`
  (`remote.rs:671-709`, `dispatch.rs:1842-1867`). Re-verified on current code.
- **C1 (cross-shard terminal drop) / C3 / I4 from the prior review:** still
  fixed — terminal envelopes overflow to the unbounded local `VecDeque`
  instead of dropping (`threaded_multi_shard.rs:1158-1209`), drained first
  each pass; class-alternating budgets (`:1035-1075`) and rotating
  `next_start` (`:1228-1256`) hold. Local-command fairness under remote flood
  still reads the command queue every pass (`:1081-1096`;
  `multishard_fairness.rs` tests pass by inspection of the loop shape).
  Explicit-step `multi_shard.rs` mirrors the same terminal class split and
  overflow flush (`:344-348`, `:442-497`) — no sim/live drift in the drop
  semantics (drift remains only in budgets/rotation, as documented before).
- **Requester-mailbox-full completion drop (`CallCompletionRejected {
  MailboxFull }`, `dispatch.rs:1925-1947`):** by design and tested
  (`tina-runtime/tests/capacity_truth.rs:96,258`) — bounded-capacity truth
  policy, not a fresh bug. Noted asymmetry: runtime-call continuations got a
  non-droppable overflow lane (D2 fix, `enqueue_call_continuation`,
  `registration.rs:528-559`) while isolate-call outcomes stay droppable; that
  is a recorded policy choice, not silent loss.
- **Direct-send mailbox wake hook:** provided mailboxes ring on every
  empty→non-empty transition under the lock (`mailbox.rs:69-86, 155-173`);
  level-triggered park makes the pre-park race safe. Residual footgun only:
  `Mailbox::set_wake_hook` defaults to a no-op (`tina/src/isolate.rs:167`), so
  a custom mailbox written by external threads that ignores it can strand a
  message against a block-forever park — documented contract, flagged here
  for awareness, not filed.
- **`let _ = reply_tx.send(...)` host paths** (`threaded_multi_shard.rs:794,
  915`): still not a settle drop — host dropped its waiter, authority gone.

## Invariants violated (summary)

- "Idle (or blocked-on-pending) runtime does not burn CPU" + comment-vs-code
  truth — C-1.
- "A call settles exactly once, against its own callee turn" — C-2 (context
  misbinding).
- "Bounded capacity bounds the real thing" / "steady state pays nothing" —
  C-3.
- "Queue pressure delays, never erases, a control obligation" — C-4.

## Suggested tests

1. Multi-shard no-spin matrix: pending sleep / pending accept / in-flight
   cross-shard call, assert bounded wakeups or CPU-time per wall-time (port of
   `readiness_park.rs` shape to `ThreadedMultiShardRuntime`). (C-1)
2. Bootstrap context pairing: enqueue a context-carrying message behind a
   queued registration bootstrap; assert the bootstrap pairs `None` and the
   call settles with the callee's real reply. Plus the threaded race harness.
   (C-2)
3. Spawn-churn boundedness: N spawn/stop cycles → entries and child_records
   return to baseline; `has_stopped_entries` false at steady state. (C-3)
4. Orphan-stop property: parent dies while pair queue full → remote child
   eventually stops (DST-able: exactly-once child-terminal property). (C-4)
5. Regression pins: keep the existing exactly-once / terminal-overflow tests;
   add one for `drain_terminal_overflow` ordering under a Failed target
   (Closed-drop counts as progress — assert no spin).

## Coverage map (this track)

Worker loops (threaded single + multi, explicit-step multi) read in full; all
cross-shard envelope variants traced source→queue→harvest→terminal; settle
paths (`complete_isolate_call`, timeouts, cancel, owner-stop, late replies)
re-verified; park/doorbell machinery read down through the vendored kqueue
backend; Phase 150/151 diffs reviewed change-by-change; GC/swap_remove index
invariants checked for entries, pending calls, in-flight calls, translators,
promoted slots. Not covered (other tracks): bridge crates, HTTP/gRPC protocol
surfaces, persistence lanes, trace determinism (G), macros.
