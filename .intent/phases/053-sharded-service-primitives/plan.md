# Phase 053: Sharded Service Primitives

## Goal

Give users small Seastar-shaped data/service patterns.

Not distributed data structures. Not magic.

053 answers:

> Can Tina make common shard-owned service state easy without hiding placement,
> pressure, or partial failure?

Near-grug:

> key goes to shard. shard owns map. fanout asks shards. aggregate tells truth.

## Baseline

Already exists:

- shard ids;
- multi-shard runtime;
- cross-shard sends and calls;
- topology reports;
- simulator multi-shard coverage;
- Eiffel keyspace comparison;
- Eiffel findings about sharded maps/counters being absent.

Compromise:

- use normal Rust `HashMap`, `BTreeMap`, counters, and vectors inside isolates;
- Tina owns routing, placement, fanout, timeout, full/closed outcomes, and trace;
- first form is examples plus small helper types where repetition proves value,
  not a polished data-structure library.

## Non-Goals

- No durable distributed database.
- No consensus.
- No automatic rebalancing.
- No transparent shared map.
- No exactly-once update.
- No remoting/clustering requirement.
- No hiding partial failure.
- No stable placement across shard-count changes unless a specific scheme is
  introduced and tested.

## Rules

- Placement must be visible.
- Key-to-shard function must be deterministic.
- Scatter/gather has bounded fanout.
- Aggregates report partial success, full, closed, timeout, and failed shard.
- Hot-key pressure remains visible.
- State stays owned by isolate/shard.
- Docs must say these are local multi-shard patterns, not clustering.

## Rocks

1. **Key Placement Helper**

   Requirements:

   - deterministic key-to-shard mapping;
   - visible placement report;
   - shard-count-change behavior documented;
   - wrong-shard/wrong-key rejection pattern;
   - tests for stable mapping.

2. **Sharded Counter First Form**

   Requirements:

   - increment local shard counter;
   - read local;
   - aggregate total across shards;
   - partial aggregate reports missing/full/timeout shards;
   - simulator tests for reorder/failure.

3. **Sharded Map Pattern**

   Requirements:

   - key routes to owner shard;
   - owner isolate uses normal map;
   - get/put/delete with timeout;
   - full/closed/timeout visible;
   - no global lock, no shared map.

4. **Scatter/Gather Helper**

   Requirements:

   - bounded fanout;
   - per-target timeout;
   - aggregate timeout;
   - public or example-owned partial result type;
   - cancellation/shutdown behavior visible;
   - no hidden unbounded reply collection.

5. **Hot-Key Pressure Policy**

   Requirements:

   - hot owner shard can reject full;
   - retry/backoff pattern through Tina timers;
   - metrics/report includes full/timeout per shard;
   - no automatic queue growth.

6. **Examples**

   Add examples:

   - sharded counter;
   - sharded map/keyspace;
   - scatter/gather aggregate;
   - partial failure report.

   Reuse Eiffel comparison style where useful.

7. **DST**

   Required histories:

   - reorder fanout replies;
   - one shard full;
   - one shard closed/failed;
   - aggregate timeout;
   - hot key pressure;
   - saved seed for partial aggregate.

## Required Proof

- Multi-shard sharded counter works live and in sim.
- Sharded map routes keys deterministically live and in sim.
- Scatter/gather reports partial outcomes.
- Hot-key pressure is visible as full/timeout, not hidden buffering.
- Docs say these are primitives/patterns, not a database.
- Docs say this is not remoting or clustering.

## Done Means

- Users have copyable shard-owned data patterns.
- Tina looks more Seastar-shaped in the app layer.
- Placement, pressure, and partial failure stay visible.
