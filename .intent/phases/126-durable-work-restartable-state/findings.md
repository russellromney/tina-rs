# Phase 126 findings: durable local state and IPC

What shipped, what was reverified, and the ergonomic / capability gaps worth a
follow-up. Written against the first form of `DurableOutbox`.

## What shipped

- `tina_runtime::DurableOutbox<W>` and its lifecycle types. Sync state machine:
  it frames the durable record and consumes the append result; Tina keeps the
  `journal_append` / `journal_replay` I/O. At-least-once, bounded, not a mailbox.
- Append-before-apply as a type rule, with trybuild proofs. Idempotent complete.
  Recovery report separating clean / truncated-repaired / uncertain-commit, with
  corrupt and over-capacity rejected by name.

## Reverified, not rebuilt (blast radius)

File streaming under a cap, the Unix socket sidecar, framed local IPC, the
snapshot/journal corruption / truncation / replay paths, and the simulator
durable-image replay were all first-form already. They still pass unchanged; no
recovery outcome was renamed. Tests: `tina-runtime` persistence (11) + lib
(499), `tina-codec` (34), `tina-sim` persistence_simulation (7), and
`specimen_local_io_codec_ipc` (8).

## Gaps worth a follow-up

### 1. Journal compaction is out of the first form (capability gap)

The outbox is journal-only. The in-memory `completed` set is watermark-pruned so
it stays bounded, but the **on-disk journal grows without bound** until something
truncates it. Pending capacity is bounded; the log is not. A compaction snapshot
(fold completed work away, rewrite the journal to its pending tail) is the
missing piece. This is the same shape the existing snapshot+journal helpers use;
the outbox just does not own it yet.

### 2. `UncertainCommit` provenance is caller-carried (ergonomic gap)

`recover` takes `CommitConfidence` as an input. To report `UncertainCommit`
honestly across a restart, the caller must persist its last commit's confidence
itself (a commit fence) — the outbox does not own that disk marker in the first
form. This is honest (the outbox surfaces exactly what it was told) but pushes
bookkeeping onto every user. A small `commit_fence` helper in `persistence`
(write intent, clear on clean dir-fsync, read leftover on recovery) would make
`UncertainCommit` turnkey and would naturally pair with compaction (gap 1).

### 3. No built-in resume driver (ergonomic gap)

`RecoveryReport.pending` is a `Vec<RecordedWork>`. Resuming N items durably means
either one append in flight at a time (to keep journal indices strictly
increasing) or a batch the caller assembles. The integration test hand-rolls a
one-at-a-time `drive_resume`. A bounded "drain pending, re-apply, re-complete"
helper would remove that boilerplate and pin the index-ordering rule in one
place.

### 4. `DurablePayload` is hand-encoded (minor ergonomic gap)

Application payloads implement `to_durable_bytes` / `from_durable_bytes` by hand.
Fine and dependency-free for the first form. A `serde` feature bridge or a derive
would lower the adoption cost without changing the honesty boundary.

### 5. Double framing (minor, accepted)

The outbox frames `[tag][work_id][payload]` inside the journal's own
`[magic][index][len][checksum][payload]` framing. Two layers, one extra header
per record. Acceptable for the first form; a dedicated journal record kind could
collapse it later if the overhead ever matters.

### 6. Standalone webhook-outbox specimen deferred (documentation gap)

The enqueue → send → mark-sent → restart → resume-unsent flow is proven end to
end in `tina-runtime/tests/durable_outbox.rs`. A runnable `examples/` crate (dual
tina/tokio impl, matching the other specimens) would document it for users; it is
polish, not proof, so it was deferred.
