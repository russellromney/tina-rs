# Phase 145 Review

## Hostile Pass 1

- Good: the plan starts with the runtime hot path, not HTTP. The Phase 144 rows
  show HTTP is slow, but `call_blocking` and observed send are already broken
  before protocol code enters the room.
- Good: the plan names the actual code suspects. The `1ms` sleep after progress
  is especially suspicious and must be fixed or disproven first.
- Risk: "direct host call" could become a second call system. The plan now
  requires the same public `CallOutcome` vocabulary, bounded host storage, late
  reply trace truth, shutdown truth, and timeout truth.
- Fixed: the first draft said "still terrible." The plan now names a local
  release threshold: if same-shard immediate `host_request_reply` remains above
  `500us` p50 after the worker-loop fix, the per-call driver path must be fixed
  in this phase.
- Fixed: the host-call shape now has guardrails. A runtime-owned pending table
  or one persistent internal endpoint is acceptable. Per-call driver isolate
  registration is explicitly not.
- Risk: removing the sleep can create a CPU spin. The plan requires a
  non-hot-spin idle/pending policy and tests for pending timer/I/O.
- Risk: a hot isolate can starve host commands if the worker drains ready work
  forever. The plan requires shutdown/new-command fairness, likely with a small
  step budget if needed.
- Risk: allocation probes can lie if they only see the load thread. The plan
  asks for warmed allocation tests on the optimized runtime paths, not just
  client-side counts.
- Risk: performance work can accidentally weaken Tina. The plan repeats the
  invariants: no unbounded queues, no lost `Full`/`Closed`/`Timeout`/`Rejected`,
  no trace/replay/cancel removal.

Verdict: implementation-ready. Start with worker-loop progress and stage
reports. Only build the direct host-call path if the worker-loop fix does not
make `call_blocking` sane.
