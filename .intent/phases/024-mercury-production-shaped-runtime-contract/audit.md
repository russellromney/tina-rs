# 024 Mercury Audit

Session:

- A (substrate seam audit)
- B (Tokio bridge/backend decision)

## Current Runtime Substrate Shape

`tina-runtime` has three important layers:

- `Runtime<S, F>` is the semantic engine. It owns isolate state, mailboxes,
  supervision state, trace, timers, and runtime-owned TCP resources. It is
  intentionally one-thread-owned.
- `IoBackend` is the current Betelgeuse-backed completion backend. Runtime
  calls submit `CallInput` values to the backend; `Runtime::step()` advances the
  backend and harvests `CallOutput` completions.
- `ThreadedRuntime` and `ThreadedMultiShardRuntime` are live runner shells.
  They construct `Runtime` on worker threads and drive `step()` for users.

This means the current live substrate is real but simple: workers step a
semantic runtime that still uses Betelgeuse's step-driven I/O loop.

## Monoio Facts Checked

`cargo info monoio` reports current crate `monoio = 0.2.4`, described as "A
thread per core runtime based on iouring." Its default features include
`iouring`, `legacy`, `macros`, `utils`, `bytes`, and `async-cancel`.

Local source inspection shows:

- `monoio::start::<Driver, _>(future)` and `monoio::Runtime::block_on(...)`
  own a current-thread async runtime.
- `monoio::net::TcpListener::bind(...)` is synchronous enough for bind setup.
- `TcpListener::accept()` is async and returns `(TcpStream, SocketAddr)`.
- `TcpStream` has async read/write traits and `local_addr()` / `peer_addr()`.
- Linux io_uring is behind the `iouring` path; non-Linux has a legacy driver
  path. A true io_uring proof likely needs Linux CI or a Linux-only test gate.

## Seam Assessment

Monoio can likely host Tina's substrate, but not by replacing handlers with
async code.

The clean integration is a new runner/backend layer:

- one monoio current-thread runtime per Tina shard worker
- Tina handler turns remain synchronous and effect-returning
- runtime-owned TCP calls become monoio tasks/futures owned by the worker
- completions are translated back into the existing `CallOutput` path
- `Runtime` remains the semantic engine and trace owner

The risky integration would be:

- make `Runtime::step()` itself async
- make `handle(...)` async
- move raw monoio sockets into isolate state
- use unbounded async channels as hidden completion queues

Those are Mercury pause-gate violations.

## Tokio Current-Thread Assessment

Tokio current-thread is not the purest Tina substrate, but it is the most
practical comparison/backend substrate for Mercury.

Useful facts for Mercury:

- Tokio has a current-thread scheduler that executes tasks on the current
  thread.
- Tokio can enable I/O and time drivers on that runtime.
- Tokio is the known Rust ecosystem substrate, so a comparison against Tokio is
  credible and easy for Rust users to understand.
- A Tina shard can be shaped as one OS thread, one Tokio current-thread
  runtime, and one Tina interpreter owned by that thread.

The clean integration is:

- user handlers stay synchronous and effect-returning
- Tina owns admission, mailbox capacity, supervision, trace, and completion
  routing
- Tokio owns socket/timer mechanics underneath runtime-owned Tina effects
- completions come back as ordinary Tina messages

The risky integration would be:

- expose `tokio::Handle` in Tina context
- accept arbitrary user futures as normal effects
- let user handlers become `async fn`
- hide unbounded Tokio/library queues behind an `Accepted` Tina outcome
- run the Tina shard on Tokio multi-thread and still claim thread-per-shard

Those are Mercury pause-gate violations.

## Smallest Monoio Spike

Smallest useful monoio spike:

1. Add a gated monoio substrate module or sibling crate skeleton.
2. Run one shard on one monoio current-thread runtime.
3. Support only the TCP call subset needed by TCP echo:
   bind, accept, read, write, close.
4. Keep timer behavior on existing runtime clock for the spike unless monoio
   timers are required by the shape.
5. Prove TCP echo final bytes match the explicit-step oracle and the existing
   threaded substrate.
6. Assert a shared trace subset: registration, handler start/end, call
   dispatch/completion, send accepted/rejected, stop.

If this cannot be done without async handlers or unbounded queues, defer monoio
and continue Mercury on the threaded+Betelgeuse substrate with that decision
written down.

## Smallest Tokio Current-Thread Proof

Smallest useful Tokio proof:

1. Add a narrow backend module or runner path for Tokio current-thread.
2. Run one Tina shard on one OS thread that owns the Tokio runtime.
3. Support only TCP/time effects needed by the overload lab.
4. Keep isolate handlers sync and keep backend handles out of user context.
5. Prove the same Tina workload reaches the same accepted/rejected/timeout
   outcomes as the semantic oracle for a bounded scenario.
6. Add naive Tokio and hardened Tokio comparison programs/tests for the same
   overload shape.

## Current Design Debt Found

1. **I/O backend is concrete.**
   `Runtime` directly owns `IoBackend`; there is no `IoBackend` trait or
   backend parameter. A monoio spike may need a small backend abstraction or a
   separate runner that owns async I/O and injects completions.

2. **Completion delivery is already well-shaped.**
   The existing `dispatch_call` / `deliver_completion` path can accept
   completions from another backend without changing isolate semantics.

3. **Live cross-shard sendability is root-biased.**
   Huygens added sendable erasure for live cross-shard root handlers.
   Spawned child cross-shard send still needs proof/support or an explicit
   guard. Mercury must not let this stay implicit.

4. **Observed backpressure is trace-first today.**
   Runtime knows `Full` / `Closed` when it dispatches sends, but the sender
   isolate gets no normal message unless Mercury adds an observed-send path.

## Recommendation

Do not start Mercury with a full `tina-runtime-monoio` crate.

Start with the Tina semantics and overload lab:

1. observed send outcome
2. mandatory-timeout isolate call
3. semantic overload lab through runtime/simulator/live threaded runner
4. runner lifecycle, live cross-shard child behavior, and live supervision
5. narrow Tokio current-thread TCP/time backend for the overload lab
6. naive Tokio and hardened Tokio comparison variants

Keep monoio as the likely native thread-per-core backend candidate after this
proof. The Mercury demo needs a known substrate and a fair Tokio comparison
more than it needs the perfect native backend on day one.
