# Simulation And DST

Tina runs the same isolate code live and in simulation.

That is the big testing bet.

```text
same seed, same story
saved seed, saved bug
seed alone is not replay
seed + config + history + expected trace shape is replay
```

Sim proves state-machine interleavings. Live proves physics.

## What Sim Gives You

- deterministic time
- deterministic scheduling
- seeded faults (timer wake delay, local-send delay, TCP completion delay)
- byte-for-byte replay of saved seeds
- scripted I/O for TCP, UDP, DNS, TLS, signals, processes, storage faults
- tiny tests for ugly interleavings

## What Sim Does Not Prove

- kernel socket buffers
- real CPU starvation
- allocator behavior
- cgroup memory kill behavior
- live deployment behavior

For those things you still need real I/O and real processes.

## The Workflow

DST is not one huge random test. It is a normal debug loop:

```text
1. write service logic once
2. run it in sim with an explicit history
3. sweep seeds locally to find a bad one
4. save the bad seed as a ReplayCase
5. shrink the history while the bug still reproduces
6. commit the saved ReplayCase as a regression test
7. live tests still cover physics
```

The user should not feel like they are using a simulator API. They
should feel like they are putting a bug in a box.

## Pick Your Op Alphabet

Before any of the rest of this matters, you have to map the real
service onto a small explicit `enum Op { ... }`. This is the mental
move. Everything else is ceremony.

Rules:

- one variant per externally-visible action: ingress message, kicked
  burst, drained step, scripted IO arrival, deliberate clock advance.
  Do not make ops that just "let the simulator run for a while" —
  that hides timing the seeded faults need.
- each op must have a visible effect on the trace. Deleting an op
  must change the event count or trace hash. If a shrink reduces the
  history to one op without breaking the bug, the other ops were
  decorative; rebuild the alphabet.
- keep ops small and copy-friendly. `Op::Burst { size: 4 }` over
  `Op::BurstFour`. `Op::Tick(u32)` over six numbered variants.
- match isolate roles, not handler internals. The runner translates
  `Op::Burst` into "send the burst message to the source isolate";
  the case never reaches inside an isolate's state.

A good alphabet usually has three or four variants: one or two
"queue work", one "drain time", maybe one "introduce pressure". The
`burst overflow` saved case (`tina-sim/tests/saved_replay_cases.rs`)
is `Burst { size }` and `Step`. The `eiffel_replay_dst` example is
`Tick(u32)` and `Drain`. Both fit on one screen.

When the alphabet feels right, the case literal reads like a script
of the bug — and the shrink output reads like the smallest script
that still proves it.

## A ReplayCase Is A Bug In A Box

`tina_sim::dst::ReplayCase<Op>` is plain data. Everything needed to
redo a failure is visible. Use `ReplayCase::new(...).expecting(...)`
plus `ReplayConfig::with_faults(...).with_mailbox(...)` so the name
and seed are typed once and the config builds in three lines:

```rust
use tina_sim::dst::{ReplayCase, ReplayConfig};

const SOURCE: &str = "source";
const SINK: &str = "sink";

fn case() -> ReplayCase<Op> {
    let config = ReplayConfig::with_faults(my_faults())
        .with_mailbox(SOURCE, 8)
        .with_mailbox(SINK, 2);
    ReplayCase::new(
        "mailbox full under local-send delay",
        42,
        config,
        "burst of sends races a delayed delivery round",
        vec![Op::Burst, Op::Burst, Op::Drain],
        "remote_full rejection appears in the trace",
    )
    .expecting(47, 0x9c4f_2d18_aabb_ccdd)
}
```

Role names for mailboxes live as `const &'static str` next to the
case so a typo is one find/replace, not a silent runtime panic.

Rules the case must obey:

- the seed, scenario, full simulator config, mailbox capacities, and
  history are explicit Rust data — nothing the runner needs to
  reproduce the bug lives outside the case
- `case.name` and `case.seed` must match `case.history.name()` /
  `case.history.seed()`; `ReplayCase::new` enforces this and
  `check_replay_case` debug-asserts it
- `expected_event_count` and `expected_trace_hash` are pinned with
  conscious review, not generated — see [Discover The Constants](#discover-the-constants)
- the trace hash uses `tina_runtime::stable_trace_hash`, never debug
  strings
- live-only knobs do not belong in `ReplayConfig`

A case is pasteable as Rust data — copy the `case()` function into
another file, rebuild, and the bug travels.

The `Display` impls on `SweepFailure`, `ShrinkReport`, and
`ReplayMismatch` print the case as readable lines (not as Rust source).
That output is for bug reports and PR descriptions; for code, copy the
`case()` function itself.

For more nuance — when to keep `ReplayCase { ... }` struct literals,
when to skip `.expecting(...)`, how `ReplayConfig` composes with
scripted IO — the public docs on the types are the source of truth.

## The Runner

The runner is a normal function. `case.simulator_config()` returns a
`SimulatorConfig` with the case's seed already set, so the runner is
short:

```rust
use tina_sim::dst::{ReplayCase, ReplayReport};

fn run_case(case: &ReplayCase<Op>) -> ReplayReport<MyProjection> {
    let mut sim = Simulator::new(MyShard, case.simulator_config());

    // Mailbox capacities come from the case too. `mailbox(role)`
    // panics loudly if the case forgot to declare a role.
    let sink = sim.register_with_mailbox_capacity(
        Sink::default(),
        case.config.mailbox(SINK),
    );

    // drive case.history.operations(), etc.

    let projection = my_projection_of(&sim);
    ReplayReport::from_case_and_events(case, sim.trace(), projection)
}
```

`ReplayReport::from_case_and_events` fills in event count and trace
hash. The runner only owns the projection.

## Discover The Constants

A new case needs an `expected_event_count` and an
`expected_trace_hash` that nobody can guess in advance. The blessed
discovery shape is `observe_replay_case` — it runs once, returns the
report, does not compare against anything:

```rust
use tina_sim::dst::observe_replay_case;

#[test]
#[ignore] // run once to discover, then chain `.expecting(...)` on the case
fn pin_constants() {
    let report = observe_replay_case(&case(), run_case);
    println!("{}", report.pinned_constants());
    // prints two lines, e.g.
    //   expected_event_count: 34
    //   expected_trace_hash: 0xe22d12a51cd8cf10
}
```

Then chain `.expecting(34, 0xe22d_12a5_1cd8_cf10)` on the
`ReplayCase::new(...)` call, drop the `#[ignore]` test, and use
`assert_replay_case` for the regression. The next time the trace
shape drifts, the panic tells you what to look at.

## Run Same Case Twice

Once you have a case and a runner, the saved-seed regression test is
one line:

```rust
use tina_sim::dst::assert_replay_case;

#[test]
fn saved_seed_replays_bug() {
    assert_replay_case(&case(), run_case);
}
```

When the saved hash changes, the test fails with a message that names
the case, seed, scenario, invariant, expected vs actual count and hash,
and the next decision:

```text
replay case `...` no longer matches saved trace shape
  seed:      42
  scenario:  burst of sends races a delayed delivery round
  invariant: remote_full rejection appears in the trace
  config:    ReplayConfig { faults: ... }
  events:    expected 47, got 49 (diverged)
  hash:      expected 0x..., got 0x... (diverged)
  next step: decide whether behavior changed or only trace
             vocabulary/order changed, then either fix the regression
             or update expected_event_count/expected_trace_hash on the
             case.
```

Bump the constants only after that decision. PR notes should say
whether behavior changed or only trace vocabulary/order changed.

## Sweep Seeds Locally

`sweep_seeds` is the hand-cranked search for a bad seed. It is not
QuickCheck.

```rust
use tina_sim::dst::sweep_seeds;

#[test]
#[ignore] // local search, not every PR
fn seed_sweep() {
    let outcome = sweep_seeds(
        "mailbox full under local-send delay",
        0..1024,
        |seed| make_case(seed),  // pure, deterministic
        run_case,
        |report| {
            if report.output.saw_remote_full {
                Err("remote_full rejection appeared".into())
            } else {
                Ok(())
            }
        },
    );

    if let Err(failure) = outcome {
        // The failing case has refreshed expected_event_count and
        // expected_trace_hash and is ready for assert_replay_case.
        eprintln!("{failure}");
        panic!("found a bad seed; paste the case above into a regression test");
    }
}
```

Rules:

- `make_case(seed)` is pure: every operation is materialized into the
  returned `ReplayCase.history` before the simulator runs
- two calls to `make_case(seed)` produce the same visible case
- the helper has no hidden random generator

## Shrink The History

Once you have a saved bad case, shrink it down to the smallest history
that still proves the bug:

```rust
use tina_sim::dst::{ShrinkConfig, shrink_replay_case};

let report = shrink_replay_case(
    &big_case,
    ShrinkConfig::default(),
    "remote_full rejection survives",
    run_case,
    |report| report.output.saw_remote_full,
);

eprintln!("{report}");
```

The shrinker:

- preserves `name`, `seed`, `config`, `scenario`, `invariant`
- deletes one operation at a time and re-runs
- refreshes `expected_event_count` and `expected_trace_hash` on the
  shrunk case so it can be replayed by `assert_replay_case` directly
- honors `ShrinkConfig::max_attempts`

The output ends with a review step. Paste the new constants only after
confirming the smaller case still proves the same bug.

## Live Vs Sim Comparison

The full trace hash is simulator-only truth. Live runs do not replay
byte-for-byte; they cover physics.

If you want to compare live and sim behavior, project both runs into
the same semantic value:

```rust
let sim_projection = run_case(&case).output;
let live_projection = run_live_case(&case);

tina_sim::dst::assert_projection_eq("sim", &sim_projection, "live", &live_projection);
```

Rules for projections:

- keep event kind and terminal outcome visible
- do not build a projection DSL
- one explicit projection per pair of tests

## Bug Report Shape

Paste a case into a bug report by copying these fields:

```text
Replay case:
- name:
- seed:
- config:           # ReplayConfig debug
- scenario:
- history len:
- expected events:
- expected hash:
- invariant:
- command:          # cargo test path
```

This is what `SweepFailure::Display` and `ShrinkReport::Display`
already print, so you can paste their output directly.

## What To Simulate

Good Tina simulation targets:

- mailbox full
- call timeout (once 066 lands; otherwise use a domain Stop message)
- partial write
- child restart budget exhausted
- storage commit uncertain
- cross-shard delivery delay
- timer/backoff interleaving
- sharded fanout partial aggregate

## See Also

- `examples/eiffel_replay_dst` — the copyable specimen.
- `tina_sim::dst` module — `History`, `ReplayCase` (with `new` /
  `expecting` / `simulator_config`), `ReplayReport` (with
  `from_case_and_events` / `pinned_constants`), `ReplayConfig` (with
  `with_faults` / `with_mailbox` / `mailbox`), `observe_replay_case`,
  `assert_replay_case`, `check_replay_case`, `sweep_seeds`,
  `shrink_replay_case`, `assert_replays`, `delete_shrink`,
  `InvariantSuite`.
- `tina_runtime::stable_trace_hash` — the canonical fingerprint.
