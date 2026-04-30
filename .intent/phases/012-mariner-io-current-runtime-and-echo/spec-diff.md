# 012 Mariner I/O, Current Runtime, And Echo

Session:

- A

## In Plain English

The next honest step for `tina-rs` is to let isolates ask the runtime to do
real outside work without breaking the model we have already proven.

Today handlers can return `Send`, `Spawn`, `Stop`, and `RestartChildren`, but
they still cannot describe runtime-owned TCP or similar operations honestly in
the intended Mariner substrate.
That means the current runtime cannot host a real echo server without cheating
and doing I/O somewhere outside the effect model.

This phase package fixes that in one coherent move:

- write the runtime-owned I/O/call contract
- teach `tina-runtime-current` to execute it on an explicit completion-driven
  current-thread backend
- prove the result with an assertion-backed TCP echo workload and runnable
  example

This is one reviewed package, but it is meant to execute as several internal
implementation slices under autonomous bucket mode. We should only stop for a
human if a real semantic fork shows up.

## Why This Intent Changed

Earlier drafts assumed a private Tokio-backed driver for convenience. That is
no longer the right default for Mariner.

The reason is simple: `tina-rs` is trying to preserve Tina's model, not just
its surface syntax. The load-bearing ideas from Peter Mbanugo's writing and the
Tina-Odin reference are:

- synchronous handlers
- explicit stepping
- runtime-owned effects
- no hidden task scheduler as the programming model
- deterministic simulation testing as a real future goal, not a slogan

If those are the priorities, then a completion-driven backend is a better
default fit than a futures executor, even a private one. The Mariner target for
this package is therefore Betelgeuse: caller-driven `step()`, completion
ownership in the runtime/application state machine, and no background task
model leaking into isolate semantics.

This package does **not** promise permanent commitment to one third-party
backend crate forever. It does commit to the shape of the design:

- completion-driven before async-executor-driven
- visible stepping before hidden task progression
- DST compatibility as a first-class constraint
- Tokio bridge work, if needed, belongs later in Apollo rather than defining
  Mariner's core runtime story

This is also a proportionate choice for where `tina-rs` is today. `tina-rs` is
still a proof-of-concept Rust implementation of Tina's reference ideas and blog
post discipline, not a mature production platform with long-term substrate
guarantees. That lowers the cost of choosing the most principled backend first,
so long as the `tina` boundary stays neutral enough to learn and pivot
honestly.

## What Changes

- Add one new runtime-owned effect family at the `tina` boundary instead of
  adding a separate top-level effect variant for every I/O verb.
- `Isolate` gains one new associated type for runtime-owned call descriptors.
- `Effect` gains one new variant for a runtime-owned call request.
- The contract is still synchronous from the isolate's point of view:
  - a handler returns a call description
  - the runtime executes it outside the handler
  - completion comes back later as an ordinary isolate message
- Call completion uses the isolate's existing `Message` type rather than a
  second handler entry point. The call request carries the translation needed
  to turn a runtime result into one ordinary later message for that isolate.
- `tina-runtime-current` defines the first concrete call family for this model.
- The intended first backend for `tina-runtime-current` in this package is
  Betelgeuse.
- The first echo topology should continue slice 011's user-shape where it helps:
  a listener isolate owns accept/bind state and supervises restartable
  connection-handler isolates rather than bypassing the isolate/supervision
  model for connection work.
- The first Betelgeuse-backed current-runtime call family is broad enough for:
  - TCP listener bind
  - TCP accept
  - TCP read
  - TCP write
  - TCP close
- Runtime-owned I/O resources are represented as opaque ids in isolate-facing
  messages and state. Raw backend handles do not cross into isolate code.
- The current runtime keeps ownership of TCP listeners and TCP streams.
- The runtime trace grows to record the new I/O/call behavior with the same
  causal-link discipline already used for send/spawn/stop/restart behavior.
- `tina-runtime-current` grows a current-thread completion-driven execution path
  that executes the new call family honestly.
- `CurrentRuntime::step()` stays synchronous. The current runtime owns backend
  integration internally rather than turning step-driving into async API
  surface.
- Add an assertion-backed TCP echo proof surface:
  - bind listener
  - accept connection
  - read request bytes
  - write response bytes
  - close connection cleanly
- Add a runnable `tcp_echo` example that mirrors the tested workload shape.
- The echo proof remains black-box: it drives the real current runtime and
  asserts on observed network behavior plus runtime trace evidence.

## What Does Not Change

- Handlers do not become async.
- The runtime still owns all real I/O.
- No public logical-name registry is added.
- No simulator or `tina-sim` crate is added in this phase.
- No cross-shard I/O or multi-shard routing is added.
- No Tokio bridge work is added here.
- No futures-executor API becomes part of the `tina` programming model here.
- No broad runtime allocation claim is added here.
- No production-readiness promise is implied just because Betelgeuse is the
  first Mariner backend target.
- The task-dispatcher proof from 011 remains valid and stays in place.
- This package does not widen supervision semantics beyond what Mariner already
  has.
- This package does not claim the first call family is "done" just because it
  handles echo. The contract must still read as a plausible fit for future file
  I/O, UDP, HTTP client work, child-process spawn, and deferred timers even
  though those are not implemented here.
- Runtime-owned sleep/timer wake remains required for the project, but it is
  not a gate for this first Betelgeuse-backed TCP slice.

## How We Will Verify It

- Trait-surface tests prove the new call effect family is available from
  `tina` without forcing async handlers.
- Trait and runtime shaping should leave a plausible path for future file I/O,
  UDP, HTTP client work, child-process spawn, and deferred timers without
  redesigning the `tina` boundary.
- Backend selection notes should show why the chosen Mariner backend direction
  is more faithful to explicit stepping and DST than Tokio-first alternatives,
  even if later runtime crates choose different substrates.
- The recorded backend-fit evidence should name at least these DST-friendly
  constraints and show they are not violated by the chosen design:
  - completion ordering must be runtime-controllable rather than hidden behind
    executor wakeups
  - no wall-clock time may leak into the `tina` boundary as a required semantic
  - runtime-owned resource ids should be deterministic where the runtime, not
    the OS, assigns them
- Current-runtime tests prove:
  - call effects are dispatched only by the runtime
  - call completions come back on later turns as ordinary isolate messages
  - I/O resources are runtime-owned opaque ids, not raw backend handles
  - repeated scripted non-network call histories remain deterministic where the
    runtime controls the ordering
- The runtime trace proves visible outcomes for call dispatch the same way it
  already does for local sends and child restarts.
- An echo integration test proves:
  - a listener bind succeeds
  - a client can connect
  - bytes written by the client are read by the echo isolate
  - the echo isolate writes the same bytes back
  - the connection can be closed cleanly
- The echo proof asserts on trace subtrees that matter for I/O dispatch rather
  than on logs alone.
- Echo tests bind to a concrete high loopback port.
- This package does not claim honest runtime-owned ephemeral-port discovery on
  Betelgeuse yet; if the backend cannot report its actual bound address, the
  proof should use a concrete loopback port rather than reconstructing a
  `:0` bind through a side probe.
- The runnable echo example works without needing hidden helper threads or
  direct I/O inside handlers.
- This package proves correctness of the first Rust I/O shape; it does not
  claim or require the future 100k-connection Mariner benchmark target yet.
- This package does not require a runtime-owned sleep primitive to close the
  TCP-first Betelgeuse bring-up honestly.
- `make verify` passes.

## Notes

- This package should execute as an autonomous stacked slice sequence after the
  package spec/plan is reviewed:
  - contract
  - backend fit and driver
  - echo proof/example
  - closeout
- The call contract should stay substrate-neutral enough that `tina-sim` and
  future runtime crates can implement the same meaning without copying any one
  backend's API shape into `tina`.
- Live network tests prove correctness, not global determinism. Replay-grade
  determinism for I/O belongs to Voyager's simulator work, not to OS-backed
  socket tests.
