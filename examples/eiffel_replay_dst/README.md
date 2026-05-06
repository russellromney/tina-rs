# Eiffel Replay DST

Paired Tokio-vs-Tina demonstration of the *capability* the two ecosystems
do not share: deterministic replay.

This is not a benchmark. The point is the property:

```text
tina-sim:  same seed in -> byte-identical trace out, every time
tokio:     same logic in -> wall-clock timings drift even on the same machine
```

The workload is small on purpose:

- one `Producer` isolate that ticks 6 times, sleeping 1–3 ms between
  each send;
- one `Sink` isolate that records arrivals;
- a `FaultConfig` that deterministically delays 1-in-3 timer wakes and
  1-in-4 local sends — the seed is "load-bearing" so different seeds
  produce different traces.

The Tokio side runs the same shape on `Builder::new_current_thread()`
with `tokio::time::sleep`. It is correct (same message sequence each
run) but the wall-clock arrival times drift.

Run both sides:

```bash
cargo run --manifest-path examples/eiffel_replay_dst/Cargo.toml -- compare
```

Run one side:

```bash
cargo run --manifest-path examples/eiffel_replay_dst/Cargo.toml -- tokio
cargo run --manifest-path examples/eiffel_replay_dst/Cargo.toml -- tina
```

Sample output:

```text
side=tina seed=42 run1_events=64 run2_events=64 fingerprints_match=true \
  seed_b=99 run_b1_fingerprint_differs_from_a=true messages_received=6
side=tokio messages_match=true timings_match=false \
  run1_us=[18, 14503, 27266, 44592, 50258, 57334] \
  run2_us=[10,  3936, 14795, 20037, 25104, 31129]
```

The `assert_replay_distinction` check pins three properties at once:

1. The Tina trace fingerprint is byte-identical across two runs of
   the same seed.
2. The Tina trace fingerprint *differs* across two distinct seeds, so
   property (1) is non-trivial.
3. The Tokio side is functionally correct (same message ordering)
   even though its wall-clock timings drift.

## What this comparison taught us

### Tokio side

- The Tokio side has no analogue. Tokio does not provide a
  deterministic-time-source replay mode; even `start_paused: true`
  on the test runtime is a paused-clock affordance, not a seeded
  scheduler. We just run the same workload twice and watch the
  microseconds wander.
- The drift is real but small. On a quiet laptop, two back-to-back
  runs of a six-message producer can land within a few hundred
  microseconds of each other or many tens of milliseconds apart,
  with no obvious pattern. There is no story for "make the next run
  reproduce the bug we just saw".
- This is the part of Tina's pitch the user has to be told about.
  The other comparisons (chat, keyspace, axum, supervised worker,
  persistent counter) all show *visible behavior* differences. This
  one is invisible until something has gone wrong, which is exactly
  when you most want it.

### Tina side

What worked well:

- `Simulator::new(shard, config)` plus `register_with_mailbox_capacity`
  plus `try_send` plus `run_until_quiescent` is the entire bootstrap.
  No threads, no runtime config, no tokio. The whole comparison runs
  on one OS thread, completes in microseconds, and produces a trace
  the test can hash.
- `SimulatorConfig::seed` plus `FaultConfig::{timer_wake, local_send}`
  is the seam where deterministic perturbation lives. Set the same
  seed → identical trace. Change the seed → different trace.
  Property and counter-property assertable in one report.
- `sim.trace()` returns `&[RuntimeEvent]` directly. Phase 047 added
  `stable_trace_hash(...)`, so the fingerprint no longer depends on
  `Debug` formatting or `DefaultHasher`. Five lines of code; the rest
  of the runtime did the work.
- `tina_runtime::sleep(Duration).reply(...)` is *also* what runs in
  live `ThreadedRuntime` — same handler code, same effect type. The
  comparison's punchline is that the shape under tina-sim is the same
  shape under tina-runtime, which is what makes the replay claim
  meaningful in production.

What was awkward or surprising:

- `Sink` had to use `#[tina_runtime::isolate(...)]` rather than the
  pure `#[tina::isolate(...)]`. The former wires
  `Call = RuntimeCall<Msg>` which `Simulator::register_with_mailbox_capacity`
  requires; the latter wires `Call = Infallible` and the simulator
  rejects it with a `type mismatch resolving <Sink as
  Isolate>::Call == RuntimeCall<SinkMsg>`. The error names the
  required bound, but a first-time reader would not know which macro
  to switch to. A "use tina_runtime::isolate for anything that lives
  in a Simulator" diagnostic would help; alternatively, lifting the
  `Call` requirement on the simulator would.
- The fault config is opaque on first read. `LocalSendFaultMode::
  DelayByRounds { one_in: 4, rounds: 1 }` is a real and useful
  perturbation, but the *meaning* ("1 in every 4 sends gets pushed
  back by 1 delivery round, deterministically chosen by seed") has
  to be looked up. Naming is fine; documentation that lists "what
  changes when I bump `one_in`" would shorten the on-ramp.
- ~~Building the trace fingerprint via `format!("{event:?}").hash(...)`
  works but is slightly cheesy.~~ **Resolved in phase 047:**
  `RuntimeEvent::stable_hash()` and `stable_trace_hash(...)` give this
  comparison a first-class trace fingerprint.
- The simulator does not expose a "current virtual time" accessor at
  the public API surface (or if it does, it isn't named in the
  examples). The only way to reason about *when* an event happened
  is to read the event itself. Fine for fingerprints; awkward for
  ergonomics like "wait for time X".
- All three runs (`run_a1`, `run_a2`, `run_b1`) fully construct a new
  `Simulator`. There is no "run twice with the same history"
  affordance at this layer (`run_twice_same_history` exists in the
  DST helpers, but it is `#[cfg(test)]` and behind types we cannot
  reach from a downstream crate). The example's
  `run_once(seed) -> (events, fingerprint)` plus three calls is
  fine, but the public surface for "I want this exact replay
  property" is not yet front-and-center.

### Tokio shape vs. Tina shape, in one paragraph

The Tokio side prints two arrays of microsecond-jittered timings
that nobody can stare at and turn into a reproducer. The Tina side
prints one fingerprint that matches between two runs and changes
when you change the seed; the same handler code, sent through
`Simulator` instead of `ThreadedRuntime`, is the test harness. That
is the property the rest of Eiffel relies on: every other
comparison in this directory could, in principle, be replayed under
seeded faults the same way.
