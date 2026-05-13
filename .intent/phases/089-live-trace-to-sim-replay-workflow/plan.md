# 089 Live Trace To Sim Replay Workflow

## Status

- Ready to implement.
- One PR.
- Can run beside 087/088. This owns replay tooling/docs/specimen, not protocol
  or bridge behavior.

## Grug Truth

Sim-to-sim replay is good.

Live-to-sim is harder.

Live physics is not replay.

Inputs can be replayed.

Runtime completions can be recorded.

But only if the rail exposes enough facts.

Topology and config must be visible.

If facts are missing, say missing facts.

Do not pretend logs are a replay case.

## Goal

Make DST usable as an ops workflow:

```text
live run
capture replay facts
save a case
run in sim
compare
shrink
commit bug in a box
```

This builds on 069 saved replay cases. The new thing is starting from live
evidence, not a hand-written sim case.

## Non-Goals

- no general production observability platform;
- no automatic replay of arbitrary live sockets;
- no claim that kernel timing is deterministic;
- no replay of AWS/SQLx/reqwest internals unless the bridge recorded enough
  facts;
- no giant trace storage format;
- no hidden randomness;
- no replacing `ReplayCase`;
- no magic "turn any trace into a replay case".

## Vocabulary

Add small data types if missing:

- `LiveReplayCapture`;
- `LiveReplayConfig`;
- `LiveReplayFact`;
- `LiveReplayExport`;
- `LiveReplayMismatch`;
- `LiveReplayReport`.

Names can change, but the concepts must stay clear.

Facts should be explicit:

- runtime config / simulator config;
- shard topology;
- isolate roles and mailbox capacities;
- ingress messages chosen for replay;
- timer wakes if needed;
- runtime-owned I/O completions if modeled;
- expected trace count/hash or comparison predicate;
- unsupported live facts.

First form is user-guided capture. The app/test harness records typed replay
ops while the live run happens. The helper packages those ops with trace/config
facts. It must not infer arbitrary app messages from raw trace.

## Rock 0: Audit Current Replay Surface

Read:

- `tina-sim/src/dst.rs`;
- `docs/tina-user-guide/08-simulation-and-dst.md`;
- saved replay tests;
- `specimen_replay_dst`;
- runtime trace helpers.

Write down what is already enough:

- `ReplayCase`;
- `ReplayConfig`;
- `History`;
- `observe_replay_case`;
- `discover_constants`;
- `check_replay_case`;
- `shrink_replay_case`.

Do not duplicate these.

## Rock 1: Capture Format

Add a small capture/export shape.

It must be plain Rust data first.

JSON/text export is optional and only for non-generic metadata. Do not require
generic `Op: Serialize` unless the caller opts in.

Required fields:

- name;
- seed if known;
- config;
- topology;
- role/mailbox capacities;
- history operations;
- expected trace shape;
- unsupported facts list.

The caller supplies history operations. If a live event cannot be represented by
the supplied operation vocabulary, keep it in `unsupported_facts` and make replay
report say so.

## Rock 2: Projection Contract

Live trace is too detailed. Sim trace may differ in live-only event details.

Define the comparison level:

- exact stable hash only when facts are fully modeled;
- projected hash/count when live-only facts are stripped;
- typed mismatch when projection is not possible.

Do not silently ignore events. Projection is fail-closed:

- every ignored event kind must be named by the projection;
- unknown event kind means `Unsupported`, not pass;
- projection config is visible on the report.

## Rock 3: Tooling Helpers

Add helpers for the user workflow:

```rust
let capture = LiveReplayCapture::from_trace(...)?;
let case = capture.to_replay_case(...)?;
let report = check_live_replay_case(&case, runner)?;
```

Or a better shape after reading code.

Must include:

- result-returning check helper;
- assert wrapper for tests;
- discover constants path;
- saved-case print/export path;
- mismatch display that names next action.

No panic-only API.

Helpers should make the blessed path obvious:

```rust
let mut capture = LiveReplayCapture::new("case", config);
capture.record_op(MyOp::Submit { id: 1 });
let report = capture.finish_from_trace(trace)?;
```

The exact names can change. The shape should not pretend the trace knows how to
rebuild `MyOp`.

## Rock 4: Service-Shaped Proof

Add one real Tina service proof.

It must include at least one pressure/lifecycle fact:

- `Full`;
- `Closed`;
- cancel/timeout;
- late reply;
- retry timer order;
- partial aggregate.

Good candidate:

- a small live `ThreadedRuntime` service with bounded mailbox pressure, then
  equivalent sim runner with scripted ops.

The proof should show:

- live capture records explicit operations/config;
- sim replay reproduces the projected trace/invariant;
- changing config changes or invalidates replay;
- missing facts produce useful `Unsupported`/`Mismatch`, not a fake pass.

Do not use arbitrary TCP/AWS/SQLx completions for first-form proof unless that
rail already has exact scripted facts. App-level ops plus Tina-owned
pressure/lifecycle events are enough.

## Rock 5: Shrink

Hook live-derived cases into existing shrink flow.

Rules:

- shrink only materialized history ops;
- after shrinking, recompute expected count/hash through `observe` report;
- output says "review and paste these constants";
- do not shrink config/topology in first form.

## Docs

Update `docs/tina-user-guide/08-simulation-and-dst.md`.

Add copied workflow:

```text
run live specimen
capture facts
print case
run sim check
discover constants
shrink
commit saved case
```

Explain:

- live physics is not replay;
- unsupported facts are good honesty;
- bridges need their own recorded facts;
- seed alone is not replay.

## Tests

Required tests:

- capture includes config/topology/mailboxes;
- same capture converts to same replay case twice;
- changed config invalidates or changes replay visibly;
- unsupported live fact is reported;
- unknown event kind fails closed;
- projected comparison names ignored event kinds;
- service-shaped pressure/lifecycle case replays;
- shrink refreshes constants for shrunk live-derived case.

## Required Checks

- `cargo fmt --all --check`
- `cargo test -p tina-sim live_replay --tests`
- targeted runtime tests if trace helpers change
- `cargo clippy -p tina-sim --tests -- -D warnings`
- `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`

## Done Means

- user can produce a replay case from live evidence;
- missing facts are explicit;
- one service-shaped live-to-sim proof exists;
- docs show the workflow;
- no claim that arbitrary production I/O replays automatically.
