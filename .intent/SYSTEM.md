# tina-rs SYSTEM

This file says what `tina-rs` is supposed to be.

It is here to protect the shape of the system while the code changes fast. If a
change breaks one of these ideas, we should notice and decide on purpose, not
by accident.

## What tina-rs is

`tina-rs` is Tina for Rust: bounded, shared-nothing concurrency.

The big ideas are:

- application logic is written as small synchronous state machines called
  isolates
- each isolate owns its state and handles one message at a time
- handlers return effects instead of doing I/O themselves
- mailboxes are bounded, and backpressure is visible
- shards own isolate execution, runtime-owned resources, timers, and
  cross-shard queues
- runtimes schedule isolates and interpret effects
- replay and simulation matter from the start, not as an afterthought

`tina-rs` is not trying to replace Tokio or monoio wholesale. It is the Tina
rule set as Rust crates: isolate state, explicit effects, bounded queues,
supervision, deterministic simulation, and shard-owned runtime execution.
Live runtime substrates must preserve those rules instead of becoming a
generic async scheduler.

## Where this came from

`tina-rs` is a Rust port of Peter Mbanugo's Tina project in Odin.

The original Tina repo is:

- [pmbanugo/tina](https://github.com/pmbanugo/tina)

The main blog post behind the project is:

- [The Tokio/Rayon Trap and Why Async/Await Fails Concurrency](https://pmbanugo.me/blog/why-async-await-complect-concurrency)

We are trying to carry over Tina's shape, not copy every detail one-for-one.

## What ships now

Today the repo ships `tina`, `tina-mailbox-spsc`, `tina-supervisor`,
`tina-runtime`, `tina-sim`, and `tina-tokio-bridge`.

`tina` owns the shared words: `Isolate`, `Address`, `Context`, `Effect`,
`Outbound`, child definitions, and supervision policy types. It does not pick
a runtime backend. Concrete runtime calls live in runtime crates.

The normal user path is the prelude:

- `tina::prelude::*`
- helpers such as `send`, `reply`, `spawn`, `stop`, `batch`, and `noop`
- context helpers such as `ctx.me()` and `ctx.send_self(...)`
- typed runtime calls such as `sleep(...).reply(...)`
- `#[tina::isolate(...)]` or `#[tina_runtime::isolate(...)]` for ordinary
  isolate impls; `tina::isolate_types! { ... }` remains the explicit low-level
  escape hatch

Low-level constructors still exist for tests and advanced consumers, but they
are not the teaching path.

`Effect` is the closed language between isolate code and a runtime:

- `Send` delivers an `Outbound<M>` to another addressed isolate.
- `Reply` returns a value to the outside caller.
- `Spawn` creates a child isolate from a `ChildDefinition` or
  `RestartableChildDefinition`.
- `Call` asks the runtime to own an operation and later translate its
  `CallOutput` into an ordinary message.
- `Batch` sequences effects left-to-right; `Stop` short-circuits the rest.
- `Stop` ends the current isolate incarnation.

`ChildDefinition` describes one child incarnation. `RestartableChildDefinition`
describes how to make a fresh incarnation after restart. Both may carry an
initial message through `with_initial_message`, delivered after spawn and after
each restart for restartable children.

`tina-runtime` is the explicit-step runtime. It handles same-shard sends,
same-shard child spawn, supervision, restart, sends into the runtime from
outside, bounded mailboxes, stale generation rejection, panic capture, abandon
tracing, time calls, runtime-owned TCP/UDP/DNS/TLS/file/path/process/signal
calls, and local snapshot/journal persistence calls. Its `step()` is
synchronous from the outside: the runtime collects finished owned work,
translates completions into messages, and then handles ready mailbox work.

`tina-runtime` also has a narrow live substrate: `ThreadedRuntime` for
one shard and `ThreadedMultiShardRuntime` for a fixed shard set. Each
shard runtime is constructed and owned by one OS worker thread; handles
communicate through bounded command queues. Live cross-shard sends move
`Send + 'static` payloads through bounded worker queues, and live cross-shard
isolate calls route typed reply/full/closed/timeout outcomes back to the
requester shard. This is an execution path, not a second semantic model: the
explicit-step runtime and simulator remain the oracle. Peer quarantine,
cross-shard child ownership, shard restart propagation, hard OS thread
pinning, and network remoting are not claimed.

`LocalSystem` and `LocalMultiShardSystem` are the preferred local live app
owners. They wrap the live substrate with a public bounded-shape config,
topology/capability reports, partial/complete trace APIs, and terminal
shutdown reports. They are not a separate runtime.

The vendored Betelgeuse backend has a completion-driven native I/O shape that
fits Tina, plus a narrow deterministic simulated TCP backend used for
substrate proof. `tina-sim` remains the broad semantic oracle; the Betelgeuse
simulated backend proves the same runtime-owned TCP effects can run through a
seeded, step-driven substrate without OS sockets. Broader live-substrate
liveness faults are not claimed.

The shipped runtime call types are `RuntimeCall<Message>` over
`CallInput`, `CallOutput`, and `CallError`. Today it covers runtime-owned sleep,
TCP listener/stream/client-connect operations, UDP sockets, DNS lookup, native
TLS client/server operations, local file/path operations, bounded process runs,
runtime shutdown notification, Unix signal capture, and local snapshot/journal
persistence. Runtime-owned sockets, TLS streams, files, and related resources
are opaque ids; raw OS handles do not live in isolate state.

If a handler was invoked by an isolate call, later messages produced by
runtime-owned calls or observed-send completions preserve that original call
context. This lets a service receive a request, perform runtime-owned I/O or
persistence, and reply afterward without the reply becoming trace-only noise.
Timeout, full, closed, requester-closed, and mailbox-full paths remain visible.
The portable service harness and simulator DST directly prove both continuation
families: backend/persistence completions and observed-send completions,
including observed-send `Full`.

Persistence is deliberately local and domain-level. `snapshot_commit`,
`snapshot_load`, `journal_append`, and `journal_replay` persist user-provided
bytes with Tina metadata around them. Snapshot metadata includes
`last_journal_index`; journal records include monotonic `record_index`.
Append-before-apply is the intended state rule: user state mutates only after
the durable append succeeds. Truncated journal tails are visible; complete
checksum failures, duplicate indexes, and out-of-order indexes are corrupt
records. If snapshot rename succeeds but the final durability step cannot be
proven, the result is `CallError::CommitUncertain`; consumers must recover from
disk before assuming either old or new snapshot state. This is not a database,
durable mailbox, durable work queue, or exactly-once system.

Persistence support is named by `LOCAL_PERSISTENCE_SUPPORT`, including
temp-write, rename, file fsync, parent-directory fsync, truncated-tail warning,
and checksum validation. Unsupported platform durability strength must remain
visible as `NotClaimed`, not upgraded in prose.

Persistence calls are runtime-owned from the isolate's point of view and live
snapshot/journal helper execution runs through a bounded storage lane instead
of synchronously inside the shard worker. Storage-lane admission can fail
visibly as `CallError::StorageFull` or `CallError::StorageClosed`, and canceled
queued storage work must not start after cancellation. Already-started local
filesystem work still cannot be preempted by Tina; do not claim a full
nonblocking storage reactor, durable mailbox, durable work queue, or
exactly-once semantics.

The explicit-step runtime remains the semantic oracle: its snapshot/journal
helpers complete inline when the driver is stepped directly. The bounded
storage worker lane belongs to the preferred live `ThreadedRuntime` and
live `ThreadedMultiShardRuntime` paths.

Storage lane capacity means total accepted pending storage work, not only
buffered channel slots. Running work counts against capacity until its
completion is harvested or canceled. This keeps `StorageFull` deterministic
under fast worker scheduling.

Runtime resource capability reports are part of the Tina contract. If a
resource family is supported, unsupported, simulator-only, adapter-only,
lane-backed, poll-backed, completion-backed, cancelable, tombstoned, or
shutdown-limited, that shape must be visible in `RuntimeCapabilities` rather
than implied by docs or hidden behind an adapter.

Live UDP is a Tina-owned nonblocking driver rail over runtime-owned
`UdpSocketId`s. It is `PollBacked`, not Betelgeuse-backed. Receive lanes are
per socket; duplicate pending receives and closes during a pending receive
surface `ResourceBusy`. Datagram truncation is visible.

Live DNS is a Tina-owned bounded blocking driver rail. DNS lane capacity is
visible, queued work is cancelable, and already-started resolver work is
tombstoned on timeout/cancel because standard OS resolver calls are not
preempted by Tina. Do not claim hidden unbounded resolver queues or preemptive
DNS cancellation.

Native TLS is a layer over the runtime's own Betelgeuse TCP rail, not a separate
blocking-socket subsystem: the runtime owns a rustls connection (sans-I/O) per
runtime-owned `TlsStreamId` and drives the handshake/read/write/close state
machine on the shard thread as Betelgeuse harvests TCP completions — no worker
thread, no second socket stack, so a Tina TLS client and server can share one
runtime. The TLS layer owns the underlying TCP socket exclusively and serializes
its own internal socket ops (at most one read *or* write in flight), so a single
`tls_*` call can interleave reads and writes without tripping the rail's
one-pending-op rule. It uses real TLS semantics for cert validation, SNI/name
check, and DER-root policy, distinguishes a clean `close_notify` from
truncation, reports certificate/name/handshake/I/O/full/closed/timeout outcomes,
and enforces one pending TLS operation per TLS stream at the isolate boundary.
`tls_lane_capacity` bounds the shard-total count of in-flight TLS ops. Handshake
asymmetric crypto runs on the shard thread (an accepted tradeoff: visible and
boundable by accept rate). Simulator TLS is unchanged — semantic scripted I/O,
not cryptography.

Runtime shutdown notification is a Tina-owned signal rail: shutdown can deliver
a bounded `"shutdown"` notification into waiting isolates before the worker
stops. On Unix, raw `SIGINT`/`SIGTERM` capture is installed through
`signal-hook` and delivered through the same bounded signal rail. It is not a
Tokio signal task, async handler, custom unsafe handler, or broad
process-supervision policy.

Richer local filesystem/path operations are runtime-owned calls with typed
outcomes: metadata, rename-replace, remove-file, read-dir, and parent sync.
Platform durability and replacement differences must remain visible as
supported, unsupported, uncertain, or I/O failure instead of being papered over.

Bounded local process execution is a lane-backed runtime call. It uses
command-plus-args, null stdin, bounded stdout/stderr capture, timeout
kill/reap, and visible `ProcessFull`, `ProcessClosed`, `Timeout`, and
`KillUncertain` outcomes. It is not shell-by-default, interactive process I/O,
or a process-tree semantics claim.

`LocalSystemTerminalReport::summary()` is trace-derived terminal accounting. It
may count completed, failed, rejected, abandoned, journaled, and recovered work
that Tina can see in the final trace. It must not grow hidden metrics channels
that disagree with trace truth.

`make verify` is the single project gate. It runs format, check, workspace
tests, loom, docs, clippy, the executable capability matrix, the canonical
portable service harness, service-level DST, bridge cancellation checks, and the
local cost-smoke command. Cost rows include small live Tina paths for ingress,
local send, cross-shard send, isolate call, and local TCP loopback, but remain
smoke evidence unless a later benchmark phase adds policy and thresholds.

The runtime trace is a deterministically ordered causal tree. Each event has at
most one cause, but one event may directly cause many later events. Trace
consumers must not flatten this into a single causal chain.

`Address<M>` names one isolate incarnation, not a logical service name. Its
identity includes shard id, isolate id, and generation. Runtime sends and
outside sends into the runtime reject stale known generations as closed instead
of silently delivering to a newer incarnation.

`tina-sim` is the deterministic simulator for the same model. It uses virtual
time, scripted TCP resources, deterministic durable images, seeded
delays/reordering, replay records, and checker failures, while keeping the live
runtime event types. Replay records reproduce against the same workload binary
and simulator version; they do not serialize arbitrary isolate values, spawn
factories, bootstrap closures, or TCP scripts.

The simulator can move simulator-owned events without changing the model:
local-send delivery, timer wake delivery, and TCP completion order can shift in
controlled seeded ways. These shifts are there to find ordering bugs, not to
create a second meaning for the program.

`tina_sim::dst` is the reusable deterministic-simulation-testing surface.
Generated histories are data: they must carry enough seed/history information
to replay the same run. DST failures should shrink by deleting irrelevant
operations, or the test must explain why shrinking does not apply. Common trace
invariants live in one place so tests do not silently copy weaker checkers.
Simulator storage faults are simulator-only durable-image faults; they are not
native filesystem crash-consistency claims. Live-vs-sim differential tests
compare semantic projections rather than raw trace identity unless a test
explicitly claims byte-identical replay.

`tina-runtime` and `tina-sim` now both expose multi-shard runners. A
multi-shard runner is still an explicit-step model, not real parallel shard
execution. It owns several shard-local runtimes/simulators, routes roots by
shard id, preserves a globally monotonic event id source, and uses bounded
shard-pair queues for cross-shard sends.

Cross-shard sends have two visible stages. Source-time dispatch says whether
the sender could enter the shard-pair queue at all, including queue `Full`.
Destination-time delivery says what happened when the target shard tried to
deliver the message, including unknown isolate, closed generation, or target
mailbox `Full`. Remote messages become visible on the next global step, not
the same step that sent them.

Supervision remains owned by the parent isolate's shard. Multi-shard
runners may route root `supervise` config to the owning shard, but
they do not invent cross-shard child ownership. Children spawned by a parent
belong to the parent's shard, and restart policy applies to direct children on
that shard. Once an isolate is placed, its shard ownership stays stable for
that incarnation.

The current explicit-step multi-shard runner has no peer-unavailable signal.
Address-local remote failures stay address-local: an unknown, stopped, stale,
or full remote target does not poison the whole destination shard. There is no
shard-down, peer-down, shard-restarted, or peer-restarted event vocabulary yet.
Peer quarantine and shard-restart rules are still later design work.

There is a narrow Tokio/Tower bridge, but the bridge is not the main runtime
story. Tokio owns the edge; Tina owns isolate state. The runtime call types do
not pick a backend, so the simulator, the current runtime, the Betelgeuse
substrate, and later runtimes can share one meaning model.

## Crate boundaries that must not drift

- `tina` owns the shared words of the system and small shared policy types.
- `tina` should not quietly grow runtime helpers, scheduler helpers, or queue
  internals.
- mailbox behavior belongs in mailbox crates such as `tina-mailbox-spsc`.
- runtime scheduling, polling, effect execution, and runtime event traces
  belong in runtime crates.
- supervision policy/config types may live in `tina` and
  `tina-supervisor`, but runtime-owned supervision state and restart execution
  belong in runtime crates.
- the simulator must use the real runtime event model. It must not make up a
  second visible model with different rules.

## Isolate and effect model

- an isolate handles one message at a time
- handlers change local state and return an `Effect`
- the effect language is intentionally closed at the trait boundary
- runtime-owned calls are the only place real I/O happens
- if runtime code quietly moves I/O into handlers, helper traits, or test-only
  shims, that is a design break

## Mailbox model

- mailboxes are bounded
- backpressure is explicit through `Full` and `Closed`
- mailboxes do not hide overflow in a secret fallback queue
- the SPSC mailbox has a one-producer, one-consumer contract
- if code breaks that SPSC contract, it may panic; it must not silently turn
  into MPSC or MPMC behavior
- the hot SPSC path is meant to avoid per-message allocation after warm-up for
  fixed-size payloads
- claims about allocation behavior must stay narrow and be backed by evidence
- the current runtime and simulator do not have a broad allocation-free
  framework claim; boxed erasure, call translators, trace storage, replay
  records, completion slots, and coordinator storage may allocate
- selected runtime hot paths are pinned numerically by allocation tests; those
  numbers are cost-model evidence, not marketing benchmarks
- optimizations must not disable trace/replay, hide bounded pressure, or move
  work into background queues to make allocation counts look better
- reusable scratch buffers and preallocation are allowed when tests prove the
  warmed path and semantics still hold, including entry counts above tiny
  defaults
- dynamically sized payloads, if supported, travel behind owning pointers; the
  ring stores fixed-size slot values, not inline DST payloads

## Shard model

- a shard is an ownership boundary
- current multi-shard runners are explicit-step models over many shards,
  not real parallel execution
- cross-shard queues are bounded and visible
- source-time queue entry and destination-time delivery are separate
  stages
- cross-shard send payloads are moved into erased runtime storage at the
  effect boundary, then moved through the shard-pair queue into the
  destination mailbox; the core transport does not require user-message
  cloning
- the current explicit-step coordinators store cross-shard work in one bounded
  `VecDeque` per source/destination shard pair; no hidden unbounded overflow
  queue is part of the model
- remote messages become visible on the next global step
- event ids are globally monotonic across shards in a multi-shard run
- supervision is owned by the parent's shard
- address-local remote failures do not become shard-liveness facts
- full peer-quarantine and shard-restart rules are a later design step,
  not something to quietly smuggle into the current multi-shard model

## Bridge posture

- the Tokio bridge is for small, gradual adoption inside existing Tokio apps
- the Tokio bridge is not the main runtime story
- bridge work must not copy Tokio patterns into tina when they fight the
  isolate/effect model
- real parallel shard execution belongs to later runtime backend work, not
  to the current explicit-step runtime

## Things that should feel wrong

These are warning signs:

- `tina` grows helper APIs only to make runtime tests easier
- runtime code depends on simulator-only ideas
- the simulator adds events the runtime does not emit
- proof claims get bigger than the evidence
- examples become the only proof that something works
- the bridge starts being treated like the main runtime story

If a change needs one of those moves, the intent should change first and the
code should change second.
