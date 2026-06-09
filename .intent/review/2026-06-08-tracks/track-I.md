# Track I — Performance as Correctness (2026-06-08)

Checkout: working tree, HEAD `49c3580` (branch `codex/review-fix-wave-record-2026-05-21`).
Scope: tina-runtime dispatch/scheduler hot paths, tina-http hot paths, per-call/per-connection data structures.

## TL;DR / Critical branch fact

**The branch under review predates the I1–I5 fixes.** The prior review (2026-05-20)
filed I1/I2/I3 with fix PRs that DID land — commits `296caa7` (I1), `61519d6` (I2),
`25cef21` (I3) — and they are ancestors of `origin/main`. I4 and I5 were also fixed
on main (round-robin remote drain with rotating `next_start`; the 1 ms worker sleep
replaced by readiness-driven park / `recv_timeout`, phases 145/151).

But HEAD `49c3580` is **NOT** an ancestor of `origin/main`:

```
git merge-base --is-ancestor 296caa7 HEAD  -> NO   (I1 fix not in this branch)
git merge-base --is-ancestor 61519d6 HEAD  -> NO   (I2 fix not in this branch)
git merge-base --is-ancestor 25cef21 HEAD  -> NO   (I3 fix not in this branch)
git merge-base --is-ancestor 296caa7 origin/main -> YES
git merge-base --is-ancestor HEAD origin/main     -> NO
```

So on THIS checkout I1–I5 are live, unfixed source. On `main` they are fixed.
The actionable Track-I content is therefore:

1. **If this branch is ever merged forward / used as a base, it regresses I1–I5.**
   Rebase it on `main` (or cherry-pick `296caa7 61519d6 25cef21` + the I4/I5 commits)
   before doing anything else. This is the single most important Track-I action.
2. **Three flat-Vec linear-scan defects survive even on `main`** and are the genuinely
   open Track-I work: `gc_stopped_entries` O(N²) removal + per-step rescan (I8),
   `PromotedSlots::sweep_dropped` O(P) per step + O(P²) drain (I9), and HTTP/2
   `find_stream` O(S) per-frame scans, multiple per frame (I10).

Severity below is stated for THIS checkout. Where the issue also exists on `main`
it is flagged `(also on main)`.

---

## Findings

### I1 (regressed on this branch; FIXED on main) — Per-completion O(N) scan over all registered isolates
- Severity: High · Confidence: High · LLM-pattern: yes (flat Vec keyed by id)
- `tina-runtime/src/dispatch.rs:1189`, `:1432`, `:1688`, `:1852`
- Data: `Runtime.entries: Vec<RegisteredEntry>` (`lib.rs:317`). No id→index map on this branch.
- Invariant: bounded/constant-time call settlement; one isolate per connection must not
  make settlement scale with total live connections.
- Bug: every observed-send completion (`:1189`), cancel-call completion (`:1432`),
  isolate-call completion (`:1688`), and immediate/driver completion (`:1852`) does
  `self.entries.iter().position(|e| e.id == .. && e.generation == ..)` — O(N) over all
  registered isolates. With one isolate per connection, every I/O completion is O(N).
  Under C10k the shard turns super-linear and collapses.
- Workload that triggers collapse: K idle connection-isolates + steady reply traffic;
  replies/sec falls as K grows even though per-reply work is constant.
- Repro/bench: K idle isolates + 1 busy caller; assert replies/sec stays flat as
  K → 10⁴. Fails here.
- Fix: maintain `HashMap<IsolateId, usize>` beside `entries`; look up the index, then
  still check `generation` after lookup (stale-map guard). Rebuild map after
  `gc_stopped_entries` compaction. This is exactly what `296caa7` did on main.
- Proof of on-main fix: commit `296caa7` ("index registered isolates for completion
  delivery"); ancestor of `origin/main`, not of HEAD.

### I2 (regressed on this branch; FIXED on main) — O(K) scans + O(K) removes per driver completion (O(K²) drain)
- Severity: High · Confidence: High · LLM-pattern: yes
- `tina-runtime/src/dispatch.rs:1764-1777` (`deliver_completion`); same shape at
  `:1078-1092` (`cancel_in_flight_call_for_resource_close`), `:149-155`
  (`remove_translator`), `:126-146` (`cancel_driver_calls_for_requester`).
- Data: `in_flight_calls: Vec<InFlightCall>`, `translators: Vec<StoredTranslator>`
  (`lib.rs:327-328`), both flat Vecs keyed by `call_id`.
- Bug: `deliver_completion` (called once per driver result, in the `advance_driver`
  drain loop `:1758`) does `in_flight_calls.iter().position()` (O(K)) +
  `Vec::remove` (O(K) shift) + `translators.iter().position()` (O(K)) +
  `Vec::remove` (O(K)). Draining K completions in one turn is O(K²).
  `cancel_driver_calls_for_requester` is also O(K²): `while` loop with
  `in_flight_calls.remove(index)` + `remove_translator` (another position+remove)
  per matched call — fires when a connection-isolate with K outstanding bridge
  calls is dropped.
- Workload: a gateway isolate with K concurrent outbound bridge calls; a burst of
  completions, or the gateway closing, is O(K²).
- Repro/bench: open K concurrent driver calls, complete all in one driver advance;
  wall time should be O(K), measured O(K²) here.
- Fix: key `in_flight_calls` and `translators` by `HashMap<CallId, _>`, or keep flat
  storage + side index and use `swap_remove`. `61519d6` did this on main.

### I3 (regressed on this branch; FIXED on main) — Timeout harvest O(n) min-scan every step, O(e·n) on expiry
- Severity: High · Confidence: High · LLM-pattern: yes
- `tina-runtime/src/dispatch.rs:1495-1535` (`harvest_isolate_call_timeouts`), called
  every `step_with_remote` (`:195`).
- Data: `pending_isolate_calls: Vec<PendingIsolateCall>` (`lib.rs:330`).
- Bug: worse than the prior writeup. The `while let` re-runs a full O(n)
  `iter().enumerate().filter(deadline<=now).min_by(deadline, insertion_order)` to find
  the single earliest-expired entry, then `pending_isolate_calls.remove(index)` (O(n)
  shift), then loops. Harvesting `e` expired entries from `n` pending is O(e·n) scans +
  O(e·n) shifts. Even with zero expiries it pays one full O(n) scan PER STEP.
- Workload: a router holding n in-flight isolate calls; once a timeout wave hits
  (n entries expire together), harvest is O(n²) inside one step, stalling the shard.
- Repro/bench: park n pending calls with the same deadline; advance clock past it;
  one step should be O(n) but is O(n²).
- Fix: `BTreeMap<(Instant, insertion_order), idx>` or a `BinaryHeap` keyed by deadline;
  pop expired in order. Or cache `earliest_deadline` and early-return when
  `now < earliest`. `25cef21` indexed this on main.

### I4 (FIXED on main; live here) — drain_remote_inbound source starvation
- Severity: Medium-High · Confidence: High · LLM-pattern: yes
- `tina-runtime/src/threaded_multi_shard.rs:1014-1031`.
- Bug: `for (_, receiver) in remote_receivers` drains sources in fixed slice order
  under one shared `budget`. A flood from the first source(s) consumes the whole
  budget; later source shards are never read this turn → cross-shard starvation.
- Fix (already on main): rotate a `next_start` cursor so each turn begins draining at
  a different source; round-robin fairness. See `drain_remote_inbound(..., next_start)`
  on `origin/main:threaded_multi_shard.rs:1211`.

### I5 (FIXED on main; live here) — fixed 1 ms thread::sleep caps shard throughput
- Severity: Medium (throughput/availability) · Confidence: High · LLM-pattern: yes
- `tina-runtime/src/threaded_multi_shard.rs:986`.
- Bug: after any productive step (`delivered>0 || remote_delivered>0 ||
  has_in_flight_calls`), the worker does `thread::sleep(Duration::from_millis(1))`.
  Under steady traffic the worker sleeps 1 ms after every step, capping the shard at
  ~1000 steps/sec regardless of queued work. A backlog of 10⁵ messages drains in
  ~100 s wall instead of being CPU-bound. This is a hard throughput ceiling, not a
  micro-opt.
- Repro/bench: enqueue 10⁵ messages on an idle core; time the drain. Should be
  CPU-bound (sub-second); is ~100 s here.
- Fix (on main): readiness-driven park / `recv_timeout` with pending-work awareness;
  the unconditional 1 ms sleep is gone (phases 145/151).

### I6 (live here AND on main) — WaitList::park does ~5 full O(capacity) scans per admission
- Severity: Medium · Confidence: High · LLM-pattern: yes
- `tina-runtime/src/wait_list.rs:390-416` (`park` / `park_call`).
- Data: `slots: Vec<Option<WaitEntry>>` (`:121`).
- Bug: each `park` calls `sweep()` (O(cap), `:374-388`), `count_for_key` (O(cap),
  `:451`), `len()` (O(cap) filter-count, `:320`), then `store_entry` which does
  `position(|s| s.is_none())` (O(cap), `:459-463`) plus another `len()` (O(cap), `:469`).
  ≈ 5 full O(capacity) passes per admission. N admissions into a capacity-C list is
  O(N·C); for a router waiting on thousands of replies this is effectively O(N²).
- Workload: a fan-in service that parks many concurrent request-callers under one
  large-capacity WaitList; admission latency grows with capacity, not with live count.
- Repro/bench: WaitList capacity 10⁴, park 10⁴ callers; per-park cost should be ~O(1),
  measured O(cap).
- Fix: free-slot stack (`Vec<u32>` of free indices) + incremental `live` counter +
  per-key count map; drop the sweep-on-every-park (sweep lazily / on reply).

### I7 (live here AND on main) — WorkerPool acquire scans waiter slab for count + free slot
- Severity: Low-Medium · Confidence: High · LLM-pattern: yes
- `tina-runtime/src/pool.rs:359-361` (`live_waiter_count`), `:423-430`
  (`alloc_waiter_slot`), `:367-384` (`sweep_waiters`); all hit on the acquire path
  (`handle_acquire`, `:505`,`:564`).
- Bug: each acquire does `sweep_waiters` (O(max_waiters)), `live_waiter_count`
  (O(max_waiters) filter-count), and `alloc_waiter_slot` (O(max_waiters) linear scan).
  Smaller blast radius than I1–I3 because `max_waiters` is config-bounded and pools are
  usually small, but it is the same anti-pattern.
- Fix: free-slot stack + incremental live counter; sweep lazily.

### I8 (live here AND on main) — gc_stopped_entries: per-step O(N) rescan + O(N²) burst removal
- Severity: Medium-High · Confidence: High · LLM-pattern: yes
- `tina-runtime/src/dispatch.rs:2353-2403`; runs at the end of EVERY
  `step_with_remote` (`:349`).
- Bug: `gc_stopped_entries` walks all entries every step (O(N) even when nothing is
  stopped). For each stopped-but-not-yet-collectable entry, `can_gc_stopped_entry`
  (`:2364`) runs FOUR linear scans — `child_records` (O(C)), `supervisors` (O(Sup)),
  `in_flight_calls` (O(K)), `pending_isolate_calls` (O(P)) — every step until the entry
  clears. A stopped connection-isolate that still has K in-flight bridge calls pays
  O(C+Sup+K+P) per step until those settle. When a burst of isolates becomes
  collectable in one step, `entries.remove(index)` (O(N) shift) inside the `while`
  loop makes the collection O(N²). On main this still uses `entries.remove(index)`
  (`origin/main:dispatch.rs:3200`).
- Workload: connection-close storm (mass disconnect) → many isolates stop with pending
  driver calls; per-step GC cost scales with N and with in-flight depth, and the actual
  removal is O(N²).
- Repro/bench: register N isolates, stop all in one step with pending in-flight calls;
  the GC step should be O(N), measured O(N²); steady-state idle shard with N stopped-
  but-blocked isolates burns O(N·K) per step.
- Fix: collect indices to remove in one pass and `retain`/swap-compact once (O(N) total,
  not O(N²)); track a `has_collectable_stopped` flag so the scan is skipped entirely
  when no entry is stopped; cache per-entry refcounts instead of re-scanning four Vecs.

### I9 (live here AND on main) — PromotedSlots::sweep_dropped: O(P) per step + O(P²) on drop wave
- Severity: Medium · Confidence: High · LLM-pattern: yes
- `tina-runtime/src/deferred.rs:104-115` (`sweep_dropped`); called every step via
  `sweep_dropped_deferred_slots` (`dispatch.rs:439`, run at `:348` each step).
  Allocate/take paths (`:60`, `:75`) also use `position` (O(P)).
- Data: `PromotedSlots.slots: Vec<DeferredSlotRecord>` (`:45`).
- Bug: `sweep_dropped` does a `while` loop checking `Arc::strong_count` for every
  promoted slot (O(P) per step) and `self.slots.remove(i)` (O(P) shift) for each
  dropped slot — O(P²) when many promises drop together. Fires every step regardless
  of whether anything dropped. A gateway holding thousands of outstanding deferred
  replies pays O(P) per shard turn; a fan-out cancel/close wave is O(P²). Still
  `slots.remove(i)` on main (`origin/main:deferred.rs:110`).
- Workload: fan-out router promoting many deferred replies; under steady promotion the
  per-step sweep dominates the turn; under a mass cancel it is quadratic.
- Repro/bench: promote P slots, drop all in one step; sweep should be O(P), measured
  O(P²); idle shard with P live promises burns O(P) per step.
- Fix: swap-compact (`swap_remove`) instead of `remove`; or store as
  `slab + free-list`; skip the sweep when a "maybe-dropped" generation counter is
  unchanged.

### I10 (live here AND on main) — HTTP/2 find_stream: O(S) scan, multiple per frame
- Severity: Medium · Confidence: High · LLM-pattern: yes
- `tina-http/src/http2/server.rs:1835-1837` (`find_stream`), called many times per
  frame across `handle_frame` and sub-handlers (`:672,773,927,957,1002,1025,1036,
  1049,1083,1095,1100,1103,...`); client mirror at
  `tina-http/src/http2/client.rs:860,1176,1423,1631,1716,1802,1829`. Present on main
  (`origin/main:server.rs:2192`).
- Data: `streams: Vec<ActiveStream>` (`server.rs:328`), keyed by `id`.
- Bug: every DATA/HEADERS/WINDOW_UPDATE/RST_STREAM/CONTINUATION frame does
  `streams.iter().position(|s| s.id == stream_id)` — O(S) where S = concurrent open
  streams — and several handlers call it 2–3 times per frame. A multiplexed connection
  with S active streams processes O(S) frames per round, each O(S) → O(S²) per round.
- Why it's Medium not High: `max_concurrent_streams` defaults to 64 and is the cap, so
  S is small-bounded per connection. Removal already uses `swap_remove` (O(1)). The
  collapse risk is bounded by the cap; but operators who raise the cap (or many
  connections each at the cap) pay the quadratic.
- Repro/bench: one connection, S = max_concurrent_streams concurrent streams each
  trickling DATA frames; per-frame service cost should be ~O(1), measured O(S).
- Fix: `HashMap<u32, usize>` (stream-id → slot) beside the Vec, or
  `BTreeMap<u32, ActiveStream>`; keep `swap_remove` + fix the index. Apply the same to
  the client.

---

## Disproven / not-a-bug (recorded with proof)

- **WebSocket `msgs.remove(0)`** — `connection.rs:1709`. One-time front-pop of a
  coalesced frame batch; remainder goes into `pending_app_msgs`, a `VecDeque` capped at
  `WEBSOCKET_PENDING_APP_MSG_CAP = 4` (`:70`) drained via `pop_front` (`:1486,:1805`).
  Bounded, not quadratic. Not a defect.
- **`compact_trace_prefix` per event** — `dispatch.rs:2336`. Early-returns when
  `trace_start == 0` (`:2337`), which is the steady state under `TraceRetention::Full`
  (the default). No per-event O(n) cost. Not a defect. (`Full` trace growth is a
  memory/by-design concern, out of Track-I scope.)
- **`push_event` synchronous observer** — `dispatch.rs:2285-2287`. Fires
  `obs.on_event` on every event, but only when an observer is installed (`Option`).
  No default observer; the hook is opt-in and documented ("A panic here kills the
  recording thread by design"). Not a hidden hot-path observer. Worth a note only if a
  heavy observer is ever wired in by default.
- **keepalive pool stop loop** — `keepalive.rs:1218`. Sequential `call_blocking` per
  connection, but on the pool-close/shutdown path, not the per-request hot path.
  Acceptable.
- **HTTP router match** — `router.rs:133,261`. `for route in &self.routes` per request
  is O(R) over registered routes; R is a static, small, app-defined route table, not a
  per-connection-growing structure. Out of scope (not a capacity/availability bug).

---

## Invariants violated (Track I)
- "Bounded/constant-time call settlement, independent of total live isolates" —
  violated by I1 (and I8/I9 on the per-step path).
- "One traffic class cannot starve another" — violated by I4 (remote-source vs
  remote-source) on this branch.
- "A backlog drains as fast as the CPU allows" — violated by I5 on this branch.
- "Per-frame protocol work is constant-time in concurrent stream count" — violated by
  I10 (bounded by the 64 cap).

## Suggested tests / benches
- `runtime_settles_flat_as_isolate_count_grows`: K idle isolates + 1 busy caller;
  assert replies/sec flat for K ∈ {10², 10³, 10⁴} (catches I1).
- `driver_completion_drain_is_linear`: K concurrent driver calls, complete all in one
  advance; assert wall ≈ O(K) (catches I2).
- `timeout_harvest_is_linear`: n pending calls, all expire same instant; assert one
  step ≈ O(n) (catches I3).
- `backlog_drain_is_cpu_bound`: enqueue 10⁵ messages on an idle core; assert sub-second
  drain (catches I5).
- `stopped_entry_gc_is_linear`: stop N isolates with pending in-flight calls in one
  step; assert GC ≈ O(N) (catches I8).
- `promoted_slot_sweep_is_linear`: P promoted slots, drop all in one step; assert
  sweep ≈ O(P) (catches I9).
- `http2_per_frame_constant_in_stream_count`: one conn, S = cap streams, trickle DATA;
  assert per-frame cost flat in S (catches I10).
