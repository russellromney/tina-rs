# system_live_replay_bugbox

Live capture → sim replay → shrink, in one specimen.

## What this pulls on

- `tina_runtime::ThreadedRuntime::try_with_config_and_trace_observer` to
  fallibly start the worker and wire a live trace observer before the first
  event.
- `tina_proof_harness::LiveTrace` to capture the live trace shape
  (event count + `stable_trace_hash`).
- `tina_sim::dst::capture_overload_run`, `save_overload_bug`,
  `read_saved_replay_case`, `replay_overload_bug`, and
  `shrink_captured_replay` for the live-to-sim handoff.
- `tina_sim::dst::ReplayCase`, `assert_replay_case`, and
  `discover_constants` for the deterministic sim side.

The "bug in a box" is a contrived rare-drop sink that silently
discards `POISON_VALUE = 7`. The same isolate logic runs once live and
once in the sim; the saved sim case pins the trace shape with
`expected_event_count` + `expected_trace_hash`. The live capture also
saves source metadata, a capacity fact, explicit history, and an
unsupported-fact proof that fails closed.

## Commands

```sh
# Smoke (run end-to-end):
cargo test --manifest-path examples/systems/system_live_replay_bugbox/Cargo.toml

# Replay-regression PR gate (alias for the above):
make proof-replay-regression
```

The smoke prints one summary line on success, for example:

```text
bugbox live_received=4 live_events=54 live_hash=0xc878d2a439129480 \
  sim_events=54 sim_hash=0xc878d2a439129480 \
  shrunk_from=8 to=5 discovered_seeds=4 live_pressure_nonzero=false \
  capture_blocked=false unsupported_proof=true saved_bugbox=/tmp/...
```

The numbers mean:

| Field | Meaning |
| --- | --- |
| `live_received` | live sink received N non-poison values |
| `live_events`, `live_hash` | live trace shape captured via `LiveTrace` |
| `sim_events`, `sim_hash` | sim trace shape that the saved case pins |
| `shrunk_from`, `to` | live-derived shrink: original → smaller fact-preserving history |
| `discovered_seeds` | rows printed by `discover_constants` for the seed sweep |
| `live_pressure_nonzero` | `false` on a clean run; `true` if any `SendRejected`/`CallCompletionRejected`/`CallReplyRejected` event was captured. Pressure facts come from `tina_runtime::PressureSummary` via `LiveTrace::pressure_summary()`. |
| `capture_blocked` | `true` if unsupported/partial/truncated capture truth blocks exact replay |
| `unsupported_proof` | `true` when an intentionally unsupported live fact failed closed |
| `saved_bugbox` | temp saved-case path written during the run so a human/agent can replay the exact captured evidence |

## What finding the run exposes

- Live trace shape drift — adding or removing an isolate, changing an
  event kind, or shipping a new `RuntimeEventKind` variant changes
  `live_events` / `live_hash`. The smoke prints them so a regression
  is one diff away.
- Sim trace shape drift — same idea, but pinned via the saved case so
  `assert_replay_case` fails loudly with the case's history printed in
  the panic.
- Shrink failure — if the deletion shrinker stops shrinking before the
  smaller fact-preserving subset, the original history is logged so a
  coding agent reading only the smoke output can copy the failing case.
- Unsupported fact loss — the smoke adds an unsupported live-only fact
  and proves `check_captured_replay` rejects it instead of silently
  dropping it.
- `discover_constants` output — `eprintln!("{d}")` per row prints the
  commented `expected_event_count` / `expected_trace_hash` block ready
  to paste into a new `.expecting(...)` chain on a sibling case.

## Copied workflow

1. Run the live smoke with `LiveTrace` installed before the first event.
2. Inspect the capture summary line (`capture_blocked=false` means exact
   replay is allowed).
3. Save the case with `save_overload_bug`.
4. Read it back with `read_saved_replay_case` and convert it with the
   typed `ReplayConfig`.
5. Replay it with `replay_overload_bug`.
6. Shrink it with `shrink_captured_replay`.
7. Commit the shrunk saved case and the regression test that proves it.

## How to add a new saved case

1. Write a small `pub fn my_case() -> ReplayCase<Op>` without
   `.expecting(...)`.
2. Call `observe_replay_case(&my_case(), run_case)` and print
   `report.pinned_constants()`.
3. Chain `.expecting(event_count, trace_hash)` on the case.
4. Add a `#[test]` that calls `assert_replay_case(&my_case(), run_case)`.

`discover_constants` does the same thing for a batch of cases — handy
when you want a small seed sweep all at once.

## Protocol facts in this workflow

This bugbox uses application-level `Op` history; it does not exercise
protocol facts. The protocol-fact form of the same workflow lives in
`tina-sim/tests/protocol_fact.rs` and in the `tina-http` HTTP/2 path:
the connection isolate emits `ProtocolFact::Http2StreamOpened` /
`Http2StreamClosed` / `Http2StreamReset` / `Http2FlowControlFull`
through `Effect::Fact`, and the sim re-emits the same events via
`RuntimeEventKind::FactObserved`. Use
`TraceProjection::protocol_facts()` (or the named siblings) when you
want to compare only protocol behaviour rather than full trace shape.

## Findings

What felt good:
- `LiveTrace::new()` → `observer()` → `try_with_config_and_trace_observer`
  is a one-line "wire the live trace" — no `Arc<Mutex<_>>` plumbing.
- `assert_replay_case` panics with the full case history, the
  expected vs actual count + hash, and the next review step — enough
  for a coding agent to make a decision from the panic alone.
- `shrink_captured_replay` keeps the proving fact set intact while it
  refreshes the shrunk case's expected count/hash.

What felt rough:
- Pinned constants drift the second you change isolate logic; the
  workflow assumes you accept that. The shrink-and-refresh helper
  (`discover_constants`) is the same line for one case or four, so
  the cost is small in practice.
- The live runtime drains continuously — `Op::Drain` is sim-only.
  Splitting the typed `Op` alphabet across both runtimes would let a
  live workload express explicit barriers; the trade-off is that the
  alphabet would no longer be the case-level source of truth. We
  keep one alphabet on purpose.

Tina capability pulled:
- `tina_runtime::TraceObserver`, `try_with_config_and_trace_observer`,
  `stable_trace_hash`.
- `tina_sim::dst::ReplayCase`, `assert_replay_case`,
  `discover_constants`, `capture_overload_run`, `replay_overload_bug`,
  `save_overload_bug`, `read_saved_replay_case`,
  `shrink_captured_replay`, `ShrinkConfig`.
- `tina_proof_harness::live_replay::LiveTrace`.

Suggested follow-up:
- Add a typed `LiveTrace::project(filter)` helper if a future
  specimen wants to ignore RuntimeEventKind variants it does not
  care about (e.g., when a service-shaped specimen wants to compare
  only handler-started/handler-finished pairs across live/sim).
- The live path still uses `ThreadedRuntime` directly even though
  `LocalSystemBuilder::trace_observer(...).try_build()` already provides the
  fallible facade form. This example is pending that `LocalSystem` migration;
  it is not evidence of a missing framework helper.

Verdict:
- keep. The user-proof gate "live capture → save → replay → shrink"
  ships as a runnable specimen, not a doc claim.
