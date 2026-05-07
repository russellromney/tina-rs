# Phase 049: Betelgeuse Linux And Tracing

## Goal

Turn the old "North Sea builds io_uring" idea into the better truth:
Betelgeuse already gives Tina a completion-based Linux `io_uring` backend,
a macOS `kqueue` backend, and a simulated backend. Tina should prove, surface,
and harden those paths instead of building a second I/O substrate.

Tracing is still the ops lane:

> Tina runtime truth can flow into the Rust `tracing` ecosystem without users
> writing custom exporters.

049 answers:

> Can Tina honestly report and operate its real Betelgeuse backend while keeping
> Tina semantics above it?

Near-grug:

> We already have io_uring. It came in Betelgeuse. Prove it. Do not rebuild it.

## Baseline

Already exists:

- explicit-step runtime as semantic oracle;
- threaded live runtime over Betelgeuse;
- Betelgeuse native backends:
  - Linux: `io_uring`;
  - macOS: `kqueue`;
  - simulated: deterministic socket/file completion testing;
- runtime-owned TCP, UDP, DNS, TLS, file/path, process, signal, persistence;
- runtime capability reports;
- topology and terminal reports;
- trace event stream;
- cost smoke rows;
- Baobab readiness rails.

Old assumption that was wrong:

- Linux `io_uring` backend is **not** missing. Betelgeuse already owns that
  substrate. Tina's job is adapter truth, proof, and hardening.

Missing for the Seastar-lineage claim:

- Linux Betelgeuse/`io_uring` cost and pressure rows on real Tina services;
- backend-name/capability truth that distinguishes:
  - `betelgeuse-linux-io-uring`;
  - `betelgeuse-darwin-kqueue`;
  - `betelgeuse-simulated`;
- completion/resource lifetime hardening from 060;
- long-lived completion-slot/slab plan;
- buffer ownership story;
- HTTP/RPC/native service pressure proof;
- memory-pool-per-shard discipline beyond current preallocation knobs.

Missing for ops:

- `RuntimeEvent` to `tracing` adapter;
- span/event naming policy;
- OTel/W3C propagation plan;
- stable event identity/fingerprint path from 047.

## Coordination

049 can start now.

Coordinate with:

- 047 for stable trace fingerprints / event identity;
- 048 for HTTP streaming and buffer ownership needs;
- 051 for bridge tracing context;
- 060 for Betelgeuse close/cancel/resource lifetime hardening;
- 054 later, because userspace TCP research should wait for Linux
  Betelgeuse/`io_uring` pressure evidence.

## Non-Goals

- No Tina-owned `io_uring` backend unless Betelgeuse cannot satisfy a named Tina
  contract.
- No replacing Betelgeuse.
- No native HTTP implementation. That is 048.
- No DPDK.
- No userspace TCP.
- No broad zero-copy claim in first proof pass.
- No replacing the explicit-step runtime oracle.
- No Linux-only default that breaks macOS.
- No hidden fallback between Betelgeuse backends without capability truth.
- No production benchmark claim.

## Rules

- Explicit-step runtime remains the semantic oracle.
- Betelgeuse is the canonical portable live backend.
- Linux `io_uring`, macOS `kqueue`, and simulated I/O are Betelgeuse backend
  capabilities, not separate Tina programming models.
- Backends may differ in capability, not in user-visible Tina semantics unless
  capability reports say so.
- Tina should borrow Betelgeuse substrate machinery before rolling its own:
  completion model, simulated backend, backend name hooks, cancellation hooks,
  and fixed-capacity slab patterns.
- Tracing export must not become a hidden metrics channel that disagrees with
  trace truth.
- Tracing export must be bounded or lossy-by-policy, not secretly unbounded.

## Betelgeuse Capability Audit

Betelgeuse has useful substrate pieces Tina should deliberately use or
deliberately reject:

- **Native backends:** Linux `io_uring`, macOS `kqueue`, simulated I/O.
  Tina should surface which one is active.
- **Caller-owned completions:** Tina uses heap-stable boxed completions today.
  Future work should move toward long-lived per-resource/per-lane completions.
- **Fixed-capacity slab:** Betelgeuse ships a slab. Tina should consider it or
  the same shape for completion/resource storage instead of per-op allocation.
- **Simulated backend:** Tina already has `tina-sim`, but Betelgeuse simulated
  I/O is perfect for hostile substrate tests where the backend delays
  completion release.
- **Backend lifecycle hooks:** `pending_completion_count()` and
  `cancel_pending_completions()` are already in use and should become explicit
  in the driver contract.
- **Backend names:** `backend_name()` should feed Tina capability/topology
  reports.
- **TCP_NODELAY:** Betelgeuse exposes `IOSocket::set_nodelay`. Tina should add a
  runtime call for it if HTTP/RPC pressure shows latency benefit.
- **Task/coroutine helpers:** useful reference, not Tina user shape. Tina should
  not expose Betelgeuse `spawn!`/`io_await!` as its programming model.

Things Tina currently rolls itself that Betelgeuse does not appear to own:

- timers;
- UDP;
- DNS;
- TLS policy/handshake lane;
- process runs;
- signal capture;
- higher-level path operations (`metadata`, `read_dir`, `rename`);
- snapshot/journal persistence semantics.

Those can stay Tina-owned unless Betelgeuse grows matching substrate support.

## Rocks

1. **Rename North Sea Semantics**

   Update roadmap/docs so North Sea no longer means "write io_uring backend."

   New meaning:

   ```text
   North Sea = Linux proof/tuning through Betelgeuse io_uring.
   ```

   Required:

   - roadmap says Betelgeuse is canonical portable live backend;
   - docs say Linux is already `io_uring` through Betelgeuse;
   - future Tina-owned `io_uring` code is evidence-gated, not assumed.

2. **Backend Capability Truth**

   Surface the actual backend in runtime reports.

   Required rows:

   - backend name;
   - platform;
   - TCP accept/connect/read/write/close support;
   - file open/read/write/fsync/size/mkdir support;
   - cancellation support;
   - shutdown support;
   - pending completion count support;
   - simulated backend support;
   - unsupported or Tina-owned rails (UDP/DNS/TLS/process/signal/path extras).

   Target names:

   ```text
   betelgeuse-linux-io-uring
   betelgeuse-darwin-kqueue
   betelgeuse-simulated
   ```

3. **Linux Betelgeuse Pressure Rows**

   Run platform-gated Linux rows using the existing Betelgeuse `io_uring`
   backend.

   Rows:

   - loopback TCP echo;
   - accept/connect churn;
   - small write;
   - large streaming write;
   - close while read/write/accept pending;
   - shutdown with pending ops;
   - native HTTP smoke/pressure if 048 is available;
   - native RPC smoke/pressure if 052/058 is available.

   Print:

   ```text
   backend platform scenario accepted full closed timeouts pending rss exit
   ```

   No broad speed claims. These are cost-smoke and behavior rows.

4. **Betelgeuse Simulated Backend Hostile Tests**

   Use Betelgeuse simulated I/O more deliberately.

   Required:

   - delayed completion after close/cancel;
   - partial send/read shape;
   - pending completion count behavior;
   - parity note between `tina-sim` semantic replay and Betelgeuse simulated
     substrate tests.

   This can overlap with 060; 049 records substrate evidence, 060 hardens the
   adapter if evidence shows a sharp edge.

5. **Long-Lived Completion Slot Plan**

   Record the path from boxed per-op completions to Betelgeuse-shaped long-lived
   slots.

   Candidate:

   - listener owns one accept slot;
   - stream owns read and write slots;
   - file owns read/write/fsync/size slots or borrows from a bounded slab;
   - driver owns fixed-capacity completion slabs;
   - warm path avoids per-op `Box`;
   - close tombstones keep slots/resources alive until backend release.

   This rock may be design-only. Implementation can live in 060 or a later
   performance phase.

6. **TCP_NODELAY Runtime Call Decision**

   Audit whether Tina should expose Betelgeuse `set_nodelay`.

   Required:

   - decide API shape:

     ```rust
     tcp_set_nodelay(stream, true).reply(...)
     ```

   - prove live/sim behavior or document simulated no-op;
   - decide default for HTTP/RPC connections;
   - keep it explicit unless pressure data justifies a config default.

7. **Tracing Adapter**

   Add or prototype an adapter from Tina runtime events to `tracing`.

   Requirements:

   - event ids and cause ids map to fields;
   - isolate id, shard id, call id, resource id, outcome, and rejection reason
     appear where useful;
   - backend name appears where useful;
   - span naming policy is documented;
   - request id is correlation, not a metrics cardinality label;
   - export can be disabled;
   - export has bounded/drop policy;
   - adapter does not change runtime semantics;
   - at least one test subscriber captures emitted events and asserts key
     fields.

8. **OTel / Trace Context Notes**

   Write the distributed tracing shape, even if not implemented.

   Questions:

   - how HTTP incoming trace context enters Tina;
   - how outbound HTTP/DB/AWS bridge calls carry context;
   - how isolate messages preserve or intentionally drop context;
   - how simulator/replay handles trace context;
   - what belongs in Tina vs. app code.

## Required Proof

- Docs no longer say Linux `io_uring` is missing from the Betelgeuse path.
- Capability report or draft report can say what backend is active.
- Linux Betelgeuse rows compile/run on supported Linux or are visibly skipped
  elsewhere.
- macOS/default `make verify` path remains unaffected.
- Betelgeuse simulated backend is used in at least one hostile substrate test or
  explicitly delegated to 060.
- Tracing adapter/prototype emits representative runtime events into
  `tracing`.
- Export policy is bounded or explicitly lossy.

## Done Means

- Tina can honestly say it already uses Betelgeuse `io_uring` on Linux.
- North Sea no longer means "build duplicate io_uring."
- Operators can start seeing Tina runtime truth through normal Rust tracing
  tools.
- The remaining substrate work is proof, hardening, buffer/slot ownership, and
  Linux pressure rows.
- No one has claimed DPDK, userspace TCP, zero-copy, production performance, or
  full Tokio replacement before proof exists.
