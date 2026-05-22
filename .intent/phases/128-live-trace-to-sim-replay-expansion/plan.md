# Phase 128: Live Trace To Sim Replay Expansion

## Status

- Future implementation plan.
- Runs after Phase 121 has at least one real load/weirdness report.
- Can also use Phase 127 protocol/session facts if they are already on main.
- Can run beside docs-only work. Do not run beside large `tina-sim::dst`
  rewrites unless this phase owns the replay API changes.

## Purpose

Make Tina's "bug in a box" workflow real for live services.

User story:

```text
I saw weird live behavior, captured the facts, replayed it in the simulator,
shrunk it, and committed the small regression case.
```

## Starting Facts

- `tina-sim::dst` already has `ReplayCase`, `ReplayReport`,
  `LiveReplayCapture`, saved replay case I/O, `check_captured_replay`,
  unsupported facts, discovery constants, seed sweep, and shrinking.
- Current live replay proof is useful but still library-shaped. Users must know
  which trace/report/facts to gather and how to wire save/replay/shrink.
- `tina-proof-harness` already records load-ish reports. Phase 121 should add
  more pressure/fairness reports. Phase 128 turns those into replay artifacts.
- Not every live fact can replay. That must be explicit data, not a silent pass.

## Includes

- a blessed capture builder for live runs
- capture from `TraceSnapshot`, shutdown/resource reports, capacity summaries,
  protocol facts, and proof-harness reports
- bounded capture limits for event/fact/string payload counts
- visible capture-source metadata: runtime kind, live/sim backend, crate/git
  revision when available, platform, schema version, and complete/partial trace
  status
- fact extractors for:
  - capacity high-water/full/final-current
  - `Full` / `Closed` / `Timeout` / `Rejected` terminal facts
  - request-scope cancellation
  - HTTP/2, gRPC, and WebSocket protocol facts
  - pool/resource lifecycle reports
  - durable recovery facts
  - shutdown choreography facts
  - topology roles, shard ids, mailbox config, and replay config
- explicit unsupported facts for live-only or not-yet-modeled behavior
- proof-harness integration that emits a capture summary and saved case
- live-derived shrink helper or blessed wrapper around existing shrink helpers
- refresh `examples/systems/system_live_replay_bugbox` into the canonical
  workflow proof
- add one Phase-121-derived real capture if Phase 121 is on main:
  hot actor/session/load pressure, not a toy counter
- docs and examples for the copied workflow

## Does Not Include

- no byte-perfect replay of arbitrary live TCP/WebSocket traffic
- no production daemon recorder
- no replay of external AWS/SQLx/reqwest side effects unless represented as
  materialized facts
- no hidden unbounded trace/fact collection
- no wall-clock determinism claim
- no replacement for `RuntimeEvent` trace as the canonical runtime truth
- no broad timeline/Perfetto exporter; that is separate trace UX work

## Must Not Change

- Existing `ReplayCase`, `ReplayReport`, and `check_replay_case` semantics stay
  stable unless a failing test proves a bug.
- Existing saved replay cases keep reading, or get a versioned migration with a
  test.
- Unsupported facts must fail closed. Do not weaken them to warnings.
- Trace hash tags append only. Never renumber stable trace/protocol fact tags.
- Simulator replay stays deterministic from visible seed + config + history +
  expected facts. No ambient defaults.

## Implementation Shape

Use user-workflow names:

```text
capture_live_run
LiveReplayCaptureBuilder
LiveReplayFact
ReplayFact
UnsupportedLiveFact
CaptureSummary
ReplayCaptureReport
shrink_captured_replay
```

Target copied path:

```rust
let capture = capture_live_run("slow peer eviction")
    .with_config(config)
    .with_trace(runtime.trace())
    .with_report(service_report)
    .with_fact(websocket_fact)
    .finish()?;

write_saved_replay_case("cases/slow-peer.case", &capture, encode_op)?;
assert_captured_replay(&capture, &capture.to_replay_case(), run_sim)?;

let shrunk = shrink_captured_replay(&capture, run_sim, check_bug)?;
write_saved_replay_case("cases/slow-peer-small.case", shrunk.capture(), encode_op)?;
```

Likely homes:

- `tina-sim/src/dst/live.rs` or adjacent `dst` module for capture/shrink API
- `tina-proof-harness` for proof-harness capture output
- `tina-sim/tests/saved_replay_cases.rs` for round-trip and mismatch proof
- `examples/systems/system_live_replay_bugbox` for the copied workflow

Rules:

- Capture is bounded. If the cap is hit, emit `CaptureFull` /
  `TraceTruncated` / `FactTruncated`.
- A truncated capture may be saved as evidence. It must not pass exact replay
  unless truncation is itself explicit expected truth and the replay produces
  the same truncation fact.
- Every capture carries seed, replay config, topology roles, materialized
  history ops, invariant text, expected event count/hash, live facts, and
  unsupported facts.
- Every capture also carries `CaptureSource`: runtime kind, live/sim backend,
  schema version, platform, optional crate/git revision, and trace completeness.
- Capture builders must not inspect arbitrary user state. Users pass facts in or
  pass existing Tina reports to blessed adapters.
- Required adapters: `with_capacity_summary(...)`, `with_shutdown_report(...)`,
  `with_protocol_report(...)`, `with_resource_report(...)`, and
  `with_proof_harness_report(...)` if the source report exists on main.
- Fact extractors are boring adapters from existing reports to `LiveReplayFact`.
- Fact comparison is by stable names and values, not debug strings.
- Missing fact, changed fact, extra unsupported fact, seed drift, config drift,
  and hash/count drift must produce different mismatch reasons.
- Shrinking live-derived cases refreshes expected event count/hash for the
  shrunk case. No stale constants.
- Shrinking must preserve the facts that prove the bug by default. If shrinking
  changes, removes, or adds facts, the shrink report names the fact delta and
  refuses the candidate unless the caller explicitly accepts that changed fact
  set.
- Saved case output is readable enough for code review.

Exact replay passes only when all of these are true:

- seed matches
- config matches
- topology/source shape matches
- materialized history matches
- expected event count/hash match
- every required live fact matches
- no unsupported live fact is present unless the expected outcome is
  unsupported
- capture was complete, or truncation is explicitly modeled and expected

Public mismatch shape must be precise, not one string:

```text
CapturedReplayMismatch
SeedDrift
ConfigDrift
HistoryDrift
TopologyDrift
SourceDrift
MissingFact
ChangedFact
UnexpectedFact
UnsupportedFact
CaptureTruncated
PartialTrace
CountDrift
HashDrift
```

## User Proof Specimens

Required:

- `examples/systems/system_live_replay_bugbox` becomes the canonical example.
  It must run a live-shaped smoke, save a case, replay it, shrink it, and keep a
  shrunk regression test.
- The canonical example must exercise at least:
  - one capacity/pressure fact
  - one lifecycle/terminal fact
  - one unsupported-fact or mismatch proof
- If Phase 121 is on main, add one load/weirdness capture from Phase 121:
  hot actor/session pressure or protocol-session pressure.

Optional only after the required proof:

- WebSocket slow-peer eviction if Phase 127 facts are ready
- HTTP/2 stream reset or flow-control blocked if Phase 127 facts are ready
- gRPC status/deadline mismatch if Phase 127 facts are ready
- pool retire/shutdown weirdness if Phase 119 facts are ready
- durable recovery truncated-tail/corrupt-tail if Phase 126 facts are ready

The specimen README must show:

```text
1. run the live smoke
2. inspect capture summary
3. save the case
4. replay the case
5. shrink the case
6. commit the shrunk regression
```

## Required Proof

- golden path: live workload -> capture -> save -> read -> replay -> compare ->
  shrink -> save shrunk case -> assert shrunk case
- capture converts to the same `ReplayCase` twice
- saved case round-trips identity, config, topology roles, history, facts,
  unsupported facts, event count, and trace hash
- changing mailbox capacity, capacity cap, topology role, or seed invalidates
  replay with a named mismatch
- missing fact fails by fact name
- changed fact fails with expected/actual
- unsupported fact fails closed and names the unsupported fact
- bounded capture cap produces explicit truncation/full truth
- truncated capture cannot pass exact replay through the default helper
- partial/missing-shard live trace is rejected or saved with explicit partial
  truth that replay checks
- shrink refreshes event count/hash and writes the refreshed constants
- shrink refuses by default when the candidate deletes or changes the fact that
  proves the bug
- real system capture includes at least one pressure/lifecycle/protocol/durable
  fact, not only trace hash
- real system shrunk regression is committed as a test
- unsupported facts cannot be accidentally dropped by `to_replay_case`
- saved cases cannot be built without config, materialized history, expected
  shape, and fact set
- docs snippets compile or are marked `ignore` with a reason
- proof-harness integration prints one grep-friendly summary line naming saved
  path, unsupported facts count, event count, trace hash, and shrink result
- blast-radius proof: existing `tina-sim::dst` replay, sweep, shrink, saved
  case, protocol fact, and live replay tests still pass

Suggested verification commands:

```sh
cargo test -p tina-sim live_replay -- --nocapture
cargo test -p tina-sim saved_replay_cases -- --nocapture
cargo test -p tina-proof-harness --tests -- --nocapture
cargo test --manifest-path examples/systems/system_live_replay_bugbox/Cargo.toml -- --nocapture
cargo test -p tina-sim --doc
```

## Docs And Examples

Update:

- `docs/tina-user-guide/08-simulation-and-dst.md`
- `docs/tina-user-guide/00-agent-quickstart.md`
- `examples/systems/README.md`
- `examples/FINDINGS.md`

Docs should teach:

```text
Do not debug by vibes.
Capture facts.
Replay facts.
Shrink facts.
Commit facts.
```

## Hostile Review Notes

- Do not ship helper-only work without the full end-to-end workflow proof.
- Do not let unsupported live facts become warnings.
- Do not compare fact debug strings.
- Do not make capture unbounded because traces are "just tests."
- Do not hide config/topology/seed outside the saved case.
- Do not hide capture source metadata outside the saved case.
- Do not call a hash match replay if required facts are missing.
- Do not let shrink remove the fact that made the live run interesting.
- Do not let a truncated capture pass as exact replay.
- Do not invent a second replay format unless saved-case versioning is tested.
- Do not turn this into trace timeline export.
- Do not leave the specimen as prose. It must run.
