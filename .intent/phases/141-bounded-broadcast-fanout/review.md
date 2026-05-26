# Phase 141 Review

## Hostile Pass 1

- The plan must not build "fanout by convenience." It must build "fanout by
  service-owned bound." The `BroadcastTargets` wrapper is load-bearing because
  it forces the bound before effects exist.
- `BroadcastTracker` needs target identity, not just counts. Without keys, late
  duplicates and unknown outcomes become silent math bugs.
- The helper must still use ordinary continuation messages. No callback mutates
  isolate state.
- The specimen migration is required. If the helper only has unit tests, we did
  not prove the user path got better.
- The compile-fail proof is required. This phase is partly about making the
  wrong shape harder for agents to write.

