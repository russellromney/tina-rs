# Phase 121: Fairness And Load Behavior

## Status

- Future IDD outline for Wave B.
- Runs after Phase 116 so protocol-session fairness can use real HTTP/2/gRPC
  client/server surfaces.
- Can run in parallel with Phase 122 if ownership stays in scheduler
  proof/reporting, soak harnesses, and systems.

## Purpose

Prove Tina behaves honestly under pressure.

The user story:

```text
one hot actor/session/client should not quietly starve the rest of my service
```

## Includes

- fairness proofs for hot isolate mailboxes
- timer fairness under hot mailbox load
- protocol session fairness for WebSocket/HTTP2/gRPC
- remote inbound drain fairness where live multi-shard paths exist
- starvation-ish lag counters where Tina can observe them honestly
- load/soak harness that records high-water, full counts, late replies, leaks,
  and trace fingerprints
- CPU and memory constrained system runs
- use existing cooperative fairness tests and hot-key specimen as the seed, but
  expand to protocol sessions and live soak

## Does Not Include

- no strict real-time guarantee
- no global priority scheduler
- no benchmark bragging
- no hidden buffering to improve fairness numbers
- no admission/rate policy objects; Phase 118 owns pressure policy

## Proof Shape

- hot-key workload does not starve unrelated key/session beyond documented
  bounds
- slow WebSocket/session does not starve unrelated session work
- timers still fire under hot send/call traffic
- reports expose unfairness/lag when it happens
- soak runs show bounded surfaces plateau or fail visibly
