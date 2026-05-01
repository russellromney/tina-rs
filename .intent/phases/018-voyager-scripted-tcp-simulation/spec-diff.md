# 018 Voyager Scripted TCP Simulation

Session:

- A

## In Plain English

016 gave `tina-rs` a simulator foothold with virtual time and replay for
runtime-owned timers.

017 is planned as the slice where the seed becomes real, narrow faults arrive,
and checker failures become reproducible artifacts.

018 should make the next simulator capability useful for server-shaped work:
simulate the already-shipped TCP call family without opening real sockets.

The live runtime already proved TCP bind, accept, read, write, close, listener
re-arm, bounded overlap, partial-write retry, and graceful listener shutdown.
The simulator does not need to rediscover those semantics from OS behavior. It
needs a deterministic scripted network model that lets ordinary isolates issue
the same `CallRequest` values and receive ordinary `CallResult` completions
under replay.

This slice should let the TCP echo proof shape move through `tina-sim`:

- listener binds to a scripted endpoint
- scripted peers connect
- accepted streams receive scripted bytes
- writes are captured as deterministic peer output
- close calls release simulator-owned resources
- replay artifacts reproduce the same network interaction exactly

That is the next honest Voyager step toward DST: not full chaos, not a real
kernel, but a deterministic network substrate that can drive real server
isolates through the same effect boundary.

## What Changes

- `tina-sim` grows a scripted TCP model for the existing
  `tina-runtime-current` call vocabulary.
- The simulator supports at least these call requests:
  - `TcpBind`
  - `TcpAccept`
  - `TcpRead`
  - `TcpWrite`
  - `TcpListenerClose`
  - `TcpStreamClose`
- Scripted peers become simulator-owned test inputs, not handler shortcuts.
- TCP resources remain opaque simulator/runtime ids; raw sockets do not enter
  isolate state.
- Replay artifacts capture enough scripted-network configuration and observed
  peer output to reproduce a TCP simulation run exactly.
- A simulator-backed TCP echo workload lands with assertions over:
  - event trace meaning
  - peer-visible output
  - final resource/close behavior

## What Does Not Change

- `tina` stays substrate-neutral.
- 018 stays single-shard.
- 018 does not open real sockets.
- 018 does not replace the live Betelgeuse-backed runtime proof.
- 018 does not add multi-shard networking semantics.
- 018 does not add a general packet/network DSL.
- 018 does not simulate arbitrary kernel errors unless the slice needs one
  narrow scripted failure to prove replay.
- 018 does not broaden into Gemini release-story work.
- 018 assumes 017's seeded fault/checker package has either landed or remains
  an explicit prerequisite; it should not silently fork a second replay/fault
  artifact shape.

## Acceptance

- A scripted TCP configuration surface exists in `tina-sim`.
- The simulator can drive the existing TCP call family without changing the
  `tina` trait boundary or the `tina-runtime-current` call vocabulary.
- A listener isolate can bind, accept, read, write, and close entirely under
  simulation.
- A TCP echo-shaped workload proves at least:
  - one-client echo
  - sequential multi-client echo
  - graceful listener close/stop
- Replay from the saved artifact reproduces the same event record and
  peer-visible output.
- The proof asserts trace/call meaning, not only final bytes.
- Blast-radius proof shows prior simulator timer/replay behavior still passes.

## Notes

- This is scripted network simulation, not a virtual TCP stack.
- The simulator should model the runtime-owned resource contract, not OS socket
  internals.
- Prefer a small explicit script format over callback-heavy test magic.
- If seeded faults from 017 are present, TCP simulation should compose with the
  existing fault/checker surface rather than inventing new randomness.
