# Eiffel Persistent Counter

Paired Tokio-vs-Tina implementation of a counter that survives a simulated
process restart via snapshot + journal.

The script for both sides is the same:

```text
phase A: fresh data dir, recover (empty), increment 5 times, commit snapshot
phase B: same data dir, recover (snapshot=5), increment 3 times, exit
```

Both sides emit the same numbers, asserted in `assert_equivalent`:

```text
phase_a_final=5 snapshot_committed=true
phase_b_recovered=5 phase_b_final=8
journal_records_phase_b=3 exit_clean=true
```

Run both sides:

```bash
cargo run --manifest-path examples/eiffel_persistent_counter/Cargo.toml -- compare
```

Run one side:

```bash
cargo run --manifest-path examples/eiffel_persistent_counter/Cargo.toml -- tokio
cargo run --manifest-path examples/eiffel_persistent_counter/Cargo.toml -- tina
```

## What this comparison taught us

### Tokio side

- The framing is yours. There is no `tokio::persistence`. We invent the
  layout (16-byte snapshot header + 16-byte journal records), the
  ordering (write before update in memory), the recovery rule (apply
  records with `index > last_journal_index`), the snapshot commit shape
  (write to `.tmp`, rename, *probably* fsync the parent), and the corrupt
  /torn-tail policy (panic vs. truncate vs. ignore). Every one of these
  is a one-line decision that becomes a one-week incident if you get it
  wrong.
- "Atomic snapshot" is a vibe, not an API. We use rename-on-write. We
  do not fsync the parent directory. The code admits this in a comment.
  A junior engineer reading this would not necessarily realize the
  difference.
- Async-fs is not really helping here. `tokio::fs::File` plus
  `sync_all` does the right thing, but the ergonomics are the same as
  `std::fs` with extra `.await`s. The persistence story does not get
  better by being async.
- The shape transfers. We could pull in `sled` or `redb` and outsource
  most of the framing decisions, but that is a different blast radius.
  The Tokio side here is "what people actually write when they don't
  want a database".

### Tina side

What worked well:

- `snapshot_load` / `snapshot_commit` / `journal_append` / `journal_replay`
  are runtime-owned calls returning typed continuations. The
  `snapshot.last_journal_index` field links the two — a snapshot
  *knows* which journal records it supersedes, which is exactly the
  information a recovery rule needs and exactly what hand-rolled Tokio
  code forgets to record half the time.
- Each operation is one match arm + one continuation message. The state
  machine is legible: `Recover -> SnapshotLoaded -> JournalLoaded ->
  (idle)`. `Increment -> AppendDurable -> publish`. `CommitSnapshot ->
  SnapshotCommitted -> publish`. This is the keyspace pattern again,
  applied to durability instead of network I/O.
- "Append before apply" is enforced by the message shape. We *cannot*
  update `self.value` until `AppendDurable(Ok(()))` returns, because
  that's the only message variant where the new value is known. The
  Tokio version could trivially be written in the wrong order and only
  break under crash; the Tina version cannot.
- `JournalReplayWarning::TruncatedTail` is a typed warning, not a
  silent decision. The runtime tells you a partial write was detected
  and it kept the valid prefix. That's a real property the hand-rolled
  Tokio version simply doesn't have unless someone wrote it.

What was awkward or surprising:

- `CounterMsg` ballooned. Each operation needs:
  1. an inbound request variant (`Increment { op }`),
  2. a runtime-call continuation variant (`AppendDurable { op, index,
     value, result }`),
  3. for recovery, a chain (`Recover -> SnapshotLoaded -> JournalLoaded`
     all need to thread the same `op`).
  This is the "Continuation Enum Growth" sharp edge from the user
  guide's ergonomics page, but it lands harder here than in the
  keyspace example because durability genuinely needs more steps.
- We carry an `op: u64` correlation id through every continuation so
  the driver thread can know when the *specific* operation it requested
  has finished. There is no first-class "wait for this call to finish"
  primitive at the public threaded-runtime API surface. This is the
  third-party shape of the same problem `eiffel_supervised_worker`
  hit (slot + generation) and `eiffel_mini_keyspace` hit (`BoundAddr`
  + trace polling): the runtime knows when work is done, but the
  driver-thread side has to reach in and observe a side channel.
- `journal_append(path, index, bytes)` takes the index from the user.
  Forgetting `next_index = self.last_journal_index + 1` and writing
  `0` instead would silently never recover. The sequencing
  responsibility is shared between the user and the runtime, which is
  fine but worth flagging.
- "Simulated process restart" still requires shutting down the whole
  `ThreadedRuntime` and creating a new one against the same data dir.
  We split the work into two `run_phase()` calls. This works, but it
  reads more like "two embedded services" than "one service across a
  restart" — there is no public "warm-restart" or "re-recover" path on
  a live runtime. Probably correct (you really do want a fresh runtime
  on real restart), but the example's narrative has to make this clear
  every time.
- The `Mailbox` + `MailboxFactory` boilerplate is here too, for the
  fourth example in a row.
- Continuation closures need to capture state. `journal_append(...)
  .reply(move |result| CounterMsg::AppendDurable { op, index, value,
  result })` is reasonable, but you build that closure per call, and
  it grows quickly when the variant has more fields.

### Tokio shape vs. Tina shape, in one paragraph

The Tokio side is what people write when they don't want to take a
dependency on a database — and the shape is competent and small, but
every framing/ordering/durability decision is up to the author and
absent from the type system. The Tina side has more variants, more
ceremony, and a fatter `CounterMsg`, but `snapshot_commit` linking
`last_journal_index` to a journal `record_index` is an actual
correctness property: you cannot recover into the wrong state by
forgetting to record which records the snapshot already covers. The
Tokio side fits in a one-page review; the Tina side fits in a one-page
review too, but the second page (the runtime helpers) was already
reviewed once on your behalf.
