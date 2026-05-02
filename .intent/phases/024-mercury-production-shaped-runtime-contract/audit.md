# 024 Mercury Audit

Session:

- A (substrate seam audit)

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

## Smallest Spike

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

Do not start with a full `tina-runtime-monoio` crate.

Start with one narrow monoio spike behind a feature or experimental module. If
the spike proves the seam, promote to a sibling crate later. If it fights the
model, keep threaded+Betelgeuse as Mercury's tryable substrate and spend the
phase on user-visible backpressure, timeout call, live supervision, spawned
child cross-shard proof, and dogfood workload.
