# Simulation And DST

Tina wants same app logic live and simulated.

That is the big testing bet.

Simulation gives:

- deterministic time
- deterministic scheduling
- seeded faults
- replay
- scripted I/O
- tiny tests for ugly interleavings

## Basic Shape

Look in `tina-sim/tests` for current API examples.

Common shape:

```rust
use tina_sim::{Simulator, SimulatorConfig};

let mut sim = Simulator::new(AppShard::default(), SimulatorConfig::default());

let addr = sim.register_with_mailbox_capacity(MyIsolate::default(), 8);
sim.try_send(addr, Msg::Start).unwrap();
sim.run_until_quiescent();

assert!(sim.trace().iter().any(|event| /* thing happened */));
```

Names may vary by test helper. The shape matters:

```text
construct sim
register isolate
send messages
advance
assert trace and outputs
```

## DST Rule

DST is not one huge random test.

Use layers:

- one deterministic happy path
- one deterministic overload case
- one deterministic failure case
- seeded randomized case
- replay artifact for any interesting failure

## What To Simulate

Good Tina simulation targets:

- mailbox full
- call timeout
- stream read failure
- partial write
- child restart budget exhausted
- storage commit uncertain
- cross-shard delivery delay

## What Cannot Be Proven In Sim

Simulation does not prove OS behavior.

It cannot fully prove:

- kernel socket buffers
- real CPU starvation
- allocator behavior
- cgroup memory kill behavior
- Fly machine behavior

That is why Tina still needs real I/O pressure tests too.

Use sim for interleavings and logic. Use real processes for physics.
