# eiffel_replay_dst

A demonstration of the *capability* the two ecosystems don't share:
deterministic replay.

The Tina side runs under `tina-sim` with seeded fault injection. Two
runs at the same seed produce byte-identical traces — same length,
same `stable_trace_hash`. A run at a *different* seed produces a
different fingerprint, which proves the property is non-trivial.

The Tokio side runs the same nominal workload twice. Messages are
deterministic; wall-clock timings are not.

## Run

```sh
cargo run --manifest-path examples/eiffel_replay_dst/Cargo.toml -- both
cargo run --manifest-path examples/eiffel_replay_dst/Cargo.toml -- tokio
cargo run --manifest-path examples/eiffel_replay_dst/Cargo.toml -- tina
```

You'll see something like:

```
side=tina  seed_a=42 run_a1_events=64 run_a2_events=64 fingerprints_match=true
           seed_b=99 run_b1_fingerprint_differs=true messages_received=6
side=tokio messages_match=true timings_match=false
           run1_us=[8, 3142, 8200, 12699, 16521, 25867]
           run2_us=[18, 3837, 8908, 12119, 15787, 20855]
```

`fingerprints_match=true` is the Tina property. `timings_match=false`
is the Tokio property — and the README *won't* assert it (under
unusually quiet systems both runs can land on identical microsecond
boundaries by accident; the smoke test asserts only what's
non-flakey).

## Read

- [`src/tokio_impl.rs`](src/tokio_impl.rs)
- [`src/tina_impl.rs`](src/tina_impl.rs)

## Tokio shape

A `current_thread` runtime; a producer task that writes 6 numbers
into an mpsc with 1–3ms sleeps; a consumer that records each message
plus its `Instant::now().elapsed()`. Run twice. Messages stay
stable; timings drift because there is no virtual clock.

## Tina shape

`tina_sim::Simulator` with seeded fault injection: 1-in-3 timer
wakes get pushed by an extra millisecond, deterministically chosen
by the seed; 1-in-4 local sends get delayed by a round. The seed is
the input; the trace is the output.

The fingerprint comes from `tina_runtime::stable_trace_hash(...)`
(047 Rock 3) — a deterministic hash over the typed trace events,
not `format!("{event:?}").hash(...)`.

## Discussion

What feels better:

- **Replay is a primitive, not a discipline.** Same seed, same
  trace, byte-for-byte. There is no "be careful where you reach for
  `Instant::now()`" rule because the simulator doesn't have a
  wall clock to reach for.
- **`stable_trace_hash` is the canonical fingerprint.** No
  `format!("{event:?}")` hashing; the runtime owns the stable
  serialization.
- **Faults are knobs, not happenstance.** `FaultMode::DelayBy {
  one_in: 3, by: 1ms }` makes the seed do real work — the test isn't
  asserting on a quiet system.

What feels worse:

- **Two parallel report shapes.** Each side observes a different
  thing (Tina: trace fingerprints; Tokio: timings drift), so each
  has its own `Report`. The shared template doesn't fit; the
  smoke tests are structurally different.
- **Continuation `_ => Tick(next)` discards `sleep`'s `Result`.**
  Per the new checklist, runtime-call replies should carry
  `Result<T, CallError>`; here we map the unit success to a typed
  `Tick(u32)` directly. For sleep this is OK (the producer treats
  any sleep failure as "still tick"), but it's the same pattern
  that `IoFailed` had before we cleaned it up elsewhere.

What this suggests:

- The replay property is the strongest single argument for the
  Tina runtime model. It's also the property that's hardest to
  *show* without a comparison; this example is the canonical "look
  at the same fingerprint twice" demo.
- The fault-injection knobs (`FaultMode::DelayBy`,
  `LocalSendFaultMode::DelayByRounds`) are the surface most worth
  growing for serious DST work — different fault modes per call
  kind, scenario presets, etc.
