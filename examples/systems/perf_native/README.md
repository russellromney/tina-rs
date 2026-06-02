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
- `host_request_reply` is still ~10× Tokio at roughly 200 µs p50. The per-call
  driver registration is gone, but the truthful Tina path still crosses from
  host thread -> shard worker -> target isolate -> dispatcher -> host.
- The Tokio comparison is partly architectural: Tokio's `current_thread`
  runtime has zero cross-thread host-call cost. Tina's `ThreadedRuntime` is
  shard-per-thread by design and pays worker-turn wakeups.
- The HTTP rows now have whole-process allocation rows. Tina HTTP still
  allocates roughly 1.45-1.8x the Axum comparison row on the same request.

**Rock 5 landed**: the per-call `HostCallDriver` registration is gone,
replaced by a pool of `HOST_CALL_DISPATCHER_POOL_SIZE = 8` long-lived
dispatcher isolates per worker, round-robin selected via a wrapping atomic
counter. Each `call_blocking` now pushes a `Box<dyn HostCallTaskBegin<S>>`
onto an already-registered dispatcher's mailbox instead of registering a
fresh isolate (mailbox + adapter + handler box + isolate entry + call-context
queue) per call.

DST replay parity was preserved by giving the simulator a
`reserved_system_isolates` knob so user-isolate ids stay equal in both live
and sim. The bugbox replay runner sets it to
`tina_runtime::HOST_CALL_DISPATCHER_POOL_SIZE` and the captured trace
replays bit-exact again. The bugbox's `SAVED_TRACE_HASH` was rebaked once
to reflect the shifted-but-now-deterministic ids.

Measured before/after the host-call dispatcher pool and reply channel pool:

| metric                              | before  | after   |
|-------------------------------------|---------|---------|
| `call_blocking` process allocs/call | 17      | **7**   |
| `call_blocking` host allocs/call    | 5       | **2**   |
| probe p50 (single host thread)      | ~190 µs | ~198 µs |
| perf row p50 (4 concurrent threads) | ~210 µs | ~205 µs |

## Phase 146 owned-buffer pass

The next pass added Tina-shaped owned-buffer calls:

- `tcp_read_buf` / `tls_read_buf`;
- `tcp_write_owned` / `tls_write_owned`.

Buffers move through effects and come back on success. Native HTTP/1 server,
one-shot client, keepalive client, and server WebSocket paths use that shape
now. This removes the obvious "read fresh Vec, copy into HTTP buffer, drop it"
and "clone pending write bytes before every write" waste.

Current local truth is still not a production claim:

- HTTP/1 fixed-body close is closer to Axum in good runs.
- HTTP/1 close and keepalive are still slower/noisier than Axum.
- Whole-process allocation rows still show Tina HTTP around 1.4-1.8x Axum on
  these rows.
- Standalone WebSocket client and HTTP/2 still use the compatibility helpers.
- Owned buffers return on success; the current `TypedCall<T>` error envelope
  does not return the buffer on driver failure.

Suggested next follow-ups:
- Smaller HTTP request/response allocation shapes (`SmallVec` headers,
  response body reuse) before making any public HTTP performance claim.
- Reduce HTTP worker-turn/scheduling cost now that the obvious buffer waste is
  gone.
- Add more repeated-run history before any public performance claim.
- Verify Linux/x86_64 perf behavior; these rows were measured on macOS aarch64.

Verdict:
- keep
