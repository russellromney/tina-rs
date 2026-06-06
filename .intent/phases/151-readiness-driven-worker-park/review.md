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
