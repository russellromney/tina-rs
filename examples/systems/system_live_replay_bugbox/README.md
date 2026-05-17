# system_live_replay_bugbox

Live capture → sim replay → shrink, in one specimen.

## What this pulls on

- `tina_runtime::ThreadedRuntime::with_config_and_trace_observer` to
  wire a live trace observer before the first event.
- `tina_proof_harness::LiveTrace` to capture the live trace shape
  (event count + `stable_trace_hash`).
- `tina_sim::dst::ReplayCase`, `assert_replay_case`,
  `observe_replay_case`, `discover_constants`, `delete_shrink` for the
  deterministic sim side.

The "bug in a box" is a contrived rare-drop sink that silently
discards `POISON_VALUE = 7`. The same isolate logic runs once live and
once in the sim; the saved sim case pins the trace shape with
`expected_event_count` + `expected_trace_hash`, and the shrinker
reduces the original 8-op history down to its minimal trigger.

## Commands

```sh
# Smoke (run end-to-end, all five assertions):
cargo test --manifest-path examples/systems/system_live_replay_bugbox/Cargo.toml

# Replay-regression PR gate (alias for the above):
make proof-replay-regression
```

The smoke prints one summary line on success, for example:

```text
bugbox live_received=4 live_events=54 live_hash=0xc878d2a439129480 \
  sim_events=54 sim_hash=0xe0f7990dddf0fb49 \
  shrunk_from=8 to=1 discovered_seeds=4
```

The numbers mean:

| Field | Meaning |
| --- | --- |
| `live_received` | live sink received N non-poison values |
| `live_events`, `live_hash` | live trace shape captured via `LiveTrace` |
| `sim_events`, `sim_hash` | sim trace shape that the saved case pins |
| `shrunk_from`, `to` | deletion shrink: original → minimal bug-preserving history |
| `discovered_seeds` | rows printed by `discover_constants` for the seed sweep |

## What finding the run exposes

- Live trace shape drift — adding or removing an isolate, changing an
  event kind, or shipping a new `RuntimeEventKind` variant changes
  `live_events` / `live_hash`. The smoke prints them so a regression
  is one diff away.
- Sim trace shape drift — same idea, but pinned via the saved case so
  `assert_replay_case` fails loudly with the case's history printed in
  the panic.
- Shrink failure — if the deletion shrinker stops shrinking before the
  bug-preserving subset, the original history is logged so a coding
  agent reading only the smoke output can copy the failing case.
- `discover_constants` output — `eprintln!("{d}")` per row prints the
  commented `expected_event_count` / `expected_trace_hash` block ready
  to paste into a new `.expecting(...)` chain on a sibling case.

## How to add a new saved case

1. Write a small `pub fn my_case() -> ReplayCase<Op>` without
   `.expecting(...)`.
2. Call `observe_replay_case(&my_case(), run_case)` and print
   `report.pinned_constants()`.
3. Chain `.expecting(event_count, trace_hash)` on the case.
4. Add a `#[test]` that calls `assert_replay_case(&my_case(), run_case)`.

`discover_constants` does the same thing for a batch of cases — handy
when you want a small seed sweep all at once.

## Findings

What felt good:
- `LiveTrace::new()` → `observer()` → `with_config_and_trace_observer`
  is a one-line "wire the live trace" — no `Arc<Mutex<_>>` plumbing.
- `assert_replay_case` panics with the full case history, the
  expected vs actual count + hash, and the next review step — enough
  for a coding agent to make a decision from the panic alone.
- `delete_shrink` is exactly the shape the bug-shrinking workflow
  wants: one closure (`still_fails`) drives everything.

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
- `tina_runtime::TraceObserver`, `with_config_and_trace_observer`,
  `stable_trace_hash`.
- `tina_sim::dst::ReplayCase`, `assert_replay_case`,
  `observe_replay_case`, `discover_constants`, `delete_shrink`,
  `ShrinkConfig`, `ShrunkFailure`.
- `tina_proof_harness::live_replay::LiveTrace`.

Suggested follow-up:
- Add a typed `LiveTrace::project(filter)` helper if a future
  specimen wants to ignore RuntimeEventKind variants it does not
  care about (e.g., when a service-shaped specimen wants to compare
  only handler-started/handler-finished pairs across live/sim).
- Consider promoting the `with_config_and_trace_observer` setup into
  a builder shortcut on `LocalSystemBuilder`-shaped helpers.

Verdict:
- keep. Phase 108's user-proof gate "live capture → replay → shrink"
  ships as a runnable specimen, not a doc claim.
