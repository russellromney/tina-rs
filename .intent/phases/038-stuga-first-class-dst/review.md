# Phase 038: Stuga Review

## Implementation Review

Verdict: Stuga closes the main proof-infrastructure gap it named.

What landed well:

- `tina_sim::dst` is small and useful, not a pretend property-test framework.
  It owns history-as-data, exact replay assertion, deletion shrinking, failure
  reports, common trace invariants, durable-image replay, visible-pressure
  detection, and semantic projection comparison.
- Existing random single-shard and multi-shard tests now use the shared
  history/replay/shrink/invariant surface.
- Persistence and TCP cancellation matrices are history-shaped and use shared
  replay/invariant checks.
- Simulator storage faults are explicit and simulator-only:
  journal append failure, snapshot commit failure, truncated tail, corrupt
  record, and commit-uncertain snapshot.
- Bridge model DST uses the same history and shrink discipline while staying
  honest that it is model DST, not Tokio determinism.
- Live-vs-sim parity now uses semantic projection comparison instead of raw
  trace equality.
- `TINA_DST_LONG=1` adds a deterministic long sweep without slowing normal
  verify.

Hostile review notes:

- The `tina_sim::dst` module is public because integration tests need it. This
  is acceptable for now, but it is still test-support API, not polished user
  API.
- Storage fault ordinal means "selected snapshot/journal operation ordinal,"
  not every possible file operation. The docs say simulator-only; keep that
  narrow.
- Deletion shrinking is intentionally simple. It can miss smaller reproducers
  that need operation simplification, but that is the right first rock.
- Differential helpers compare projections only. Tests must continue to name
  what the projection preserves so they do not hide real semantic drift.

No P1/P2 bugs found in this review pass.

## Proof

Targeted proof passed:

- `cargo test -p tina-sim`
- `cargo test -p tina-tokio-bridge --test bridge_model_dst`

Full closeout proof passed:

- `make verify`

That includes format, check, workspace tests, doctests, loom SPSC, docs, and
clippy with `-D warnings`.
