# Carve-out: tina-tracing install/live + multishard shutdown trace flush — 2026-06-09

Reviewed at HEAD `0cd6a31` (= origin/main). Scope per brief: tina-tracing
install.rs / live.rs, live multishard shutdown trace-flush completeness
(threaded_multi_shard.rs + dispatch shutdown paths), interaction with the
G1 `LiveTraceProofError::Multishard` fail-closed fix. Track G (TG-1..TG-6)
explicitly excluded this area; nothing below duplicates a TG finding —
CT-3 in fact falsifies an assumption TG-2's prose relied on.

## Scope reality check

- `tina-tracing/src/install.rs` (31 lines): one `set_global_default`
  wrapper. No state, no buffer.
- `tina-tracing/src/live.rs` (153 lines): synchronous per-snapshot
  emission. No background thread, no buffer.
- The "bounded live buffer" of the brief lives in `tina-runtime`:
  the retention ring (`dispatch.rs:3131-3186`) and
  `BufferedTraceObserver` (`tina-runtime/src/observer.rs:44-95`).
  Shutdown-flush completeness is a tina-runtime question; investigated
  there.

## Answers to the brief's three questions

**Q1 (multishard shutdown trace collection):** the clean path is sound.
Each worker collects `runtime.trace().to_vec()` *after*
`deliver_shutdown_signal_and_drain` + `cancel_in_flight_calls_for_shutdown`
(`threaded_multi_shard.rs:1143-1150`, `threaded.rs:1828-1835`); the joiner
joins every worker, merges exits, and sorts `(shard, id)` with an honest
"stable grouping, not temporal order" comment (`shutdown.rs:500-542`).
Events recorded just before shutdown cannot vanish from the terminal
report on that path. **But the path itself can blow up:** the multishard
shutdown drain runs `Runtime::step()`, whose remote-route closure panics
on any cross-shard effect — a busy multishard shutdown panics the worker
and loses that shard's *entire* trace (CT-1, High; acknowledged in an
in-tree test comment but tracked nowhere).

**Q2 (install.rs / live.rs / record path):** install.rs is correct
(double-install → typed `SetGlobalDefaultError`; atomic in `tracing`).
live.rs emission is correct per its level table, and it dutifully emits
`trace_dropped` — which is hardcoded `None` at the source, so the live
drop surface is dead (CT-3, Medium). Bounded-buffer drops in
`BufferedTraceObserver` are counted, but the counter cannot distinguish
"dropped" from "not yet drained", and there is no flush/barrier API, so
the documented proof recipe is racy (CT-2, Medium). Record-path panic
semantics are documented and deliberate ("a panic here kills the
recording thread by design", `dispatch.rs:3139-3143`); poison on the
`LiveTrace` mutex is practically unreachable (D4).

**Q3 (G1 interaction, single-shard lost-on-shutdown):** the in-tree
single-shard proof path is sound: `LiveTrace`'s observer is synchronous on
the recording thread, and `shutdown()` blocks until every worker is
joined, so a post-shutdown `snapshot_complete(0)` sees every event
(D3). The hole is the `BufferedTraceObserver` wrapper the proof docs
explicitly bless: `dropped_count() == 0` does not mean drained, so a
single-shard proof can hash a silent prefix and label it complete —
G1's `Multishard` and `Lossy` gates both pass (CT-2).

## Findings

### CT-1 — Multishard shutdown drain panics on cross-shard effects; whole-shard trace loss
1. **Severity:** High
2. **Confidence:** High (mechanism proven by code; existence acknowledged
   by an in-tree workaround comment)
3. **File/line:** `tina-runtime/src/threaded_multi_shard.rs:1091` and
   `:1131-1133` (worker calls `deliver_shutdown_signal_and_drain` on
   `Shutdown`); `tina-runtime/src/threaded.rs:1838-1849` (the drain loops
   `runtime.step()` up to 1024 times); `tina-runtime/src/dispatch.rs:225-263`
   (`step()` wraps `step_with_remote` with a closure that `panic!`s on
   **every** `QueuedRemoteEnvelope` variant: "cross-shard send is out of
   scope in this slice"); `dispatch.rs:796/884/1001/1118/1307/1467` (the
   closure fires synchronously from handler effect processing — ordinary
   sends, call dispatch/replies, deferred replies, child control).
   Smoking gun: `tina-runtime/tests/multishard_fairness.rs:426-429` —
   "Stop the source first so shutdown-time `runtime.step()` does not try
   to route cross-shard sends (that path lacks a remote handler and
   panics — separate pre-existing failure mode, not A12)." Grep of
   `.intent/` finds no tracking of this failure mode.
4. **Violated invariant:** `shutdown_report` doc contract — "Requests
   shutdown and joins every worker, **always returning terminal truth**"
   (`threaded_multi_shard.rs:666`); and the brief's Q1 invariant: events
   recorded before shutdown must reach the terminal trace.
5. **Concrete bug:** when a multishard worker reads `Shutdown` while its
   local mailboxes still hold messages whose handlers produce any
   cross-shard effect, the drain's `step()` hits the panicking
   route-remote closure. The worker thread unwinds; its
   `ThreadedWorkerExit` (carrying the shard's full trace) is never
   produced. The joiner's `handle.join()` returns `Err`
   (`shutdown.rs:513-521`): shard marked `Failed`,
   `failure = WorkerStopped`, **zero events from that shard** in the
   terminal report. Other shards' drains can panic the same way, so a
   busy 2-shard shutdown can lose both traces. The normal worker loop
   routes remote effects correctly (`step_with_remote` with the real
   pair-queue router, `:1098`); only the shutdown drain swaps in the
   panicking `step()`.
6. **Real-use scenario:** any production multishard service shut down
   while cross-shard traffic is in flight — i.e. the routine case. A
   clean `shutdown()` request returns `Err(WorkerStopped)`, the operator
   loses the incident-relevant tail of the trace (the whole shard's
   retained trace, not just the tail), and the panic message blames a
   "slice scope" that has nothing to do with the user's code.
7. **Failing test idea:** copy `shutdown_under_remote_flood_completes_bounded`
   (`multishard_fairness.rs:402`) but delete the
   `running.store(false)` workaround at `:429`: flood cross-shard sends,
   call `runtime.shutdown()` mid-flight, assert it returns `Ok` and the
   report state is `Closed` with events from both shards. Today the
   shard-11 worker panics. (The pinger workload from
   `tina-proof-harness/tests/multishard_trace_determinism.rs` without its
   `drain()` call works too.)
8. **Fix sketch:** give the multishard worker a
   `deliver_shutdown_signal_and_drain_with_remote(&mut runtime, &mut route_remote)`
   that reuses the loop's existing lossless router (peers may still be
   alive and draining; a dead peer surfaces as
   `SendRejected{Closed}` — recorded honestly). Keep plain `step()` for
   the single-shard worker, where a foreign-shard address is a real
   programmer error. Also remove the now-unneeded workaround comment in
   `multishard_fairness.rs`.
9. **LLM-pattern?** Yes — helper written for the single-shard slice
   (`deliver_shutdown_signal_and_drain` predates multishard) reused
   verbatim on the multishard path because it compiled; the test that
   would have caught it was bent around the bug instead.

### CT-2 — `BufferedTraceObserver` has no drain barrier; `dropped_count()==0` does not mean complete
1. **Severity:** Medium (latent: no in-tree proof path uses it; the
   public docs actively recommend the broken recipe)
2. **Confidence:** High on mechanism
3. **File/line:** `tina-runtime/src/observer.rs:58-78` (`new` spawns a
   detached drain thread, handle discarded; no flush/join API),
   `:86-94` (`on_event` = `try_send`, drops counted);
   `tina-proof-harness/src/live_replay.rs:23-27` and `:144-147` (proof
   docs: "If you install `BufferedTraceObserver`, pass its
   `dropped_count()`" to `snapshot_complete`); `observer.rs:41-43`
   ("Do not feed a proof/replay hash from a buffered observer unless the
   proof path first checks `dropped_count() == 0`").
4. **Violated invariant:** the G1 fail-closed contract — a proof-grade
   snapshot must refuse a capture that is missing events. The documented
   loss signal (`dropped_count`) only counts *rejected* events, not
   *undrained* ones.
5. **Concrete bug:** events accepted into the bounded queue are invisible
   to the downstream observer until the detached drain thread gets to
   them. There is no way to wait for that: the queue depth is
   unobservable, the drain thread's handle is discarded in `new()`, and
   no flush method exists. A caller that runs a workload, calls
   `runtime.shutdown()` (which joins workers — so all `try_send`s have
   *happened*, but not all `recv`s), then takes
   `snapshot_complete(buffered.dropped_count())` can read a strict
   prefix of the event stream with `dropped_count() == 0`. Both G1
   gates pass (single shard, "no loss"); the resulting `TraceShape`
   pins a hash of a timing-dependent prefix as proof material. Dropping
   the buffered observer doesn't help — the channel disconnects but the
   reader still can't observe when the drain finishes.
6. **Real-use scenario:** exactly the user the type is for — "production-
   style observation where blocking the shard is worse than losing an
   event" — following the crate's own proof recipe after an incident:
   capture with a buffered observer, shut down, snapshot, save the shape
   as a regression baseline. The baseline flaps with drain-thread
   scheduling; later `compare_live_shape_complete` failures get blamed
   on the workload.
7. **Failing test idea:** wrap a `LiveTrace` observer in
   `BufferedTraceObserver::new(4096, ...)` whose downstream is gated on a
   slow `Mutex` (reuse the `BlockingObserver` pattern from
   `observer.rs:161-206`); record 100 events through a runtime, shut
   down, assert `dropped_count() == 0` and then that
   `snapshot_complete(0)` either errors or sees all 100 events —
   currently it returns Ok over a prefix.
8. **Fix sketch:** keep the `JoinHandle` + `SyncSender` in the struct;
   add `fn flush(self_or_&self) -> u64` (or `close(self) -> u64`) that
   drops/then-fences the sender, joins the drain thread, and returns the
   final dropped count; document that proof-grade reads require
   `close()` first, and have `snapshot_complete` docs point at it.
   Cheaper alternative: a `drained_seq`/`sent_seq` pair so callers can
   poll `is_quiescent()`.
9. **LLM-pattern?** Yes — the failure mode that was *named* (drops) got a
   counter; the adjacent one the same design creates (lag) got nothing,
   and the docs assert the counter covers the proof question.

### CT-3 — `LiveShardReport.trace_dropped` is hardcoded `None`; the live drop surface is dead
1. **Severity:** Medium
2. **Confidence:** High
3. **File/line:** `tina-runtime/src/live_report.rs:268` (`report()`
   literal: `trace_dropped: None`); no writer anywhere — repo grep for
   `trace_dropped:` finds only this literal, the runtime's private
   counter init (`lib.rs:731`), and a test fixture (`tests.rs:3686`).
   Dead consumers: `live_report.rs:436` (getter),
   `tina-tracing/src/live.rs:42` + `:82` (emits the field per shard),
   topology snapshots in terminal reports (`shutdown.rs:528`,
   `:567`).
4. **Violated invariant:** retention drops must be operator-visible —
   the premise of the whole `trace_dropped` plumbing, and the assumption
   Track G's TG-2 prose leaned on ("surfaced in
   `LiveShardReport.trace_dropped`" — it is not).
5. **Concrete bug:** the real counter lives on the `Runtime` inside the
   worker thread (`Runtime::trace_dropped()`, `lib.rs:782`), and nothing
   ever copies it into the `LiveShardMetrics` block that `topology()` /
   `emit_snapshot` read. Under `TraceRetention::Bounded`/`Off` — the
   production configuration — a shard can drop millions of events while
   every live topology report and every `tina-tracing` `live_shard`
   event shows `trace_dropped = "-"` (the `OptValue(None)` rendering).
   There is also no `ThreadedRuntime`/`ThreadedMultiShardRuntime` public
   accessor for it, so a live operator has **no** way to see drops at
   all. Combined with TG-2 (`complete_trace()` launders the truncated
   suffix), bounded retention currently has zero honest surfaces on the
   threaded runtimes.
6. **Real-use scenario:** operator runs bounded retention, watches the
   `tina_runtime::live` stream, sees `trace_dropped=-` forever, and
   reasonably concludes no events were dropped before exporting a
   timeline or summarizing pressure from the retained suffix.
7. **Failing test idea:** `ThreadedRuntime` with
   `TraceRetention::Bounded(3)`, push >3 events, quiesce, assert
   `runtime.topology().shards()[0].trace_dropped() == Some(n)` with
   n > 0 — currently `None`.
8. **Fix sketch:** add an `AtomicU64 trace_dropped` to
   `LiveShardMetrics`; publish it where the worker already touches
   metrics each turn (`set_resource_counts` call sites, or a dedicated
   store on the park/command turns plus once at exit before
   `runtime.trace().to_vec()`); `report()` reads
   `Some(load)` once the worker has started. This also gives TG-2's fix
   the signal it needs at the wrapper layer.
9. **LLM-pattern?** Yes — schema-complete, data-flow-absent: field,
   getter, emitter, and docs all exist; the single store that would make
   them true was never written. Compiles, renders, lies.

## Disproven suspicions (with proof)

- **D1 — terminal-report trace loses pre-shutdown events (clean path):**
  disproven. Worker exits collect the runtime trace *after* the shutdown
  drain and in-flight cancellation record their events
  (`threaded_multi_shard.rs:1143-1150`; `cancel_in_flight_calls_for_shutdown`
  pushes `CallCompletionRejected{RequesterClosed}` / `CallCancelled`
  events *before* the trace copy, `dispatch.rs:94-130`). The joiner joins
  every signaled worker before caching the report
  (`shutdown.rs:500-542`); `ensure_joiner_started` refuses to start
  until all workers are signaled (`:258`), so no worker is joined while
  still able to record. Sort is `(shard, id)` with an explicit
  "not a temporal order" comment — no cross-shard order is faked.
- **D2 — `trace()`/`complete_trace()` racing shutdown silently lose a
  shard:** disproven. A worker that took `Shutdown` before the trace
  `Run` command drops the command's reply channel at exit →
  `call_on` returns `WorkerStopped` → the shard lands in
  `missing_shards` and `TraceSnapshot::is_complete()` is false
  (`threaded_multi_shard.rs:592-603`); `complete_trace()` errors
  (`:606-613`). Fail-visible. (Retention laundering on the non-racing
  path is TG-2, not re-filed.)
- **D3 — single-shard proof path loses lost-on-shutdown events (Q3,
  direct wiring):** disproven for the synchronous observer. The observer
  fires inside `push_event` on the worker thread before retention
  (`dispatch.rs:3139-3143`); `shutdown()` →
  `wait_report_blocking` blocks until the joiner has joined the worker,
  and joining a thread happens-after everything it did — so after
  `shutdown()` returns, every recorded event has been pushed into
  `LiveTrace`. The in-tree proof users follow exactly this order
  (`multishard_trace_determinism.rs:222-250` snapshot after
  `shutdown()`; `mini_saas_api/src/tina_impl.rs:1694` single-shard
  synchronous wiring). The buffered wrapper is the exception — CT-2.
- **D4 — `LiveTrace` mutex poison panics the record path:** practically
  disproven. Poison requires a panic while the lock is held; holders are
  `Vec::push`/`clone`/`len` (alloc failure aborts rather than unwinds in
  practice) — no user code runs under the lock. Observer panics killing
  the worker is documented design (`observer.rs:10`,
  `dispatch.rs:3139-3140`); note it shares CT-1's loss mode (panicked
  worker returns no trace), which is one more reason CT-1's panic is
  expensive.
- **D5 — install.rs double-install race / install-after-events:**
  disproven as a bug. `set_global_default` is atomic in `tracing` (one
  winner, losers get the typed error; the in-tree test pins the second
  call). Events emitted before install go to the no-op default
  dispatcher — inherent `tracing` semantics, demo-only crate role
  (module doc says so), and the runtime's own trace ring is unaffected,
  so no proof surface depends on subscriber timing.
- **D6 — live.rs flattens truth:** disproven. Levels match the module
  doc exactly (Running/Stopped/Failed → INFO/WARN/ERROR per state;
  remote queue WARN only on `rejected_full > 0`); every typed field is
  emitted, `Failed(reason)` flattening to `"Failed"` is documented with
  the reason kept on the report (`live.rs:141-153`). Ingress
  `rejected_full` not escalating the *shard* level is a defensible
  level-policy choice, not a truth flattening — the count is in the
  event.
- **D7 — queued cross-shard envelopes / `terminal_overflow` silently
  destroyed at shutdown lie in the trace:** not filed. The dropped
  envelopes' *work* is lost (bounded-drain shutdown semantics), but the
  trace does not lie: the source shard recorded its send/reply events,
  the requester's in-flight calls get explicit
  `CallCompletionRejected{RequesterClosed}` terminal events at exit, and
  un-handled accepted messages are detectable as accepted-without-
  handler-start in the merged trace. No counter summarizes it, but no
  surface claims otherwise.
- **D8 — 1024-step shutdown-drain cap silently abandons work:** same
  shape as D7 — intentional bound, detectable from the trace, no surface
  claims completeness of *work*. (On multishard the cap is currently
  unreachable past the first cross-shard effect because of CT-1.)
- **D9 — live `trace()` is a non-atomic cross-shard cut labeled
  complete:** true but not filed. Per-shard snapshots are taken
  sequentially (`threaded_multi_shard.rs:595-600`), so a later shard's
  slice can contain effects of an earlier shard's not-yet-snapshotted
  events (dangling `cause` across shards). Consumers: the proof gates
  refuse multishard traces outright (G1), and the timeline export is
  explicitly visual with unmatched-event handling; its multishard id
  semantics are already TG-5. Inherent to observing a free-running
  system; would only deserve a flag (`"cut": "non-atomic"`) as part of a
  TG-5-style metadata fix.

## Ranked fixes

1. CT-1 — multishard shutdown drain must route (or typed-reject)
   cross-shard effects, never panic; un-bend the fairness test.
2. CT-3 — publish the runtime's `trace_dropped` into `LiveShardMetrics`
   (prerequisite for TG-2's wrapper-level fail-closed fix).
3. CT-2 — `BufferedTraceObserver::close()/flush()` drain barrier; align
   the proof docs in `live_replay.rs`.

## Suggested tests

- Multishard shutdown under live cross-shard flood returns `Ok`/`Closed`
  with both shards' events present (CT-1; delete the
  `running.store(false)` pre-quiesce in a copy of
  `multishard_fairness.rs:402`).
- Bounded retention: `topology()` shard report carries
  `trace_dropped == Some(n>0)` after overflow (CT-3); `tina-tracing`
  `live_shard` event renders the number.
- Buffered observer: slow downstream + post-shutdown
  `snapshot_complete(dropped_count())` must not return Ok over a prefix
  (CT-2; passes only with a drain barrier).

## Coverage map

| Area | Result |
|---|---|
| tina-tracing install.rs | D5 — clean |
| tina-tracing live.rs | D6 emission clean; consumes dead field (CT-3) |
| tina-tracing observer.rs / TracingObserver | stateless, panic rules documented (D4) |
| threaded_multi_shard.rs shutdown path | CT-1 (drain panics); D1/D2 clean-path collection verified |
| threaded.rs single-shard shutdown path | D1/D3 verified; plain `step()` correct for single shard |
| shutdown.rs joiner | verified: join-before-report, (shard,id) sort, panic-safe joiner, spawn-failure inline fallback (E1 test) |
| dispatch.rs cancel/notify shutdown helpers | record events before trace copy; no route_remote use (D1) |
| Runtime trace ring + observer hook (dispatch.rs:3131-3207) | observer-first, drops counted; counter never published (CT-3) |
| BufferedTraceObserver | CT-2 |
| proof-harness LiveTrace / snapshot_complete / RunCapture | G1 gates verified on this surface; CT-2 is the remaining side door for Q3 |
