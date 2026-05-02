# 024 Mercury Overload Lab And Runtime Contract Plan

Session:

- A
- B (overload-lab rewrite)

## What We Are Building

Mercury makes Tina's primitive visible under load.

Huygens proved the core model and landed the first live worker-owned runtime
substrate. Mercury now builds the proof app that shows why the model matters:

> Under constrained memory and bounded queues, Tina rejects or times out excess
> work explicitly, keeps accepted work bounded, recovers when load drops, and
> can replay the same failure shape in simulation.

This is still not a broad production-ready claim. Mercury must keep the claim
selected, concrete, and evidence-backed:

> You can try replacing selected Tokio-shaped state-machine workloads with Tina
> when you want bounded queues, shard-owned state, timeout-based request/reply,
> deterministic testing, and a live thread-per-shard runtime path.

## What Will Not Change

- Mercury does **not** turn handlers async.
- Mercury does **not** let user handlers pass arbitrary futures.
- Mercury does **not** expose `tokio::Handle`, raw backend sockets, or backend
  spawn APIs through normal Tina context.
- Mercury does **not** make `Runtime` internally thread-safe through
  `Arc<Mutex<Runtime>>`.
- Mercury does **not** make unbounded queues acceptable.
- Mercury does **not** claim naive Tokio is the best possible Tokio.
- Mercury does **not** publish crates or freeze semver.
- Mercury does **not** claim broad production readiness.
- Mercury does **not** claim Tina-Odin's full no-hidden-allocation story unless
  direct probes prove a narrow path.
- Mercury does **not** make simulation/replay subordinate to the live runtime.
  The explicit-step runtime and simulator remain the semantic oracle.

## Core Decisions

### 1. Proof Shape: Overload Lab First

The load-bearing proof is the overload lab, not a backend spike.

The overload lab is a small stateful TCP-shaped service:

- ingress accepts or simulates client requests
- per-connection or per-session isolates own request state
- worker/session isolates do bounded work
- workers can be slowed or crashed by test configuration
- request paths use mandatory-timeout calls
- overload paths use observed send/call outcomes
- recovery after load drops is asserted
- simulator replay and live runner exercise the same isolate logic where
  feasible

The first live version may use the existing threaded+Betelgeuse substrate. The
later live-network version should run on the narrow Tokio current-thread
backend if that backend lands in Mercury.

### 2. Tokio Current-Thread Is The Practical Bridge

Mercury adds a narrow Tokio current-thread backend only for the proof app's
runtime-owned TCP/time needs.

Expected shape:

- one OS thread per Tina shard
- one Tokio current-thread runtime on that shard thread
- one Tina interpreter owned by that shard thread
- Tina handlers remain synchronous and effect-returning
- Tina TCP/time effects are implemented by backend-owned Tokio operations
- completions return as ordinary Tina messages

This is not the Apollo Tokio bridge. It does not host arbitrary Tina isolates
inside existing Tokio apps, and it does not expose async handlers.

The reason to use Tokio here is practical: it is known, cross-platform, and
lets Mercury compare Tina against Tokio-shaped code honestly. `monoio` remains
the likely native thread-per-core backend candidate after this proof, but it is
not the first blocker.

### 3. User-Visible Backpressure

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

### 4. Timeout Request/Reply

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

### 5. Live Runner Contract

Threaded runners need lifecycle semantics that a user can understand:

- construct with bounded capacities
- register roots on chosen shards
- optionally configure supervision
- send ingress through bounded handoff
- drain/shutdown deterministically enough for tests
- surface worker stopped / worker panicked / control errors
- collect globally sorted trace

Mercury should improve the runner surface only where needed for the overload
lab and tests. Do not build an application framework.

### 6. Spawned Children On Live Multi-Shard

Mercury must either:

- support spawned children sending cross-shard on the live substrate, or
- add an explicit guard that rejects unsupported behavior clearly.

Expected direction: support it for `Send + 'static` outbound payloads, because
per-connection child isolates must be able to talk to worker/session shards in
the overload lab.

### 7. Live Supervision

Mercury must prove supervision on the live substrate:

- a worker child panics
- parent supervisor restarts it
- stale pre-restart address fails safely
- later work succeeds through the replacement
- trace proves the restart path

This should be a direct threaded-runtime test, not only simulator or
explicit-step proof.

### 8. Comparison App

Mercury includes three versions of the same constrained workload:

- naive Tokio: ordinary async shape with the overload mistake visible
- hardened Tokio: bounded channels/semaphores/timeouts/load shedding
- Tina: no special optimization beyond Tina's normal bounded primitive

The comparison must be honest:

- do not claim Tokio cannot be written safely
- do show that safe Tokio requires explicit user discipline
- do show that Tina makes the bounded shape the default path

Measured outputs should include accepted count, rejected/full count, timeout
count, recovery after load drop, max queue depth where visible, latency for
accepted requests, and memory/RSS or a narrower allocation proxy if RSS is too
noisy for CI.

### 9. Capacity And Allocation Claim

Expected direction:

- claim bounded configured queues and visible overload
- keep broad runtime allocation-free behavior as a non-claim
- add focused probes for any new hot path introduced by Mercury if it would be
  easy to accidentally allocate per message

### 10. Public-ish DST Harness

Mercury may expose a tiny testing surface for user workloads:

- run a workload under simulator config
- replay from saved config
- attach a checker
- return trace/failure artifact

This is not Gemini documentation. It is just enough API/helper shape so the
overload lab and future users do not depend on private test-only glue.

## Build Order

1. **Observed send outcome**
   - done: `send_observed(...).reply(...)`
   - done: runtime/simulator support
   - done: `Accepted`, `Full`, and `Closed` are proved as user-visible
     messages
   - done: ordinary `send(...)` remains unchanged
2. **Timeout call**
   - done: `call(..., timeout).reply(...)`
   - done: runtime/simulator same-shard request/reply support
   - done: reply, target full, target closed, and timeout are proved as
     user-visible messages
   - done: direct requester-stopped rejection proof
   - remaining: cross-shard call reply transport is not claimed yet
3. **Semantic overload lab**
   - build the core state-machine workload without relying on a new backend
   - prove overload, timeout, rejection, recovery, and replay
4. **Runner lifecycle hardening**
   - add only the lifecycle controls needed by overload lab/tests
   - prove bounded handoff and clean shutdown/drain behavior
5. **Live spawned-child cross-shard behavior**
   - support or explicitly guard it
   - add direct live threaded proof
6. **Live supervision proof**
   - add threaded restart workload proof
7. **Tokio current-thread backend**
   - add the narrow backend seam needed for TCP/time
   - keep user handlers sync
   - prove TCP/time completion parity with the oracle on the overload lab
8. **Tokio comparison workload**
   - implement naive Tokio and hardened Tokio comparison variants
   - prove the naive overload failure and hardened survival honestly
9. **Capacity/allocation probe**
   - add focused probes or document narrow claim
10. **Public-ish DST harness**
   - extract only the small reusable surface needed by overload lab and later
     users
11. **Closeout**
   - update SYSTEM/ROADMAP/CHANGELOG
   - write closeout matrix
   - run `make verify`

## Pause Gates

Pause and ask before continuing if:

- observed send seems to require immediate same-turn mailbox mutation from
  handler code
- timeout call requires async handlers
- Tokio current-thread integration requires exposing arbitrary futures to user
  handlers
- Tokio current-thread integration requires making `Runtime` `Send` or `Sync`
- any backend integration requires replacing the semantic runtime rather than
  adapting the substrate
- any path wants an unbounded queue
- comparison starts straw-manning Tokio instead of including a hardened version
- overload lab cannot use the same isolate logic across live and simulator
  proof paths
- public API names become contentious enough to affect `tina`
- a claim starts sounding like production/Tokio replacement instead of selected
  workload tryability

## Proof Matrix

| Capability | Runtime oracle | Simulator/replay | Live substrate | Tokio backend/comparison |
|---|---|---|---|---|
| Observed send accepted/full/closed | required | required | required for threaded runner | required if backend supports sends |
| Timeout call reply/full/closed/timeout | required | required | required | required |
| Semantic overload lab | required | required replay/checker | required | parity target |
| TCP/time backend parity | existing Betelgeuse proof | simulated TCP/time already covered where feasible | existing threaded proof | required narrow proof |
| Spawned child cross-shard send | required if semantics change | optional unless simulator surface changes | required or guarded | required if overload lab uses it |
| Supervision restart | already exists, add any new call/backpressure interaction | required for overload lab if simulated | required direct proof | required if backend runs the full lab |
| Naive vs hardened Tokio comparison | n/a | n/a | n/a | required |
| Capacity/allocation claim | focused probe where meaningful | non-claim acceptable | focused probe where meaningful | focused probe or RSS artifact |

## Done Means

Mercury is done when:

- app code has a blessed overload-observation path
- isolate-to-isolate timeout call exists and is tested
- overload lab proves reject/timeout/recover under constrained capacities
- overload lab runs through simulator replay/checker pressure
- live runner lifecycle is sufficient for overload lab and tests
- live spawned-child cross-shard behavior is supported or explicitly guarded
- live supervision restart is directly proved
- a narrow Tokio current-thread TCP/time backend either runs the overload lab
  or is explicitly deferred with the exact missing seam recorded
- naive Tokio and hardened Tokio comparison variants exist if the Tokio backend
  lands; if not, their design remains pinned for the backend follow-up
- capacity/allocation claims are honest and evidence-backed
- a tiny public-ish DST harness exists if overload lab needs reusable glue
- SYSTEM/ROADMAP/CHANGELOG and closeout record exact claims/non-claims
- `make verify` passes
