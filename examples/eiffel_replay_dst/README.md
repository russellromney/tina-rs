# eiffel_replay_dst

The copyable specimen for the Tina DST workflow.

```text
same seed, same story
saved seed, saved bug
```

The Tina side runs under `tina-sim` with seeded fault injection and a
saved `ReplayCase`. `assert_replay_case` re-runs the case and checks
the observed event count and `stable_trace_hash` against pinned
constants. Same seed, byte-identical fingerprint. Different seed,
different fingerprint.

The Tokio side runs the same nominal workload twice on a
`current_thread` runtime. Messages are deterministic; wall-clock
timings drift. There is no replay story.

## Run

```sh
cargo run --manifest-path examples/eiffel_replay_dst/Cargo.toml -- both
cargo run --manifest-path examples/eiffel_replay_dst/Cargo.toml -- tokio
cargo run --manifest-path examples/eiffel_replay_dst/Cargo.toml -- tina
```

The Tina mode prints the saved case shape, a tiny seed sweep, and a
deletion-shrink demo. The shrink output is the format you paste into a
bug report.

## Read

- [`src/tina_impl.rs`](src/tina_impl.rs) — `Op`, `case()`, `run_case`.
- [`src/tokio_impl.rs`](src/tokio_impl.rs) — same workload, no replay.
- [`tests/smoke.rs`](tests/smoke.rs) — `assert_replay_case`,
  same-seed/same-hash, different-seed/different-hash.

## ReplayCase Shape

```rust
ReplayCase {
    name: "eiffel_replay_dst saved seed",
    seed: 42,
    config: ReplayConfig {
        simulator: SimulatorConfig {
            faults: FaultConfig {
                local_send: LocalSendFaultMode::DelayByRounds { one_in: 4, rounds: 1 },
                ..Default::default()
            },
            ..SimulatorConfig::default()
        },
        mailboxes: [("producer", 16), ("sink", 16)].into_iter().collect(),
    },
    scenario: "history-driven ticks fan out into a sink under seeded delivery delays",
    history: History::new("...", 42, vec![Op::Tick(0), ..., Op::Drain]),
    expected_event_count: 54,
    expected_trace_hash: 0xc878_d2a4_3912_9480,
    invariant: "every Tick op produces one SinkMsg::Got(value) in trace order",
}
```

This is plain Rust data. Copy the literal into another file and the
bug travels — every knob the runner needs (full `SimulatorConfig`,
seed, every mailbox capacity, the operation history) is on the case.

## Copy This Into Your Bug Report

When `assert_replay_case` fires or `sweep_seeds` returns a failure,
the panic / `Display` form already includes the case shape as
readable lines (not as Rust source). Paste it into the issue:

```text
Replay case:
- name:                 eiffel_replay_dst saved seed
- seed:                 42
- config:               ReplayConfig { simulator: SimulatorConfig { ... }, mailboxes: {...} }
- scenario:             history-driven ticks fan out into a sink under seeded delivery delays
- invariant:            every Tick op produces one SinkMsg::Got(value) in trace order
- history (8 ops):      Tick(0), Tick(1), Tick(2), Drain, Tick(3), Tick(4), Tick(5), Drain
- expected events:      54
- expected hash:        0xc878d2a439129480
- command:              cargo test --manifest-path examples/eiffel_replay_dst/Cargo.toml
```

For code, copy the `case()` function in [`src/tina_impl.rs`](src/tina_impl.rs).

## Tokio Shape

A `current_thread` runtime; a producer task that writes 6 numbers into
an mpsc with 1–3ms sleeps; a consumer that records each message plus
its `Instant::now().elapsed()`. Run twice. Messages stay stable;
timings drift because there is no virtual clock.

## Tina Shape

`tina_sim::Simulator` with seeded fault injection: 1-in-3 timer wakes
get pushed by an extra millisecond, deterministically chosen by the
seed; 1-in-4 local sends get delayed by a round. The seed plus the
explicit `History` plus the visible `ReplayConfig` plus the pinned
event count and trace hash specify the run.

The fingerprint comes from `tina_runtime::stable_trace_hash` — a
deterministic hash over the typed trace events, not
`format!("{event:?}").hash(...)`.

## Why This Is The Specimen

- **Replay is a primitive, not a discipline.** Same `ReplayCase`,
  same hash, byte-for-byte. There is no "be careful where you reach
  for `Instant::now()`" rule because the simulator does not have a
  wall clock to reach for.
- **`stable_trace_hash` is the canonical fingerprint.** No
  `format!("{event:?}")` hashing; the runtime owns the stable
  serialization.
- **Faults are knobs, not happenstance.**
  `FaultMode::DelayBy { one_in: 3, by: 1ms }` makes the seed do real
  work — the test does not assert on a quiet system.
- **Bugs travel as data.** The whole case is one `struct` literal.
  Paste it into a bug report; the next reader replays it.

For the full workflow (sweep seeds, shrink history, save bad seed),
read [`docs/tina-user-guide/08-simulation-and-dst.md`](../../docs/tina-user-guide/08-simulation-and-dst.md).
