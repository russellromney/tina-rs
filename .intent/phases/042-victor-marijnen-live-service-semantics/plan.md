# Phase 042: Victor Marijnen Live Service Semantics

## Goal

Make Tina's live local runtime behave like a real service substrate.

Jan de Quay gave Tina local I/O rocks: DNS, TLS client, paths, UDP, process,
files, persistence, TCP, timers, and shutdown notification. Victor Marijnen is
about what happens when those rocks run together under pressure.

At closeout, Tina should be able to say:

> A local multi-shard Tina service can accept bounded traffic, call across
> shards, own and clean up runtime resources, shed load, drain or cancel during
> shutdown, and report health/config truth without hidden queues.

This is core runtime work. It is not flow syntax, docs polish, or release
story. Barend flow ergonomics waits until these rocks are boring.

## Why Now

The missing parts are service semantics, not more I/O vocabulary:

- live cross-shard isolate calls still do not round-trip replies;
- runtime resources exist, but ownership/cleanup is not first-class enough;
- shutdown notification exists, but graceful drain is not a full service mode;
- capacities exist in many places, but users need one manifest and one health
  picture;
- Tina can make outbound TLS calls, but cannot host inbound TLS services;
- shard failure is visible, but the after-failure contract is too thin.

## Non-Goals

No flow macro, alternate handler DSL, remoting, clustering, membership,
placement, durable mailbox, exactly-once claim, broad performance claim,
Tower/Axum-inside-Tina claim, new async runtime, hidden unbounded queue, or
silent fallback.

## Core Rules

- This phase is local-process only: one Tina system, many shard workers.
- Cross-shard sends and calls use bounded shard-pair paths.
- Requester shard owns pending call state until exactly one terminal outcome.
- Destination shards never own requester liveness.
- Reply transport carries completion data only.
- Timeout is owned by the requester shard.
- Late replies reject and trace; they do not become success.
- Every accepted operation reaches a terminal outcome: completed, failed,
  rejected, canceled, abandoned, or tombstoned.
- Every overload point is visible: mailbox, ingress, remote queue, DNS, TLS,
  storage, process, signal, listener, stream, file, child, trace.
- Runtime resources have owners, and owner stop/panic/restart has a tested
  cleanup rule.
- Shutdown is a mode: stop accepting, drain until deadline, cancel leftovers,
  report what happened.
- Health summarizes trace/counter truth. It is not a hidden correctness oracle.
- No broker, registry, service locator, remoting rail, or second call API.

## Build Order

Work in this order. Review code bugs after each rock.

1. Audit current live service semantics in `review.md`.
2. Add `LocalSystemConfig`, runtime health, and shutdown report.
3. Add resource ownership cleanup and graceful service drain.
4. Add live cross-shard isolate calls.
5. Add inbound/server-side TLS.
6. Pin shard failure/quarantine behavior.
7. Add typed acceptor/service pattern.
8. Tighten normal call/reply type safety.
9. Add hard e2e and DST service tests.
10. Do positive, blast-radius, and hostile review. Fix findings.

## Rock 1: Audit

Write current facts in `review.md`. No separate audit file. Cover resource
handles/owners, capacities/defaults, pending work, stop/shutdown behavior, live
cross-shard call rejection, shard failure, call/reply type erasure and
downcast-panic paths, and public API blast radius.

## Rock 2: Runtime Config Manifest

Add preferred public config type `LocalSystemConfig`. It must name every bounded
live queue/resource family that exists at phase start, plus every bounded family
added in this phase:

- ingress queue;
- remote queues;
- trace retention;
- storage, DNS, TLS, process, and signal capacities;
- mailbox defaults where applicable;
- TCP, UDP, file, resource, and child limits where applicable.

Invalid zero or contradictory capacities reject before runtime start.

If a family is intentionally fixed or unsupported, show that in audit and
health. Existing scattered builders may adapt into this shape, but
`LocalSystemConfig` is the teaching path.

## Rock 3: Runtime Health And Reports

Add a read-only health snapshot with shard lifecycle state; ingress, remote, and
mailbox pressure where available; in-flight calls by resource family; owned
listener, stream, TLS, file, and UDP counts; tombstoned/late-completion counts;
failed/stopped/draining state; and trace dropped-event counters.

Consistency rule: per-shard fields are internally consistent. Whole-system
snapshots are best-effort across shards, with monotonic counters. Tests compare
health to trace-visible events and existing pressure counters within that model.

Shutdown terminal reports must include final state, clean-vs-deadline result,
canceled counts, tombstoned counts, rejected-after-drain counts, failed shard
ids, and remaining owned-resource counts.

## Rock 4: Resource Ownership Cleanup

Make ownership explicit for TCP listeners/streams, TLS streams, files, UDP
sockets, pending DNS/TLS/storage/process/signal/TCP/UDP calls, and child
isolates where relevant.

When an owner stops, panics, or restarts, owned resources close/cancel/tombstone
visibly, stale resource use rejects, unrelated owners are not harmed, and
quiescence cannot wait forever on leaked pending work.

Owner transfer must be explicit and tested: listener-to-child accepted streams,
raw stream-to-TLS after handshake, cleanup ownership for already-accepted
resources, failed transfer leaving one clear owner or visible close/reject,
pending accept cancel/tombstone on stop/drain/failure, and late accepted streams
after cancellation closing/rejecting visibly.

If a resource is intentionally runtime-global, name and test that rule.

## Rock 5: Graceful Service Drain

Add first-class shutdown drain.

States:

- `Running`: normal admission.
- `Draining`: external ingress rejects; listener accepts stop; already-accepted
  work may finish; owned cleanup effects may run; new cross-shard calls reject
  unless they are part of already-accepted cleanup.
- `Stopped`: no new work; leftovers canceled or tombstoned.
- `Failed`: same as stopped plus failed shard reason.

Drain must deliver shutdown notification, drain accepted work until deadline,
cancel/tombstone leftovers, reject new work with typed closed/shutdown outcome,
and return terminal report data.

Prove clean drain, timeout, pending I/O, requester stop, remote full, cleanup
work started by shutdown handler, late completion, and non-preemptable work not
hanging shutdown.

## Rock 6: Live Cross-Shard Isolate Calls

Implement bounded live cross-shard call replies, or pause before claiming local
multi-shard service completion.

Required outcomes:

- success;
- source request full;
- destination mailbox full;
- reply path full;
- target stale/closed/unknown;
- requester stopped/full at completion;
- timeout;
- source/destination/reply shard failed.

Trace request path, destination enqueue, reply path, timeout, and late-reply
rejection directly.

If this needs a call/reply representation change, do the minimal Rock 10
type-safety work first. Do not build cross-shard calls on a known-wrong type
shape.

Use existing `call(...).reply(...)`. No second cross-shard call API.

## Rock 7: Inbound TLS

Add the smallest honest server-side TLS rail: raw TCP accept, server handshake
with static cert/key, `TlsStreamId`, existing TLS read/write/close helpers, safe
per-stream pending-op ownership, typed cert/config/handshake/timeout/full/closed
errors, deterministic local cert tests, and simulator scripts that model
outcomes, not cryptography.

No client auth, ALPN/SNI routing, cert reload, or system trust-store policy.
Pause if this wants a large cert-policy framework. Do not fake secure hosting.

Failed handshake must close or return raw stream to one clear owner, terminate
the pending op, and leak no `TlsStreamId`.

## Rock 8: Shard Failure Contract

Pin live behavior after one shard fails:

- failed shard is quarantined;
- ingress to failed shard rejects;
- sends/calls to failed shard reject;
- healthy shards keep running;
- health and terminal report name the failed shard;
- no automatic shard restart unless this phase proves ownership semantics.

Pending work owned by a failed shard is canceled or tombstoned. In-flight
cross-shard request/reply messages involving that shard reject when observed.
Driver completions after failure become terminal trace, never delivery to a dead
owner.

Test same-shard work, cross-shard work, pending completions, and shutdown after
failure.

## Rock 9: Typed Acceptor / Service Pattern

Add a small safe pattern for bounded listener services: listener isolate owns
listener, accepted connection gets a bounded child, child owns stream/TLS
stream, overload rejects/closes visibly, shutdown stops accepting and
drains/cancels children, capacities are explicit.

This is not flow syntax. First prove service shape with normal Tina effects.
Helper/macro is optional closeout polish only if it expands to normal Tina
effects and keeps capacity, timeout, ownership, and failure visible.

## Rock 10: Call/Reply Type Safety

Reduce normal-path `Box<dyn Any>` downcast panic risk.

Expected direction: typed addresses keep reply type tied to call helper; common
wrong-reply mistakes become compile errors; low-level erased escape hatches may
remain, but must be loud and tested; remaining runtime panics require deliberate
low-level misuse.

Do not add a second call API. Do not start a deep rewrite unless Rock 6 forces
it.

## Rock 11: Proof

Direct tests must cover invalid config rejection; health vs trace/pressure;
owner stop/panic cleanup for TCP/TLS/file/UDP/pending calls; unrelated-owner
safety; drain success/timeout/pending I/O/requester stop/late completion;
cross-shard call success/full/closed/stale/unknown/timeout/requester stopped/
requester full/target stopped/shard failed; inbound TLS handshake/read/write/
close/config failure/timeout/full/closed/failed-handshake cleanup; failed-shard
rejects while healthy shards continue; acceptor overload and child cleanup; and
compile-fail wrong reply types.

Add three user-shaped live e2e workloads using public `LocalSystem`, not private
driver calls:

1. Multi-shard request/reply service: client shard calls worker shard; worker
   does runtime-owned I/O; reply crosses back.
2. TLS accept service: inbound TLS, bounded connection child, file/persistence
   before reply, graceful drain.
3. Overload/failure service: ingress/remote/mailbox/resource pressure, shard
   failure, shutdown deadline, terminal health report.

Each e2e must prove positive path plus meaningful full, timeout, stopped,
shutdown, and trace/health expectations.

Extend DST over cross-shard calls, ownership cleanup, drain/cancel, inbound TLS
scripts, shard failure, resource budgets, acceptor child ownership, and
persistence around drain/failure. DST must combine rocks, prove replay, shrink
useful failures, and seed these broken-invariant probes:

- owner cleanup leak;
- drain accepting forbidden ingress;
- cross-shard late reply delivered as success after timeout.

## Done Means

- All rocks above are implemented or the phase pauses before claiming success.
- E2E and DST cover happy, negative, weird, and shutdown paths.
- `make verify` passes.
- Positive, blast-radius, and hostile reviews are written in `review.md` and
  findings are fixed.
- `SYSTEM.md`, `ROADMAP.md`, and `CHANGELOG.md` tell only landed truth.

## Pause Gates

Pause if:

- cross-shard calls want a second call API;
- resource ownership needs broad public API churn;
- inbound TLS wants a large cert/config policy framework;
- shard failure wants automatic restart instead of quarantine/reject;
- drain conflicts with bounded queue semantics;
- type-safety cleanup removes a useful escape hatch;
- helper/macro hides capacity, timeout, address, or failure policy.

## Non-Claims After This Phase

Even if this lands:

- no distributed runtime yet;
- no clustering/membership/placement;
- no durable mailbox or exactly-once delivery;
- no broad performance win claim;
- no Tower/Axum middleware inside Tina;
- no flow ergonomics yet.
