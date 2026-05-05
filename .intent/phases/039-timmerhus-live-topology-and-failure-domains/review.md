# Phase 039: Timmerhus Plan Review

Verdict: ready to implement.

What is strong:

- The phase is big enough now. It is local live runtime completion, not a
  small observability polish pass.
- The "DST native" wording is honest: native OS scheduling is not claimed
  deterministic; live/native tests compare semantic projections against the
  simulator and use stress scripts for real substrate pressure.
- Excluded rocks have homes. Funkishus owns I/O/storage breadth and substrate
  maturity. Jan Peter Balkenende owns remoting. Mark Rutte owns clustering.
- Cross-shard isolate-call reply transport is correctly named as the biggest
  decision and has a pause gate if it starts becoming remoting.
- The proof bar is user-shaped: topology reports, failed shards, stopped
  shards, cross-shard sends, shutdown accounting, live-vs-sim projection, and
  native stress.

Main implementation risks:

- Queue depth reporting can easily become false precision. Prefer stable
  capacity plus counters if exact depth needs hot-path locks.
- Lifecycle states must be honest. Do not add `Starting` or `Draining` if the
  runtime cannot actually observe them.
- Cross-shard call reply transport must stay local and bounded. If it wants
  remote-node semantics, keep rejection and defer to remoting.
- Native stress must not use sleeps as proof. Barriers/readiness channels are
  part of the test design, not decoration.

No blocking plan findings.
