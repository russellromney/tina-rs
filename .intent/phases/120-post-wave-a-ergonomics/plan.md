# Phase 120: Post-Wave-A Ergonomics

## Status

- Future implementation plan.
- Runs after phases 116-119 plus the landed 121 / 127 / 128 tranche.
- Runs before the next core-capability wave if the copied service shape would
  otherwise teach stale patterns.
- One PR when executed.

## Starting Facts

- `examples/FINDINGS.md` is useful but noisy. After big waves it must separate
  current pain from solved pain.
- Systems already surface real copied-path rough spots: pending replies,
  request context, admission vocabulary, recurring ticks, shutdown, capacity
  summaries, live replay capture, fairness assertions, and session app-control
  messages.
- Phase 110 covers workflow pending helpers. This phase should use them in the
  copied path, not rebuild them.
- Phase 121 made fairness/load observable. Phase 127 made WebSocket
  server/client sessions production-shaped enough to copy. Phase 128 made live
  capture -> replay -> shrink real. This phase must teach those as one service
  path, not as three separate libraries.
- The user story is not "prettier docs." It is "a cheap model can copy one
  production-shaped service and wire the right helpers."

## Purpose

Digest protocol-client, local-I/O, codec, IPC, pool, durable-state,
fairness/load, native session, and live-replay work into the copied service
path.

The user story:

```text
I can copy one Tina service shape and get requests, sessions, limits, reports,
shutdown, and bug-in-a-box replay without stitching ten specimens together.
```

## Includes

- refresh service skeleton with:
  - outbound HTTP/2/gRPC client
  - file/codec/local IPC examples
  - mature pools
  - durable restore path
  - admission/rate policy copied path
  - native WebSocket server/client session path
  - fairness/load report assertions
  - live run capture -> save -> replay -> shrink path
- one "whole service" specimen that uses the new Wave A primitives together
- app-side session control messages for WebSocket-style session apps; no more
  magic `Text("__bootstrap__")` / `Text("__tick:N")` in copied code
- LocalSystem/service-builder shortcut for "run with live capture installed"
- user-facing live-replay aliases/docs around "capture this run", "save this
  bug", "replay this bug", and "shrink this bug"; keep the existing
  `LiveReplayCapture` type for library authors
- copied fairness/load assertions by workload type, not only raw
  `progress_gap_turns` numbers
- update prelude/import tiers
- simplify repeated setup only after repetition is proven
- replace copied snippets that still teach old/raw paths
- update systems README and findings
- cheap-model proof using the new service skeleton

## Does Not Include

- no broad new core capability
- no broad flow macro
- no release rename
- no semantic changes to protocol/pool/persistence primitives
- no WebSocket Autobahn/compliance campaign
- no raw WebSocket byte replay
- no pooled/reconnecting WebSocket client manager
- no long soak in default CI

## Blast Radius

Small-to-medium blast radius.

- Allowed: docs, examples, specimens, prelude/import guidance, copied snippets,
  findings cleanup, and small public helpers in `tina-http` /
  `tina-proof-harness` / `tina-sim::dst` when they wrap already-landed behavior.
- Not allowed: core runtime semantics, protocol behavior, resource policy
  behavior, durability semantics, or new major nouns.
- If a helper needs behavior changes in a core crate, stop and make a separate
  implementation phase.
- A small public helper is allowed only if it removes repeated safe ceremony
  without hiding request/reply authority, pressure, shutdown, trace, or replay
  facts.

## Implementation Shape

Touch copied paths, examples/docs, and the small public helper surfaces named
below. Do not invent extra helpers beyond this list.

- Add `examples/systems/system_copied_service_path` as the canonical copied
  service skeleton. Keep existing systems as evidence; do not turn
  `mini_saas_api` into the one giant example.
- Add one tiny companion crate named
  `examples/systems/system_copied_service_path_companion`. The companion owns
  the proof steps that would make the canonical skeleton hard to read. The
  README for the canonical skeleton must link to the companion and explain the
  one copied path through both.
- Across the canonical skeleton and companion, prove:
  - HTTP or gRPC request entry
  - one native protocol client path (HTTP/2/gRPC/WebSocket client session)
  - one native WebSocket session or session-like long-lived client path
  - DB/pool or durable outbox/state recovery
  - admission/rate or shared-capacity policy
  - lifecycle/readiness/shutdown report
  - fairness/load report assertion
  - live capture -> save/read -> replay -> shrink workflow
- Add a tiny session-app control lane for WebSocket apps. User-facing spelling
  should be about what the app does, not the old problem:
  - add `WebSocketSessionMsg::AppControl(WebSocketSessionControl)` with small
    built-in control events: `Start`, `Tick(u64)`, and `Drain`
  - these events are app-injected only; the WebSocket connection owner never
    emits them from wire input, never treats them as peer text, and never turns
    them into protocol facts
  - delivery is ordinary bounded app-message delivery, so `Full` / `Closed` /
    `Timeout` truth stays exactly where it is today
  - app code chooses the meaning of `Tick(u64)` and `Drain`; Tina only provides
    the non-string lane
  - keep `WebSocketSessionMsg::Shutdown` for shutdown
  - copied examples must remove string prefixes like `__bootstrap__` and
    `__tick:N`
- Add one service-builder/live-capture shortcut. User-facing spelling should be
  "capture this run":
  - add `tina_proof_harness::RunCapture`
  - `RunCapture::new("name").observer()` installs before the first event
  - `RunCapture::finish(...)` must require explicit replay inputs:
    `ReplayConfig`, materialized history, invariant text, expected trace shape,
    topology/mailbox roles, and any live facts / unsupported facts the user
    wants to preserve
  - `finish(...)` may fill source metadata, trace completeness, event count,
    live trace hash, pressure summary, and capture truncation/loss truth
  - it must install the trace observer before the first event
  - it must return the same bounded `LiveReplayCapture` / report truth, not a
    hidden global collector and not guessed config/history/facts
- Add small user-facing wrappers or docs aliases for bug workflow:
  - `capture_run(...)`
  - `save_bug(...)`
  - `replay_bug(...)`
  - `shrink_bug(...)`
  These are thin wrappers over `capture_live_run`,
  `write_saved_replay_case`, `assert_captured_replay`, and
  `shrink_captured_replay`. Do not duplicate replay semantics.
- Add fairness/load assertion helpers to the proof harness where the repeated
  shape is now clear:
  - "cold work made progress"
  - "timer kept firing"
  - "surface plateaued cleanly"
  - "no leaked capacity at shutdown"
  Names should describe the user claim, not the internal counter.
  Public helpers return `Result<..., LoadAssertionFailure>` (or a narrower
  typed error) and have panic wrappers only for tests. Failure output names the
  observed report line and the user claim that failed.
- Add a short "which noun do I use?" guide for the new primitives. Keep it
  grouped by task, not by type list:
  - "limit work"
  - "retry after Full"
  - "call a protocol client"
  - "stream local bytes"
  - "own durable state"
  - "shut down"
  - "capture a bug"
  - "prove a hot path did not starve a cold path"
  - "control a session app"
- Update prelude/import docs only where the copied path proves repetition.
- Move solved findings from `examples/FINDINGS.md` to history or mark closed
  with phase numbers.
- Update `examples/systems/README.md` so each completed system has a smoke
  command and names which Wave A primitive it exercises.
- Add executable cheap-model proof, not just instructions:
  - create one tiny completed system/specimen named
    `examples/systems/system_copied_service_path_smoke` that follows only the
    refreshed skeleton README;
  - it must compile and run by a documented command;
  - its README has a short checklist: "what I copied", "what was not obvious",
    and "what got fixed in the copied path";
  - if writing it required unstated lore, fix the copied path or docs before
    the PR is ready.
- Keep names task-shaped:
  - "call another service"
  - "limit work"
  - "read local bytes"
  - "recover state"
  - "shut down"
  - "capture this run"
  - "save/replay/shrink this bug"
  - "prove progress"
  - "control this session"
  Avoid type-index docs as the first learning path.

## Proof Shape

- systems still pass
- every edited specimen/system README command runs
- docs show one production-shaped client/server/stateful service
- cheap-model copied-path proof runs as a real smoke test
- solved pain moved out of current findings
- `system_realtime_rooms` or its successor no longer uses magic string
  `Text("__bootstrap__")` / `Text("__tick:N")` for app control
- `WebSocketSessionControl` has tests for Start, Tick, and Drain delivery
  through ordinary bounded app messages
- live capture is wired through the copied service path by one builder/helper,
  and the test proves the observer catches events from the beginning
- `RunCapture` has tests for complete capture and dropped/truncated capture
  truth; it must not hide `LiveTraceLoss`
- the copied bug workflow runs: capture -> save -> read -> replay -> shrink
- fairness/load assertions are used by a real specimen/system instead of raw
  "stare at progress_gap_turns" interpretation
- these common wrong setups become compile-fail or impossible through the
  copied path:
  - trying to use WebSocket app control by sending peer `Text("__tick:N")`
    from the skeleton path
  - trying to build a captured replay without explicit config/history
  - trying to treat `progress_gap_turns` as an automatic unfairness failure
    through the assertion helper
- the refreshed skeleton has a smoke test, a load-ish test, a shutdown test, and
  one bad-config/bad-input test
- the skeleton includes one overload path and one recovery/shutdown path,
  because those are where copied examples usually lie
- the skeleton includes a long-run/soak command documented as ignored/opt-in,
  not part of default CI
- the skeleton includes one protocol/session pressure path and one live-replay
  path; do not prove those only in separate toy examples
- the skeleton proves at least one compile-time guardrail from recent phases by
  linking to or adding a trybuild case for the copied mistake
- every changed snippet compiles or is marked `ignore` with a reason
- rustdoc links stay clean:
  `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`
- findings diff proves no stale "Eiffel" or pre-helper wording returned
- docs honestly keep these gaps open: raw WebSocket byte replay, WebSocket
  compliance/Autobahn, pooled/reconnecting WebSocket client manager, and
  broader live fact coverage

## Hostile Review Notes

- Do not make this a docs-only victory lap. The service skeleton must run.
- Do not hide Tina truth behind a giant facade. Helpers can reduce ceremony, not
  remove named pressure/cancel/reply outcomes.
- Do not add new major nouns in this phase. Small task-shaped wrappers are okay
  only when they wrap landed semantics and make the copied path harder to wire
  wrong.
- Do not leave stale current findings that describe already-solved pain.
- Do not rename `LiveReplayCapture` away for library authors. Add user-facing
  copied workflow names around it.
- Do not call a raw progress gap unfairness unless the workload makes that
  claim true. Assert workload progress first.
- Do not fake a WebSocket manager in this phase. A single explicit client
  session is landed; pooled/reconnecting managers remain separate work.
