# 030 Review

## Plan Review 1

Verdict: structurally on-shape and ready to hand to implementation after the
initial capability audit records exact test gaps.

What looks strong:

- The phase is framed as core runtime completion, not a demo/proof theater
  phase.
- It correctly starts from the real baseline: Tina already has a Betelgeuse
  live runtime, simulated I/O, `RuntimeDriver`, `tina-sim`, Ranger cancellation
  semantics, and Surveyor completion-slot release semantics.
- The expected direction is concrete: one composed local server workload with
  listener, connection, worker pool, supervisor, bounded overload, runtime-owned
  TCP/time/calls, and explicit shutdown.
- It refuses the right temptations: remoting, clustering, persistence,
  Tower/Axum, async handlers, a new general-purpose runtime, and demo logs as
  evidence.
- The proof bar is user-shaped: live Betelgeuse, Betelgeuse simulated I/O, and
  `tina-sim` where modeled, with direct assertions around overload, restart,
  timeout, shutdown, and replay.

Implementation cautions:

1. **Do not let the composed workload become a mini framework.**

   The phase needs production-shaped code, but not a reusable server framework
   yet. If helper APIs are required, they should be tiny and obviously part of
   the existing preferred Tina surface. Broader ergonomics belong to Joop den
   Uyl.

2. **Keep live-native and simulated-I/O claims separate.**

   Native Linux/macOS CI is good for real backend ownership and live TCP smoke.
   Simulated I/O is where slow peers, partial writes, delayed completions, and
   exact shutdown interleavings can be made deterministic. The implementation
   should not pretend native CI can prove every interleaving.

3. **Make backpressure direct, not inferred.**

   The important Tina story is visible overload. Tests should force
   `IngressFull`, mailbox `Full`, call timeout, and requester-closed paths
   without sleep-as-proof.

4. **Make shutdown the hard center of the phase.**

   If the server-shaped workload passes but pending accept/read/write/timer/call
   shutdown is not directly asserted, the phase is not done. Surveyor removed
   the leak wart; Willem Drees should prove users can rely on the result.

5. **Do not overclaim performance.**

   Allocation/backpressure probes should catch accidental unbounded buffering or
   obvious new hot-path cost. Full cost-model work belongs to Ruud Lubbers.

Recommended first implementation step:

- Audit the current tests listed in the plan and append an "Implementation Audit
  1" section here with exact existing coverage and exact missing proof. Then
  build the composed workload against the largest missing gap first, likely
  shutdown/backpressure under live threaded runtime plus simulated slow peer
  behavior.

## Plan Review 2

Verdict: ready to implement, with one hard instruction for the first build
step: do the capability audit before writing the composed workload.

Why this is the right next phase:

- 029 is closed enough to build on: local `make verify` passed, Linux/macOS CI
  passed, the shutdown leak fallback is gone, and Surveyor's typed lifecycle
  failure path is tested.
- The roadmap now says the next missing core claim is not docs, not remoting,
  not Akka parity, and not a Tokio bridge. It is the local production runtime
  story.
- 030 targets exactly that: one-process listener/connection/worker/supervisor
  behavior under pressure, with live Betelgeuse, Betelgeuse simulated I/O, and
  `tina-sim` oracle proof where each belongs.

Plan strengths:

- It refuses demo work and release-story polish.
- It keeps remoting, persistence, clustering, Tower/Axum, and async handlers out
  of scope.
- It requires one composed server-shaped workload, which should catch
  integration bugs the current narrow proofs can miss.
- It puts shutdown and backpressure at the center instead of treating them as
  edge cases.
- It requires direct assertions rather than logs or "works on my machine"
  behavior.

Implementation cautions:

1. **Audit first.**

   The repo already has a lot of TCP, shutdown, restart, replay, and overload
   proof. The first implementation step must classify what exists and name the
   missing direct proof. Otherwise 030 can accidentally duplicate old rocks.

2. **One workload, multiple substrates.**

   Prefer one canonical local-server workload shape, then run equivalent slices
   through live Betelgeuse, Betelgeuse simulated I/O, and `tina-sim`. Avoid
   three unrelated test stories that merely share vocabulary.

3. **Keep native and simulated claims honest.**

   Native CI should prove real backend ownership and ordinary TCP smoke.
   Simulated I/O should prove slow peers, partial writes, delayed completions,
   and exact shutdown interleavings. Do not pretend one proves the other.

4. **Do not invent a server framework.**

   If 030 needs helpers, they must be tiny and obviously part of the existing
   Tina shape. Bigger ergonomics belong to Joop den Uyl.

5. **Backpressure must be forced.**

   Tests should directly hit ingress full, worker/mailbox full, timeout, stale
   address, and requester-closed paths without sleeps-as-proof.

Decision:

- Start 030 implementation with an "Implementation Audit 1" section in this
  review file.
- Then build the first composed workload around the largest missing proof,
  likely live/simulated shutdown plus bounded worker pressure.

## Implementation Audit 1

Verdict: the current repo has strong narrow runtime proof, but not yet the
single composed local production workload Willem Drees needs.

### Existing Direct Proof

Live Betelgeuse runtime:

- `tina-runtime/tests/tcp_echo.rs`
  - `betelgeuse_runtime_tcp_echo_round_trips_reference_workload` proves native
    live TCP echo through the threaded runtime and compares semantic counts with
    the explicit-step oracle.
  - `betelgeuse_runtime_can_run_over_simulated_io_backend` proves the threaded
    runtime can run over Betelgeuse simulated I/O.
  - `betelgeuse_runtime_shutdown_rejects_outstanding_tcp_accept_completion`
    proves shutdown rejects a pending accept completion.
  - `betelgeuse_runtime_try_send_surfaces_ingress_full_without_blocking_on_worker`
    proves bounded command ingress full without sleep-as-proof.
  - `betelgeuse_runtime_surfaces_closed_mailbox_after_stop` proves stopped
    mailbox rejection through the live handle.
- `tina-runtime/tests/betelgeuse_substrate.rs`
  - `betelgeuse_runtime_timer_retry_runs_without_manual_stepping` proves live
    runtime-owned timers.
  - `betelgeuse_runtime_shutdown_rejects_outstanding_timer_completion` proves
    live timer shutdown rejection.
  - `betelgeuse_runtime_shutdown_reports_driver_release_failure` proves the
    Surveyor typed lifecycle failure path.
  - `betelgeuse_runtime_worker_panic_returns_typed_handle_error` proves worker
    panic visibility at the live handle boundary.
  - `betelgeuse_runtime_local_mailbox_full_is_visible_in_trace` proves local
    mailbox full in the live runtime.
  - multi-shard tests prove live worker-thread dispatch, bad remote isolation,
    cross-shard call rejection, and remote queue full.

Explicit-step runtime / driver:

- `tina-runtime/tests/call_dispatch.rs`
  - invalid listener/stream ids, port-zero bind, and peer address shape are
    directly tested.
  - stopped requester cancellation is direct for pending accept, read, and
    delayed write.
  - delayed write cancellation is stepped past maturity and proves no late
    translated message or `CallCompleted`.
  - full-duplex read/write on one stream is allowed.
  - duplicate read and close-while-read are rejected as `ResourceBusy`.
  - canceled read does not close the stream for later write.
- `tina-runtime/src/tests.rs`
  - `runtime_shutdown_cancels_non_betelgeuse_driver_pending_call` proves generic
    runtime shutdown cancels pending driver work.
  - `runtime_shutdown_surfaces_driver_completion_release_failure` proves
    driver shutdown failure propagates.

`tina-sim` oracle:

- `tina-sim/tests/io_simulation.rs`
  - scripted TCP echo covers one client, overlapping clients, sequential
    clients, tangled overlap, partial reads/writes, and single-byte drain.
  - invalid resources, pending completion capacity exhaustion, listener/stream
    close while pending, duplicate read, full-duplex lane separation, stopped
    requester cancellation, requester mailbox full at completion, replay, seed
    divergence, ready reordering, and checker replay are directly tested.
- `tina-sim/tests/multishard_dispatcher.rs`
  - TCP workload composes with seeded TCP completion faults.
  - supervision workload composes with seeded local-send delay.
- `tina-sim/tests/tokio_vs_tina_examples.rs`
  - comparison examples cover user-facing send/call/full/closed/timeout,
    ingress backpressure, restart, cross-shard full, and shutdown-vs-stop
    shapes. These are useful ergonomics evidence, not core runtime proof.

### Surrogate Coverage

The pieces for a production-shaped local service exist separately:

- Listener/connection lifecycle exists in echo tests.
- Runtime-owned TCP cancellation exists in driver tests.
- Runtime-owned timer cancellation exists in substrate tests.
- Mailbox full exists in live substrate tests.
- Ingress full exists in live TCP tests.
- Restart/stale-address behavior exists in simulator and dispatcher-style tests.
- Slow/partial peer behavior exists in simulated I/O echo tests.
- Replay and perturbation exist in `tina-sim`.

This is good grug, but still surrogate. The tests do not yet prove that all
these behaviors compose in the same local-server workload.

### Missing Direct Proof

The main missing proof is one canonical local production workload:

- listener/control isolate;
- connection isolates;
- bounded worker pool;
- supervisor policy;
- runtime-owned TCP and time;
- explicit shutdown path;
- bounded ingress and bounded mailboxes.

Specific gaps:

- No live or simulated workload where a TCP connection calls/sends into a
  bounded worker pool and observes `Full`, `Closed`, and `Timeout` in the
  connection/control path.
- No live server-shaped test where a supervised worker/child restarts while TCP
  work is in flight, and stale addresses reject under that workload.
- No one-workload graceful shutdown proof that covers pending accept, pending
  read/write, pending timer, pending isolate call, and queued worker messages.
  Individual shutdown proofs exist, but not composed.
- No deterministic Betelgeuse simulated-I/O server workload that combines slow
  reader/writer, partial write, delayed completion, and bounded worker pressure.
- No `tina-sim` oracle equivalent for the local server workload with replay and
  perturbation.
- No memory/backpressure guard around server-shaped buffering. Existing
  allocation probes protect hot paths, but not a listener/connection/worker
  workload's own buffers.

### Recommended First Build Slice

Build one new canonical workload, likely in new focused test files rather than
stretching `tcp_echo.rs` further:

- `tina-runtime/tests/local_production_runtime.rs` for live Betelgeuse and
  threaded Betelgeuse simulated-I/O slices.
- `tina-sim/tests/local_production_runtime.rs` for oracle/replay slices, if the
  code volume stays sane.

First slice should prove:

- listener accepts multiple connections;
- connection parses one small request and submits bounded worker work;
- worker mailbox capacity is small enough to force `Full`;
- one request times out through runtime-owned call timeout;
- shutdown while at least one accept/read/write/timer/call is pending returns
  cleanly or with the Surveyor typed lifecycle failure path;
- assertions are on typed outcomes and trace events, not logs.

Then add restart/stale-address and simulated slow-peer pressure as the second
slice if the first slice stays readable.
