# Phase 143: Overload Bugbox Replay

Status: implemented in PR. `tina_sim::dst` now exposes overload bugbox
helpers and assertions; `system_live_replay_bugbox` uses the overload names.

## Goal

Turn overload incidents into Tina bugboxes:

```text
capture overload facts
save small case
replay in sim when supported
fail closed when not supported
shrink the case
```

Phase 128 built the live trace -> sim replay workflow. This phase points it at
the most common production failure: too much work in flight.

## Build

1. Add overload-focused capture helpers.
   - User-facing names: `capture_overload_run`, `save_overload_bug`,
     `replay_overload_bug`.
   - Captured facts include capacity high-water, full counts, broadcast report
     counts, pool waiters, request-scope cancellations, and relevant protocol
     facts.
   - Unsupported facts are listed and make replay fail closed.

2. Add assertions for copied tests.
   - `assert_no_hidden_buffering(report)` for surfaces that have a configured
     cap and observed high-water/full facts.
   - `assert_overload_visible(report)` for cases expecting `Full`/shed/timeout.
   - These return typed reports; panic wrappers are thin test sugar.

3. Add two real saved cases.
   - Broadcast/slow-peer overload from Phase 141 or an existing chat specimen.
   - Pool/resource pressure from DB/HTTP keepalive or worker pool.
   - Each saved case has config + history + expected trace shape/hash.

4. Update systems/specimens.
   - At least one system prints a one-line overload bugbox path on failure.
   - Examples/FINDINGS should say which overload facts are now captured and
     which remain unsupported.

## Must Not

- Do not silently drop unsupported live facts.
- Do not create a generic "replay all live traces" claim.
- Do not make capture unbounded. Captured facts have a configured max.
- Do not hide pressure policy. Reports must name `Full`, `Closed`, `Timeout`,
  `Cancelled`, or `Unsupported`.

## Proof

- Unit tests for supported fact projection and unsupported-fact failure.
- DST/sim saved-case tests for two overload cases.
- Live specimen test captures then replays one small overload incident.
- Shrink proof: removed non-essential operations refresh expected count/hash.
- Failure-message test: a user gets the command/path needed to replay.
