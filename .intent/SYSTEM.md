# tina-rs SYSTEM

This file says what `tina-rs` is supposed to be.

It is here to protect the shape of the system while the code changes fast. If a
change breaks one of these ideas, we should notice and decide on purpose, not
by accident.

## What tina-rs is

`tina-rs` is a way to build servers out of many small state machines.

The big ideas are:

- each isolate is one small state machine with its own state
- handlers are synchronous and return effects instead of doing I/O themselves
- mailboxes are bounded, and backpressure is visible
- runtimes do the scheduling and run the effects
- replay and simulation matter from the start, not as an afterthought

`tina-rs` is not a new scheduler. It is not trying to replace Tokio or
monoio. It is a small set of crates that sit on top of existing runtimes.

## Where this came from

`tina-rs` is a Rust port of Peter Mbanugo's Tina project in Odin.

The original Tina repo is:

- [pmbanugo/tina](https://github.com/pmbanugo/tina)

The main blog post behind the project is:

- [The Tokio/Rayon Trap and Why Async/Await Fails Concurrency](https://pmbanugo.me/blog/why-async-await-complect-concurrency)

We are trying to carry over the shape and discipline of Tina, not copy every
detail one-for-one.

## Current shipped surface

Today the repo ships `tina`, `tina-mailbox-spsc`, `tina-supervisor`,
`tina-runtime`, and `tina-sim`.

`tina` provides the shared words and types, including supervision policy types.

`tina-supervisor` provides supervisor configuration vocabulary. Runtime-owned
supervision state and restart execution stay in runtime crates.

`tina-runtime` is still a small, in-progress runtime. Today it has
deterministic runtime events, a single-shard stepping model, local
same-shard send dispatch, local same-shard spawn dispatch, typed runtime
ingress for sending to registered isolates, stored direct parent-child lineage
for spawned children, restartable child records, and direct-child
`RestartChildren` execution. It can also apply configured supervisor policy and
runtime-lifetime budget state when a direct child handler panics.

`tina` now also names a runtime-owned call effect family
(`Effect::Call(I::Call)` plus the `Isolate::Call` associated type). The trait
crate stays substrate-neutral: concrete request and result vocabulary live
in runtime crates. Completion is delivered as an ordinary later `Message`
via a translator carried inside the call payload, never as a second public
handler entry point.

`tina` also ships an ordered batch effect,
`Effect::Batch(Vec<Effect<I>>)`, for sequencing existing effects
left-to-right without opening up a new callback or effect language. In
`tina-runtime`, later effects in the batch still run after earlier
non-terminal effects, and `Stop` short-circuits the rest of the batch.

`tina-runtime` ships the first TCP call family on Betelgeuse
(nightly Rust): TCP listener bind, accept, stream read, stream write,
listener and stream close. Resources are runtime-owned opaque ids; raw
sockets never escape into isolate state. `step()` stays synchronous: the
runtime drives the Betelgeuse loop forward at the start of each step,
harvests completed operations from caller-owned typed completion slots,
runs the per-call translator, and enqueues the resulting `Message` in the
requesting isolate's mailbox. Synchronous Betelgeuse ops (bind, close)
complete inline during dispatch; async ops (accept, recv, send) stay in a
pending list until their slot has a result.

`tina-runtime` now also ships the first runtime-owned time call
verb: `CallInput::Sleep { after }` with `CallOutput::TimerFired`. The
runtime owns a monotonic clock and samples it once at the start of each
`step()`; timers due at or before that sampled instant are harvested and
delivered as ordinary later `Message` values through the same call
translator path. Equal-deadline timers wake in deterministic request
order. A crate-private manual clock seam lets tests prove timer semantics
directly without brittle wall-clock sleeps. A separate assertion-backed
integration test drives the public runtime path with the shipped monotonic
clock and proves a real "fail, back off, retry, succeed" workload.

`ChildDefinition` and `RestartableChildDefinition` now carry an optional bootstrap
message that the runtime delivers to the new child immediately after spawn
(and after each restart, for restartable specs). This is what lets a
listener isolate spawn a connection-handler child with its initial
`Start` kick without forcing the test harness to peek at trace events.

The repo now also has an assertion-backed task-dispatcher proof workload and a
matching runnable example. They show the current reference shape for supervised
work: clients send tasks to a dispatcher isolate, the dispatcher asks a small
registry isolate for the current worker address, and later work can continue
through replacement workers after a panic.

A live TCP echo proof and matching runnable example exist for the call
effect path: a listener isolate is the supervised parent that issues bind,
accept, and listener-close calls, and each accepted connection becomes a
restartable connection-handler child that issues read / write / close
calls. The listener captures its own typed address, uses
`Effect::Batch(Vec<Effect<I>>)` to sequence "spawn child, then send self a
re-arm or close message," and spawns the connection child via
`RestartableChildDefinition::with_initial_message`, so the connection's first read
fires through the normal handler pipeline rather than via test-harness
trace introspection. The proof binds to `127.0.0.1:0`, relies on the
runtime to report the actual bound address, and now covers one-client
smoke, sequential multi-client handling, bounded overlap, graceful
listener close, and listener stop. The connection isolate honors partial
writes through a small pending-buffer + drain pattern, exercised by a
separate unit test, and crate-local runtime tests directly prove that two
accepted stream reads can be pending in `IoBackend` at the same time.

The runtime trace is a deterministically ordered causal tree. Each event has at
most one cause, but one event may be the direct cause of many later events.

`Address<M>` names one isolate incarnation, not a logical service name. Its
identity includes shard id, isolate id, and generation. Runtime sends and
runtime ingress reject stale known generations as closed instead of silently
delivering to a current incarnation.

In `tina-runtime`, an accepted message does not disappear silently. It
is either handled by an isolate or recorded in the trace as abandoned if the
isolate stops first.

If a handler unwinds with a panic in `tina-runtime`, that panic becomes
a runtime event. The isolate is stopped and traced instead of tearing down the
whole round.

Some runtime proofs live in crate-local unit tests when the thing being proved
is internal runtime state that should not become public API yet.

Generated runtime-property tests also exist for small dispatcher workloads.
Those tests do not just run live histories twice; they also replay the runtime
trace and prove the trace can recover the same worker completions and restart
outcomes that the live workload observed.

There is not yet a Tokio bridge. The runtime-owned call contract is
substrate-neutral by design so `tina-sim` can implement the same
vocabulary against virtual time and replay without redefining `tina`.

`tina-sim` is intentionally still narrower than the live runtime. Today
it is a single-shard virtual-time simulator for the shipped
`Sleep { after }` / `TimerFired` call contract plus the simplest seeded
perturbation layer over simulator-owned surfaces:

- local-send delivery may be held back by one additional delivery round
- timer wake delivery may be delayed in virtual time

The simulator reuses the live runtime event vocabulary, captures replay
artifacts containing config/event-record/final-virtual-time and optional
checker failure, and now supports a small checker surface that can halt a
run with a structured reason. Proof workloads cover both timer-driven
retry/backoff under seeded timer perturbation and a deliberate-bug
harness under seeded local-send perturbation. Replay artifacts reproduce
against the same workload binary and simulator version; they do not serialize
arbitrary isolate values, spawn factories, or bootstrap closures.

`tina-sim` also executes the single-shard spawn and supervision surface that
Mariner already shipped live: `ChildDefinition`, `RestartableChildDefinition`,
runtime-owned parent-child lineage, restartable child records, direct-child
`RestartChildren`, bootstrap re-delivery after restart, and supervised panic
restart using `SupervisorConfig` policy/budget state. The proof surface covers
same-step spawn ordering, all shipped restart policies, non-restartable skip
events, stale-address send rejection as `Closed`, budget exhaustion,
multi-restart replay, and direct-child-only restart scope. Spawn/restart paths
compose additively with 017's seeded fault surfaces, but 018 does not add new
spawn/restart-specific perturbation.

`tina-sim` now also executes the shipped single-shard TCP call surface through
scripted virtual listeners and streams: bind, accept, read, write, listener
close, and stream close. Bind/close still complete inline during dispatch;
accept/read/write stay pending and complete on later simulator steps through
the same translated-message path as the live runtime. Scripted listeners report
deterministic `local_addr`, accepted streams report deterministic `peer_addr`,
partial reads/writes remain visible through the live `CallOutput` shapes, and
virtual listener queues, peer read buffers, peer output buffers, and pending
TCP completion queues are explicitly bounded by simulator config rather than
hidden unbounded buffering. The proof surface now covers one-client echo,
overlap echo, invalid-resource failures, mailbox-full and stopped-requester
completion rejection, listener-close cancellation, same-config replay of peer
output, and checker-backed replay of seeded TCP completion reordering. Replay
artifacts still reproduce against the same workload binary and simulator
version; they do not serialize arbitrary isolate values, TCP payload scripts,
spawn factories, or bootstrap closures. Multi-shard semantics remain out of
scope.

## Crate boundaries that must not drift

- `tina` owns the shared words of the system and small shared policy types.
- `tina` should not quietly grow runtime helpers, scheduler helpers, or queue
  internals.
- mailbox behavior belongs in mailbox crates such as `tina-mailbox-spsc`.
- runtime scheduling, polling, effect execution, and runtime event traces
  belong in runtime crates.
- supervision policy types may live in `tina`, but the actual supervision
  mechanism does not ship before there is a runtime to host it.
- the simulator must use the real runtime event model. It must not make up a
  second visible model with different rules.

## Isolate and effect model

- an isolate handles one message at a time
- handlers change local state and return an `Effect`
- the effect language is intentionally closed at the trait boundary
- the dispatcher is the only place real I/O happens
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
- dynamically sized payloads, if supported, travel behind owning pointers; the
  ring stores fixed-size slot values, not inline DST payloads

## Committed future constraints

- the first runtime introduces the deterministic runtime event trace with
  causal links
- that trace is part of the design, not optional test decoration
- trace consumers must not assume the trace is a linear chain; restart execution
  can produce multiple direct consequences from one cause
- the simulator uses the runtime trace vocabulary as its meaning model
- the Tokio bridge is for small, gradual adoption inside existing Tokio apps
- the Tokio bridge is not the main runtime story, and it is not a reason to
  copy every Tokio pattern into tina
- cross-shard rules must be written down before multi-shard runtime work starts
- cross-shard behavior must not be guessed from code after the fact

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
