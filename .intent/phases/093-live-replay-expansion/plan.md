# 093 Live Replay Expansion

## Status

- Ready to implement.
- One PR.
- Can run beside 087 WebSocket and 057 gRPC if it stays in `tina-sim`, docs,
  and one narrow specimen/test.

## Grug Truth

089 gave us live capture.

Now make it useful on a real edge.

Live is not sim.

Only captured facts replay.

Missing facts must fail closed.

Do not scrape logs.

## Goal

Extend the shipped `LiveReplayCapture` workflow with one more real Tina fact set
pulled from a protocol/service workload.

The user outcome:

```text
run live thing
capture typed facts
save case
run sim projection
see mismatch or pass
shrink history
commit bug box
```

This phase should make live replay feel less like a library demo and more like
an ops/debugging workflow.

## Non-Goals

- no general trace database;
- no arbitrary live TCP replay;
- no AWS/SQLx internals replay unless the bridge records typed facts;
- no raw log parsing;
- no "ignore unknown event" default;
- no broad new simulator rail unless one tiny missing fact demands it.

## Pick One Specimen Edge

Choose one.

Preferred:

1. HTTP/2 pressure/cancel edge if it has simulator-representable facts.
2. WebSocket room pressure if 087 lands first.
3. Existing HTTP/1 keepalive/body pressure if it is easier and already stable.
4. Sharded hot-key pressure if protocol work would conflict.

Do not wait for all of them.

The case must include at least one real Tina fact:

- `Full`;
- `Closed`;
- cancel;
- timeout;
- late reply;
- retry timer order;
- partial aggregate;
- capacity high-water.

## Rock 0: Audit Current Live Replay API

Read:

- `tina-sim/src/dst.rs`;
- `tina-sim/tests/saved_replay_cases.rs`;
- `docs/tina-user-guide/08-simulation-and-dst.md`;
- the chosen specimen/test.

List what 089 already has:

- `LiveReplayCapture`;
- `LiveReplayReport`;
- `TraceProjection`;
- `UnsupportedLiveFact`;
- `SavedReplayCase`;
- `check_captured_replay`;
- saved-case read/write.

Do not rebuild those.

## Rock 1: Add Missing Fact Support

Add only facts needed by the chosen edge.

Examples:

- capacity surface snapshot fact;
- HTTP/2 stream-cap fact;
- pool pressure fact;
- timeout/cancel fact;
- runtime event projection name.

Every fact needs:

- typed representation;
- display/debug text useful in failure output;
- fail-closed behavior when missing;
- test.

If the fact cannot be replayed honestly, add it to `UnsupportedLiveFact` and
make the report say exactly that.

## Rock 2: Capture Helper Polish

Make the copied path shorter if the chosen edge shows ceremony.

Acceptable helpers:

- build capture from `ReplayCase` + live trace + capacity snapshots;
- append one typed unsupported fact;
- print saved-case constants for many cases;
- compare projected live/sim reports with clearer mismatch text.

Not acceptable:

- panic-only APIs;
- helpers that infer app operations from raw trace;
- helpers that silently ignore unknown event kinds.

## Rock 3: End-To-End Proof

Add one end-to-end test:

1. Run live or live-shaped workload.
2. Record app operations explicitly.
3. Capture config/topology/mailboxes/projection/facts.
4. Build or save replay case.
5. Run simulator projection.
6. Assert pass.
7. Mutate config or omit a fact.
8. Assert useful mismatch/unsupported output.

If live timing would be flaky, use a deterministic live-shaped `ThreadedRuntime`
test with explicit waits. Do not build a sleep-based fake pass.

## Rock 4: Shrink Path

Prove the captured case can enter existing shrink flow.

Rules:

- shrink only materialized history ops;
- recompute expected count/hash after shrink;
- output says what constants to paste;
- config/topology are not shrunk in this phase.

## Rock 5: Docs

Update `docs/tina-user-guide/08-simulation-and-dst.md`.

Add one copied workflow:

```text
capture live facts
check replay
see unsupported fact
add/support fact or narrow projection
save case
shrink
commit
```

Docs must say:

- unsupported facts are success of honesty, not failure of Tina;
- projection must name included/ignored event kinds;
- seed without config/history/facts is not replay.

## Required Checks

- `cargo fmt --all --check`
- `cargo test -p tina-sim live_replay --tests`
- targeted test for chosen specimen/crate
- `cargo clippy -p tina-sim --tests -- -D warnings`
- `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` if docs/rustdoc
  changed

## Done Means

- One real edge uses live capture end to end.
- Missing facts fail closed with useful output.
- Saved case / projection / shrink path is proved.
- Docs show the copied workflow.
