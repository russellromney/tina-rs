# 014 Mariner TCP Completeness

Session:

- A

## In Plain English

Phase 012 proved that `tina-rs` can drive real TCP work through a
runtime-owned call contract on a completion-driven backend. Phase 013 closed
the remaining address-honesty gap by restoring truthful `127.0.0.1:0` binds
and accepted-stream `peer_addr`.

The biggest remaining weakness in Mariner's TCP story is not honesty of one
operation. It is that the reference workload is still shaped like a one-shot
demo:

- one listener accept
- one accepted connection
- no listener re-arm
- no proof that the runtime keeps behaving like a real server once the first
  client is done

That is enough for a first end-to-end proof, but not enough for a credible
TCP server surface.

This package finishes the first TCP server shape. The listener should be able
to keep accepting, spawning, and supervising connection handlers across more
than one client connection without falling back to harness tricks or one-shot
topology assumptions.

This is honestly the deferred server-completeness part of 012, not a brand-new
capability class. 012 proved the TCP call path end-to-end; 014 proves that the
same surface behaves like a small but real server instead of a one-client demo.

To do that honestly, 014 also needs one small `tina` boundary extension:
today a handler can return only one effect per turn, but a credible listener
needs to express "spawn this accepted connection child, then re-arm accept"
without hidden callbacks or harness tricks. This package therefore adds an
ordered batch effect and uses it immediately in the TCP listener workflow.

## What Changes

- The listener/accept path in the reference TCP workload becomes re-arming
  instead of one-shot.
- `tina::Effect` gains an ordered batch/sequence form so one handler turn can
  request more than one existing effect without introducing a new open-ended
  effect language.
- The chosen re-arm shape is the smallest one that stays inside today's
  `tina` boundary:
  - the listener captures its own typed address via
    `ctx.current_address::<ListenerMsg>()`
  - an accepted connection turn can return an ordered batch equivalent to
    "spawn child, then send self `ReArmAccept`"
  - listener close remains an explicit later turn (`TcpListenerClose` then
    `Stop`), not a hidden callback
- The runtime proof surface grows from "one client can connect and echo" to
  "the listener can continue serving across multiple client connections."
- The runnable `tcp_echo` example becomes a small but honest server-shaped
  example that accepts exactly `N` clients, asserts on them, closes the
  listener, and exits.
- The tests should cover both:
  - sequential clients
  - a small bounded amount of overlap, concretely:
    at least two connection isolates with pending TCP completions in the
    runtime at the same time
- Graceful shutdown of the listener is in scope for this package. The reference
  workload should prove that the listener can be told to close its listening
  socket and stop cleanly once the bounded client workload is complete.

## What Does Not Change

- `tina` remains substrate-neutral.
- Handlers remain synchronous.
- `CurrentRuntime::step()` remains synchronous and caller-driven.
- The effect set remains closed. 014 does not introduce an open effect
  language or arbitrary callbacks; it adds one explicit ordered composition
  form over the existing closed set.
- Runtime-owned sleep / timer wake is still out of scope for this package.
- This package does not introduce a benchmarking claim or a "many thousands of
  connections" claim.
- This package does not turn the runtime into a general-purpose production
  network server; it proves the first honest TCP server shape.
- This package does not add a logical-name registry, a new public effect shape,
  or any cross-stream ordering guarantee.

## Acceptance

- The listener re-arms `TcpAccept` through the normal runtime/isolate workflow.
- The ordered batch effect is directly proved:
  - effects execute left-to-right
  - later effects still run after earlier non-terminal effects
  - `Stop` short-circuits later effects in the same batch
- The TCP proof surface demonstrates successful handling of multiple client
  connections without rebuilding the listener for each client.
- The proof demonstrates:
  - at least one sequential multi-client run
  - at least one run where two accepted connections are in flight at once
- The accepted connection lifecycle remains runtime-owned:
  - listener accepts
  - listener spawns/supervises connection child
  - child boots through runtime workflow
  - child reads/writes/closes through `Effect::Call`
- The listener can be told to close cleanly:
  - it emits `Effect::Call(TcpListenerClose { .. })`
  - it stops through the normal runtime path
- Trace assertions pin multiplicity and topology:
  - exactly one successful `TcpBind`
  - at least `N` successful `TcpAccept`
  - at least `N` connection-child spawns
  - at least `N` successful `TcpStreamClose`
  - no listener rebuild in the same run
- The proof still asserts on runtime events, not just payload round-trips.
- The proof does not assert cross-stream ordering that the runtime never
  promised; assertions stay per-stream.
- The runnable example remains assertion-backed.
- `make verify` passes.

## Notes

- This is intentionally a larger coherent package than 013. The point is to
  finish the first TCP server surface, not to split accept re-arm, example
  refresh, and proof expansion into separate bookkeeping slices.
