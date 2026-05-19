# Phase 121: Fairness And Load Behavior

## Status

- Future implementation plan for Wave B.
- Runs after Phase 116 and Phase 124. Protocol-session fairness needs real
  HTTP/2/gRPC client/server surfaces and the second-pass HTTP/2
  strictness/fairness fixes.
- Can also benefit from Phase 118 admission reports and Phase 119 resource
  reports, but must not require them if the first fairness proof is ready.
- Can run in parallel with Phase 122 if ownership stays in scheduler
  proof/reporting, soak harnesses, and systems.

## Starting Facts

- Existing cooperative fairness and hot-load tests are narrow. They prove some
  runtime behavior, not whole-service fairness.
- Phase 124 owns one specific multi-shard remote-drain starvation bug. This
  phase generalizes the proof to service workloads and reports.
- `mini_saas_api` already has a small soak through `tina_proof_harness::load`.
  Use that style, but broaden the surfaces.
- Roadmap still names real chat load, CPU contention, memory-tier runs, and
  bad-peer/load harnesses as future proof.
- Protocol reports often carry counters locally; not all of them become runtime
  trace facts yet. This phase can report honestly without claiming trace replay
  for every protocol fact.

## Purpose

Prove Tina behaves honestly under pressure.

The user story:

```text
one hot actor/session/client should not quietly starve the rest of my service
```

## Includes

- fairness proofs for hot isolate mailboxes and self-send loops
- timer fairness under hot mailbox load
- protocol session fairness for WebSocket/HTTP2/gRPC
- remote inbound drain fairness where live multi-shard paths exist
- bridge/pool fairness under one slow external rail and one healthy rail
- starvation-ish lag counters where Tina can observe them honestly
- load/soak harness that records high-water, full counts, late replies, leaks,
  and trace fingerprints
- CPU and memory constrained system runs
- use existing cooperative fairness tests and hot-key specimen as the seed, but
  expand to protocol sessions and live soak
- CI-sized load profiles plus ignored/opt-in soak profiles

## Does Not Include

- no strict real-time guarantee
- no global priority scheduler
- no benchmark bragging
- no hidden buffering to improve fairness numbers
- no admission/rate policy objects; Phase 118 owns pressure policy
- no promise that every client gets equal latency under OS scheduling

## Blast Radius

Medium blast radius, but tests can be expensive.

- Allowed: proof harnesses, reports, small runtime/protocol counters when they
  expose already-real scheduling/pressure facts, systems/specimens.
- Not allowed: hidden queues, scheduler rewrite, priority scheduler, throughput
  benchmark marketing, or retry policy hidden inside the harness.
- CI tests must stay small. Put long soak/load profiles behind ignored tests or
  explicit commands.

## Implementation Shape

Add a small proof harness, not a benchmark framework:

```text
LoadProfile
LoadRunReport
FairnessReport
SurfacePlateau
LagObservation
```

Rules:

- CI profiles are small and deterministic enough to run often.
- Soak profiles are opt-in/ignored and print copyable pressure lines.
- Reports include: submitted, completed, full, closed, timeout, cancelled,
  late, high-water, final-current, per-session/per-key counts, and trace hash.
- "Lag" means something Tina can observe: message turns between ready and
  handled, timer lateness against runtime time, or protocol session progress
  counts. Do not invent wall-clock precision Tina cannot guarantee.
- Use names like `ready_turn_lag`, `timer_late_by`, `session_progress`, and
  `remote_drain_yielded`. Avoid "scheduler latency" unless it is actually
  measured.
- The harness never hides `Full` by retrying. If it retries, the retry policy is
  explicit in the profile.
- Load tests assert final resource/current counts return to zero unless the
  scenario intentionally leaks and reports that leak.

## User Proof Workloads

- Hot isolate vs quiet isolate: one self-sending actor cannot starve an
  unrelated actor beyond a named profile bound/report.
- Timer under hot mailbox: recurring tick still records progress and missed
  ticks under send/call pressure.
- WebSocket slow peer plus active peer: slow peer is evicted or backpressured;
  active peer continues.
- HTTP/2/gRPC many streams: one flow-control-blocked stream does not stop other
  admitted streams from completing.
- HTTP/2/gRPC client and server together: one blocked outbound response stream
  does not starve unrelated inbound request handling.
- Live multi-shard remote drain: one hot remote edge fills visibly without
  starving another shard's local work.
- Bridge/pool: one slow SQLx/AWS/HTTP rail does not hide pressure or starve a
  healthy admitted request beyond the documented report.
- CPU/memory constrained smoke: service either plateaus or fails with typed
  pressure, not hidden buffering.

## Proof Shape

- hot-key workload does not starve unrelated key/session beyond documented
  bounds
- slow WebSocket/session does not starve unrelated session work
- timers still fire under hot send/call traffic
- reports expose unfairness/lag when it happens
- if a fairness bound cannot be met, the test must assert the report exposes
  the bad condition instead of weakening the scenario silently
- soak runs show bounded surfaces plateau or fail visibly
- final reports prove no leaked leases/permits/body charges/pending calls after
  shutdown
- at least one bad-peer load test forces reset/half-close/stalled writer truth
- CI profile must finish quickly and deterministically. Longer soak profiles are
  ignored/opt-in, but must be runnable by documented command.

## Hostile Review Notes

- Do not turn this into throughput marketing.
- Do not "fix" fairness by adding hidden queues.
- Do not assert fairness from one happy-path unit test.
- Do not pretend OS scheduling is deterministic. Assert Tina-visible facts.
- If a test flakes twice, treat it as a bug in the workload or runtime truth.
