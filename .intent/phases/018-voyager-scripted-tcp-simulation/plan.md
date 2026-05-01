# 018 Voyager Scripted TCP Simulation Plan

Session:

- A

## What We Are Building

Build the next Voyager simulator slice: deterministic scripted TCP simulation
for the already-shipped runtime-owned TCP call family.

Concretely, this package adds:

- a `tina-sim` scripted-network configuration/input surface
- simulator-owned listener and stream resources
- deterministic handling for TCP bind/accept/read/write/close calls
- peer-visible output capture
- replay artifact coverage for scripted network input and observed output
- a simulator-backed TCP echo proof workload

## What Will Not Change

- `tina` remains substrate-neutral and does not gain simulator-only public API.
- The simulator keeps using `tina-runtime-current`'s event vocabulary as its
  meaning surface.
- This slice stays single-shard.
- This slice does not open real sockets.
- This slice does not add multi-shard routing or cross-shard network semantics.
- This slice does not build a general packet scheduler or virtual TCP stack.
- Existing 016 timer/replay behavior and 017 fault/checker behavior must remain
  green.

## How We Will Build It

- Start from the existing live TCP call vocabulary, not a new simulator-only
  call model.
- Add an explicit scripted network config that names:
  - bindable addresses
  - inbound peer connections
  - bytes each peer will make readable
  - expected or captured bytes written back to each peer
- Keep virtual listener queues, stream read/write buffers, and pending TCP
  completion queues explicitly bounded by simulator config. Overflow or
  exhaustion must surface through visible call failure, completion rejection,
  or scripted peer/backpressure outcomes rather than hidden unbounded
  buffering.
- Keep resource ids runtime/simulator-owned and deterministic.
- Make accept/read/write completions ordinary call completions delivered through
  the existing translator path.
- Keep replay artifacts structured: config plus event record plus observed peer
  output, not logs.

## Build Steps

1. Confirm whether 017's seeded fault/checker implementation has landed.
   If not, keep 018 artifacts and code explicitly dependent on that pending
   prerequisite instead of inventing duplicate replay/fault machinery.
2. Add a small scripted TCP config type to `tina-sim`.
3. Extend simulator state with listener and stream tables.
4. Implement simulated `TcpBind`:
   - accept configured bind addresses
   - assign deterministic listener ids
   - report local address through `CallResult::TcpBound`
5. Implement simulated `TcpAccept`:
   - consume scripted inbound peers in deterministic order
   - assign deterministic stream ids
   - report peer address through `CallResult::TcpAccepted`
   - keep pending accepts honest if no peer is available yet
6. Implement simulated `TcpRead`:
   - deliver scripted input bytes
   - surface EOF as an empty byte vector
   - preserve read ordering per stream
7. Implement simulated `TcpWrite`:
   - capture peer-visible bytes
   - optionally model deterministic partial writes only if needed to prove the
     existing echo retry path
8. Implement listener/stream close calls and resource cleanup.
9. Extend replay artifacts with scripted TCP config and observed output.
10. Add direct simulator proofs:
    - bind/accept/read/write/close call semantics
    - invalid resource behavior, if it can be tested without bloating scope
    - replay reproduces event record and peer output
11. Add a user-shaped TCP echo simulator proof:
    - one client
    - sequential multiple clients
    - graceful close/stop
12. Close out docs/evidence:
    - `.intent/SYSTEM.md`
    - `ROADMAP.md`
    - `CHANGELOG.md`
    - phase `commits.txt`
    - review receipt

## How We Will Prove It

Direct proof for the changed behavior:

- focused scripted-TCP tests for:
  - bind returns deterministic listener id and local address
  - accept consumes scripted peers in order
  - read delivers scripted bytes and EOF
  - write captures peer-visible output
  - close releases listener/stream resources
- replay tests for:
  - same script/config/seed => same event record
  - saved replay artifact => same event record and observed peer output
- e2e simulator test for TCP echo:
  - client payloads are echoed exactly
  - trace proves the path went through simulated TCP calls
  - listener exits cleanly after configured clients

Proof modes expected in this package:

- unit proof:
  scripted network resource tables and ordering
- integration proof:
  TCP call request -> simulated completion -> translated message
- e2e proof:
  TCP echo workload through `tina-sim`
- replay proof:
  saved artifact reproduces event record and peer output
- blast-radius proof:
  existing simulator timer/replay and fault/checker tests still pass

Surrogate proof that helps but does not close the claim:

- comparing echoed bytes without asserting runtime event meaning
- tests that use real sockets instead of the scripted simulator model
- logs of peer interaction without structured replay artifact data

## How We Will Prove We Did Not Break Earlier Intent

Blast-radius proof for already-shipped behavior:

- existing `tina-sim` timer/replay tests still pass
- 017 seeded fault/checker tests still pass when that package exists
- existing `tina-runtime-current` TCP echo proofs still pass
- `make verify` still passes
- no `tina` boundary changes are required

## Pause Gates

Stop and ask for human input only if one of these happens:

- TCP simulation appears to require changing the `tina` trait boundary
- the simulator needs a different event vocabulary than the live runtime emits
- exact replay requires a broad artifact compatibility promise we do not want
  yet
- a useful scripted network model cannot be kept smaller than a virtual TCP
  stack
- 017 has not landed and implementation would need to duplicate its
  seed/fault/checker machinery

## Traps

- Do not open real sockets in `tina-sim`.
- Do not make test harnesses bypass handlers by injecting completions directly.
- Do not invent simulator-only TCP events.
- Do not over-model TCP packet behavior when the runtime contract only exposes
  call results.
- Do not let replay depend on logs.
- Do not hide peer output in assertions only; make it part of the artifact or
  an explicit simulator observation.

## Files Likely To Change

- `tina-sim/src/lib.rs`
- `tina-sim/tests/`
- `.intent/SYSTEM.md`
- `ROADMAP.md`
- `CHANGELOG.md`

## Areas That Should Not Be Touched

- `tina` trait boundary
- mailbox crates
- Betelgeuse backend internals
- multi-shard runtime code
- Tokio bridge code

## Report Back

- exact scripted TCP config surface
- exact replay artifact additions
- exact peer-output observation surface
- whether 017 was a prerequisite or already integrated
- what is directly proved vs surrogate-supported
- exact verification results
