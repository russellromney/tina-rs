# Phase 050: Eiffel Round 2 Harder Pressure

## Goal

Run the second Eiffel discovery pass after 047 lands.

Round 1 asked:

> Can we port small Tokio-shaped things to Tina, and where does it feel good or
> bad?

Round 2 asks:

> Did 047 actually remove the round-1 pain, and what breaks when Tina's model
> claims face harder pressure?

Near-grug:

> First round found grit. 047 sands grit. Round 2 checks if grit gone, then
> pushes harder.

## Baseline

Already built:

- root-level Eiffel comparison suite;
- `examples/FINDINGS.md`;
- 047 ergonomics harvest plan;
- CPU and memory runner shells;
- real I/O chat comparison with a promised future load driver;
- mini keyspace, bridge HTTP, WebSocket, mux client, persistence, replay,
  supervised worker, outbound fetch, and graceful shutdown comparisons.

Expected before this phase starts:

- 047 has landed;
- round-1 examples have been updated to use 047 primitives where applicable;
- `examples/FINDINGS.md` marks resolved round-1 ergonomic pain or explains why
  it remains model truth.

## Non-Goals

- No native HTTP implementation. That is 048.
- No I/O substrate work.
- No ecosystem bridge adapters. That is 051.
- No Tina RPC product surface. That is 052.
- No sharded primitive library. That is 053.
- No broad benchmark claim.
- No production-readiness claim.
- No making every comparison huge. Each comparison should answer one sharp
  question.

## Rules

- Same Eiffel discipline: paired Tokio-vs-Tina where it makes sense.
- Tina-only adversarial probes are allowed when the question is a Tina model
  claim rather than a Tokio comparison.
- Every comparison gets its own directory under `examples/`.
- Each comparison has clear run commands and a README verdict.
- Each comparison records findings in `examples/FINDINGS.md` with
  `Surfaced by:` tags.
- Report accepted/full/closed/timeouts where pressure matters.
- Prefer real I/O where the claim is about live behavior.
- Use simulator/DST where the claim is about replay, interleavings, or faults.
- Do not hide 047 regressions. If the same boilerplate or side channel returns,
  say so.
- Not all rocks must land in one PR or one session. Land useful batches.
- Every comparison updates `examples/README.md`.

## Tiers

Tier 1 should land first:

- round-1 regression pass;
- real chat load driver;
- multi-shard request routing;
- backpressure chain.

Tier 2 follows:

- dynamic worker pool;
- outbound connection pool;
- streaming response;
- hot-key fairness;
- periodic batcher.

Tier 3 is the deeper proof wall:

- stateful HTTP session;
- heterogeneous workload;
- adversarial probes;
- seeded-fault crash recovery;
- live trace to sim trace identity;
- session fanout;
- TLS variant.

## Rocks

1. **Round-1 Regression Pass**

   Rerun the existing Eiffel comparisons after 047.

   Required:

   - mailbox boilerplate is gone or explicitly justified;
   - `Arc<Mutex<Option<_>>>` ready smuggles are gone or explicitly justified;
   - trace polling loops are gone or explicitly justified;
   - replay uses stable trace fingerprint if 047 shipped it;
   - bridge shutdown examples use the new lifecycle shape if 047 shipped it.

   Update `examples/FINDINGS.md` before adding new findings.

2. **Multi-Shard Request Routing**

   Shape:

   - N shards;
   - hash-routed requests;
   - some cross-shard calls;
   - visible wrong-shard/wrong-key behavior.

   Learning:

   - validate the Seastar-style sharding claim;
   - test cross-shard `call` ergonomics;
   - test address/shard identity;
   - test mailbox composition across shards.

3. **Dynamic Worker Pool**

   Shape:

   - coordinator spawns 10 workers;
   - fan out work;
   - join all results;
   - partial failure aggregation.

   Learning:

   - dynamic spawn at scale;
   - post-047 host observation handle ergonomics;
   - whether `batch(...)` is enough for fanout/join;
   - whether Tina needs a `JoinSet`-equivalent.

4. **Cancellation Chain**

   Shape:

   - one request fans into 5 downstream calls;
   - caller cancels mid-flight;
   - downstream work may still complete late.

   Learning:

   - expose external cancellation gap or prove it is already covered;
   - trace cancellation vs. late completion;
   - define what "caller went away" means in Tina.

5. **Outbound Connection Pool**

   Shape:

   - fetch M URLs;
   - at most N concurrent connections;
   - K per host;
   - retries with backoff;
   - per-request deadline.

   Learning:

   - decide whether Tina needs a pool primitive;
   - queue-when-full vs. shed-when-full;
   - pressure shape for future HTTP client and DB adapters.

6. **Backpressure Chain**

   Shape:

   - A calls B;
   - B calls C;
   - C is slow;
   - whole chain has 100 ms shared deadline.

   Learning:

   - does visible shedding compose;
   - does timeout-as-load-control compose;
   - does Tina need deadline propagation as a first-class concept.

7. **Hot-Key Fairness**

   Shape:

   - mini-keyspace under skewed traffic;
   - one key gets 90 percent of writes;
   - cold keys still receive traffic.

   Learning:

   - hot isolate mailbox saturation;
   - cold-key starvation;
   - scheduler fairness vs. user responsibility.

8. **Streaming Response**

   Shape:

   - TCP service streams 100 MB response;
   - reader is slow;
   - server stays memory bounded.

   Learning:

   - partial-write loops at scale;
   - slow-reader backpressure on server;
   - write-in-flight memory bound.

9. **Periodic Batcher**

   Shape:

   - producer pushes items;
   - batcher flushes at 100 ms or 1000 items;
   - flush is durable.

   Learning:

   - timer + state + bounded buffer + persistence in one isolate;
   - common Kafka/log/metrics producer shape;
   - cancellation-as-message-arm under timer load.

10. **Stateful HTTP Session**

   Shape:

   - `POST /login` issues cookie;
   - `GET /me` reads per-session state;
   - session lifecycle and GC.

   Learning:

   - isolate-per-session pitch through the bridge or native HTTP if 048 is
     available;
   - cookie/header round trip;
   - session cleanup ergonomics.

11. **Heterogeneous Workload**

   Shape:

   - TCP listener;
   - timer-driven flusher;
   - bridge ingress;
   - persistence;
   - one runtime.

   Learning:

   - subsystem interaction;
   - pressure across different runtime rails;
   - closer-to-real-service behavior.

12. **Owned-State Leak Attempt**

   Tina-only adversarial probe.

   Try in good faith to leak shared mutable state through:

   - `Rc<RefCell<T>>` in messages;
   - `Address` captured into escaping closures;
   - `&mut self.value` into runtime-call reply closure;
   - any other plausible user trick.

   Positive result: leak attempt fails or is clearly outside Tina's claim.

   Any successful leak is a critical finding.

13. **Durability-Misorder Attempt**

   Tina-only adversarial probe.

   Try to update state before journal commit completes.

   Learning:

   - append-before-apply is type/system shape, not just discipline;
   - persistence examples cannot accidentally lie about durability ordering.

14. **Non-Determinism-In-Isolate Attempt**

   Tina-only adversarial probe.

   Inject inside handlers under `tina-sim`:

   - `SystemTime::now()`;
   - `HashMap` iteration;
   - `thread_rng()`;
   - other common nondeterminism.

   Learning:

   - simulator replay claim strength;
   - whether nondeterminism can be detected or only documented.

15. **Seeded-Fault Crash Recovery**

   Shape:

   - persistent counter under simulator fault config;
   - mid-len, mid-payload, mid-checksum failures;
   - recovery path replayed from seed.

   Learning:

   - torn-tail handling;
   - journal-replay correctness;
   - snapshot atomicity;
   - deterministic crash recovery, not just deterministic scheduling.

16. **Live Trace To Sim Trace Identity**

   Shape:

   - record a trace under `ThreadedRuntime`;
   - replay against `Simulator` with same seed and inputs.

   Learning:

   - test the direct end-to-end DST claim;
   - learn whether live and sim traces can be byte-identical or only
     semantically comparable.

17. **Session Fanout**

   Shape:

   - 1000 concurrent session isolates;
   - each has mailbox, lifecycle, periodic flush.

   Learning:

   - isolate-per-session memory cost;
   - mailbox creation throughput;
   - address allocation cost;
   - one shard processing many isolate mailboxes.

18. **TLS Variant**

   Shape:

   - take an existing TCP comparison;
   - swap `tcp_bind`/`tcp_accept` for `tls_bind`/`tls_accept`.

   Learning:

   - exercise TLS rails in comparison land;
   - cert handling;
   - handshake error reporting;
   - mid-stream failure behavior.

19. **Real Load Driver On Chat**

   Shape:

   - deliver the driver promised by `eiffel_real_io_chat`;
   - e.g. 1000 clients x 50,000 messages;
   - same report line for Tokio and Tina.

   Learning:

   - visible `Full` under sustained burst;
   - make `eiffel_cpu_run` and `eiffel_mem_run` reports useful;
   - compare shedding vs. buffering under real load.

## Uniform Report Shape

Pressure comparisons should emit one boring report line per side when possible:

```text
comparison=name side=tina accepted=N full=N closed=N timeouts=N errors=N \
  latency_p50_ms=N latency_p95_ms=N rss_peak_mb=N exit=clean
```

Fields may be `not-measured`, but the column name should stay stable. The CPU
and memory runners should consume the same shape.

## Required Proof

- Round-1 examples rerun after 047.
- `examples/FINDINGS.md` updated before and after round-2 additions.
- New comparison directories exist for each landed comparison.
- `examples/README.md` names each new comparison and how to run it.
- Each comparison has a README with:
  - what Tokio wins;
  - what Tina wins;
  - what Tina lacks;
  - what pressure was applied;
  - how to run it.
- Pressure comparisons emit accepted/full/closed/timeouts where meaningful.
- At least one new simulator/DST history has saved seed replay.
- The real chat load driver produces a report line useful to CPU/memory runners.
- Tier 1 lands before the phase claims broad progress.

## Done Means

- Round 2 finds fewer surface ergonomic papercuts than round 1.
- Round 2 finds more missing-feature/model-claim findings than "Tina is noisy"
  findings.
- If old pain returns, 047 is called out honestly.
- The next feature phases have sharper evidence, not vibes.
