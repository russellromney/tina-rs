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

- `snapshot_load(path).then(...)` and `snapshot_commit(path,
  bytes, last_index).then(...)` — the runtime owns the temp-file +
  rename + fsync dance.
- `journal_append(path, index, bytes).then(...)` and
  `journal_replay(path).then(...)` — the runtime owns record
  framing, torn-tail detection, replay walk.
- The `Counter` isolate just sequences these calls and tracks
  `(value, last_journal_index)` as state.

Each host op is a typed request (`Recover`, `Increment`,
`CommitSnapshot`). The isolate privately sequences the IO
continuations and replies with the current value when the op
settles. The host uses `call_blocking_request` — no shared observation
slot, no op-id correlator, no host spin loop.

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
  SnapshotLoaded → JournalLoaded → reply` reads as a state
  machine, with each step's failure handled per-arm.

What feels worse:

- **The continuation enum still names every IO step.** Typed
  request/reply retires the host side channel, but the isolate still
  expands each op into explicit load/append/commit continuations.
- **The continuation enum has 4 variants for 3 user-facing
  operations.** Each runtime call needs a reply variant; the
  Recover op alone touches two. A combinator that chained "do
  `journal_replay`, then reply" would compress the state machine.
- **Result-shaped variants stack up `Err(error)` arms that differ
  only in the failure tag.** The bottom of the match has four
  `Err(error)` arms with the same body — `finish_operation(permit)`
  then `reply_to(req, CounterReply::Failed(...))` — one each for
  `CounterFailure::SnapshotLoad`, `JournalReplay`, `JournalAppend`,
  and `SnapshotCommit`. `FINDINGS.md` tracks the broader
  continuation/pipeline work.

What this suggests:

- The runtime-as-source-of-durability-decisions is the right call.
  Centralizing fsync / temp-rename / record framing in
  `tina-runtime` is the kind of work that doesn't fit anywhere else.
- Typed request replies closed the host-side observation slot. The
  remaining cost is isolate-side continuation growth for multi-step
  durable ops.
- Continuation enum growth keeps showing up. A combinator that
  hides the per-step variant for "linear pipelines" would help
  here, in `specimen_mini_keyspace`, and in `specimen_mux_client`.
