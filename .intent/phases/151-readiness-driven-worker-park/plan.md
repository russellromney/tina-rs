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
- no change to bounded hot-drain or completion-drain semantics from Phase 150;
- ready scheduling may change only through Rock 3's mailbox-owned readiness
  proof; no enqueue-side ready mark is allowed;
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

## Rock 0: Betelgeuse provenance marker, no broad re-vendor

Do NOT turn this performance fix into a giant vendored-dependency rewrite.
There is no `vendor-betelgeuse/VENDOR.md` marker today. Add one, with:

- upstream repo URL and commit if known;
- if unknown, say "unknown historical vendor point" honestly;
- short list of tina-local patch families already present (Unix sockets,
  native buffer reuse, completion-release hooks, parallel substrate,
  F_FULLFSYNC, address introspection).

Do not re-vendor to latest unless the implementer can name a specific upstream
change needed by this phase. If a re-vendor is required, split it into its own
first commit and prove the workspace before adding the blocking wait. Normal
path: keep the current fork, add the marker, then implement Rocks 1-3.

## Rock 1: Betelgeuse blocking wait + waker (additive)

Add two capabilities to the IOLoop, leaving `step()` and the op/completion
machinery untouched:

- `fn step_blocking(&self, timeout: Option<Duration>) -> Result<bool>`: same
  drain as `step()`, but after submitting queued work the backend may sleep up
  to `timeout` for a completion or doorbell event. `None` means block until a
  real event, but only after the backend has a doorbell/watch or in-flight work
  armed; never call a kernel wait with no possible wake source.
- `fn waker(&self) -> IOWaker` where `IOWaker: Send + Sync + Clone` with
  `wake()`. It must be a separate OS handle, not a clone of the `Rc<dyn
  IOLoop>`; host threads may use it, but must not touch backend state.
- Linux:
  - create one `eventfd` owned by the backend;
  - keep a persistent poll/read interest for that eventfd in the ring, with
    user_data reserved for "doorbell", never a completion pointer;
  - `wake()` writes 8 bytes and coalesces `EAGAIN` as already-awake;
  - `step_blocking(Some(t))` uses a real timeout path that does not leak timeout
    CQEs or confuse timeout completions with operation completions;
  - `step_blocking(None)` is legal only when eventfd/in-flight work can wake it.
- macOS:
  - register an `EVFILT_USER` doorbell with reserved `udata`;
  - `wake()` = `kevent(NOTE_TRIGGER)`;
  - blocking wait passes a real optional `timespec` to `kevent`;
  - event `udata` is classified before casting to `CompletionInner`.
- simulated Betelgeuse backend used by threaded tests:
  - it must not spin if the worker calls `step_blocking(None)`;
  - use a small condvar/doorbell or a bounded timeout path so a threaded runtime
    over simulated I/O still sleeps and wakes on command;
  - `tina-sim` deterministic replay remains unchanged because it does not use
    the live threaded park.

Rules:
- no change to existing `step()` semantics (the non-blocking drain stays for
  the hot-drain inner loop);
- the waker is level-triggered/coalescing so a `wake()` before the block still
  wakes it;
- a wake event is observable as "work happened" but is not a runtime trace
  event and must not perturb replay hashes;
- exposed on `IOLoopHandle` (and the `IOLoop` trait) so tina-runtime can hold
  the waker and call `step_blocking`.

## Rock 2: tina-runtime — block in the io_loop, doorbell on send

- wrap the command `SyncSender` in `CommandSender { tx, waker }` with:
  - `try_send(cmd)`: preserves today's bounded `Full` / `Disconnected` outcomes
    and wakes only after successful admission;
  - no new unbounded blocking send in hot paths. Existing blocking `call(...)`
    helper either uses `try_send + CommandFull` or proves it cannot deadlock
    with the worker asleep;
  - shutdown uses `try_send` and wakes after admission, preserving retry-on-Full;
  - every host path (`try_send`, send/observe, `call_blocking`, observation,
    shutdown, cross-shard inbound) goes through the doorbell sender;
- replace the single-shard worker park (`receiver.recv_timeout(park)`) with:
  - `let deadline = self.next_wake_deadline();  // min(next timer, next call deadline); None = forever`
  - `self.io_loop.step_blocking(deadline)?;`
  - then loop: drain commands (`try_recv` until Empty) + the existing bounded
    hot-drain (whose `step()` now finds the ready completions);
- multi-shard is out of this phase's implementation scope. Its cross-shard
  inbound queues are not io_loop wake sources yet, so it keeps the command-queue
  park and remains a named follow-up rather than a hidden claim;
- add `Runtime::next_wake_deadline()` = earliest of
  `pending_isolate_call_deadlines.first()` and a new
  `driver.next_timer_deadline()`; None -> block indefinitely (true zero-wakeup
  idle);
- keep the Phase 150 bounds (hot-drain rounds/elapsed, completion-drain budget)
  exactly as-is; only the park changes.

Rules / correctness (the doorbell race):
- successful `try_send` happens-before `waker.wake()`, and the worker always
  drains the mpsc AFTER waking, so a command can never be
  enqueued-but-missed. The level-triggered/coalescing doorbell guarantees a
  pre-block wake is observed;
- a `Full` command queue must not rely on a wake to make progress: either an
  earlier admitted command already rang the doorbell, or the path returns
  typed `Full` immediately;
- no host thread touches isolate state; the doorbell only wakes the worker;
- shutdown is a command -> doorbell -> wake -> drain -> shutdown.

## Rock 3: Ready-isolate scheduler, done correctly (was Phase 150 Rock 4)

Phase 150 prototyped a ready-isolate scheduler (replace the per-step
`recv_boxed`-every-entry scan with an O(ready) snapshot) and REVERTED it: it
marked readiness only in `enqueue_entry_message`, but the explicit `Runtime`'s
direct `mailbox.try_send` seam bypasses that, so directly-seeded messages were
never scheduled (caught by `tina-runtime/tests/address_liveness.rs`). The scan
is correct precisely because `recv` reflects the real mailbox state regardless
of how a message arrived; a separate enqueue-side mark cannot.

The correct version needs the MAILBOX to be the readiness authority, which is a
core `tina::Mailbox<T>` trait change — and this phase is already in the mailbox
neighbourhood (the doorbell, the park), so it belongs here, not bolted onto a
near-done PR.

Plan:
- extend `Mailbox<T>` (and `ErasedMailbox`) with a cheap readiness primitive —
  prefer `is_empty()` first, because it is correct for EVERY ingress path
  (mediated runtime sends and direct `mailbox.try_send` handles). It skips
  expensive empty `recv_boxed` calls but still scans entries;
- only build a true FIFO ready queue if the mailbox can notify the runtime on
  empty -> non-empty transition for direct pushes too. Do not reintroduce an
  enqueue-side ready mark owned only by the runtime;
- thread the new method through every mailbox impl (default factory mailboxes,
  the threaded mailboxes, test mailboxes, any user-facing impls);
- rebuild the snapshot as "skip-empty scan" first; if benchmark proves the
  remaining entry scan is the cost, add the mailbox-owned ready queue in the
  same PR with direct-push proof. Restore the no-starvation / round-robin
  proofs and ADD a direct-`mailbox.try_send` test so the seam that broke Rock 4
  is covered;
- prove DST byte-identical (behaviour-preserving) AND run the full
  `cargo test --workspace`, not a subset — the Phase 150 miss was trusting a
  hand-picked test set;
- BENCHMARK before claiming a win: a many-quiet-isolates workload (e.g. 1 hot +
  N idle isolates, sweep N) showing the scan cost the optimization removes.
  Phase 150 reverted partly because this win was never measured; do not reland
  it without that row.

Rules:
- correct for all ingress (mediated + direct mailbox push), proven by test;
- public `Mailbox<T>` gains readiness hooks:
  - required `is_empty()` (no default) so the scheduler can skip quiet mailboxes
    without missing direct pushes;
  - optional `set_wake_hook(...)` for threaded/external-producer mailboxes. The
    blessed threaded and SPSC mailboxes must wake on empty -> non-empty. Custom
    mailboxes that expose direct producer handles should do the same;
- do not provide a default `is_empty() == false` that silently destroys the
  optimization or hides missing impls; update all in-tree mailbox impls and use
  compile errors as the rail;
- only reland a true ready queue if the benchmark justifies it; otherwise keep
  skip-empty scan and record that the remaining scan was fast enough.

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
- command-full proof: when the command queue is full, `try_send`/shutdown still
  return typed `Full` and do not block waiting for a doorbell;
- pre-wake race proof: enqueue a command immediately before the worker enters
  `step_blocking`; it must be observed without waiting for the timeout;
- direct mailbox proof: a message pushed into a runtime-owned mailbox without
  `ThreadedRuntime::try_send` wakes an idle worker and is delivered;
- simulated threaded proof: a `ThreadedRuntime` built with Betelgeuse simulated
  I/O does not spin and still wakes for host commands;
- DST fingerprint unchanged (sim never blocks);
- existing live tests (threaded_call_blocking, host_burst, multishard_fairness,
  scheduler_turn_tail, scheduler_fairness, address_liveness, the full
  tina-http suite) all pass;
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
- `vendor-betelgeuse/VENDOR.md` records provenance/patch families, and the
  additive `step_blocking` + `waker` path is shipped without a broad re-vendor
  unless a separate evidence-backed re-vendor commit was required.
