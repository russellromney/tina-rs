# Track I — Performance as Correctness (2026-06-09)

Checkout: `/Users/russellromney/Documents/Github/tina-rs-adv`, HEAD `0cd6a31` = `origin/main`.
Scope: tina-runtime dispatch/scheduler/park/pool/deferred hot paths, tina-http per-frame and
per-request work, mailbox crates. Prior-review context: I1–I10 were fixed before this HEAD;
the explicitly accepted residual is the per-step O(P) `Arc::strong_count` sweep in
`deferred.rs`. This pass scrutinized the fixes themselves and hunted for symmetric twins.

Method note: every candidate was attacked first. Disproven suspicions are recorded at the
bottom with proof.

---

## Findings

### I-NEW-1 [High / Confidence: High] — Every local send/call dispatch is an O(N) scan over all registered isolates

- **Where:** `tina-runtime/src/remote.rs:314-345` (`dispatch_local_send_with_context`),
  the scan at `remote.rs:327-331`:
  ```rust
  let Some(entry_index) = self
      .entries
      .iter()
      .position(|entry| entry.id == send.target_isolate)
  ```
- **Invariant violated:** bounded per-message cost; one isolate per connection must not make
  message delivery scale with total live connections (the exact invariant the I1 fix was
  for).
- **Bug:** `entry_indexes: HashMap<IsolateId, usize>` exists and is maintained
  (`lib.rs:414`, insert at `registration.rs:393/653/851`, rebuild at `dispatch.rs:3237`),
  and the *completion* delivery paths use it via `entry_index()` (`registration.rs:561-565`).
  But the **send/dispatch ingress path does not**. Call sites that funnel through the linear
  scan: plain sends from effects (`dispatch.rs:645`, `:736`, `:1305`), **every isolate call
  dispatch** (`dispatch.rs:1465`), bridge self-poll continuations (`dispatch.rs:2914`), and
  every cross-shard send harvested on the destination shard
  (`remote.rs:356` → `harvest_remote_send` → `dispatch_local_send_with_context`).
- **Why it bites in real use:** an HTTP/WS shard registers one isolate per connection. At
  C10k, *every* send and every call admission is a 10k-entry walk — the same super-linear
  shard-turn collapse the prior review filed as I1, just on the symmetric twin (ingress
  instead of completion egress). Perf phases 147/150/152 benchmarked few-isolate setups, so
  this never showed in their rows.
- **Repro / failing test idea:** register K idle isolates plus one sender/receiver pair;
  measure sends/sec as K goes 10 → 10⁴. Should be flat; will fall ~linearly.
- **Fix (small, idiomatic):** mirror `entry_index()`:
  ```rust
  let Some(&entry_index) = self.entry_indexes.get(&send.target_isolate) else {
      return Err(SendRejectedReason::Closed);
  };
  ```
  then keep the existing generation check (generation mismatch already returns `Closed`,
  same outcome as today). One-line semantics-preserving swap.
- **LLM-pattern:** yes — textbook "index one lookup site, leave the sibling linear scan"
  (the prior review's named symmetric-twin pattern).

### I-NEW-2 [High / Confidence: High (code), Medium-High (impact)] — Multi-shard worker busy-spins a full core whenever any in-flight work exists

- **Where:** `tina-runtime/src/threaded_multi_shard.rs:1101-1140`. The in-flight branch:
  `:1113` `if !runtime.has_in_flight_calls() { park } else { thread::yield_now() }`
  (`:1139`). Also the `continue`-without-park when `!terminal_overflow.is_empty()`
  (`:1101-1104`).
- **Invariant violated:** "no busy-poll / core-burning spin" — stated as a non-goal in
  `.intent/phases/151-readiness-driven-worker-park/plan.md:75`, and the in-code comment at
  `threaded_multi_shard.rs:1106-1112` claims "the runtime step blocks inside the betelgeuse
  io_loop while a timer or lane op is pending, so this yield does not hot-spin (verified...)".
  **That claim is false on current code.**
- **Proof the step cannot block:** lanes drive the substrate via `IOLoop::step()`, which is
  a zero-timeout poll on both platforms — `vendor-betelgeuse/io/darwin.rs:881-888`
  (`kevent` with `timespec {0,0}`) and `vendor-betelgeuse/io/linux.rs` `step()` (submit +
  `harvest(false)`, no wait). Blocking only happens in `step_blocking`, which only `park_io`
  calls — and the multi-shard loop never calls `park_io` (`threaded_multi_shard.rs:62-64`
  documents that multi-shard keeps the command-queue park). `set_blocking_socket_io` does
  not change this: on Linux it only drops `MSG_DONTWAIT` from io_uring SQEs
  (`io/linux.rs:688,734`), and Darwin doesn't implement it at all (trait default no-op,
  `lib.rs:126`).
- **What `has_in_flight_calls()` covers** (`host_call.rs:47-51`): `in_flight_calls`
  (any armed `tcp_accept`/`tcp_read`!), `driver.has_pending()` (**armed sockets, any
  pending timer, any signal interest** — `driver/mod.rs:568-578`), and
  `pending_isolate_calls` (any cross-shard call awaiting reply).
- **Concrete bad states:**
  - A multi-shard runtime hosting a TCP/HTTP listener has a pending accept call at all
    times → every such shard spins at 100% CPU forever, even with zero traffic.
  - One isolate sleeping on a 60 s timer → its shard spins for 60 s.
  - Any cross-shard call in flight → the caller's shard spins until the reply lands.
  - A peer shard whose inbound queue stays full while `terminal_overflow` is non-empty →
    the sender loops `continue` with no park at all, hammering `try_send` on a full
    channel.
  - Each spin iteration is not even cheap: `resource_report()` refresh (refresh_metrics
    stays true on the yield path), per-source `try_recv`, a full `step_with_remote`
    (O(entries) round probe + ~6 lane substrate steps = multiple zero-timeout
    kevent/io_uring syscalls), timeout harvest.
- **Why this is Track-I correctness:** it converts "idle with one armed op" into a
  hard CPU ceiling per shard (power, thermal, noisy-neighbor, oversubscribed-host collapse),
  and the in-code comment actively asserts the opposite — a truth gap, not just a perf nit.
- **Repro / failing test idea:** mirror `readiness_park.rs::idle_worker_makes_near_zero_wakeups`
  for `ThreadedMultiShardRuntime`: register a single isolate holding a pending timer (or an
  armed `tcp_accept`), sample process CPU over 250 ms; assert <5% of a core. Current code
  pegs the core.
- **Fix:** bounded park in the in-flight branch instead of `yield_now()` — e.g.
  `receiver.recv_timeout(idle_repoll_interval.min(idle_wait))`; a pending cross-shard reply
  costs at most the repoll interval, which is the same deal the single-shard fallback lanes
  already accept. Long-term: the named follow-up from phase 151 (cross-shard queues as
  io_loop wake sources + real `park_io` for multi-shard). At minimum, correct the comment
  and document the spin.
- **LLM-pattern:** yes — comment asserts a verified property the code no longer has
  (the claim predates the lane/park refactors and was carried forward through phase 150).
- **Honest caveat:** phase 151's plan explicitly deferred the multi-shard io park, so "no
  readiness park on multi-shard" is a known limitation. The *spin* (vs the old 1 ms sleep,
  removed in `5ce45b9` for I5) and the false "does not hot-spin" comment are not documented
  anywhere I could find.

### I-NEW-3 [Medium / Confidence: High] — Per-frame `Vec::drain` compaction: O(frames × buffered bytes) per read on HTTP/2 client, WebSocket (both sides), and gRPC request decode — server got the cursor fix, twins didn't

- **Where:**
  - `tina-http/src/http2/client.rs:1852-1885`: `handle_read` loops frames and calls
    `self.read_buf.drain(..meta.total)` **per frame** (`:1863` DATA, `:1877` inline).
  - `tina-http/src/websocket.rs:880`: `parse_frame` ends with `buf.drain(..frame_end)` per
    frame; driven in loops by the WS **server** (`connection.rs:1747`) and WS client
    (`websocket_client.rs:528` `drain_frames` loop).
  - `tina-http/src/grpc.rs:644`: `next_buffered_message` does
    `self.buffer.drain(..end).collect()` per pulled message.
- **Invariant violated:** per-byte work bounded by a constant; a peer must not be able to
  multiply the cost of bytes it already paid flow-control/caps for.
- **Bug:** each `drain(..n)` memmoves the entire remaining buffer. A 64 KiB read containing
  F small frames costs O(F × 64 KiB). Concretely: 64 KiB of 9-byte HTTP/2 frames ≈ 7 000
  frames × ~32 KiB average shift ≈ 230 MB of memmove **per read**; 6-byte masked WS frames
  ≈ 10 900 frames ≈ 350 MB per read — from an *untrusted* WS client. The HTTP/2 **server**
  had exactly this and was fixed in the phase 152/153 work with a cursor + single
  `buf.drain(..consumed)` (`server.rs:700-705`, comment narrates the fix). The client and
  WS/gRPC paths were not given the same treatment.
- **Why real:** WS server peers are untrusted (tiny-frame flood is free for the attacker);
  the HTTP/2 client talks to semi-trusted servers but a hostile/buggy server can stall a
  bridge shard; the gRPC request decoder serves untrusted clients (buffer bounded by
  `max_buffered_request_bytes`, but the quadratic factor still applies within it).
- **Repro / failing test idea:** feed a single read buffer of N minimum-size frames; assert
  processing time scales ~O(N), not O(N²) (or count bytes moved via a counting allocator /
  criterion slope).
- **Fix:** the same cursor pattern as `server.rs:process_frames`: track `consumed`, slice
  `&buf[consumed..]`, drain once after the loop. For `parse_frame`, take `&buf[cursor..]`
  and return consumed length instead of mutating the Vec per frame.
- **LLM-pattern:** yes — fix applied to the reviewed site only; symmetric twins untouched
  (the exact failure mode this track was told to hunt).

### I-NEW-4 [Medium / Confidence: High] — `build_message_caller` linearly scans `pending_isolate_calls` on every delivered local call message

- **Where:** `tina-runtime/src/dispatch.rs:443-485`, scan at `:467-473`:
  `self.pending_isolate_calls.iter().find(|p| p.call_id == call_id)`.
- **Invariant violated:** per-delivery cost independent of concurrent in-flight calls (the
  I3 invariant).
- **Bug:** `pending_isolate_call_indexes: HashMap<CallId, usize>` exists (`lib.rs:433`) and
  the timeout/removal paths use it (I3 fix), but this per-delivery lookup — run inside
  `step_with_remote` for every delivered message that carries a local call context
  (`dispatch.rs:330`) — still walks the Vec. A shard with P concurrent local in-flight
  calls pays O(P) per delivered call message → O(P²) to deliver one wave.
- **Fix:** `self.pending_isolate_call_indexes.get(&call_id).map(|&i| self.pending_isolate_calls[i].expected_reply_type_id)`.
- **LLM-pattern:** yes — same missed-sibling-lookup shape as I-NEW-1.

### I-NEW-5 [Medium / Confidence: Medium-High] — PromotedSlots: O(P) linear lookups on the deferred-reply settle, cancel, and timeout paths (beyond the accepted sweep residual)

- **Where:** `tina-runtime/src/deferred.rs:60-87` (`take_by_handle` and
  `take_by_local_call_id`, both `iter().position(...)`), used by:
  - `dispatch.rs:1045` `execute_reply_to` — **every deferred reply settle** is O(P);
  - `dispatch.rs:1819-1828` `close_deferred_slot_for_call_with_reason` — called for
    **every** cancelled (`:1622`), timed-out (`:1730`), and owner-stop-cancelled (`:1778`)
    call, *whether or not that call has a deferred slot*, so a timeout wave of e calls on a
    shard holding P promoted slots is O(e × P).
- **Invariant violated / accepted-residual boundary:** the 2026-06-08 resolution log
  accepted only the per-step `Arc::strong_count` sweep over live slots (I9 residual). These
  per-settle/per-cancel O(P) lookups are a different cost on a hotter path and were not
  filed or accepted.
- **Why real:** the deferred-capture pattern is the blessed shape for "service replies
  later" (gateways, pools, bridges). A fan-in gateway holding P = thousands of promoted
  replies pays O(P) per settle → O(P²) to drain, plus O(P) per unrelated call timeout.
- **Repro / failing test idea:** promote P slots, settle all P via `reply_to`; assert total
  time ~O(P) (slope test). Today it is O(P²).
- **Fix:** side index `HashMap<CallId, usize>` (slots already carry unique `call_id`;
  `take_by_handle` can go through `shared.call_id()` or a ptr-keyed map) maintained with
  the same swap_remove re-point pattern used everywhere else; `take_by_local_call_id`
  becomes O(1) and the no-slot fast path for `close_deferred_slot_for_call_with_reason`
  becomes a map miss.
- **LLM-pattern:** partially — the sweep got swap_remove + empty-skip (the filed defect),
  the lookups next to it kept their scans.

### I-NEW-6 [Medium / Confidence: Medium] — Stop/disconnect churn is O(n²): per-stop full rebuilds and the I8-residual four-Vec rescan

- **Where:**
  - `tina-runtime/src/dispatch.rs:1750-1791` (`cancel_pending_isolate_calls_for_owner`):
    per stopping isolate, `mem::take` + partition over **all** pending calls plus
    `rebuild_pending_isolate_call_indexes()` (`:1569-1578`, O(n) HashMap+BTreeMap rebuild).
    Called from every `stop_entry` (`:2433-2438`). A mass-disconnect of S connection
    isolates on a shard with n pending calls is O(S × n) — O(n²) when each closing
    connection holds calls.
  - `tina-runtime/src/dispatch.rs:3241-3276` (`can_gc_stopped_entry`): for every
    stopped-but-not-yet-collectable entry, **every step**, four linear scans —
    `child_records` (O(C)), `supervisors`, `in_flight_calls` (O(K)),
    `pending_isolate_calls` (O(n)). The I8 fix made the *removal* one-pass and added the
    `has_stopped_entries` skip, but a close-storm where S stopped isolates remain blocked
    on in-flight work burns O(S × (C + K + n)) per step until they clear. The I8 filing
    named this ("cache per-entry refcounts instead of re-scanning four Vecs"); the fix
    didn't include it and the resolution log records no acceptance.
- **Why real:** connection-close storms and OneForAll restart waves are exactly the
  "restart churn" regime; per-step cost scales with stopped×in-flight until drains finish.
- **Fix:** for the cancel path, only rebuild indexes when something was removed and prefer
  swap_remove-by-index over whole-Vec partition for small owned sets; for GC, keep per-entry
  counters (calls/children/supervisors outstanding) decremented on settle instead of
  rescanning.
- **LLM-pattern:** yes — burst-tested fix, steady-state residual left.

### I-NEW-7 [Low-Medium / Confidence: High] — Supervision/spawn paths keep linear entry/child lookups the existing index makes free

- **Where:**
  - `tina-runtime/src/registration.rs:567-569` `entry_by_isolate` =
    `entries.iter().find(...)` and `:593-617` `try_registered_address` (same scan) — both
    sit next to the indexed `entry_index()` and could be `entry_indexes.get`. Callers:
    restart/stop/supervision (`dispatch.rs:2527,2633,2696,2958`), remote spawn-reply
    (`remote.rs:471`), spawn (`registration.rs:427`), child lifecycle
    (`child_lifecycle.rs:91`).
  - `registration.rs:580-591` `child_record_index_by_child` / `supervisor_index` and the
    `child_records.iter().position` scans (`dispatch.rs:2812`, `remote.rs:544`) — O(C) per
    child stop/restart event.
  - `dispatch.rs:811-815` `pending_remote_spawns` `position` + `Vec::remove` (O(n) shift),
    same in `remote.rs:446-449`.
- **Why filed:** under restart churn (supervisor restarting thousands of children) each
  event is O(N entries + C child records) — O(N²) per storm. Not per-message hot, hence
  Low-Medium, but `entry_by_isolate` is a two-line fix via the existing map.
- **LLM-pattern:** yes — index exists, siblings bypass it.

### I-NEW-8 [Low-Medium / Confidence: High; possibly by design] — Default `TraceRetention::Full` is an unbounded per-shard buffer in production

- **Where:** `tina-runtime/src/threaded.rs:145` (`trace_retention: TraceRetention::Full` in
  `ThreadedRuntimeConfig::default()`); growth at `dispatch.rs:3131-3163` (`push_event`,
  ≥3 events per delivered message); worker exit also does `runtime.trace().to_vec()`
  (O(total events), `threaded.rs:1832`).
- **Invariant:** "bounded capacity means the real thing is bounded." Every default-config
  production runtime accretes trace memory forever (~10k msg/s ≈ tens of MB/min, plus
  realloc churn on the hot path).
- **Why "possibly by design":** Full-by-default was the safe choice for proof truth (G4).
  But nothing in the config docs warns that the default leaks on long-running servers;
  `Bounded` exists and is one line away.
- **Fix:** either default `Bounded(large)` for the threaded runtimes (keeping `Full` for
  sim/proof paths), or document the leak loudly on `ThreadedRuntimeConfig`.

### I-NEW-9 [Low / Confidence: High] — `WaitList::park` with a `per_key_limit` still pays 2×O(capacity) per admission

- **Where:** `tina-runtime/src/wait_list.rs:406-419` (and `park_call` `:436-449`): when
  `per_key_limit.is_some()`, every park runs `sweep()` (O(cap), `:381-394`) plus
  `count_for_key` (O(cap), `:461-470`). The I6 free-slot-stack fix covers only the
  no-per-key-limit configuration. `capacity_report` → `live_len()` is another O(cap) scan
  (`:357-366`) but is report-path only.
- **Why low:** capacities are documented small-ish, and per-key configs are opt-in. Still
  the "half of the filed fix" pattern; a `HashMap<K, usize>` live-count map finishes it.

### Accepted / noted, not filed as defects

- **`terminal_overflow` unbounded `VecDeque`** (`threaded_multi_shard.rs:943`, pushes at
  `:1162-1170`): each element corresponds to an already-admitted call/child operation, so
  occupancy is structurally bounded by cross-shard admission caps; this is the C1 fix's
  deliberate lossless-terminal trade. The *spin* interaction is covered in I-NEW-2.
- **O(entries) round probe per step** (`dispatch.rs:279-292`): every step probes every
  registered entry (`stopped` flag + `entry_has_pending_message`). With C10k mostly-idle
  connection isolates this is ~O(10k) cheap probes per delivered-message round — a real
  per-message overhead ceiling, but it is the documented "deterministic round" design and
  the probe is intentionally cheap (phase 150). Worth a future ready-queue, not a defect.
- **Synchronous trace observer on the shard turn** (`dispatch.rs:3141-3143`): documented
  contract ("Hot path. Bound your work."), with `BufferedTraceObserver` as the bounded
  escape hatch (`observer.rs:34-97`). Matches the playbook concern but is an explicit,
  documented design.
- **I9 residual sweep** (`deferred.rs:113-126`): per-step O(P live) `Arc::strong_count`
  scan — already accepted in the 2026-06-08 resolution log; unchanged; not re-filed.

---

## Disproven suspicions (with proof)

1. **swap_remove index-map corruption in dispatch.rs** (in_flight_calls / translators /
   pending_isolate_calls): disproven. All three removers re-point the moved tail's map
   entry guarded by `index < len` (`dispatch.rs:186-194`, `:207-215`, `:1553-1567`); both
   pushers assert no duplicate key; `cancel_driver_calls_for_requester` does not advance
   `index` after a swap_remove so the moved element is re-checked (`dispatch.rs:144-168`);
   shutdown clears Vec+maps together (`:94-110`).
2. **gc_stopped_entries swap_remove skip**: disproven — on removal the loop re-checks the
   swapped-in tail (`dispatch.rs:3220-3233`); `has_stopped_entries` is set on every stop
   site (single setter `dispatch.rs:2417-2418` inside `stop_entry`, which all stop paths
   funnel through) and re-derived by the sweep, so no stopped entry is stranded.
3. **HTTP/2 stream index drift (I10 fix)**: disproven on both sides. Server
   `push_stream`/`remove_stream` keep the map consistent including the tail case
   (`server.rs:2272-2288`); client `swap_remove_stream_at` mirrors it
   (`client.rs:1536-1547`); every bulk teardown pairs `streams.drain(..)` with
   `stream_index.clear()` (`client.rs:2469-2470`, `:2494-2495`, `:2667-2668`); the GOAWAY
   loop does not advance `idx` after `fail_stream`'s swap_remove (`client.rs:1961-1975`);
   in-repo regression tests exist (`server.rs:2664-2682`).
4. **Readiness-park lost wakeup (single-shard)**: no defect found. The doorbell is
   coalescing with a pre-park pending flag consumed inside `step_blocking`
   (`io/darwin.rs:34-46`, `:890-899`; condvar reference impl `lib.rs:171-220`); host
   mailbox sends ring the doorbell on the empty→non-empty transition via the shared wake
   hook (`mailbox.rs:193-199`, `registration.rs:61-67`); channel-delivered lanes and
   carried completions force a capped re-poll via `park_needs_repoll`
   (`dispatch.rs:2016-2031`); `next_park_deadline` includes both driver timers and call
   deadlines (`dispatch.rs:2002-2014`); a stale-resource-count park is pre-empted by the
   explicit refresh before parking (`threaded.rs:1786-1792`). Could not construct a parked
   state with deliverable work and no wake source.
5. **Hot-drain starvation of commands**: disproven — the burst re-polls the command queue
   between rounds and is double-bounded (rounds cap 4096 + 50 ms elapsed cap checked every
   64 rounds, `threaded.rs:1737-1776`, constants `:162-171`).
6. **Keepalive chunked re-decode O(body²)**: disproven — the decoder is incremental via
   `chunked_raw_consumed` (`keepalive.rs:770-805 in HEAD`); head re-parse per read is
   bounded by head-size limits. The committed over-send retire (A-F3 fix) is present
   (`read_buf.len() > body_end` → `must_retire`, `keepalive.rs:815-817`); chunked-complete
   always retires, covering trailing-bytes desync on that path.
7. **gRPC client stream decoder quadratic**: disproven — cursor + single drain
   (`grpc_client.rs:560-590`). (The *server* request decoder is not, see I-NEW-3.)
8. **Pool acquire O(waiters) regression (I7)**: disproven — free-slot stack +
   incremental `live_waiters` counter with checked underflow (`pool.rs:235-236`,
   `:430-451`); `handle_acquire_slot` is O(1) (`:575-596`); sweeps are maintenance-path.
9. **SPSC mailbox probe cost**: fine — `is_empty` is an atomic load; wake hook fires only
   on empty→non-empty with an installed-flag fast path (`tina-mailbox-spsc/src/lib.rs:193-206`).

## Invariants violated (this track)

- *Per-message cost independent of resident population* — I-NEW-1 (sends/calls scale with
  registered isolates), I-NEW-4 (with in-flight calls), I-NEW-5 (with promoted slots).
- *No busy-poll / core-burning spin* (phase 151's own stated rule) — I-NEW-2, plus the
  in-code comment asserting the opposite (truth gap).
- *A peer's bytes cost O(bytes)* — I-NEW-3 (frame-count amplification).
- *Bounded means bounded* — I-NEW-8 (default-Full trace).

## Suggested tests

- Throughput-vs-population slope tests: sends/sec with K idle isolates (I-NEW-1); settle
  time for P promoted replies (I-NEW-5); delivery cost with P pending calls (I-NEW-4).
  Assert flat/linear slope, not super-linear.
- Multi-shard idle-CPU proof mirroring `readiness_park.rs`: pending timer / armed accept on
  a `ThreadedMultiShardRuntime`, assert near-zero CPU over a window (I-NEW-2).
- Frame-flood slope tests: one read buffer of N minimum-size frames through the HTTP/2
  client loop, WS `parse_frame` loop, and `GrpcRequestStream::next_buffered`; assert ~O(N)
  (I-NEW-3).
- Close-storm step-cost test: stop S isolates each holding an in-flight driver call; bound
  per-step time while they drain (I-NEW-6).
- Long-run memory ceiling test under default config: assert trace memory bounded or
  document `Full` loudly (I-NEW-8).

## Coverage map

| Area | Result |
|---|---|
| dispatch.rs ingress (send/call dispatch) | I-NEW-1 (High) |
| dispatch.rs delivery loop / caller build | I-NEW-4 (Med) |
| scheduler/park single-shard | clean (disproof #4, #5) |
| scheduler/park multi-shard | I-NEW-2 (High) |
| deferred.rs | I-NEW-5 (Med); I9 residual unchanged/accepted |
| stopped-entry GC / stop churn | I-NEW-6 (Med, I8 residual) |
| wait_list / pool | I-NEW-9 (Low); pool clean |
| http2 server per-frame | clean (cursor fix verified) |
| http2 client / WS / gRPC-request per-frame | I-NEW-3 (Med) |
| keepalive client | clean (disproof #6) |
| index/swap_remove fix audit (I1/I2/I3/I8/I9/I10) | maps consistent (disproofs #1-#3); fixes correct but each left an unindexed sibling (I-NEW-1/4/5/7) |
| mailbox crates | clean (disproof #9) |
| trace/observer | I-NEW-8 (Low-Med, possibly by design); observer documented |
