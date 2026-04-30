# 016 Voyager Virtual Time And Replay

Session:

- A

## In Plain English

Mariner proved that `tina-rs` can run real single-shard workloads with
explicit effects, runtime-owned I/O, runtime-owned time, supervision, and
deterministic runtime traces.

Voyager is where that discipline is supposed to pay off.

The first Voyager package should not try to simulate every runtime capability
at once. The smallest honest simulator slice is:

- virtual time
- the shipped `Sleep { after }` call contract
- deterministic execution
- replay artifacts
- one real user-shaped workload that already exists in the runtime today

That workload should be the retry/backoff path landed in 015.

Why start there:

- it exercises runtime-owned time directly
- it has a real user-shaped behavior, not just a primitive wakeup
- it avoids dragging TCP simulation in too early
- it lets `tina-sim` prove something the live runtime cannot: exact replay
  under virtual time

So 016 introduces the first `tina-sim` surface as a single-shard simulator for
the existing runtime-owned timer call family. It should execute the same
semantic model as `tina-runtime-current` for timer-driven workloads, but under
virtual time and replayable config/event-record state.

This is the moment where `tina-rs` starts becoming "Tina with DST payoff" and
not just "a disciplined small runtime."

## What Changes

- A new `tina-sim` crate lands.
- `tina-sim` executes a single-shard subset of the existing runtime event
  model rather than inventing a second user-visible semantics.
- The first simulated capability is runtime-owned `Sleep { after }`
  / `TimerFired`, not TCP.
- The simulator owns:
  - virtual monotonic time
  - deterministic execution state
  - replay artifact capture
- A retry/backoff workload that already exists in `tina-runtime-current`
  gets a simulator-backed proof:
  - same logical workload
  - virtual time instead of wall clock
  - config captured for replay
- Replay artifacts for this slice include at least:
  - simulator config
  - resulting trace or equivalent reproducible event record
  - final virtual time

## What Does Not Change

- `tina` stays the trait boundary. Voyager does not add simulator-only public
  semantics to `tina`.
- The simulator uses the real runtime event vocabulary as its meaning model.
- `tina-runtime-current` remains the shipped live runtime; 016 does not replace
  it.
- This slice does not simulate TCP yet.
- This slice does not introduce failure injection yet.
- This slice does not implement multi-shard simulation yet.
- This slice does not publish a final checker DSL or a full PRNG-tree design.
- This slice does not broaden into release-story work or bridge work.

## Acceptance

- A `tina-sim` crate exists and can execute a single-shard timer-driven
  workload under virtual time.
- The simulator supports the shipped `Sleep { after }` semantics:
  - no early wake
  - one-shot wake
  - deterministic equal-deadline ordering
- The retry/backoff workload runs under the simulator and reaches the same
  logical outcome as the live runtime.
- Repeated runs with the same workload/config produce the same event record.
- Replay from a saved config/event record shape reproduces the same event
  record.
- The simulator proof asserts on event/trace meaning, not just final state.
- Blast-radius proof shows existing Mariner runtime proofs still pass.

## Notes

- This is intentionally a first Voyager foothold, not "full DST."
- Starting with timers is a feature, not a shortcut: it forces the simulator to
  own virtual time immediately, which is one of the load-bearing Tina promises.
- `SimulatorConfig.seed` is reserved in the artifact shape now, but 016 does
  not yet consume it. Seed-driven perturbation belongs to later Voyager slices.
