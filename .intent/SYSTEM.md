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

Today the repo ships `tina`, `tina-mailbox-spsc`, and `tina-runtime-current`.

`tina` provides the shared words and types, including supervision policy types.

`tina-runtime-current` is still a small, in-progress runtime. Today it has
deterministic runtime events, a single-shard stepping model, local
same-shard send dispatch, local same-shard spawn dispatch, typed runtime
ingress for sending to registered isolates, stored direct parent-child lineage
for spawned children, restartable child records, and direct-child
`RestartChildren` execution.

The runtime trace is a deterministically ordered causal tree. Each event has at
most one cause, but one event may be the direct cause of many later events.

`Address<M>` names one isolate incarnation, not a logical service name. Its
identity includes shard id, isolate id, and generation. Runtime sends and
runtime ingress reject stale known generations as closed instead of silently
delivering to a current incarnation.

In `tina-runtime-current`, an accepted message does not disappear silently. It
is either handled by an isolate or recorded in the trace as abandoned if the
isolate stops first.

If a handler unwinds with a panic in `tina-runtime-current`, that panic becomes
a runtime event. The isolate is stopped and traced instead of tearing down the
whole round.

Some runtime proofs live in crate-local unit tests when the thing being proved
is internal runtime state that should not become public API yet.

There is not yet a simulator or Tokio bridge.

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
