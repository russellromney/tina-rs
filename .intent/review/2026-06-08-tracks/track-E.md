# Track E — Resource ownership and drop paths (2026-06-08)

Scope: `tina-runtime/src/pool.rs` (changed), `tina/src/pool.rs` (new vocab),
`tina-runtime/tests/pool_lifetime.rs` (new), `tina-supervisor`, plus the
supervised-restart / shutdown-joiner paths in `tina-runtime/src/dispatch.rs`
and `tina-runtime/src/shutdown.rs`. HEAD 49c3580 + uncommitted working tree.

Top-priority target was the NEW pool code (`Maintain` / `Refill` /
`ResourceLifetime` and the `Retired { next_generation }` change). Verdict:
the new pool code is **correct** under the invariants attacked here. The one
live bug this track found is E2 (restart-factory panic), which the prior
review claimed fixed but is not fixed in the current tree.

---

## E2 (re-confirmed) — Restart-factory panic escapes the supervised boundary and crashes the shard

- Severity: High. Confidence: High. LLM-pattern: yes (panic-safety scoped to
  the obvious call but not the failure-recovery call).
- File: `tina-runtime/src/dispatch.rs:2239` (`restart_child_record`,
  `recipe.create(self, parent)`), reached from
  `supervise_panic` (`dispatch.rs:2102`) at `dispatch.rs:285`, driven by
  `runtime.step()` in the worker loop (`threaded.rs:1437`) with no enclosing
  `catch_unwind`.
- Invariant violated: "Shutdown/containment eventually settles even when user
  code wedges or panics; a supervised child failure must not take down the
  shard." A handler-turn panic is contained; the *restart* of that child is
  not.
- Concrete bug: the only `catch_unwind` in the dispatch hot path is at
  `dispatch.rs:244`, and it wraps **only** `handle_call_boxed` /
  `handle_boxed` (the handler body). When that body panics, the `Err(_)` arm
  (line 266) runs `stop_entry` + `supervise_panic` **outside** the
  `catch_unwind`. `supervise_panic` → `restart_child_record` →
  `recipe.create(self, parent)`. If the user's restart factory panics (e.g. a
  factory that builds a fresh connection and the connect call panics, or any
  `create` that allocates/constructs and trips an assertion), the panic
  unwinds straight through `step_with_remote` and `runtime.step()`. The worker
  thread (`threaded.rs:1437`) has no `catch_unwind`, so the whole shard thread
  dies. Every other isolate on that shard is lost; the shutdown joiner later
  reports the worker as failed/gone.
- Why it happens in real use: restart factories run user code on the failure
  path — the exact place code is least exercised. A factory that re-opens a DB
  connection, re-binds a socket, or re-reads config can panic on a transient
  error. The first child panic that triggers a restart then escalates a
  single-isolate fault into a shard-wide crash.
- Repro / failing test idea: supervised parent with one child; restart factory
  that panics on its 2nd invocation. Send the child a message that panics in
  its handler. Assert: the shard survives, emits a
  `RestartChildSkipped`/skip event (or a typed `RestartFactoryPanicked`
  event), and the parent + siblings still process messages. Today the worker
  thread dies instead.
- Fix (small, idiomatic): wrap the factory call in `catch_unwind` and treat a
  panic as a non-restartable skip, mirroring the existing
  `RestartChildSkipped` path:

  ```rust
  let outcome = match std::panic::catch_unwind(AssertUnwindSafe(|| recipe.create(self, parent))) {
      Ok(outcome) => outcome,
      Err(_) => {
          self.push_event(parent, Some(attempted.into()),
              RuntimeEventKind::RestartChildSkipped {
                  child_ordinal,
                  old_isolate: old_child.isolate,
                  old_generation: old_child.generation,
                  reason: RestartSkippedReason::FactoryPanicked, // new variant
              });
          // keep the recipe bound so a later restart can retry; leave the
          // child slot stopped.
          self.child_records[child_record_index].restart_recipe = Some(recipe);
          return;
      }
  };
  ```

  `recipe` is moved into `restart_child_record` before this point — make sure
  the panic path re-binds it (as above) so the slot stays restartable. The old
  child is already stopped by `stop_entry_with_precollected` (line 2230), so
  on a factory panic the slot is cleanly "stopped, not replaced".

- Note on the prior review: `adversarial-review-2026-05-20.md` line 457 marked
  E2 CONFIRMED at `dispatch.rs:2239`, and the resolution log claimed "E2 #152"
  fixed it via a non-exhaustive-match (`E0004`) change. That PR addressed a
  report/match shape, not the missing `catch_unwind`. The factory call at
  2239 is still unwrapped. E2 is live.

---

## E1 (re-verified) — DISPROVEN / fixed

- File: `tina-runtime/src/shutdown.rs:249-292` (`ensure_joiner_started`).
- Prior bug: handles were pre-taken before `thread::Builder::spawn`; a spawn
  failure dropped the moved closure (and its handles) and the fallback
  re-took an empty set, leaking worker threads and reporting a false "Closed".
- Current code: on spawn failure (line 277), it re-locks state and re-takes
  the handles **from `state.workers`** (line 282-288), then runs
  `joiner_main` inline. Handles are only ever `take()`n inside this function
  (the `joinable` vec built at 261-266 is moved into the spawned closure; on
  the `Err` branch that closure is dropped and the originals are still
  `Some` in `state.workers`, so the re-take at 284-288 recovers them).
  `joiner_main` is panic-wrapped (`shutdown.rs:464`,
  `catch_unwind(... run_joiner ...)`), and `joiner_failed` drives an honest
  failed report (`wait_report_blocking` line 307). No leak, no false Closed.
- Verdict: fixed. Matches prior resolution note "E1 #183".

---

## NEW pool code — attacked, found correct

I treated `tina-runtime/src/pool.rs` and `tina/src/pool.rs` as the prime
suspects and walked every release/drop/cancel/close/maintain/refill path.

### Invariants checked and held

1. **Bounded capacity / exactly-once handout.** `mint_lease` transitions
   `Idle{next_generation:g} -> Leased{generation:g}` atomically with the lease
   mint (pool.rs:455-460) and panics on already-Leased / Retired, so the same
   slot cannot be handed to two callers. Every acquire path
   (`handle_acquire_slot`, `dispatch_to_next_waiter`,
   `handle_refill` → dispatch) goes through `mint_lease` exactly once and
   either replies-with-lease or stores the slot as a waiter — never drops it.

2. **Waiter slab is the real waiter bound.** `handle_acquire_slot` checks
   `live_waiter_count() >= max_waiters` (pool.rs:564) **before**
   `alloc_waiter_slot`, so the `unreachable!` at 429 cannot fire and the slab
   cannot grow past cap. `max_waiters == 0` sheds `Full` immediately
   (0 >= 0). No park-without-slot.

3. **Cancel-race recovery + refill ABA.** Every handler turn starts with
   `sweep_waiters()` + `sweep_in_flight()` (pool.rs:1156-1157, 1178-1179).
   An `in_flight` entry can only survive a turn while its slot is `Open`,
   which means the resource is genuinely `Leased` and not yet delivered. A
   caller cannot release a lease it has not received, and `Maintain` never
   retires `Leased` slots — so a retire/refill can never run against a slot
   that still has a live (Open) in-flight dispatch. `recover_dispatched`
   keys on `(resource_id, generation)` and only restores when the slot is
   still `Leased{generation==g}` (pool.rs:409), so a recovered entry whose
   generation has since advanced is a no-op. The new `Retired{next_generation}`
   carry (pool.rs:183, 437-449) keeps the generation counter monotonic across
   retire→refill, so a stale gen-1 lease can never alias a refilled slot's
   new generation. ABA closed.

4. **Force-close drops resource handles and clears trackers exactly once.**
   `handle_close` upgrade-to-force retires every `Leased` slot via
   `retire_slot` (drops `H`, bumps `retired` once) and `in_flight.clear()`
   (pool.rs:679-695) so the next `sweep_in_flight` cannot double-recover. Late
   releases against a force-retired slot hit `Retired` and return `PoolClosed`
   without a second retire (pool.rs:601-608). Idle slots are deliberately
   *not* retired so a stray release still reports `DoubleRelease` accurately.

5. **`Maintain` retires only idle, prunes the idle queue.** After retiring
   idle slots it prunes `self.idle` of non-idle entries (pool.rs:792-796), so
   a retired slot cannot be popped and handed out. An idle resource and a
   parked waiter can never coexist (an idle resource is dispatched on
   acquire), so an idle-retire can never strand a waiter. Over-age **leased**
   resources are reported, never stolen (pool.rs:780-785) — caller authority
   preserved.

6. **`Refill` only revives `Retired` slots.** Live (Idle/Leased) slots return
   `NotRetired`, OOB returns `UnknownResource`, closed returns `Closed`
   (pool.rs:822-831), so a live resource is never silently replaced.
   Refill continues the generation and may dispatch straight to a parked
   waiter (pool.rs:842-849, tested `refill_serves_parked_waiter`).

7. **Timestamp vectors stay slot-aligned.** `created_at` / `idle_since` are
   `cap`-sized at build (pool.rs:305-306), never resized; all indexing is by
   `resource_id` within `0..states.len()`. `retire_slot` / `mint_lease` /
   `recover_dispatched` / release all keep them in sync. No OOB, no stale
   timestamp surviving a refill (refill re-stamps, pool.rs:839-840).

8. **No leaked permit on early return / cross-shard.** `handle_acquire`
   `NoCaller` returns `noop()` (no slot taken, nothing minted, counts
   `no_caller_drops`); `CrossShardUnsupported` replies `WrongShard` without
   minting (pool.rs:534-547). `PoolLease` is `#[must_use]` and has no public
   constructor, so application code cannot forge or silently drop one into a
   leak without the lint firing.

### Minor / by-design observations (not bugs)

- **Stale-lease-after-refill classified as `DoubleRelease` not `StaleLease`.**
  A gen-1 lease from a retired-then-refilled slot, released after the slot is
  re-leased at gen-2, hits `Leased{2}` with `2 > 1` and returns
  `DoubleRelease` (pool.rs:633). It is really a stale lease, not a literal
  double release. Both are safe "reject, do nothing" outcomes — no capacity
  effect — so this is a cosmetic mislabel at most. Low / informational.

- **`Maintain` does not settle waiters when it retires the last resource.**
  By design: retire reduces capacity, waiters stay parked until refill or
  their own `call` timeout sweeps them (`cancel_count`). Documented and
  consistent with "reported, not stolen". Not a leak.

- **`tina/src/pool.rs::runtime_internal` `unsafe fn`s** can forge leases if
  called from outside the pool impl — but this is the documented contract
  (pool.rs:609-690) and `tina-runtime` is the only caller. No in-tree misuse.

### Tests run

- `cargo test -p tina-runtime --test pool_lifetime` — 18 passed.
- `cargo test -p tina-runtime --test pool` — 19 passed.
- Both clean on the working tree.

---

## Coverage note

Covered: full new lifetime/maintain/refill state machine; retire/release/close
generation bookkeeping; cancel-race / in-flight recovery vs retire/refill ABA;
waiter-slab capacity bound; force-close handle drop + tracker clear; supervised
restart-factory panic containment (E2); shutdown-joiner spawn-failure (E1).

Not deeply covered this track (suggest follow-up): a true concurrency stress /
loom-style interleaving of cancel + release + maintain + refill on the same
slot under the real threaded runtime (the per-turn single-threaded model makes
the ABA argument hold, but a property/fuzz test over message orderings would
harden it); and an end-to-end test that a panicking restart factory keeps the
shard alive (the E2 repro above), which does not exist today.
