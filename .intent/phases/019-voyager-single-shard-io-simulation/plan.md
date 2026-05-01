# 019 Voyager Single-Shard I/O Simulation Plan

Session:

- A

## What We Are Building

Build the next Voyager slice after single-shard supervision parity: extend
`tina-sim` so it can replay the full first single-shard I/O story, not just
timers and supervision. This is the simulator/DST counterpart to Mariner's
TCP echo proof package.

Concretely, this package adds:

- simulator support for the already-shipped single-shard TCP call family:
  - bind
  - accept
  - read
  - write
  - listener close
  - stream close
- virtual network resources and completion ordering for that TCP surface
- replayable proof workloads that exercise listener/connection lifecycle under
  simulation, including one-client, sequential multi-client, and bounded-overlap
  echo flows
- partial read/write modeling sufficient to prove the same connection drain
  discipline as the live Mariner echo workload
- visible invalid-resource and close/rejection paths for the simulated TCP
  resources
- seeded perturbation over simulator-owned TCP completion ordering and/or
  completion visibility
- checker-backed replay of a TCP-lifecycle/order failure

The point of 019 is to make Voyager capable of replaying the first Mariner
network-server story end to end, not merely primitive TCP calls.

This is intentionally a large Voyager slice. The review bar is correspondingly
larger than 016/017/018: 019 should not close on primitive TCP calls alone, or
on a one-client echo that dodges listener lifecycle, ordering, backpressure,
partial I/O, and replayed failure. If implementation discovers that this
requires packet-level TCP realism or new public runtime verbs, pause; otherwise
keep the package together so "single-shard I/O simulation" means the full
Mariner echo story under simulation.

## What Will Not Change

- `tina` remains substrate-neutral and does not gain simulator-only public
  API.
- This slice stays single-shard.
- This slice does not add real kernel/network integration to the simulator.
- This slice does not add multi-shard routing or placement.
- This slice does not add a broad checker DSL.
- This slice does not simulate filesystem, subprocess, UDP, TLS, DNS, or
  packet-level TCP behavior. Those are different I/O stories.
- Replay artifacts reproduce simulated TCP runs against the same workload
  binary and simulator version. They do not serialize arbitrary isolate code,
  TCP payload scripts, spawn factories, or bootstrap closures.
- 019's TCP perturbation is its own simulator surface. Existing 017
  local-send/timer perturbations and 018 spawn/restart behavior remain scoped
  to their current meanings and must keep composing additively, but 019 does
  not silently broaden them.
- Existing 016-018 timer, fault, checker, spawn, and supervision behavior must
  stay green.

## How We Will Build It

- Reuse the already-shipped public runtime-owned TCP call vocabulary from
  `tina-runtime-current`; do not invent simulator-only network verbs.
- Keep the simulator faithful to the already-shipped single-shard runtime
  meaning:
  - runtime-owned opaque TCP resource ids
  - `TcpBind` returns `CallResult::TcpBound { listener, local_addr }`
  - `TcpAccept` returns `CallResult::TcpAccepted { stream, peer_addr }`
  - `TcpRead` returns `CallResult::TcpRead { bytes }`
  - `TcpWrite` returns `CallResult::TcpWrote { count }`
  - completion translated back into ordinary later-turn messages
  - partial writes remain possible and must be modeled
  - listener close and stream close remain visible runtime-owned operations
  - simulated TCP resources belong to one simulator instance and shard
  - stale, closed, or unknown resource-id use surfaces the same way as the
    live runtime: `CallFailed(InvalidResource)`, not a simulator-only event
  - listener and stream ids remain runtime-owned handles that may be passed
    between isolates; the simulator must not add isolate-local capability rules
- Preserve the live runtime's dispatch-time shape where it is observable:
  - bind and close complete inline during dispatch
  - accept, read, and write enter pending state and complete on later steps
  - pending completions rejected because the requester stopped use the live
    `CallCompletionRejected` trace shape
- Prefer a focused virtual network model over broad realism:
  - loopback-style listener/stream pairing
  - deterministic in-memory byte buffers
  - virtual listener queues, stream read/write buffers, and pending TCP
    completion queues are explicitly bounded by simulator config
  - overflow or exhaustion surfaces through visible call failure, completion
    rejection, or scripted peer/backpressure outcomes rather than hidden
    unbounded buffering
  - per-resource FIFO completion ordering
  - global tie-break by call request order when several completions become
    visible in the same simulator step
  - deterministic completion visibility under seeded perturbation
  - partial writes caused by an explicit per-stream write cap
  - partial/short reads caused by peer buffers holding fewer bytes than
    `max_len`, plus optional scripted read chunking
- Build the simulator proof around Mariner echo parity:
  - one-client echo
  - sequential multi-client echo
  - bounded-overlap echo
  - listener re-arm
  - graceful listener close/stop
  - partial-write retry / drain behavior
  - pending operation rejection on requester stop or resource close
  - the simulated echo workload uses the same listener/connection isolate
    shape as the live `tcp_echo` proof: listener spawns each accepted
    connection as a `RestartableSpawnSpec` child with bootstrap, and listener
    re-arm happens through `Effect::Batch(Spawn, Send-self)` rather than a
    simulator-only helper
- Build one fault-sensitive TCP workload that a checker can halt and replay,
  instead of only proving happy-path bytes.

## Build Steps

1. Extend `tina-sim` with virtual listener and stream resources for the public
   TCP call family already shipped in `tina-runtime-current`.
2. Implement completion delivery for bind/accept/read/write/close using the
   same translated-message shape the live runtime uses.
3. Model partial writes and partial/short reads honestly enough to exercise the
   live echo connection's pending-buffer + drain behavior.
   - partial writes are driven by explicit per-stream write caps
   - short reads are driven by available peer-buffer bytes and/or configured
     read chunk sizes
4. Model listener re-arm and pending accept behavior without special test
   shortcuts.
5. Model close semantics visibly:
   - listener close releases listener resources
   - stream close releases stream resources
   - listener close while accept is pending rejects that pending accept through
     the live call-completion rejection shape
   - stream close while read is pending rejects that pending read through the
     live call-completion rejection shape
   - stream close while write is pending rejects that pending write through the
     live call-completion rejection shape
   - pending completions for stopped requesters are rejected through the live
     `CallCompletionRejected` trace shape
   - invalid listener/stream ids surface `CallFailed(InvalidResource)`
   - TCP completions that reach a full requester mailbox surface
     `CallCompletionRejected { reason: MailboxFull }`
6. Add named seeded TCP perturbation surfaces:
   - `TcpCompletionFaultMode::DelayBySteps { one_in, steps }` delays matching
     pending TCP completions by deterministic simulator steps
   - `TcpCompletionFaultMode::ReorderReady { one_in }` perturbs ready
     completions that are otherwise tied in the same step, still preserving
     per-resource FIFO
7. Extend replay artifacts only as needed for exact reproduction of simulated
   TCP runs.
8. Add direct proofs for:
   - bind/accept/read/write/close through the public simulator path
   - `local_addr` on bind and `peer_addr` on accept are deterministic and read
     by the workload
   - `TcpRead { bytes }` and `TcpWrote { count }` shapes match the live runtime
   - invalid resource failures
   - TCP completion into a full requester mailbox yields
     `CallCompletionRejected { reason: MailboxFull }`
   - listener close while accept is pending
   - stream close while read is pending
   - stream close while write is pending
   - requester stop while accept/read/write is pending
   - partial-write handling
   - partial/short-read handling
   - listener re-arm / repeated accept behavior
   - same config reproduces the same simulated TCP event record
   - different seeds intentionally diverge under both named TCP perturbation
     surfaces
9. Add user-shaped workloads:
   - one-client echo
   - sequential multi-client echo
   - bounded-overlap echo
   - graceful listener close/stop
   - connection-as-restartable-child with bootstrap, using the same pending
     buffer / drain connection isolate shape as the live Mariner echo proof
   - one fault-sensitive workload proving a replayable TCP ordering,
     lifecycle, or backpressure issue under seeded perturbation
10. Close out docs/evidence:
   - `.intent/SYSTEM.md`
   - `ROADMAP.md`
   - `CHANGELOG.md`
   - phase `commits.txt`
   - review receipt

## How We Will Prove It

Direct proof for the changed behavior:

- at least one public-path workload must execute the already-shipped TCP call
  surface through `tina-sim`
- focused simulator tests for:
  - bind/accept/read/write/close completion behavior
  - bind `local_addr` and accept `peer_addr` behavior
  - live `CallResult` payload shapes for read/write
  - partial writes and continued drain behavior
  - partial/short reads
  - listener re-arm behavior
  - graceful listener close/stop
  - listener close while accept is pending
  - stream close while read is pending
  - stream close while write is pending
  - requester stop while accept/read/write is pending
  - TCP completion into a full requester mailbox
  - invalid resource failures
  - stale, closed, or unknown resource use fails as `InvalidResource`
  - same config reproduces the same TCP event record
  - different seeds diverge intentionally under delayed-completion
    perturbation
  - different seeds diverge intentionally under ready-completion reordering
    perturbation
- checker/replay tests for:
  - a checker can observe a TCP-lifecycle-sensitive invariant
  - replay reproduces the same fault-sensitive TCP run exactly
- e2e-style simulated network tests for:
  - one-client echo
  - sequential multi-client echo
  - bounded-overlap echo
  - listener-as-supervisor spawning restartable connection children with
    bootstrap, through the same batch/spawn/re-arm shape as live `tcp_echo`
  - fault-triggered divergence or failure under seeded perturbation

Proof modes expected in this package:

- unit proof:
  virtual resource bookkeeping
- integration proof:
  TCP completion semantics on the simulator path
- e2e proof:
  user-shaped echo workload through `tina-sim`
- adversarial proof:
  checker-triggered network failure or ordering issue
- replay proof:
  saved artifact reproduces the same simulated TCP run
- blast-radius proof:
  all prior Mariner and Voyager suites still pass

Surrogate proof that helps but does not close the claim:

- proving resource bookkeeping without a public-path echo workload
- proving replay on timer/supervision-only workloads while claiming simulated
  TCP support
- relying only on final echoed bytes without the replayed event record
- proving only a one-client happy path while claiming Mariner echo parity
- proving TCP primitive calls without a listener/connection lifecycle workload
- using a simulator-only connection stand-in instead of the same isolate shape
  as the live echo proof
- leaving TCP perturbation as implementation-selected `and/or` behavior instead
  of proving both named perturbation surfaces

## How We Will Prove We Did Not Break Earlier Intent

Blast-radius proof for already-shipped behavior:

- existing `tina-sim` timer/replay/fault/checker/spawn/supervision tests still
  pass
- existing `tina-sim` workloads using `Spawn = Infallible` still compile and
  run unchanged
- existing `tina-runtime-current` tests still pass
- `make verify` still passes
- no `tina` boundary changes are required

## Pause Gates

Stop and ask for human input only if one of these happens:

- faithful simulator TCP support appears to require new public runtime verbs
- the only honest simulated network model is much larger than this phase should
  carry
- seeded TCP perturbation wants a much broader fault/checker architecture than
  the current Voyager surface should own
- Mariner echo parity appears to require packet-level TCP realism rather than
  the runtime-owned call/result contract
- preserving bind/close inline completion and accept/read/write pending
  completion proves incompatible with the simulator step model
- connection-as-restartable-child echo parity cannot be expressed without a
  simulator-only spawn or re-arm helper

## Traps

- Do not invent simulator-only network semantics.
- Do not fake TCP by bypassing the public runtime-owned call path.
- Do not fake listener re-arm with a simulator helper; use the existing
  batch/spawn/send-self workflow.
- Do not replace the live echo connection shape with a simulator-only stand-in.
- Do not regress timer/fault/checker/spawn/supervision slices while adding TCP
  simulation.
- Do not declare simulated single-shard I/O “done” unless Mariner's TCP echo
  story is replayed under simulation, including multi-client/re-arm behavior
  and at least one replayable TCP failure/checker path.

## Files Likely To Change

- `tina-sim/src/lib.rs`
- `tina-sim/tests/`
- `.intent/SYSTEM.md`
- `ROADMAP.md`
- `CHANGELOG.md`

## Areas That Should Not Be Touched

- mailbox crates
- Betelgeuse backend
- multi-shard runtime code
- Tokio bridge code

## Report Back

- exact simulated TCP surface
- exact virtual resource/completion model
- exact seeded TCP perturbation surface
- exact completion-ordering rule
- exact sync-vs-async dispatch-time behavior
- which echo-parity workloads proved preserved success
- which TCP workload proved replayed failure/checker behavior
- which close-while-pending and mailbox-full rejection paths are directly
  proved
- what is directly proved vs surrogate-supported
- exact verification results
