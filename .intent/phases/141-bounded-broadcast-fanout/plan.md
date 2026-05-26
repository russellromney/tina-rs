# Phase 141: Bounded Broadcast Fanout

## Status

Ready for review. `BroadcastTargets`, `BroadcastTracker`,
`BroadcastReport`, `broadcast_observed`, the chat specimen migration, docs,
and focused proofs are in the PR.

## Goal

Make the common "send this to many sessions/subscribers" path Tina-shaped:

```text
service chooses max targets
every target attempt is observed
report names accepted/full/closed
no hidden queue
```

This is the Bluesky/unbounded-fanout lesson in code. A request may contain many
items, but the service owns the fanout bound.

## Build

1. Add a small public broadcast helper in `tina-runtime`.
   - Name it for the user job: `BroadcastTargets`, `BroadcastReport`,
     `BroadcastOutcome`.
   - Construction requires `max_targets`; raw `Vec<Address<_>>` is not accepted
     by the broadcast effect helper.
   - `try_from_iter(max_targets, iter)` fails with `TooManyTargets { max,
     attempted }` before any effect is returned.
   - The helper emits a batch of `send_observed(...)` effects and one ordinary
     continuation message per target. It does not mutate state in callbacks.

2. Add a tracker for the receiver side.
   - `BroadcastTracker<K>` is bounded by the admitted target count.
   - `record(key, SendOutcome)` returns `None` until all targets are observed,
     then returns a `BroadcastReport<K>`.
   - Duplicate/unknown keys are typed errors, not silent count drift.
   - Report helpers: `accepted()`, `full()`, `closed()`, `is_complete()`,
     `assert_all_accounted_for()`.

3. Migrate one real specimen.
   - `examples/specimen_real_io_chat` should use the helper.
   - Keep the old invariant: `accepted + full + closed == burst`.
   - The connection mailbox sizing note should become simpler: capacity is
     `broadcast_target_count + slack`.

4. Add one room/session proof.
   - Use the WebSocket member/session shape or a tiny local session isolate.
   - Prove slow peer `Full`, closed peer `Closed`, and accepted peer
     `Accepted` all land in one report.

5. Docs.
   - Update boundedness/overload docs and service patterns.
   - Add the code-review question:
     "For this loop/fanout, what is the max in-flight work, and did the service
     choose it?"

## Must Not

- Do not hide fanout behind an unbounded task list.
- Do not auto-retry `Full`.
- Do not accept a request-sized iterator directly in the effect helper.
- Do not make this WebSocket-only. It is a general observed-send helper.

## Proof

- Unit tests for target construction cap, zero cap rejection, target order,
  duplicate keys, unknown keys, report counts.
- Runtime integration test: batch effects produce observed sends and complete
  the tracker under `Accepted`/`Full`/`Closed`.
- Specimen smoke: Tina chat still reports no hidden buffering.
- Compile-fail/doc proof: the broadcast effect helper requires
  `BroadcastTargets`, so a raw request `Vec` cannot be passed directly.
