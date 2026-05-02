# 024 Mercury Production-Shaped Runtime Contract Plan

Session:

- A

## What We Are Building

Mercury makes the Huygens substrate story tryable.

Huygens proved the core model and landed the first live worker-owned runtime
substrate. Mercury is the phase that closes the remaining gap between "the
model works" and:

> You can try replacing selected Tokio-shaped workloads with Tina when you want
> bounded queues, shard-owned state, timeout-based request/reply, deterministic
> testing, and a live thread-per-shard runtime path.

This is still not a broad production-ready claim. Mercury must keep the claim
selected, concrete, and evidence-backed.

## What Will Not Change

- Mercury does **not** turn handlers async.
- Mercury does **not** make `Runtime` internally thread-safe through
  `Arc<Mutex<Runtime>>`.
- Mercury does **not** make unbounded queues acceptable.
- Mercury does **not** publish crates or freeze semver.
- Mercury does **not** build the Tokio bridge.
- Mercury does **not** claim broad production readiness.
- Mercury does **not** claim Tina-Odin's full no-hidden-allocation story unless
  direct probes prove a narrow path.
- Mercury does **not** make simulation/replay subordinate to the live runtime.
  The explicit-step runtime and simulator remain the semantic oracle.

## Core Decisions

### 1. Substrate Direction

Mercury starts with a substrate seam audit and monoio spike.

Expected direction:

- Try the smallest `monoio` / io_uring substrate proof first.
- The spike succeeds only if one shard can run a current-thread monoio reactor
  while isolate handlers remain synchronous and effect-returning.
- The minimum proof is TCP echo / request-response with oracle/substrate parity
  on final bytes and a shared trace subset.
- If monoio fights the model or becomes a large backend project, record the
  reason and explicitly bless threaded+Betelgeuse as the temporary tryable
  substrate for this phase.

Fallback is allowed. Silent ambiguity is not.

### 2. User-Visible Backpressure

Ordinary `send(...)` stays fire-and-forget and simple.

Mercury adds a distinct observed-send path for user code that needs overload
policy. Because tina-rs handlers return effects, an observed send cannot
return `Full` to the same handler turn without breaking the model. The chosen
shape is:

- a new effect/call-like helper that attempts a send through the runtime
- the runtime later delivers a typed outcome message to the requester
- the outcome includes at least `Accepted`, `Full`, and `Closed`
- trace still records send attempt/accept/reject as before

Sketch:

```rust
send_observed(self.worker, WorkMsg::Run(job))
    .reply(|outcome| SessionMsg::WorkSendObserved(outcome))
```

Naming is provisional. The plan pins the semantics, not the exact final helper
name. The final name should be decided before implementation of this slice and
used consistently.

### 3. Timeout Request/Reply

Mercury adds isolate-to-isolate call with mandatory timeout.

The call path is built on the same observed-delivery principle:

- request admission can fail as `Full` / `Closed`
- accepted requests wait for a typed reply
- timeout is mandatory
- timeout produces a typed failure outcome
- the requester receives exactly one typed completion message

Sketch:

```rust
call(self.worker, WorkRequest::Run(job), Duration::from_millis(50))
    .reply(|outcome| SessionMsg::WorkReturned(outcome))
```

The call outcome must distinguish:

- request accepted and reply received
- request target full
- request target closed/stale
- timed out
- requester stopped before completion, visible in trace

### 4. Live Runner Contract

Threaded runners need lifecycle semantics that a user can understand:

- construct with bounded capacities
- register roots on chosen shards
- optionally configure supervision
- send ingress through bounded handoff
- drain/shutdown deterministically enough for tests
- surface worker stopped / worker panicked / control errors
- collect globally sorted trace

Mercury should improve the runner surface only where needed for the dogfood
workload and tests. Do not build an application framework.

### 5. Spawned Children On Live Multi-Shard

Mercury must either:

- support spawned children sending cross-shard on the live substrate, or
- add an explicit guard that rejects unsupported behavior clearly.

Expected direction: support it for `Send + 'static` outbound payloads, because
per-connection child isolates must be able to talk to worker/session shards in
the dogfood workload.

### 6. Live Supervision

Mercury must prove supervision on the live substrate:

- a worker child panics
- parent supervisor restarts it
- stale pre-restart address fails safely
- later work succeeds through the replacement
- trace proves the restart path

This should be a direct threaded-runtime test, not only simulator or
explicit-step proof.

### 7. Dogfood Workload

The named dogfood workload is `tcp_session_worker`.

Shape:

- TCP listener accepts client connections
- each connection is a child isolate
- connection/session isolate sends requests to a worker/session shard
- request path uses timeout call
- overload path uses observed send/call outcomes
- worker is supervised and can restart
- client-visible result is asserted
- same isolate logic has simulator/replay/checker coverage where feasible

This workload is the Mercury proof that the framework can be tried for a real
Tokio-shaped pattern without pretending every Tokio workload fits.

### 8. Capacity And Allocation Claim

Expected direction:

- claim bounded configured queues and visible overload
- keep broad runtime allocation-free behavior as a non-claim
- add focused probes for any new hot path introduced by Mercury if it would be
  easy to accidentally allocate per message

### 9. Public-ish DST Harness

Mercury may expose a tiny testing surface for user workloads:

- run a workload under simulator config
- replay from saved config
- attach a checker
- return trace/failure artifact

This is not Gemini documentation. It is just enough API/helper shape so the
dogfood workload and future users do not depend on private test-only glue.

## Build Order

1. **Substrate seam audit and monoio spike**
   - inspect current runtime-owned I/O seams
   - decide whether monoio can host the current effect model narrowly
   - record decision in `audit.md`
2. **Observed send outcome**
   - pin final names
   - implement runtime/simulator support
   - prove `Accepted`, `Full`, and `Closed` as user-visible messages
   - prove ordinary `send(...)` remains unchanged
3. **Timeout call**
   - implement call request/reply with mandatory timeout
   - prove reply, target full, target closed, timeout, requester stopped
   - test runtime and simulator paths
4. **Runner lifecycle hardening**
   - add only the lifecycle controls needed by dogfood/tests
   - prove bounded handoff and clean shutdown/drain behavior
5. **Live spawned-child cross-shard behavior**
   - support or explicitly guard it
   - add direct live threaded proof
6. **Live supervision proof**
   - add threaded restart workload proof
7. **Dogfood workload**
   - implement `tcp_session_worker` proof workload
   - run through live substrate and simulator/replay/checker where feasible
8. **Capacity/allocation probe**
   - add focused probes or document narrow claim
9. **Public-ish DST harness**
   - extract only the small reusable surface needed by dogfood and later users
10. **Closeout**
   - update SYSTEM/ROADMAP/CHANGELOG
   - write closeout matrix
   - run `make verify`

## Pause Gates

Pause and ask before continuing if:

- observed send seems to require immediate same-turn mailbox mutation from
  handler code
- timeout call requires async handlers
- monoio integration requires making `Runtime` `Send` or `Sync`
- monoio integration requires replacing the semantic runtime rather than
  adapting the substrate
- any path wants an unbounded queue
- dogfood workload cannot use the same isolate logic across live and simulator
  proof paths
- public API names become contentious enough to affect `tina`
- a claim starts sounding like production/Tokio replacement instead of selected
  workload tryability

## Proof Matrix

| Capability | Runtime oracle | Simulator/replay | Live substrate |
|---|---|---|---|
| Observed send accepted/full/closed | required | required | required for threaded runner |
| Timeout call reply/full/closed/timeout | required | required | required |
| TCP echo baseline | already exists | already exists | already exists; monoio spike if accepted |
| Spawned child cross-shard send | required if semantics change | optional unless simulator surface changes | required or guarded |
| Supervision restart | already exists, add any new call/backpressure interaction | required for dogfood if simulated | required direct proof |
| Dogfood `tcp_session_worker` | required if feasible | required replay/checker over core logic | required |
| Capacity/allocation claim | focused probe where meaningful | non-claim acceptable | focused probe where meaningful |

## Done Means

Mercury is done when:

- monoio is either narrowly proven or explicitly deferred with tradeoffs
- app code has a blessed overload-observation path
- isolate-to-isolate timeout call exists and is tested
- live runner lifecycle is sufficient for dogfood and tests
- live spawned-child cross-shard behavior is supported or explicitly guarded
- live supervision restart is directly proved
- `tcp_session_worker` ties together TCP ingress, child/session isolates,
  worker/session shard, timeout call, overload policy, restart, and replay
- capacity/allocation claims are honest and evidence-backed
- a tiny public-ish DST harness exists if dogfood needs reusable glue
- SYSTEM/ROADMAP/CHANGELOG and closeout record exact claims/non-claims
- `make verify` passes
