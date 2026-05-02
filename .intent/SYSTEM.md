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
Threaded runtime substrates must preserve those rules instead of becoming a
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
`tina-runtime`, and `tina-sim`.

`tina` owns the shared words: `Isolate`, `Address`, `Context`, `Effect`,
`Outbound`, child definitions, and supervision policy types. It does not pick
a runtime backend. Concrete runtime calls live in runtime crates.

The normal user path is the prelude:

- `tina::prelude::*`
- helpers such as `send`, `reply`, `spawn`, `stop`, `batch`, and `noop`
- context helpers such as `ctx.me()` and `ctx.send_self(...)`
- typed runtime calls such as `sleep(...).reply(...)`
- `tina::isolate_types! { ... }` when it removes associated-type boilerplate
  without hiding meaning

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
tracing, time calls, and Betelgeuse-backed TCP calls. Its `step()` is
synchronous from the outside: the runtime collects finished owned work,
translates completions into messages, and then handles ready mailbox work.

`tina-runtime` also has a narrow live substrate, `ThreadedRuntime`. It starts
one OS worker thread for one shard runtime, keeps root registration and ingress
behind a bounded command queue, and lets tests/users run isolates without
manually calling `step()`. This is an execution path, not a second semantic
model: the explicit-step runtime and simulator remain the oracle. The current
threaded substrate is single-shard; live cross-shard transport is not yet
claimed.

The shipped runtime call types are `RuntimeCall<Message>` over
`CallInput`, `CallOutput`, and `CallError`. Today it covers runtime-owned sleep
and TCP listener/stream operations. Sockets are runtime-owned opaque ids; raw
sockets do not live in isolate state.

The runtime trace is a deterministically ordered causal tree. Each event has at
most one cause, but one event may directly cause many later events. Trace
consumers must not flatten this into a single causal chain.

`Address<M>` names one isolate incarnation, not a logical service name. Its
identity includes shard id, isolate id, and generation. Runtime sends and
outside sends into the runtime reject stale known generations as closed instead
of silently delivering to a newer incarnation.

`tina-sim` is the deterministic simulator for the same model. It uses virtual
time, scripted TCP resources, seeded delays/reordering, replay records, and
checker failures, while keeping the live runtime event types. Replay records
reproduce against the same workload binary and simulator version; they do not
serialize arbitrary isolate values, spawn factories, bootstrap closures, or TCP
scripts.

The simulator can move simulator-owned events without changing the model:
local-send delivery, timer wake delivery, and TCP completion order can shift in
controlled seeded ways. These shifts are there to find ordering bugs, not to
create a second meaning for the program.

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

There is not yet a Tokio bridge, and the bridge is not the main runtime story.
The runtime call types do not pick a backend, so the simulator, the current
runtime, the threaded substrate, and later runtimes can share one meaning
model.

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
- the current runtime and simulator do not have a broad allocation-free hot-path
  claim; boxed erasure, traces, replay records, and coordinator storage may
  allocate
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
