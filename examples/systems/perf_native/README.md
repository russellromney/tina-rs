# Native Perf Rows

Small release-mode performance rows for native Tina designs against bounded
Tokio designs.

This is not a production benchmark suite. It is alpha evidence:

- same op count
- same worker count
- bounded capacity on both sides
- pressure and leak truth printed beside timing
- median of five measured samples after warmup
- allocation counts for work done inside the load worker op
- semantic match labeled as `exact` or `partial`

Run:

```sh
cargo test --release --manifest-path examples/systems/perf_native/Cargo.toml --test perf -- --nocapture
```

Or from the repo root:

```sh
make perf-compare
```

## Hot-path stage probes

The comparison rows say how slow a path is. The probes say *where the time
goes*. Three probes break a single op into stages, with a live trace observer
timestamping every worker turn:

```sh
cargo test --release --manifest-path examples/systems/perf_native/Cargo.toml --test hotpath -- --nocapture
```

- `hotpath_try_send` — one bounded queue handoff.
- `hotpath_send_and_observe` — one observed admission.
- `hotpath_call_blocking` — one host call, every worker turn until `Replied`.

## The worker-loop sleep (found and fixed)

The first comparison run showed tiny same-shard work in milliseconds while a
bounded Tokio design did the same job in microseconds:

| row | before p50 | after p50 | Tokio p50 |
| --- | --- | --- | --- |
| `host_enqueue` | 84 ns | 84 ns | 83 ns |
| `observed_admission` | 1.34 ms | 53 µs | 20 µs |
| `host_request_reply` | 5.79 ms | 210 µs | 20 µs |
| `service_request_reply_chain` | 12.07 ms | 400 µs | 52 µs |
| `http1_close_request` | 13.35 ms | 0.80 ms | 0.70 ms |
| `http1_keepalive_sequential` | 38.88 ms | 2.16 ms | 1.99 ms |

(Local release run, Apple Silicon. HTTP rows vary run-to-run; the host
send/call rows are stable. Saved evidence lives beside the phase plan.)

The probes named the cause exactly. `hotpath_try_send` was 42 ns before and
after — the first queue handoff was never the problem. `hotpath_call_blocking`
showed three ~1.4 ms gaps between worker turns while every actual handler ran
in well under a microsecond:

```text
before: host_submit -> mailbox        1.17 ms   <- worker asleep
        ...handlers...                < 1 µs
        send_accepted -> mailbox       1.40 ms   <- worker asleep
        ...handlers...                < 1 µs
        effect -> mailbox              1.42 ms   <- worker asleep
```

The shard worker slept a flat 1 ms after every step that made progress. A tiny
call needs several turns, so each call paid one sleep per turn. The fix: loop
immediately while a step delivers work, and park on the command queue (with the
bounded `idle_wait`) only when there is nothing to deliver. The three gaps
collapsed to ~45 µs of cross-thread scheduling. HTTP fell ~10–14× as a free
downstream effect — it never needed its own tuning, it was waiting on the same
worker.

## Findings

What felt good:
- The probes turned "Tina is slow here" into "the worker sleeps 1 ms per turn"
  in one run. A live `TraceObserver` timestamping each turn is enough; no
  production instrumentation.
- The handoff/observe/call split isolated the cost: handoff was always cheap,
  so the fix belonged in the worker loop, not the ingress path.

What is still bad (named, not hidden):
- `host_request_reply` is still ~10× Tokio at ~200 µs p50. The probe now
  measures **17 process-wide allocations per call** (host + worker thread),
  not the 4 it used to report — the host-only counter hid 13 worker-thread
  allocations from registering a fresh `HostCallDriver` isolate per call
  (mailbox + adapter box + handler box + isolate entry + in-flight-call entry
  + translator + call-context queue).
- Roughly half the latency is the three cross-thread wakeups required by the
  three runtime turns (`Begin` → target → `Returned`); the other half is the
  per-call driver registration. The Tokio comparison is partly architectural:
  Tokio's `current_thread` runtime is single-threaded so its `call_blocking`
  has zero cross-thread cost — `Tina`'s `ThreadedRuntime` is shard-per-thread
  by design and pays one wakeup per turn.
- The HTTP rows still count only load-worker allocations; server-thread
  allocation accounting needs a process/sample-level probe later.

Suggested follow-up (in order of leverage):
- **Persistent host-call dispatcher** (Rock 5): replace the per-call
  `HostCallDriver` registration with a pool of long-lived dispatcher isolates
  per worker, round-robin selected. *Attempted in this session and verified
  to deliver the reduction (17 → 11 process allocations per call, perf row
  back to ~205 µs) — but reverted because the dispatcher pool consumes
  `IsolateId`s at worker startup, breaking the live-capture → sim-replay
  determinism invariant the runtime relies on (the bugbox smoke test caught
  this).* Making Rock 5 DST-compatible needs either (a) a reserved
  system-isolate ID range so user-isolate IDs stay parity-equal between live
  and sim, (b) registering equivalent placeholder isolates in sim, or (c)
  making system isolates trace-invisible. None are simple; this is a real
  cross-cutting change, not a one-file fix.
- **Pre-allocated reply channel pool** (per-host-thread `thread_local`),
  type-erased via a slot table. Removes the `mpsc::channel` + sender-box
  allocations on the host side.
- Add repeated-run / historical tracking before any public performance claim.
- Add process-level allocation/RSS probes for HTTP and WebSocket rows.

Verdict:
- keep
