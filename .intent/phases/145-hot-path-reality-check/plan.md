# Phase 145: Hot Path Reality Check

Status: implemented.

Outcome: the shard worker slept a flat 1ms after every progress step, taxing
each runtime turn. A tiny local call needs several turns, so observed admission
and host calls ran millisecond-scale. The fix loops immediately while steps
deliver work and parks on the command queue (bounded `idle_wait`) only when
nothing is deliverable. Results (release, local):

- `observed_admission` p50 1.34ms -> 53us
- `host_request_reply` p50 5.79ms -> 210us
- `service_request_reply_chain` p50 12.07ms -> 400us
- HTTP rows fell ~10-16x as a free downstream effect

The first fix made the row acceptable, but the probes still showed avoidable
host-call work. This phase also shipped the host-call dispatcher pool and the
per-host-thread reply channel pool:

- no fresh `HostCallDriver` isolate per `call_blocking`;
- `call_blocking` process allocations 17 -> 7;
- host-thread allocations 5 -> 2 after the reply pool;
- DST replay parity via `SimulatorConfig::reserved_system_isolates`.

The remaining gap is now named: same-shard host calls still pay several
cross-thread worker turns, and HTTP still allocates 1.45-1.8x the Axum row.
See `examples/systems/perf_native/README.md` and the evidence files in this
directory.

Hostile-review follow-ups (same phase):

- Suspected multi-shard hot-spin — disproven. The multi-shard worker loop's
  pending-in-flight branch uses `thread::yield_now()`, which looked like a hot
  spin. Measuring process CPU across a held call (no pending I/O), a pending
  timer, and a cross-shard reply wait all showed near-zero worker CPU: the
  runtime step blocks inside the betelgeuse io_loop whenever there is work to
  wait on, so the yield never tight-spins. An in-progress change to replace it
  with a bounded park was reverted — it fixed no real spin and would only add
  `idle_wait` latency to cross-shard replies. No no-spin test ships, because a
  truthful one is vacuous (the property holds by construction).
- Both worker loops recomputed the O(pending) resource report at the top of
  every iteration. With loop-immediately-on-progress that ran on every
  message-delivery turn at full speed — a real per-op tax under sustained
  throughput. The snapshot is now skipped on fast delivery turns and refreshes
  on idle/command turns (and at shutdown), so observability is unchanged at
  every point an observer can read it.
- The instrumented `send_and_observe` probe cleared its timeline while the warm
  message's delivery was still pending, so warm-delivery events raced into the
  measured window. Drain the warm message first; the breakdown now shows the
  honest single admission round-trip.
- Added single-shard host-wait-timeout and shutdown-while-call-pending coverage
  (the existing matrix only had the multi-shard variants).

## Goal

Phase 144 made the truth visible:

```text
host_enqueue is tiny
observed admission is milliseconds
call_blocking is milliseconds
multi-turn call chains are worse
HTTP is slow, but do not blame HTTP first
```

This phase is a bug hunt for Tina's live hot path. The goal is not a public
performance claim. The goal is:

```text
measure every stage
find the dumb wait/allocation
fix the Tina-owned path
prove before/after with the same rows
```

The known code suspects from the planning spike:

- `tina-runtime/src/threaded.rs` worker loop sleeps `1ms` after progress. A
  tiny local call needs several runtime turns, so this can create fake
  millisecond latency.
- `ThreadedRuntime::call_blocking` creates and registers a fresh
  `HostCallDriver` isolate for every call. That is correct, but very heavy.
- `ThreadedRuntime::send_and_observe` allocates a channel and boxed command per
  observed send.
- `RuntimeCall::isolate_call` boxes messages and translators. Some boxing may
  be honest type erasure; per-op accidental boxing is not.

## Build

1. Add a hot-path stage report.
   - Add a small report type in `tina-proof-harness`, for example
     `HotPathReport`.
   - It prints grep and JSON lines.
   - It records stage timings for:
     - host submit -> command accepted
     - command accepted -> worker starts command
     - worker command -> first handler enter
     - handler enter -> effect emitted
     - effect emitted -> reply/message enqueued
     - reply/message enqueued -> host unblocked
   - Use nanoseconds and microseconds. Fast rows must not round to fake zero.
   - Add allocation counts for each measured op where the allocator probe can
     see them.

2. Add three focused probes.
   - `hotpath_try_send`
     - one host `try_send`
     - proves first queue handoff stays cheap
   - `hotpath_send_and_observe`
     - one observed send
     - reports where the wait happens
   - `hotpath_call_blocking`
     - one host call to one immediate-reply isolate
     - reports every runtime turn until the host receives `Replied`
   - Keep probes in release tests/examples, not production logging.

3. Fix worker-loop progress before touching higher layers.
   - In `threaded_worker_loop`, do not sleep after `runtime.step()` made
     progress.
   - After delivered work, loop immediately and let the shard drain ready work.
   - When no work is delivered and no runtime work is pending, park on the
     command queue with `idle_wait`.
   - When no work is delivered but runtime-owned work is pending, use a bounded
     non-hot-spin policy:
     - no unconditional `1ms` tax after every progress step;
     - no CPU-burning tight loop while waiting for timers/I/O;
     - tests prove idle CPU does not spin under a pending timer.
   - Preserve command ingress fairness: a hot isolate cannot starve host
     shutdown or new commands forever. Use a small drain/step budget if needed.

4. Rerun Phase 144 rows after the worker-loop fix.
   - `host_enqueue`
   - `observed_admission`
   - `host_request_reply`
   - `service_request_reply_chain`
   - HTTP rows only as secondary signal.
   - Save before/after rows in the phase findings or README.
   - If `host_request_reply` p50 is below `500us` on local release after the
     worker-loop fix, do not replace `call_blocking` in this phase. Document
     the next smaller bottleneck instead.
   - If `host_request_reply` is still above `500us` p50, or if stage timing
     shows most time is per-call driver setup, do Rock 5 in this phase.

5. If `call_blocking` is still terrible, replace the per-call driver path.
   - Add a direct host-call path owned by the shard runtime. Acceptable shapes:
     - a runtime-owned pending-host-call table keyed by `CallId`; or
     - one persistent internal host-call endpoint per worker.
   - Unacceptable shape: register a new `HostCallDriver` isolate per call.
   - The host call should not register a new isolate per request.
   - Add `host_call_capacity` or an equivalent explicit cap to
     `ThreadedRuntimeConfig` if a new host-pending table is introduced.
   - The host call still returns the same public `CallOutcome<R>`:
     `Replied`, `Full`, `Closed`, `Timeout`, `Rejected`.
   - It must keep Tina truth:
     - target mailbox `Full` is still `CallOutcome::Full`;
     - stopped/stale target is still `CallOutcome::Closed`;
     - target timeout is still `CallOutcome::Timeout`;
     - unsupported call is still `CallOutcome::Rejected`;
     - late replies still surface in trace;
     - host wait timeout does not pretend target work was cancelled.
   - It must keep bounded storage:
     - host pending replies have a configured cap;
     - full host pending storage returns a typed error, not an unbounded queue;
     - shutdown drains or rejects pending host callers visibly.
   - It must preserve cross-shard behavior or explicitly refuse unsupported
     cross-shard direct host calls with a typed outcome.

6. If `send_and_observe` is still above `500us` p50, reduce its overhead.
   - Keep the strict semantics: wait for worker-observed target mailbox outcome.
   - Avoid avoidable allocation in the one-shot reporting path.
   - Do not turn `MailboxFull` / `MailboxClosed` into timeout.
   - Do not make observed send async or fire-and-forget under the same name.

7. Keep the benchmark honest.
   - Re-run `make perf`.
   - Update `examples/systems/perf_native/README.md` with:
     - what got faster;
     - what is still bad;
     - which remaining rows are semantic cost versus implementation waste.
   - Do not tune HTTP until the host/call rows are no longer obviously broken.

## Must Not

- Do not weaken Tina truth to get speed.
- Do not remove trace, capacity, cancellation, reply-obligation, or replay
  facts from the measured path.
- Do not compare a truthful Tina path to an untruthful Tokio path and call it
  fixed.
- Do not hide a host wait behind a longer timeout.
- Do not add unbounded host pending tables, observer queues, or retry loops.
- Do not optimize HTTP first. HTTP is downstream of the runtime hot path.

## Proof

- Unit tests for worker-loop progress:
  - immediate local send/call does not pay a fixed 1ms delay per runtime turn;
  - pending timer/I/O does not hot-spin an idle worker;
  - shutdown command is still observed promptly under a hot local workload.
- Hot-path tests print stage reports for:
  - `try_send`;
  - `send_and_observe`;
  - `call_blocking`.
- Allocation tests pin warmed allocations for:
  - `send_and_observe`;
  - `call_blocking`;
  - direct host call if added.
- Behavioral tests for every optimized path:
  - success;
  - target mailbox full;
  - target closed/stale;
  - target timeout;
  - rejected unsupported call;
  - host wait timeout;
  - shutdown while pending.
- Cross-shard test if the optimized host path supports cross-shard. If not,
  a test proves the unsupported outcome is typed and visible.
- `make perf` passes and shows before/after rows in release mode.
- No new unbounded storage. Any new pending-host-call table has a capacity
  report or a typed `Full`.

## Done

- A user can run `make perf` and see where host send/call time goes.
- The known `1ms` progress tax is gone or explicitly disproven by stage timing.
- `call_blocking` no longer registers a fresh driver isolate per successful
  fast call, unless stage timing proves that is not the bottleneck.
- `observed_admission` and `host_request_reply` are no longer millisecond-scale
  for tiny local same-shard work on a normal machine.
- Any remaining bad rows are named with evidence and a next bottleneck.
