# Phase 151 Hostile Review

## Review 1

Findings:

- [P2] Rock 0 was too broad. "Re-vendor Betelgeuse to latest first" could turn
  the performance phase into a large dependency rewrite before the actual
  wakeup fix. The plan now requires a provenance marker and keeps the current
  fork unless a named upstream change is needed. If a re-vendor is needed, it
  must be a separate first commit with workspace proof.
- [P2] The command-doorbell rule was underspecified for bounded ingress.
  `SyncSender::send` can block; if used blindly it can deadlock or erase the
  existing typed `Full` outcome. The plan now requires a `try_send` doorbell
  path, preserves retry-on-Full shutdown, and forbids new unbounded blocking
  sends in hot paths.
- [P2] The waker could accidentally be modeled as a clone of the `Rc<dyn
  IOLoop>`. That would be unsound for host threads and would let them touch
  backend state. The plan now requires a separate `Send + Sync` OS-handle
  waker; it may wake the worker but cannot mutate backend state.
- [P2] Linux blocking wait was too hand-wavy. `submit_and_wait(1)` plus timeout
  can leak timeout CQEs or confuse timeout completions with real completions if
  user data is not reserved. The plan now requires reserved doorbell/timeout
  user data, no completion-pointer casts for those events, and no blocking wait
  with no possible wake source.
- [P2] Simulated backend behavior was wrong. A "no-op returns immediately"
  `step_blocking(None)` would make threaded runtimes over simulated I/O spin
  after Rock 2 replaces `recv_timeout`. The plan now requires simulated
  threaded proof: no spin, wakes for host commands, while `tina-sim` remains
  deterministic because it does not use the live threaded park.
- [P2] The plan contradicted itself: Non-Goals said no ready-scheduler change,
  then Rock 3 changed the ready scheduler. The plan now says bounded hot-drain
  and completion-drain semantics stay fixed, while ready scheduling may change
  only through mailbox-owned readiness proof.
- [P2] Rock 3 risked relanding the Phase 150 bug. `is_empty()` is correct for
  direct mailbox pushes but is only skip-empty scan, not a true O(ready) queue.
  The plan now makes that distinction explicit: ship skip-empty first, build a
  true ready queue only if mailbox-owned empty -> non-empty notification exists
  and benchmark evidence says the remaining scan matters.
- [P3] The Done block still required a broad re-vendor after Rock 0 stopped
  requiring it. The plan now requires `vendor-betelgeuse/VENDOR.md` plus the
  additive blocking wait/waker, with re-vendor only if separately justified.

Decision:

- Plan is stronger now. The core goal stays the same: block the worker on the
  kernel I/O loop plus a doorbell, kill the timer re-poll gap, and preserve
  Tina's bounded command truth. The important implementation constraints are
  now pinned: no blocking-send footgun, no fake sim spin, no `Rc` waker, no
  timeout CQE confusion, no enqueue-side ready scheduler.

## Implementation finding: cross-lane harvest theft (the remaining missed-wakeup race)

The rare missed-wakeup race (`grpc_streaming_peer_reset_cancels_response_source`,
~1/80 alone, ~1/34 in the single-threaded suite) was not in the cancel/reset
path. It was a latent harvest-ordering bug that the readiness park exposed.

Root cause:

- All socket/file lanes (tcp, unix, tls, storage) share ONE Betelgeuse io_loop
  (cloned handles). `driver.advance` runs each lane in turn; each lane does
  `io_loop.step()` (drain queued ops, then poll for readiness) and then harvests
  its own completions.
- `poll` only *surfaces* a ready event into the loop's queue — it does not
  execute it. The `drain` that executes it (writing the typed result) can run
  inside a *later* lane's `io_loop.step()`. That later lane harvests only its own
  ops, so a completion surfaced by an earlier lane and executed by a later one is
  left completed-but-unharvested for that turn.
- Before this phase the worker re-polled every ~1ms, so the next tick's
  `try_complete` reaped the stranded result. The readiness park blocks until an
  io_loop event, and TCP/Unix are the only lanes excluded from
  `has_unsignaled_pending` (they ride the zero-wakeup park), so nothing re-polls
  to collect their result. The worker blocks forever on an unrelated armed op
  (e.g. the listener accept) while the HTTP request sits read-but-undelivered.

How it was found:

- A low-overhead stall monitor (epoch counter + ring of block-forever park
  snapshots, printed only when the epoch froze) caught the exact stall state:
  `loop_armed=1 active=["accept:submitted", "readbuf:completed:res=true"]` — a
  recv that had completed but was never harvested, with the worker parked
  forever on the accept watch.

Fix:

- Split a harvest-only pass (no substrate step) out of the TCP and Unix lanes
  and run it once at the end of `driver.advance`, after every lane has driven the
  shared loop. It reaps any completion a sibling lane executed; it touches no
  syscall. Anything still only *queued* (surfaced, not executed) keeps the loop
  armed, so the park's own `step_blocking` drain executes it and the next step
  reaps it. TLS/storage need no such pass — they are in `has_unsignaled_pending`,
  so a pending op there already forces a capped re-poll.

Second bug, same class (stale idle metrics):

- The worker skips the O(pending) resource report on hot-delivery turns. A turn
  that followed a burst and then found nothing to do would park on a stale
  count; the old timer park refreshed within ~1ms, the readiness park never did.
  `local_system_reports_live_owned_resources_and_shutdown_cleanup` and
  `local_system_tls_failed_handshake_closes_listener_and_leaks_no_stream` failed
  100% with the readiness park (pass on the pre-park baseline). Fixed by
  publishing a fresh resource count once before parking (a park turn is never a
  hot turn, so the hot-path savings stand).

Test evidence:

- `grpc_streaming_peer_reset_cancels_response_source`: 400/400 (was ~1/80
  failing); full `grpc_live` suite single-threaded 80/80 (was ~1/34 failing).
- `cargo test -p tina-runtime`: all pass (was 2 failing: the two local_system
  tests above, 0/20 and similar before the metrics fix, 20/20 after).
- `cargo test -p tina-http`: all 42 test binaries pass.
- DST determinism: `dst_simulator` / `dst_parser` / `dst_keepalive` pass; live
  replay regression passes (the simulator never uses the live blocking park).

Residual:

- A deterministic *unit* regression test for the harvest race is not feasible:
  at the driver level a second `advance` always reaps the stolen completion, so
  the failure only manifests at the worker block-forever park, which is
  timing-dependent. The integration test above is the regression guard.

## Perf evidence (Linux/x86, Fly performance-2x, region iad, release)

Captured from the source at commit f6abb9b on a dedicated-CPU cloud machine.
Local/alpha evidence on one box, not a production claim. Full rows in
`perf_sample_linux.txt`.

What changed is the worker stopped sleeping. Before, the worker parked on a
timer and only *noticed* a ready socket on its next poll, so the HTTP host
path spent ~1.1ms per request doing nothing. The readiness park removes that:
the worker blocks on the kernel and wakes the instant a socket is ready or a
command arrives. This is the removal of a polling/wakeup gap, not an
optimization of real work.

The gap, before and after (`host_submit -> mbox`/`call_completed` stage):

  path                  Phase 150        Phase 151
  http close            ~1.105 ms        ~0.125 ms
  http keepalive        ~1.108 ms        ~0.030-0.047 ms

End-to-end p50 (same probe): http close ~1.16ms -> ~0.15ms; keepalive
~1.17ms -> ~0.15ms. The number that vanished was idle sleep; the ~0.15ms that
remains is real work the sleep had been hiding.

Where the remaining ~0.15ms goes (close, by stage): the single large stage is
`host_submit -> mbox_accepted` ~0.125ms, which is connection setup —
connect + accept + first read, i.e. 2-3 real kernel readiness round-trips on
loopback (~40-50us each on this VM), not a poll gap. Handlers themselves run in
under 3us. The warm keepalive path (one read round-trip on an established
connection) is ~0.030ms host-submit-to-completed, and a single in-process hop
(`call_blocking`, no sockets) is ~12-20us — that is the floor for one hop, and
HTTP simply has several.

Honest non-result: the stretch goal "host-submit gap in single-digit
microseconds" was NOT met for HTTP and should not have been — HTTP is
inherently multi-round-trip. Single-digit microseconds applies to one
in-process hop only. The remaining connection-setup round-trips and per-byte
copy are the next bottleneck (zero-copy + accept path), now that idle sleep no
longer dominates.

CPU: the warm keepalive path reaches ~0.15ms p50 while a fully idle worker
makes zero park wakeups (it blocks on the kernel). Phase 150 could approach
this latency only by lowering `idle_repoll` to ~100us, which re-polls
continuously while I/O is pending. So this path is both at-or-below that
latency and free of the extra wakeups, which is the CPU win the soak/idle
proof backs up.

The deprecated `idle_repoll_interval` / `idle_wait` knobs no longer drive the
single-shard idle park (they remain accepted config; the park blocks on real
readiness instead). Multi-shard still uses the command-queue park.

## Mailbox-owned readiness (skip-empty scan)

The per-step scheduler scan used to call `recv` on every isolate. It now probes
`Mailbox::is_empty()` first and skips the `recv` (a virtual call + lock +
context pop) on quiet isolates. `is_empty()` is a required trait method — no
default — so every mailbox impl answers truthfully; a wrong `true` would
silently drop scheduling. The method threads through `ErasedMailbox` and both
adapters, the default factory mailboxes, the SPSC mailbox, and all in-tree test
mailboxes (compile errors were the rail).

Why this is correct where the prior attempt was not: the earlier ready-queue
marked readiness only in the runtime's enqueue path, which the explicit
runtime's direct `mailbox.try_send` seam bypassed, so directly-seeded messages
were never scheduled. `is_empty()` queries the mailbox's real state, so it is
correct for every ingress path — mediated sends and direct `try_send` alike.
`address_liveness` (held-handle direct push) passes, and `mailbox_readiness`
adds an at-scale guard (a hot isolate served promptly among 2000 idle ones, and
a message to one of 500 idle isolates still scheduled).

Behaviour-preserving: an empty mailbox yields `None` from the scan either way
(skip vs `recv -> None`), so the delivered set and per-entry order are
identical. DST is byte-identical — `sim_same_seed_replays_to_same_trace_fingerprint`
and the saved replay cases pass unchanged.

No true O(ready) ready queue was built. Skip-empty removes the only expensive
per-quiet-isolate cost (the `recv`); what remains is an O(entries) pass of cheap
`is_empty()` probes plus the existing entry-indexed dispatch loop (which the
supervision/restart paths require). Building a mailbox-owned ready queue would
need an empty->non-empty notification that is also correct for the direct-push
seam, and the warmed hot paths have ~3 isolates, so there is no measured scan
cost to justify it. Recorded as skip-empty-is-enough, per plan.

## Hostile self-review

Attacked my own work for missed wakeups, hidden blocking, fake boundedness, DST
drift, platform lies, and happy-path-only tests.

- Missed wakeups. Three real races were found and fixed, each with a guard:
  (1) macOS `drain_queued` completes loopback ops inline — the park must not
  block when it made progress; (2) cross-lane harvest theft on the shared loop —
  a completion surfaced by one lane and executed by another was left
  unharvested, fixed by a harvest-only TCP/Unix pass at the end of `advance`;
  (3) park-gating — the worker could block forever on pending work the loop
  could not observe, fixed by `park_needs_repoll` forcing a re-poll when there
  is pending work but no armed loop op and no deadline. Guards:
  `command_admitted_around_park_is_never_missed`, the pending-read proof,
  `grpc_streaming_peer_reset_cancels_response_source` (was ~1/80, now 400/400),
  full `grpc_live` single-threaded.

- Hidden blocking. The only blocking send is the control-plane `call()`
  (register/supervise/observe), never the per-request hot path. It cannot
  deadlock against a parked worker: the worker parks only with an empty command
  queue, and every admitted command rings the coalescing doorbell, so the worker
  always wakes and drains enough to free a slot. `try_send` / `call_blocking`
  stay non-blocking and return typed `Full`/`Disconnected`.

- Fake boundedness. The command queue is the same bounded `sync_channel`; the
  doorbell adds no queue. `park_needs_repoll`'s catch-all caps at
  `idle_repoll_interval` and is self-clearing (the next `step` harvests the
  pending work), so it is a bounded re-poll, not a busy spin.

- DST drift. The blocking park and doorbell are live-only; the simulator never
  calls `step_blocking`. Skip-empty is byte-identical. Fingerprint + saved
  replay cases pass unchanged.

- Platform honesty. The Linux io_uring `step_blocking`/eventfd path cannot run
  in the local macOS suite, so the key park/wakeup tests were built into the Fly
  image and run on real Linux/x86. That caught a Linux-only busy-spin
  (`MSG_DONTWAIT`); after the fix, all seven validation binaries pass on Linux
  (see "Linux busy-spin found and fixed"). The broad DST/test matrix on Linux is
  still CI's job, but the readiness-park claims are now proven on real io_uring,
  not inferred from macOS.

- Happy-path. The proofs cover idle (~0 wakeups), pending read (~0 wakeups then
  prompt), command stress, pre-wake race, simulated no-spin, skip-empty at
  scale, cancel/reset, and DST. The one residual is the harvest race: it has no
  deterministic unit test (a second `advance` always reaps it at the driver
  level; it only manifests at the worker block-forever park), so the integration
  tests are its guard.

- Scope. Multi-shard keeps its command-queue park (its cross-shard inbound
  arrives off the loop); the readiness park is single-shard this phase. The
  `idle_repoll_interval` / `idle_wait` knobs are vestigial for the single-shard
  park (kept as accepted config).

## Linux busy-spin found and fixed (validated on real io_uring)

Running the park/wakeup tests on a Fly Linux/x86 machine (kernel 6.12) — not
only the perf binary — caught a platform-specific defect the macOS suite could
not: with a pending TCP read and a silent peer, the worker made ~58,672 park
"wakeups" in 250ms. It was busy-spinning, not blocking.

Root cause (pinned with in-image io_uring instrumentation): the Betelgeuse Linux
backend submitted `recv`/`recv_buf`/`send`/`send_owned` with `MSG_DONTWAIT`. With
that flag io_uring honours the explicit non-blocking request and returns
`-EAGAIN` instead of arming fast-poll; `should_retry` then re-queued the op, the
worker re-submitted, and `submit_and_wait` returned instantly every time
(measured `waited_us=1` against a 9.9s timeout, op discriminant = `RecvBuf`,
result `-11`). `accept` has no such flag, so it fast-polled and blocked
correctly — which is why HTTP latency still looked fine (active requests have
data ready and complete inline) while an idle/pending connection burned a core.
This is the upstream always-busy-poll design; it defeats a blocking park.

Fix: drop `MSG_DONTWAIT` from the socket read/write ops (keep `MSG_NOSIGNAL` on
sends). io_uring then fast-polls them — a request with data ready still
completes inline, and an idle/pending read parks on the kernel until readable.
Linux-only change (`vendor-betelgeuse/io/linux.rs`); macOS/darwin uses kqueue
watches and was already correct.

Validated on real Linux/x86 io_uring after the fix (all pass on the same
machine that produced the perf rows): `pending_read_park` 1 (was the 58,672
busy-spin), `readiness_park` 4, `mailbox_readiness` 2, `scheduler_turn_tail` 8,
`betelgeuse_substrate` 19, `client_against_native` 1, `grpc_live` 34. So the
zero-idle-wakeup / block-on-the-kernel claim now holds on Linux, not only macOS.

Tooling note: `cargo check --target x86_64-unknown-linux-gnu -p betelgeuse`
type-checks the Linux io_uring path locally on macOS (betelgeuse has no C-build
deps), so Linux-only compile errors no longer need a Fly round-trip.

## Orchestrator adversarial fixes

Findings fixed after a second Codex hostile review:

- Direct mailbox ingress wake. Runtime-mediated sends rang the command
  doorbell, but a direct mailbox producer (custom factory / held mailbox handle)
  could push a message after the single-shard worker parked forever. The fix is
  mailbox-owned wake truth: `Mailbox::set_wake_hook(...)` installs an
  empty -> non-empty wake hook; the runtime installs it on registered mailboxes;
  the blessed threaded mailbox and SPSC mailbox call it after publishing the
  message. Regression:
  `readiness_park::direct_mailbox_push_wakes_idle_worker`.
- Linux SQ-full doorbell. `IoUringIO::arm_doorbell` used to silently fail if the
  submission queue was full after user ops were queued, which could enter
  `submit_and_wait` without an armed eventfd poll. The fix submits to make room,
  retries, and returns an error instead of blocking with no command wake source.
  `cargo check --target x86_64-unknown-linux-gnu -p betelgeuse` covers the Linux
  shape locally; Fly io_uring validation remains the real runtime proof.
- Park errors. A backend park error no longer becomes a quiet sleep/retry loop.
  The worker now terminates with `ThreadedRuntimeError::DriverParkFailed` and
  publishes failed terminal truth.
- Scope honesty. The plan now says the readiness park is single-shard in this
  phase. Multi-shard still uses its command-queue park because cross-shard
  inbound queues are not io_loop wake sources yet.
- Perf README stale text. Phase 150's wakeup-gap diagnosis and Phase 151's
  shipped readiness park are separated so future agents do not copy "queued
  next" after the fix has landed.
