# Phase 049: North Sea And Tracing

## Goal

Make the Seastar-lineage and ops story less rhetorical.

North Sea is the Linux substrate lane:

> Tina owns shard-local progress on a polled `io_uring` backend, with capability
> truth when the platform cannot support it.

Tracing is the ops lane:

> Tina runtime truth can flow into the Rust `tracing` ecosystem without users
> writing custom exporters.

These are paired because both ask the same question from different ends:

> Can Tina be serious in production-shaped environments without becoming Tokio?

## Baseline

Already exists:

- explicit-step runtime as semantic oracle;
- threaded live runtime over portable/Betelgeuse-backed paths;
- runtime-owned TCP, UDP, DNS, TLS, file/path, process, signal, persistence;
- runtime capability reports;
- topology and terminal reports;
- trace event stream;
- cost smoke rows;
- roadmap North Sea target;
- Baobab readiness rails.

Missing for the Seastar claim:

- Linux `io_uring` backend;
- polled I/O path owned by Tina shard runner;
- zero-copy or buffer ownership story;
- HTTP/RPC native stack;
- memory-pool-per-shard discipline beyond current preallocation knobs.

Missing for ops:

- `RuntimeEvent` to `tracing` adapter;
- span/event naming policy;
- OTel/W3C propagation plan;
- stable event identity/fingerprint path from 047.

## Non-Goals

- No native HTTP implementation. That is 048.
- No DPDK.
- No userspace TCP.
- No broad zero-copy claim in first spike.
- No replacing the explicit-step runtime oracle.
- No Linux-only default that breaks macOS.
- No hidden fallback from `io_uring` to portable backend without capability
  truth.
- No production benchmark claim.

## Rules

- Explicit-step runtime remains the semantic oracle.
- `io_uring` backend must preserve Tina meanings: bounded commands, explicit
  progress, runtime-owned resources, visible cancellation, visible shutdown,
  no hidden executor tasks.
- Unsupported platforms report unsupported or portable backend. They do not
  pretend to be North Sea.
- Backends may differ in capability, not in user-visible semantics unless
  capability reports say so.
- Tracing export must not become a hidden metrics channel that disagrees with
  trace truth.
- Tracing export must be bounded or lossy-by-policy, not secretly unbounded.

## Rocks

1. **North Sea Capability Design**

   Write the backend contract before wiring code.

   Required rows:

   - TCP accept/connect/read/write/close;
   - timers if backed by `io_uring` or existing timer path;
   - cancellation;
   - shutdown;
   - resource ownership;
   - buffer ownership;
   - bounded command ingress;
   - per-shard progress;
   - platform support;
   - fallback behavior.

   Output should update roadmap/system docs only with proven truth.

2. **Small `io_uring` Spike**

   Build the smallest honest spike.

   Candidate:

   - one Linux-only crate/module or test;
   - bind loopback TCP;
   - accept one connection;
   - read bytes;
   - write bytes;
   - close;
   - explicit step/progress function;
   - no async executor.

   This can be outside the public runtime until the shape is boring.

   Done means it runs on Linux. A design-only spike is useful research, but it
   does not close this rock.

3. **Driver Runtime Integration Plan**

   Map spike into Tina's driver contract.

   Required answers:

   - where completion storage lives;
   - how call ids map to submissions/completions;
   - how cancellation tombstones or cancels pending work;
   - how resource close interacts with pending ops;
   - how shutdown drains/cancels;
   - how bounded command queues feed the ring;
   - how topology reports pending ring work;
   - how simulator proof remains comparable.

4. **Buffer Ownership And Zero-Copy Roadmap**

   Do not claim zero-copy early. Define the path.

   Required:

   - current buffer copy points;
   - possible shard-local buffer pool shape;
   - safe ownership rules for buffers in isolate messages;
   - what HTTP streaming needs from buffers;
   - what is explicitly later.

5. **Linux Gate And Platform Truth**

   Requirements:

   - Linux-only tests are gated visibly;
   - macOS/default verify stays green;
   - capability report distinguishes `io_uring` supported, portable fallback,
     unsupported, and not claimed;
   - CI plan names optional Linux job.

6. **Tracing Adapter**

   Add or prototype an adapter from Tina runtime events to `tracing`.

   Requirements:

   - event ids and cause ids map to fields;
   - isolate id, shard id, call id, resource id, outcome, and rejection reason
     appear where useful;
   - span naming policy is documented;
   - export can be disabled;
   - export has bounded/drop policy;
   - adapter does not change runtime semantics.

   If 047 stable trace fingerprint is not landed, prototype but do not promise
   stable external identity.

7. **OTel / Trace Context Notes**

   Write the distributed tracing shape, even if not implemented.

   Questions:

   - how HTTP incoming trace context enters Tina;
   - how outbound HTTP/DB calls carry context;
   - how isolate messages preserve or intentionally drop context;
   - how simulator/replay handles trace context;
   - what belongs in Tina vs. app code.

8. **Cost Smoke Preparation**

   Prepare North Sea cost rows without making benchmark claims.

   Rows:

   - loopback TCP echo;
   - accept/connect churn;
   - small write;
   - large streaming write;
   - cancellation;
   - shutdown with pending ops.

   Print environment and backend. No speed claims.

## Required Proof

- Design doc names exact capabilities and non-claims.
- Linux spike compiles and runs on supported Linux.
- macOS/default paths skip Linux-only tests visibly.
- macOS/default `make verify` path remains unaffected.
- Capability report or draft report can say what backend is active.
- Tracing adapter/prototype emits representative runtime events into
  `tracing`.
- Export policy is bounded or explicitly lossy.
- Review records what 048/native HTTP needs from North Sea.

## Done Means

- Tina has a credible path from portable runtime to Linux polled I/O.
- The project can honestly say: model is Seastar-shaped today; North Sea is how
  substrate becomes Seastar-serious.
- Operators can start seeing Tina runtime truth through normal Rust tracing
  tools.
- No one has claimed DPDK, userspace TCP, zero-copy, production performance, or
  full Tokio replacement before proof exists.
