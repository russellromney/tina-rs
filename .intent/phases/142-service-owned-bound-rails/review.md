# Phase 142 Review

## Hostile Pass 1

- Do not oversell this as total compile-time safety. Request length is runtime
  data. The win is that blessed helpers require an explicit cap before many
  effects are produced.
- `BoundedEffects` can become ceremony if used everywhere. Use it only where a
  request or external input controls cardinality.
- The error must say how many items were attempted. Otherwise users cannot tune.
- The specimen updates are part of the feature. Without them, agents will keep
  copying raw `collect::<Vec<_>>()` loops.

