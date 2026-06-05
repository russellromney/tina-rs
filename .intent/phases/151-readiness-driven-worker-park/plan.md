# Phase 151: Readiness-Driven Worker Park (kill the I/O re-poll gap)

## Status

- Follows Phase 150. Phase 150 named this as the next bottleneck.
- The whole motivation is one measured fact (Phase 150, clean Linux A/B): the
  HTTP hot path spends ~1.1ms per request doing nothing.

## Grug Truth

HTTP is not slow. HTTP is asleep.

The worker parks on the command channel and only *polls* the I/O loop every
`idle_wait` (1ms). A connection sits in the kernel for ~1ms before the worker
bothers to look. `call_blocking` is fast (~14us) because a command wakes the
worker instantly; a ready socket does not.

This phase makes the worker sleep on the kernel, not on a timer.

## The Evidence (Phase 150)

Stage breakdown, Fly performance-2x, Linux/x86, commit 0612168:

- `hotpath_http1_close_request` p50 ~1.17ms, of which ONE stage
  `host_submit -> mbox_accepted` is ~1.09ms. The rest is ~150us of real work.
- `hotpath_call_blocking` same stage is ~12us.
- Controlled single-machine A/B (`TINA_PERF_IDLE_REPOLL_US`):
  idle_repoll 1ms -> http close p50 1.157ms; 100us -> 0.234ms. The
  `host_submit` gap tracks the park interval almost exactly. Saved in
  `../150-scheduler-turn-tail-performance/idle_repoll_ab_linux.txt`.

So lowering the park interval (Phase 150's Rock 2 knob) gets ~5x by polling
harder. This phase removes the poll entirely.

## Why It Polls At All (architecture)

The worker has two event sources on two incompatible OS primitives:

- host commands -> `std::sync::mpsc::sync_channel` (a futex; NOT a file
  descriptor). The worker blocks here via `recv_timeout`.
- socket / timer / lane I/O -> Betelgeuse `io_loop` = io_uring (Linux) /
  kqueue (macOS) = a file descriptor.

A thread can only block on one. It blocks on the mpsc and *polls* the io_loop
each wakeup. Confirmed in the vendored source: `IOLoop::step()` does a
zero-timeout drain (`kevent(..., timeout={0,0})` in `io/darwin.rs`; io_uring
`ring.submit()` without `submit_and_wait` in `io/linux.rs`). The backend CAN
block; it is told not to.

This is deliberate in upstream Betelgeuse ("no waker, no executor, no hidden
tasks" — a TigerBeetle-style step-loop). That model is right for an
always-busy workload; it creates this gap for a server that idles between
requests.

## Goal

The worker blocks in the kernel and wakes the instant *either* a socket is
ready *or* a host command arrives — zero timer re-polling, zero idle wakeups,
and HTTP `host_submit -> mbox` down to syscall latency (single-digit us), so
HTTP p50 -> ~150-200us. Same win as idle_repoll=100us, but with zero wasted
wakeups instead of more of them.

Done means:
- a fully idle worker makes ~0 wakeups/sec (blocks until the kernel has work);
- a pending TCP read is serviced at kernel-wakeup latency, not a poll interval;
- HTTP close/keepalive `host_submit` gap collapses to single-digit us on Linux;
- no host command is ever missed (the doorbell race is closed);
- the simulator / DST remain byte-for-byte deterministic (the live blocking
  wait does not exist in the simulated backend);
- `idle_wait` / `idle_repoll_interval` become vestigial (kept only as a safety
  ceiling, or removed).

## Non-Goals

- no busy-poll / core-burning spin (that is the opposite extreme, for kernel-
  bypass / HFT; not this);
- no async/await, no executor inside Tina;
- no zero-copy work (separate, complementary track — see Notes);
- no change to the bounded hot-drain, ready scheduler, or completion-drain
  rocks from Phase 150 — only the *park primitive* changes;
- no change to HTTP wire bytes or body-pressure accounting.

## Betelgeuse is a tina-maintained fork

`vendor-betelgeuse` is penberg/betelgeuse (Pekka Enberg, MIT/Apache, no
releases, upstream tip ~May 2, 2026), vendored AND heavily patched by tina:
Unix sockets on the substrate (#207), native buffer reuse (#217), F_FULLFSYNC,
completion-release hooks, parallel substrate. Our copy is at ~upstream-May-2026
parity on features (it has the recent `connect` op and the
`ConnectionStep::Keep->Continue` rename) but carries a real local patch set.
Neither upstream nor our fork has a blocking wait or waker — by design.

So this phase extends a fork we already maintain. Upstreaming the blocking
wait is optional and likely an uphill push (a `submit_and_wait`-with-timeout is
more palatable to upstream than a "waker"; the doorbell can stay local).

## Rock 0: Re-vendor Betelgeuse to latest, reconcile patches FIRST

Do NOT build the reactor change on a drifted fork. First:

- snapshot upstream penberg/betelgeuse @ latest main into a scratch tree;
- three-way diff: (a) tina-only patches vs upstream, (b) upstream changes we
  lack, (c) tina patches that upstream has since SUPERSEDED (e.g. if upstream
  added something our patch hand-rolled);
- re-vendor from upstream latest and re-apply only the tina patches still
  needed; drop superseded ones; record the upstream commit sha in a
  `VENDOR.md` marker so future staleness checks are trivial;
- prove the whole tina-rs workspace builds + the betelgeuse-touching tests
  (`betelgeuse_substrate`, DST, the lane tests) pass on the reconciled base.

Only then build Rocks 1-2 on the clean base.

## Rock 1: Betelgeuse blocking wait + waker (additive)

Add two capabilities to the IOLoop, leaving `step()` and the op/completion
machinery untouched:

- `fn step_blocking(&self, timeout: Option<Duration>) -> Result<bool>`: same
  drain as `step()`, but the backend wait actually sleeps up to `timeout`
  (None = until something happens):
  - macOS: pass the real `timespec` to `kevent` instead of `{0,0}`;
  - Linux: `submit_and_wait(1)` + an `IORING_OP_TIMEOUT` SQE for the deadline
    (or `submit_with_args` timeout on supporting kernels);
  - simulated backend: no-op (returns immediately) — keeps DST deterministic.
- `fn waker(&self) -> Waker` where `Waker: Send + Clone` with `wake()`,
  registered in the backend so a `wake()` pops the blocking wait:
  - Linux: an `eventfd`; arm a `POLL_ADD` on it in the ring; `wake()` writes 8
    bytes; re-arm after each wake;
  - macOS: an `EVFILT_USER` kevent; `wake()` = `kevent(NOTE_TRIGGER)`;
  - simulated: a flag.

Rules:
- no change to existing `step()` semantics (the non-blocking drain stays for
  the hot-drain inner loop);
- the waker is level-triggered so a `wake()` before the block still wakes it;
- exposed on `IOLoopHandle` (and the `IOLoop` trait) so tina-runtime can hold
  the waker and call `step_blocking`.

## Rock 2: tina-runtime — block in the io_loop, doorbell on send

- wrap the command `SyncSender` in `CommandSender { tx, waker }` whose `send`
  does `tx.send(cmd)?; waker.wake();`. Every host path (call_blocking's
  `HostCall`, send/observe, shutdown, cross-shard inbound) goes through it;
- replace the worker park (`receiver.recv_timeout(park)`, both the single- and
  multi-shard loops) with:
  - `let deadline = self.next_wake_deadline();  // min(next timer, next call deadline); None = forever`
  - `self.io_loop.step_blocking(deadline)?;`
  - then loop: drain commands (`try_recv` until Empty) + the existing bounded
    hot-drain (whose `step()` now finds the ready completions);
- add `Runtime::next_wake_deadline()` = earliest of
  `pending_isolate_call_deadlines.first()` and a new
  `driver.next_timer_deadline()`; None -> block indefinitely (true zero-wakeup
  idle);
- keep the Phase 150 bounds (hot-drain rounds/elapsed, ready scheduler,
  completion-drain budget) exactly as-is; only the park changes.

Rules / correctness (the doorbell race):
- `tx.send()` happens-before `waker.wake()`, and the worker always drains the
  mpsc AFTER waking, so a command can never be enqueued-but-missed (standard
  self-pipe argument; the level-triggered eventfd/EVFILT_USER guarantees a
  pre-block wake is observed);
- no host thread touches isolate state; the doorbell only wakes the worker;
- shutdown is a command -> doorbell -> wake -> drain -> shutdown.

## Proof

- idle CPU sanity: a worker with no work and no timers blocks forever; measure
  ~0 loop wakeups over a fixed window (the strongest version of Phase 150's
  Rock 8 idle proof);
- pending TCP read on an open socket whose peer sends nothing: worker blocks,
  zero wakeups, services the read at kernel-wakeup latency when bytes arrive;
- HTTP rows on Linux (Fly): `host_submit -> mbox` gap single-digit us; close /
  keepalive p50 ~150-200us; no wire-byte change; body high-water/current ->
  zero;
- no missed command: a stress test hammering commands while the worker blocks;
  all observed within a bound;
- DST fingerprint unchanged (sim never blocks);
- existing live tests (threaded_call_blocking, host_burst, multishard_fairness,
  scheduler_turn_tail, ready_scheduler, the full tina-http suite) all pass;
- soak: warmed keepalive under load shows lower CPU than the Phase 150
  idle_repoll=100us config at the same latency.

## Notes — complementary tracks (not this phase)

- ZERO-COPY (separate): once the wakeup gap is gone (~150us), per-byte copy
  CPU becomes the next thing under real throughput. Targets: the `read_scratch`
  kernel->isolate->parser round-trip, and the owned-bytes
  `tcp_write`/`tcp_write_close` response path. Orthogonal to this phase.
- LEVER 1 (shipped, Phase 150): `idle_repoll_interval`. This phase makes it
  vestigial.

## Done

- idle worker ~0 wakeups/sec; pending TCP read at kernel latency;
- HTTP `host_submit` gap single-digit us, HTTP p50 ~150-200us on Linux;
- no missed command, no fairness regression, no wire-byte / pressure change;
- DST deterministic (sim unchanged);
- `vendor-betelgeuse` re-vendored to a recorded upstream sha with a minimized,
  reconciled tina patch set, plus the additive `step_blocking` + `waker`.
