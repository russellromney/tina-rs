# 035 Jelle Zijlstra Runtime-Owned I/O Breadth Plan

## Purpose

Make Tina's local I/O story big enough for real local services without
smuggling in a general async runtime.

Piet de Jong closed the local app/bridge shell around the current I/O claim:
runtime-owned time, server-side TCP, bounded ingress, shutdown, health,
hardening, and measured cost. Jelle owns the next core question: what I/O does
a bounded shared-nothing service actually need before Gemini can explain Tina
without apology.

This phase is not docs, persistence, remoting, clustering, or broad
performance marketing. It is runtime-owned I/O capability.

## Starting Baseline

Current shipped Tina I/O:

- `CallInput::Sleep` / `sleep(...)`;
- `TcpBind`, `TcpAccept`, `TcpRead`, `TcpWrite`, listener close, stream close;
- live Betelgeuse-backed driver;
- Tina simulator support for the same time/TCP call family;
- `LocalApp` and bridge tests proving time/TCP can live inside app-shaped
  services.

Current substrate wood:

- Betelgeuse is completion-based: no runtime, no hidden tasks.
- Betelgeuse native backends already expose file operations:
  `open`, `pread`, `pwrite`, `fsync`, `size`, and `mkdir`.
- Betelgeuse simulated backend currently supports narrow TCP simulation, not
  files.
- Betelgeuse socket interface currently supports bind/accept/recv/send, but no
  outbound connect operation.

Grug important: Jelle should not start by bolting DNS/TLS/process onto a
half-TCP runtime. First finish the basic local substrate shape: outbound TCP and
file I/O.

## Accepted Scope

Jelle implements these two families unless implementation discovers a hard
substrate blocker:

1. **Outbound TCP connect**
   - add completion-based connect support to `vendor-betelgeuse`;
   - add `CallInput::TcpConnect` / `CallOutput::TcpConnected`;
   - add `tcp_connect(addr)` helper;
   - support live runtime and Tina simulator;
   - prove timeout/cancellation/shutdown/invalid-resource behavior;
   - add a user-shaped client workload: Tina connects to a local echo/server,
     writes, reads, closes, and observes backpressure/partial write semantics.

2. **Runtime-owned file I/O**
   - add runtime-owned `FileId`;
   - add open/read-at/write-at/fsync/size/mkdir/close call shapes and helpers;
   - use Betelgeuse native file primitives in the live driver;
   - add deterministic simulator support for enough file behavior to replay
     config/snapshot/log-like workloads;
   - add cancellation/shutdown/resource-busy/invalid-resource tests;
   - add a user-shaped workload: Tina loads config, writes a snapshot/log,
     fsyncs, reads it back, and shuts down without orphaned pending operations.

Everything else is a decision table in this phase, not secret scope creep.

## Support Table Target

By phase close, this table must be true in the review:

| Family | Jelle target | Reason |
|---|---|---|
| Time | already supported | Baseline from earlier phases. |
| TCP server | already supported | Baseline from earlier phases. |
| TCP client connect | supported | Needed for real services to call other services. |
| File / mkdir | supported | Needed for config, local state, snapshots, logs. |
| DNS | deferred with exact reason | Needs nonblocking resolver/cache semantics; raw `SocketAddr` remains accepted. |
| TLS | deferred with exact reason | Needs TCP connect first plus rustls/handshake state-machine design. |
| UDP | deferred with exact reason | Useful, but packet semantics and multicast/buffer policy need a dedicated phase. |
| Process | deferred with exact reason | Needs child lifecycle, pipes, cancellation, zombie proof. |
| Signal | deferred with exact reason | Process-global/platform-specific; app edge can request shutdown for now. |

If implementation proves one deferred item is required by the accepted
workloads, pause and amend the plan before expanding scope.

## Design Rules

- All new I/O enters through `RuntimeCall`, not new `Effect` variants.
- Helpers use direct names: `tcp_connect`, `file_open`, `file_read_at`,
  `file_write_at`, `file_fsync`, `file_size`, `mkdir`, `file_close` if needed.
- Resource identity is Tina-owned: `StreamId`, `ListenerId`, new `FileId`.
  No raw OS handles or Betelgeuse boxes escape to isolates.
- Every waiting operation has visible timeout/cancellation behavior through the
  existing call outcome path.
- Per-call cancel must remove quiescence pressure without closing unrelated
  resource lanes.
- Closing a resource with an active lane fails visibly with
  `CallError::ResourceBusy`, unless a stronger and tested semantic is chosen.
- No alternate mailbox escape path. Jelle must keep the existing bounded
  mailbox contract; if outbound TCP or file I/O appears to require
  multi-producer mailbox support, pause and name the workload instead of adding
  a hidden second route.
- Simulator semantics may be smaller than native semantics, but they must be
  honest and deterministic.
- No hidden blocking pool, no unbounded queue, no arbitrary async future inside
  an isolate.

## Build Steps

1. Audit current Tina runtime/sim/Betelgeuse I/O boundaries and update this
   plan if the code says a different ordering is safer.
2. Add Betelgeuse completion slot and backend support for outbound TCP connect.
   Native and simulated backends both get direct tests.
3. Add Tina runtime call vocabulary and helper for `tcp_connect`.
4. Add live driver support for connect with resource lanes, cancellation,
   shutdown, and trace kind.
5. Add Tina simulator/DST support for connect, including seeded ready-order
   perturbation where it matches existing TCP fault machinery.
6. Add user-shaped outbound TCP e2e tests in live runtime and simulator.
7. Add Tina runtime file vocabulary: `FileId`, open options, read/write/fsync,
   size, mkdir, close/release if needed.
8. Add Betelgeuse driver file-resource state, completion slots, lanes,
   cancellation, shutdown drain, invalid-resource, and resource-busy behavior.
9. Add deterministic simulator file support for the accepted file workload.
10. Add user-shaped file e2e tests in live runtime and simulator.
11. Add bridge/app tests proving a `LocalApp` service can use connect/file calls
    behind the Tokio/Tower edge without async handlers.
12. Add allocation probes for the new hot paths with narrow claims only.
13. Write the final support table in `review.md`, including exact deferrals for
    DNS/TLS/UDP/process/signal.
14. Run `make verify`.

## Required Proof

Outbound TCP:

- native Betelgeuse connect completion test;
- simulated Betelgeuse connect completion test;
- live Tina runtime connects to a local server, writes, reads, and closes;
- Tina simulator replays the same logical client flow deterministically;
- connect timeout/cancel/shutdown tests;
- connect to closed/unreachable address surfaces a typed failure;
- close while read/write/connect is pending is visible and safe.

File:

- native Betelgeuse file operation tests are still green;
- simulated file backend test for open/write/read/size/fsync/mkdir behavior;
- live Tina runtime open/write/fsync/read/size/close flow;
- Tina simulator deterministic replay of the same file workload;
- pending file operation cancellation on requester stop and runtime shutdown;
- invalid file id and resource-busy tests;
- allocation probe for warmed read/write path.

App/bridge:

- `LocalApp` service uses outbound TCP and file I/O in one user-shaped flow;
- bridge-hosted service can trigger runtime-owned file/TCP work and return a
  typed response;
- cancellation before handler start still avoids user mutation;
- cancellation after handler starts does not pretend to preempt synchronous
  handler work.

## Pause Gates

Pause and update the plan if:

- Betelgeuse connect requires a large redesign of `IOSocket`;
- native macOS and Linux connect semantics diverge enough to need a smaller
  first slice;
- file simulation starts becoming a durable-storage design;
- file API needs path sandbox/security policy decisions beyond local test
  files;
- TLS/DNS/process/signal become necessary to prove the accepted workloads;
- outbound TCP or file I/O seems to require a multi-producer mailbox
  implementation;
- allocation cost shows a structural problem caused by the new resource model;
- any new I/O needs a hidden blocking pool or unbounded queue.

## Done Means

- TCP client connect is runtime-owned, helper-backed, live-tested,
  simulator-tested, cancellation-tested, and shutdown-tested.
- File I/O is runtime-owned, helper-backed, live-tested, simulator-tested,
  cancellation-tested, and shutdown-tested.
- `LocalApp` can host services that use the new I/O families.
- Bridge-hosted services can drive the new I/O families without async handlers.
- Allocation/performance notes are narrow and recorded.
- DNS/TLS/UDP/process/signal have explicit support-table status and roadmap
  homes.
- `make verify` passes.

## Non-Claims After This Phase

Even if Jelle succeeds:

- Tina is still not a general Tokio replacement.
- Tina still does not run arbitrary async ecosystem code inside isolates.
- Tina still does not claim broad throughput superiority.
- DNS, TLS, UDP, process, and signal are still not supported unless the support
  table says otherwise at close.
- Persistence remains Wim Kok.
