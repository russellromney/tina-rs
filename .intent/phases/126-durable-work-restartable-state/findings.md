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

## Closed in the follow-up wave

- **Journal compaction (was gap 1).** `recover_compacted` rebuilds the outbox and
  returns a compacted journal image (pending-only, re-indexed, completed dropped,
  `WorkId`s preserved, next index aligned). `persistence::commit_file_atomic`
  swaps it durably. The on-disk journal is now bounded by the live backlog, not
  the lifetime of completions.
- **Commit fence (was gap 2).** `persistence::{raise,clear}_commit_fence` +
  `commit_fence_present` and `CommitConfidence::from_fence_present` make
  `UncertainCommit` turnkey across restart — the caller persists a marker, not a
  bespoke confidence record.
- **Resume driver (was gap 3).** `RecoveryReport::into_resume` →
  `ResumeQueue::next_apply` drains pending oldest-first, applies through the
  outbox, and skips already-completed ids, pinning the one-at-a-time index rule.
- **Specimen (was gap 6).** `examples/specimen_webhook_outbox` runs the full
  flow, durable vs. hand-rolled.

## Gaps still open

### `DurablePayload` is hand-encoded (minor ergonomic gap)

Application payloads implement `to_durable_bytes` / `from_durable_bytes` by hand.
Fine and dependency-free for the first form. A `serde` feature bridge or a derive
would lower the adoption cost without changing the honesty boundary.

### Double framing (minor, accepted)

The outbox frames `[tag][work_id][payload]` inside the journal's own
`[magic][index][len][checksum][payload]` framing. Two layers, one extra header
per record. Acceptable for the first form; a dedicated journal record kind could
collapse it later if the overhead ever matters.

### Compaction is restart-time only (capability note)

`recover_compacted` compacts at recovery. A long-running process that never
restarts still grows its journal until the next restart. A live, in-place
rotation (snapshot the running outbox, swap the journal, realign `next_index`
without a recover) is the natural next step; it needs the outbox to retain
pending payloads in memory, which the current `apply`-moves-`W` design avoids.
