# Track E — Resource ownership and drop paths (2026-06-09)

Scope: `tina-runtime/src/pool.rs`, `tina/src/pool.rs`,
`tina-runtime/src/deferred.rs` (promoted slots + PendingReplies),
`tina-mailbox-spsc`, guard/permit/pending-map types, restart and stop
paths in `tina-runtime/src/dispatch.rs`, keepalive pool
(`tina-http/src/keepalive.rs`). HEAD `0cd6a31` (= origin/main),
read-only review, no cargo runs.

Priority per orchestrator: fresh code since 2026-05-20 — #230 (I8/I9
swap_remove fixes), #228/#217 keepalive changes, #222 mailbox-spsc wake
hooks + worker park, and the restart-factory `catch_unwind` paths.

Verdict up front: the pool core, the I8/I9 swap_remove fixes, and the
restart-panic containment are clean on HEAD. The live bugs are at the
edges: stale transport continuations crossing request generations in the
keepalive connection isolate (panic → permanent silent pool-capacity
loss, plus possible cross-request response confusion), the same family
in the one-shot `HttpClient`, and a lost-wakeup race in the new
`SpscMailbox` wake hook that can strand a message on an indefinitely
parked worker.

---

## E1 — KeepaliveConnection: stale `Connected`/`Wrote`/`Read` continuations cross request generations; panic kills the isolate and rots pool capacity permanently

- Severity: **High**. Confidence: **High**. LLM-pattern: **yes** — the
  generation guard exists but was applied only to the one continuation
  variant where the author noticed the problem (`Deadline`, see its own
  doc comment at `keepalive.rs:219-227`), not generalized to the other
  four continuations with identical staleness exposure.
- Files: `tina-http/src/keepalive.rs:214-218` (ungated continuation
  variants), `:384-398` (`Connected(Ok)` handler), `:424` (`Wrote(Ok)`),
  `:434` (`Read(Ok)`), `:660-677` (`write_more`), `:700-719`
  (`read_more`), `:870-895` (`fail_request`).
- Invariant violated: a long-lived pooled resource isolate must not let
  failure-path residue from request N act on request N+1; an isolate
  panic must not silently and permanently remove pool capacity.

### Concrete bug

`KeepaliveConnection` serves many sequential requests. Only
`Deadline { generation, .. }` carries a request generation;
`Connected`, `TlsConnected`, `Wrote`, and `Read` are guarded solely by
`self.in_flight.is_some()` — which is true again as soon as the *next*
request starts. When a request fails by timeout (`Deadline` →
`fail_request`), the in-flight runtime calls of the dead request are
**not cancelled**:

- A timeout during connect leaves the pending `tcp_connect` /
  `tls_connect` un-cancelled (`fail_request` has no transport to close
  and never cancels the connect call). Its `Connected(Ok(stream))`
  arrives later.
- A timeout mid-read closes the transport; the driver cancels pending
  ops on close (`driver/tcp.rs:972-996`) and delivers them as `Err`
  completions — or, if the data raced in before the close, as
  `Ok(bytes)` completions already harvested and possibly carried across
  steps by the completion drain budget (`dispatch.rs` `pending_completions`,
  budgeted delivery).

Interleavings once the pool re-leases the slot and request N+1 starts:

1. **Stale `Connected(Ok(T1))` during N+1's cold connect** (timeout
   during connect is common when the server is slow/down): the handler
   sees `in_flight.is_some()`, installs `T1` as the transport and
   consumes `pending_connect_bytes`. When N+1's *own*
   `Connected(Ok(T2))` arrives, `self.transport = Some(T2)` overwrites
   T1 (T1 is never closed — driver-table + FD leak), then
   `.expect("pending_connect_bytes set during cold-connect path")`
   **panics**. The handler panic is contained by the dispatch
   `catch_unwind`, but the isolate is stopped.
2. **Stale `Wrote(Ok)` during N+1's cold connect**: `handle_wrote` →
   `count >= bytes.len()` → `read_more()` →
   `self.transport.expect("transport set before read")` **panics**
   (transport is `None` while connecting).
3. **Stale `Read(Ok(bytes))` carrying request N's late response**: bytes
   are appended to N+1's `read_buf` and parsed as N+1's response head.
   If the stale read contains a complete response, **request N+1's
   caller receives request N's response** (silent response confusion).
   If incomplete, the follow-up `read_more()` panics as in (2).
4. **Stale `Read(Err)` / `Wrote(Err)`** (the close-cancelled
   completions, the most common case): `fail_request(..., true)`
   spuriously fails request N+1 with a Read/Write error it never
   caused, and clears N+1's `pending_connect_bytes`; N+1's own
   `Connected` then closes its fresh stream as dangling. Spurious
   failure, no leak.

### Why the pool makes this worse (Track E core)

The connection isolates are registered as plain top-level isolates
(`build_keepalive_pool`, `keepalive.rs:1195-1225`) — no supervisor, no
restart. After a panic the `WorkerPool` still owns the dead `Address`:
acquire succeeds, the consumer's `call` gets `CallOutcome::Closed`, and
the module docs instruct consumers to **always release `Reuse`**
(`keepalive.rs:16-22`). The slot is never retired and never heals. Each
occurrence permanently converts one pool slot into a zombie — silent
capacity rot to zero under sustained timeout load, exactly the leak
class this track hunts.

### Why it happens in real use

Timeouts fire precisely when responses/connects are slow, and slow
completions arriving just after the timeout are the norm, not the
exception. The stale-`Connected` variant needs no exotic scheduling at
all: TCP connect resolves whenever the SYN-ACK lands, trivially after
the slot has been re-leased. The `Ok`-completion variants additionally
need the completion to land after the next Request is admitted —
provided by the budgeted completion carry-over or ordinary multi-turn
release/re-acquire latency.

### Repro / failing test

Capacity-1 keepalive pool against a sim/live server that ignores the
first request's connect or delays its response past the deadline. Drive:
request A (times out) → release Reuse → request B immediately. Assert:
B neither panics the connection isolate, nor fails with a
Read/Write/Connect error it didn't cause, nor receives A's response.
Today: one of the three happens depending on interleaving. A direct
unit-shaped test can synthesize the message sequence
`Request, Deadline{gen=1}, Request, Connected(Ok(...))/Wrote(Ok)/Read(Ok)`
against the isolate and assert no panic / no cross-delivery.

### Fix (small, idiomatic)

Stamp the request generation into every continuation, mirroring
`Deadline`:

```rust
Connected { generation: u64, result: ... },
Wrote     { generation: u64, result: ... },
Read      { generation: u64, result: ... },
```

Build the `.then(...)` closures with `move` capturing
`self.request_generation` (already done for `Deadline`). On mismatch:
no-op for `Wrote`/`Read`; for `Connected(Ok(stream))` close the stream
(the `in_flight.is_none()` arm already shows the shape). This also makes
the `in_flight.is_none()` checks redundant-but-harmless.

---

## E2 — SpscMailbox: wake-hook lost-wakeup race; message can sit queued while the worker parks indefinitely

- Severity: **High** (liveness: arbitrary message — e.g. a pool
  `Acquire`/`Release` — stalls until an unrelated wake; the readiness
  park is explicitly unbounded: "`None` => block until the io_loop or
  the doorbell wakes us (true zero-wakeup idle)", `threaded.rs:1811`).
  Confidence: **Medium-High** — the race window is provable from the
  ordering; reaching it needs an spsc-factory runtime, an off-worker
  producer (host `ServiceHandle` / bridge thread), and the producer
  descheduled between its head load and tail store. No loom model covers
  the wake hook (`tina-mailbox-spsc/tests/loom_spsc.rs` models
  close/FIFO/reuse only).
- File: `tina-mailbox-spsc/src/lib.rs:178-198` (`try_send`), vs. the
  sound locked equivalent `tina-runtime/src/mailbox.rs:155-174`.
- Invariant violated: every empty→nonempty mailbox transition observable
  by a parking consumer must ring the doorbell (the park protocol's
  correctness premise, `threaded.rs:198-206`).

### Concrete bug

```rust
let tail = self.tail.load(Relaxed);
let head = self.head.load(Acquire);   // (1) pre-publish head
...
unsafe { self.slot(tail).write(message); }
self.tail.store(tail.wrapping_add(1), Release);  // (2) publish
if was_empty /* tail == head from (1) */ { wake(); }
```

`was_empty` is computed from the head value at (1). Interleaving:

1. Queue holds m1 (`head=0, tail=1`).
2. Producer P begins `try_send(m2)`: loads `tail=1`, `head=0` →
   `was_empty = false`. P is descheduled.
3. Worker C drains m1 (`head=1`), scans all mailboxes empty
   (`is_empty`: `head=1, tail=1` — P hasn't stored yet), finds no
   runtime deadline, and parks with `timeout = None`.
4. P resumes: writes the slot, stores `tail=2`, skips the wake
   (`was_empty == false`).
5. m2 is queued; the doorbell was never rung; the worker blocks until
   some unrelated wake source fires. With no timers and no fallback
   lanes, that can be forever.

The single-threaded semantics test
(`spsc_semantics.rs: wake_hook_runs_only_on_empty_to_nonempty_transition`)
pins exactly the property that is unsound under concurrency. The
runtime's own `DefaultThreadedMailbox` is immune because `was_empty` is
computed under the same mutex as the push and the consumer's emptiness
checks, giving a total order.

### Repro / failing test

Loom model: producer thread `try_send(m1); try_send(m2)` with a wake
hook that records wakes; consumer thread `recv()` once and then checks
`is_empty()`; assert the property "if `is_empty()` returned true at the
consumer's last check and a message is queued afterwards, a wake was
issued after that check". The interleaving above fails it. (A live repro
is a stall: spsc-factory runtime, idle shard, host handle `try_send`
racing the worker's drain — flaky by nature, loom is the right harness.)

### Fix (small)

Dekker-style confirm after publish:

```rust
self.tail.store(tail.wrapping_add(1), Release);
std::sync::atomic::fence(SeqCst);
let head_after = self.head.load(Relaxed);
let consumer_may_have_seen_empty = head_after == tail; // drained up to our slot
if (was_empty || consumer_may_have_seen_empty) && self.wake_hook_installed.load(Acquire) { ... }
```

with the consumer side pairing a SeqCst fence between its final
`head.store` and its park-decision `tail.load` (or: simply always ring
the level-triggered, coalescing doorbell on a successful send into a
hook-installed mailbox — one atomic flag check; the doorbell coalesces
so the cost at steady state is small). Add the loom model either way.

---

## E3 — HttpClient (one-shot): stale `Deadline` with no generation spuriously times out a later request; same stale-continuation panics/leaks as E1

- Severity: **Medium**. Confidence: **High**. LLM-pattern: yes —
  the keepalive module's own `Deadline` doc comment
  (`keepalive.rs:219-227`) names this exact failure mode ("a 2s deadline
  armed for request N would arrive during request N+1 and fail it
  spuriously"); the fix was applied to keepalive only, never backported
  to `HttpClient` which has the identical shape.
- File: `tina-http/src/client.rs:64` (`Deadline(Result<(), CallError>)`
  — no generation), `:182-188` (`if self.state.is_some() { fail(Timeout) }`),
  `:143-151` / `:296-315` (`Connected`/`Wrote` with the same
  `state.is_some()`-only guard), `:319` (`read_more` transport expect).
- Invariant violated: timeout settles the authority of *its own* call,
  and terminal causes are not converted (a healthy in-budget request
  must not be reported `Timeout`).

### Concrete bug

`HttpClient` is a long-lived isolate serving sequential requests. Every
request arms `sleep(request_timeout).then(HttpClientMsg::Deadline)`;
nothing cancels the sleep when the request completes early. The
`Deadline` handler fails *whatever request is currently active*. Under
back-to-back use, deadline N fires `request_timeout` after N started —
typically mid-request N+k — and kills it with `Timeout` after it has
consumed only a fraction of its own budget. Additionally, the
`Connected`/`Wrote`/`Read` variants have the same cross-request
staleness as E1: a stale `Wrote(Ok)` during the next request's connect
phase panics at `read_more`'s `transport.expect`, and a stale
`Connected(Ok)` overwrites the active transport mid-read (wrong-socket
reads, original stream leaked in the driver table).

The blast radius is smaller than E1 (no pool capacity behind it, the
isolate closes its transport per request), hence Medium.

### Repro / failing test

Sim: `request_timeout = 100ms`. Request A completes in 10ms. At t=95ms
start request B against a server that responds in 50ms. At t=100ms
deadline A fires → B fails `Timeout` at 5ms of its own budget. Assert B
succeeds.

### Fix

Same as E1: `request_generation` counter on the client, stamped into
all five continuation variants, mismatches no-op (and close the stream
for `Connected(Ok)`).

---

## E4 — WorkerPool force-close keeps idle resource handles alive until pool drop

- Severity: **Low**. Confidence: **High**. LLM-pattern: plausible
  (comment justifies keeping the *state*, silently also keeps the
  *handle*).
- File: `tina-runtime/src/pool.rs:693-716` (`handle_close`,
  `upgraded_to_force` block).
- Invariant: force-close's stated goal — "Drops the resource handle
  promptly so heavy H types (connections, files) release" (comment at
  `:697-699`) — is only honored for `Leased` slots.

Force-close retires every `Leased` slot (dropping its `H`) but
deliberately leaves `Idle` slots in `ResourceState::Idle` so a stray
late release still reports `DoubleRelease`. That reasoning covers the
*state*, not the handle: `self.resources[idx]` stays `Some(H)` for idle
slots until the pool isolate itself is dropped, which may be process
lifetime. Setting `self.resources[idx] = None` for idle slots under
force is safe — a closed pool never mints (`handle_acquire` checks
`closed` first), never refills, never maintains, and release on an idle
slot does not touch `resources`. For `H = Address` this is negligible;
for pools of owned heavy handles it is a real retention gap. Fix: null
the idle handles (keep the state) inside the `upgraded_to_force` loop.
Not counted as `retired` so the `DoubleRelease` reporting and counters
stay as-is.

---

## E5 (cross-track, hand to Track C) — call messages abandoned at callee stop settle as `Timeout`, not `Closed`

- Severity: **Medium**. Confidence: **Medium** (may be accepted
  semantics; filing because the cause conversion is real and silent).
- Files: `tina-runtime/src/dispatch.rs:2450-2456` (stop-drain loop),
  `:300-309` (stopped-entry round message), both push `MessageAbandoned`
  and drop the `DeliveredMessage.call_context` without
  `reject_call_context`.
- Invariant violated (playbook Phase 1): "`Full`, `Closed`, `Rejected`,
  timeout, and cancellation are never silently converted into each
  other."

When an isolate stops with call messages already accepted into its
mailbox, `stop_entry_full` drains them as `MessageAbandoned` and drops
their `MessageCallContext`. No path rejects pending calls *targeting* a
stopped isolate (`cancel_pending_isolate_calls_for_owner` is
requester-side only; I checked for a callee-side equivalent — none).
The caller's pending call sits until `harvest_isolate_call_timeouts`
delivers `CallOutcome::Timeout`. The honest terminal is
`Closed`/`Rejected(TargetStopped)` delivered at stop time; the caller
also waits the full timeout for an answer that became impossible
immediately. Calls attempted *after* stop are fine (closed mailbox →
`Closed`). Fix shape: in the two abandonment sites, if
`delivered.call_context` is `Some`, run it through `reject_call_context`
with a `TargetStopped`-flavored reason. Failing test: caller `call`s a
target with the message accepted; target stops the same round; assert
the caller settles promptly with a Closed-class outcome, not `Timeout`
after the full budget.

---

## Verified clean / disproven suspicions (with proof)

1. **I9 fix (`PromotedSlots` swap_remove) — no index-stability bug.**
   All four paths (`take_by_handle`, `take_by_local_call_id`,
   `take_by_isolate`, `sweep_dropped`, `deferred.rs:60-125`) look up by
   position per call; the two loops do not advance `i` after a
   `swap_remove` (moved tail re-checked); no structure stores promoted
   indices across calls. Cascade concern (dropping record A's `Arc`
   releasing a clone of record B's handle mid-pass) only defers B's
   collection to the next per-step sweep — table stays non-empty so the
   `is_empty` early-return cannot skip it. Tests
   `sweep_dropped_removes_exactly_dropped_and_keeps_live_resolvable`,
   `take_by_isolate_drains_only_matching_and_keeps_others_resolvable`
   pin it.
2. **I8 fix (`gc_stopped_entries` swap_remove) — no stale-index bug.**
   `dispatch.rs:3209-3239`: the only persistent index structure over
   `entries` is `entry_indexes`, rebuilt once after the compaction
   (`registration.rs:571-579`). GC runs at the end of `step` after
   `round_messages` is cleared (`dispatch.rs:434-438`), so per-round
   indices never cross the compaction. `has_stopped_entries` is set at
   the single `stopped.set(true)` site (`:2417-2418`) and re-derived by
   the GC; no other path marks entries stopped. `can_gc_stopped_entry`
   reads `entries[index]` directly, not the map.
3. **Prior-review E2 (restart-factory panic) — FIXED on HEAD.**
   `catch_unwind(AssertUnwindSafe(|| recipe.create(self, parent)))` at
   `dispatch.rs:3078` and the remote variant at `:2841`; both re-bind
   the recipe on panic (slot stays restartable) and emit
   `RestartChildSkipped { reason: FactoryPanicked }` /
   `ChildRestarted { outcome: Err(FactoryPanicked) }`. Unwind safety: the
   user factory and bootstrap factory run *before* `spawn_isolate`
   (`dispatch.rs:4378-4387`, `:4481-4513`), so a user panic unwinds with
   zero runtime mutation — no half-registered entry.
4. **Pending-map swap_remove bookkeeping — consistent.**
   `remove_in_flight_call` (`dispatch.rs:186-194`),
   `remove_translator_entry` (`:207-215`),
   `remove_pending_isolate_call` (`:1553-1567`) all reinsert the moved
   element's key; deadlines BTreeMap removed using the *removed* entry's
   own `(deadline, insertion_order)`; `harvest_isolate_call_timeouts`
   pre-removing the deadline key is idempotent against the later remove.
   `cancel_pending_isolate_calls_for_owner` partitions and fully rebuilds
   (`:1759-1765`). `cancel_driver_calls_for_requester` (`:144-167`) does
   not advance the index on the remove branch — moved tail re-checked.
5. **WorkerPool core (exactly-once handout, cancel-race recovery,
   retire/refill ABA, force-close, maintain) — re-verified on HEAD,
   matches the 2026-06-08 track-E proof.** Notably: `in_flight.clear()`
   under force prevents double-recovery against retired slots
   (`pool.rs:700-716`); `recover_dispatched` keys on
   `(resource_id, generation)` and no-ops when the generation advanced
   (`:413-428`); `Retired { next_generation }` keeps generations
   monotonic across refill; waiter slab bound enforced via
   `free_waiter_slots.is_empty()` before alloc (`:586-595`, equivalent
   to the previous count check); `live_waiters` counter replaces slab
   scans without drift (`store/remove_waiter_slot` are the only
   mutation sites). Tests `tina-runtime/tests/pool.rs` +
   `pool_lifetime.rs` exist on HEAD.
6. **`PendingReplies` / `ParkTicket`** (`deferred.rs:164-780`): hard cap
   enforced (`try_capture` checks `len() >= capacity` before consuming
   the caller; the `unreachable!` is genuinely unreachable in a
   single-threaded turn); tickets carry slot generation bumped on each
   occupy, so stale tickets against reused slots are rejected; `drain*`
   helpers consume exactly once.
7. **`DefaultThreadedMailbox` wake hook** — `was_empty` computed under
   the same mutex as the push and as the consumer's checks; total order
   closes the E2-style race (`mailbox.rs:155-174`).
8. **SpscMailbox close/drop protocol** (pre-existing) — close spins out
   a racing producer via the `STATE_PRODUCER_HELD` gate (loom-modeled:
   `close_waits_for_a_racing_successful_send_to_become_visible`); `Drop`
   frees exactly `[head, tail)`. Sound; only the new wake hook (E2) is
   defective.
9. **Stop-path cleanup completeness** (`stop_entry_full`,
   `dispatch.rs:2392-2469`): promoted slots drained by isolate, driver
   calls cancelled for requester, pending isolate calls cancelled for
   owner, mailbox closed + drained. The one gap is the dropped
   call-contexts of already-queued inbound calls (filed as E5).

## Areas needing deeper review (not covered / suggest follow-up)

- The remote-routed deferred-slot lifecycle for cross-shard `call`s into
  a `WorkerPool` (`handle_call` → `into_deferred` with
  `DeferredRouting::Remote`): the in-flight tracker's Closed/Replied
  observation for remote callers was not traced end-to-end this pass;
  the pool docs still say cross-shard pool use is unsupported while
  `handle_call` accepts it.
- `tina-http` connection.rs / http2 client buffer-reuse paths from #217
  (same continuation-staleness audit as E1/E3 should be run there — the
  server side keys streams differently and was not in this track's
  scope).
- A loom (or DST message-ordering) harness over
  cancel + release + maintain + refill on one pool slot — still missing,
  same note as the prior review.

## Suggested tests

1. Keepalive stale-continuation matrix (E1): synthesized
   `Request → Deadline(stale) → Request → {Connected|Wrote|Read}(stale)`
   sequences; assert no panic, no cross-request response delivery, no
   spurious failure. Plus the pool-level zombie-slot test: panic a
   connection isolate, assert acquire+call surfaces something a consumer
   can act on (or that the slot is retire-able and documented).
2. Loom model of the SPSC wake hook (E2) as described above.
3. HttpClient back-to-back timing test (E3).
4. Force-close idle-handle drop test (E4): pool of drop-counting handles,
   force-close with all slots idle, assert all handles dropped.
5. Callee-stop call settlement test (E5): assert Closed-class terminal,
   not Timeout, for queued-then-abandoned call messages.

## Coverage map

| Area | Result |
|---|---|
| WorkerPool state machine (HEAD) | clean (re-verified) |
| tina/src/pool.rs vocab + unsafe internals | clean |
| deferred.rs PromotedSlots (I9 fix) | clean |
| deferred.rs PendingReplies/ParkTicket | clean |
| dispatch.rs stopped-entry GC (I8 fix) | clean |
| dispatch.rs pending maps / index maps | clean |
| dispatch.rs restart paths (local+remote, panic) | clean (E2-prior fixed) |
| dispatch.rs stop_entry drop paths | E5 (cause conversion) |
| tina-mailbox-spsc wake hook (#222) | **E2 (lost wakeup)** |
| keepalive pool + connection isolate (#190/#217/#228) | **E1 (stale continuations / capacity rot)**, E4 |
| HttpClient sibling | **E3** |
