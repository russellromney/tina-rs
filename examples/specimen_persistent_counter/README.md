# specimen_persistent_counter

A counter that survives a simulated process restart via snapshot +
journal. Phase A starts fresh, applies 5 increments, takes a
snapshot. Phase B simulates a restart, recovers from disk, applies
3 more increments. Final value should be 8.

The comparison: each side has the *same* on-disk story (snapshot
file + append-only journal); the difference is who owns the framing.
Tokio writes the bytes by hand. Tina uses runtime-owned
`snapshot_*` and `journal_*` calls.

## Run

```sh
cargo run --manifest-path examples/specimen_persistent_counter/Cargo.toml -- both
cargo run --manifest-path examples/specimen_persistent_counter/Cargo.toml -- tokio
cargo run --manifest-path examples/specimen_persistent_counter/Cargo.toml -- tina
```

Both sides report:

```
phase_a_final=5 snapshot_committed=true phase_b_recovered=5
phase_b_final=8 journal_records_phase_b=3 exit_clean=true
```

## Read

- [`src/tokio_impl.rs`](src/tokio_impl.rs)
- [`src/tina_impl.rs`](src/tina_impl.rs)

Both files are self-contained — recovery, increments, snapshot.

## Tokio shape

`tokio::fs` for the snapshot file and the journal file. Every byte
layout is the example's choice; every sync point is the example's
responsibility:

- `commit_snapshot` writes a temp file, calls `flush`, calls
  `sync_all`, then `rename`. (And does not fsync the parent
  directory — but a real shop should. Whether *this* shop thought
  to is the property Tina centralizes.)
- `append_journal` opens with `create + append`, writes 16 bytes,
  flushes, syncs.
- `recover` reads the snapshot, validates the length is exactly 16,
  reads the journal, validates the length is a multiple of 16,
  walks records, takes any with `index > last_journal_index`.

It works. Every decision is visible. That's also the cost: every
decision is the example's, and a different shop will make slightly
different ones.

## Tina shape

A `Counter` isolate using runtime-owned primitives:

- `snapshot_load(path).reply(...)` and `snapshot_commit(path,
  bytes, last_index).reply(...)` — the runtime owns the temp-file +
  rename + fsync dance.
- `journal_append(path, index, bytes).reply(...)` and
  `journal_replay(path).reply(...)` — the runtime owns record
  framing, torn-tail detection, replay walk.
- The `Counter` isolate just sequences these calls and tracks
  `(value, last_journal_index)` as state.

The host correlates "did my op finish?" via a `u64` op id threaded
through every continuation message and read back from a shared
`Observation` slot. That's app-specific data the runtime can't know
about — `FINDINGS.md` tracks this as typed isolate result waiter work.

## Discussion

What feels better:

- **The runtime owns durability.** Tina's `snapshot_commit` and
  `journal_append` make the temp-file + fsync + rename + parent
  fsync decisions once. The Tokio version makes them at every call
  site, and reasonable engineers will quietly disagree on what the
  decisions should be.
- **Append-before-apply is enforced by message shape.** The Tina
  counter cannot update `self.state.value` until `AppendDurable(Ok(()))`
  arrives. The "durable first, then visible" property is a state
  machine, not a discipline. The Tokio version's "increment in
  memory, then await fsync" is the correct ordering by convention
  only.
- **Recovery is a sequence of effects.** `Recover →
  SnapshotLoaded → JournalLoaded → publish` reads as a state
  machine, with each step's failure handled per-arm.

What feels worse:

- **The `op` correlator + `Observation` slot is real boilerplate.**
  Every operation has to thread a `u64` op id through its
  continuations and write a final value back through atomics. That
  pattern recurs across persistent-state isolates. A typed
  observation handle that resolves to the isolate's outcome (with a
  `Result<T, E>` payload) would retire it.
- **The continuation enum has 7 variants for 3 user-facing
  operations.** Each runtime call needs a reply variant; the
  Recover op alone touches three. A combinator that chained "do
  `journal_replay`, then publish" would compress the state machine.
- **Result-shaped variants stack up `Err(_)` arms that all do the
  same thing.** The bottom of the match has four `Err(_)` arms
  that collapse into one `publish(op); noop()`. `FINDINGS.md` tracks
  the broader continuation/pipeline sugar work.

What this suggests:

- The runtime-as-source-of-durability-decisions is the right call.
  Centralizing fsync / temp-rename / record framing in
  `tina-runtime` is the kind of work that doesn't fit anywhere else.
- The `op` correlator + atomic publish slot is the same shape as
  the side channel from `specimen_mux_client`'s arrival log. A typed
  "operation done" handle that carries a typed result would close
  the loop on both.
- Continuation enum growth keeps showing up. A combinator that
  hides the per-step variant for "linear pipelines" would help
  here, in `specimen_mini_keyspace`, and in `specimen_mux_client`.
