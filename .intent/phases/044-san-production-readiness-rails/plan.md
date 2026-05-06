# Phase 044: San Production-Readiness Rails

## Goal

Build the first hard gate for "a real Tokio- or Glommio-shaped local service
can be moved to Tina and the important behavior stays visible, bounded, and
testable."

This is not a porting guide, demo, release story, or marketing phase. It is the
test wall and runtime rails that make later porting attempts honest.

At closeout:

> Tina has an executable readiness gate for local service ports: capability
> truth, user-perspective e2e, DST pressure, CI rails, and baseline cost numbers.

## Non-Goals

- No remoting, clustering, membership, or placement.
- No durable mailbox or exactly-once claim.
- No broad "faster than Tokio/Glommio" claim.
- No Tower/Axum middleware living inside Tina.
- No `flow!` syntax unless a tiny helper is required to keep tests readable.
- No hidden fallback queues.

## Rules

- If something can overload, a test must observe `Full` or pressure.
- If something can fail, a test must observe typed failure and trace.
- If something can race, DST or a deterministic e2e must replay it.
- If Tina cannot support a common Tokio/Glommio-shaped capability, the
  capability matrix must say so explicitly.
- Benchmarks produce numbers, not claims.
- Comparisons must test behavior first: overload, cancellation, shutdown,
  replay, and resource ownership matter more than raw throughput.

## Rocks

1. **Capability Matrix**
   Add an executable matrix for common local-service needs: TCP, DNS, TLS,
   UDP, files, process, signals, timers, calls, cross-shard sends, persistence,
   bridge ingress, cancellation, shutdown, and backpressure. Assert it matches
   `RuntimeCapabilities` and public non-claims. Include explicit columns for
   Tina, Tokio, and Glommio when a behavior is meaningfully comparable.

2. **Porting Gap Tests**
   Add compile/run tests for the patterns a small Tokio service usually needs:
   listener loop, outbound client call, timeout, retry, bounded queue, config
   file read, process helper, signal shutdown, and durable checkpoint.

3. **Service Gauntlet**
   Build one user-shaped local Tina service that uses several rails together:
   TCP/TLS ingress or loopback, DNS, file/path I/O, timer, process, persistence,
   cross-shard call, and graceful shutdown. Prove positive path and negative
   paths.

4. **Bridge Gauntlet**
   Exercise the Tokio bridge as an app boundary: bounded ingress, timeout,
   caller cancellation, retry policy naming, shutdown retry, and bridge metrics.
   No claim that arbitrary Tower/Axum middleware lives inside Tina.

5. **DST Gauntlet**
   Add randomized histories that combine at least three rails per history:
   cancellation + timeout + late completion, pressure + shard failure +
   topology, persistence + restart + corrupt/truncated data, bridge ingress +
   service shutdown + retry.

6. **Live Thread-Per-Core Pressure**
   Run live multi-shard services on real worker threads with cross-shard sends
   and isolate calls under bounded queue pressure. Prove healthy shards keep
   running when one shard fails.

7. **Lifecycle Contract Tests**
   For every public terminal API, prove clean shutdown, driver shutdown
   failure after useful trace exists, and one-shard-failed with sibling trace
   retained.

8. **Backpressure Wall**
   Add explicit overload tests for mailbox, ingress, shard-pair, bridge,
   resource-lane, and persistence-lane pressure. No sleeps-as-proof.

9. **Comparison and Cost Numbers**
   Add a stable local benchmark/report command for selected paths: local send,
   cross-shard send, isolate call, TCP loopback, TLS loopback, file read/write,
   journal append, bridge call. Include narrow Tokio and Glommio baselines
   where they are local, fair, and easy to run; skip with a visible unsupported
   row when the platform/substrate makes the comparison dishonest. Record
   allocations where current probes allow. Add runnable comparisons for
   constrained-memory and overload behavior: bounded channel pressure, many
   concurrent connections, slow peer, cancelled request, shutdown while work is
   in flight, and local disk/persistence pressure. Expected output must say
   where Tina preserves behavior, where Tina rejects earlier/more loudly, and
   where Tokio or Glommio have a feature Tina still does not claim.

10. **CI Rails**
    Add or tighten CI so the public gate runs: fmt, check, clippy, docs,
    workspace tests, selected DST seeds, platform capability tests, and the
    readiness gauntlet. Keep slow/host-specific tests named.

11. **API Readiness Sweep**
    Audit public names and helper shapes used by the gauntlet. Fix footguns
    that make Tina easy to misuse without adding duplicate ways to do the same
    thing.

12. **Readiness Report**
    Update `review.md`, `CHANGELOG.md`, and `ROADMAP.md` with landed truth:
    what Tina can now run, what the tests prove, what remains a non-claim, and
    what blocks prime-time porting.

## Required Proof

- `make verify` passes.
- The service gauntlet has positive, negative, overload, shutdown, and failure
  tests.
- DST histories replay from saved seeds and shrink at least one failing-style
  history.
- CI config exists and names platform-specific exclusions honestly.
- Cost command produces numbers locally without turning them into marketing.

## Done Means

- A future small-porting session has a real gate to run before claiming Tina is
  ready for that port.
- The matrix tells a user what works, what rejects, and what is still not Tina.
- No review note can say "this was only tested as happy path" for the rocks
  above.
