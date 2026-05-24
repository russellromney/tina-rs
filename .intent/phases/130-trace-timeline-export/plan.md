# Phase 130: Trace Timeline Export

## Status

- Future implementation plan.
- Can run beside cross-shard child ownership if it only appends trace handling
  for any new event variants after rebase.
- One PR when executed.

## Purpose

Turn Tina traces into a timeline file humans can inspect.

User story:

```text
My test or service produced a TraceSnapshot. I wrote one file, opened it in
Chrome/Perfetto, and saw shards, isolates, calls, pressure, cancellations,
restarts, and shutdown in order.
```

## Starting Facts

- `RuntimeEvent` is the canonical truth.
- `TraceSnapshot` carries events and partial/missing-shard truth.
- `RuntimeEvent` has deterministic event ids and cause ids, but no wall-clock
  timestamp.
- `tina-tracing` already maps every `RuntimeEventKind` to `tracing` events with
  stable names. Timeline export belongs there.
- `tina-sim::dst` / live replay remains the replay/debug artifact. Timeline
  export is visual inspection, not simulation truth.

## Does Not Include

- no live daemon
- no global trace sink
- no Perfetto protobuf in this phase
- no wall-clock duration claim
- no byte-level network capture
- no replacement for `RuntimeEvent` / stable trace hash
- no hidden unbounded event retention

## Decisions

- Export Chrome Trace Event JSON first.
- Use logical time:
  - `ts = event_id` in microsecond-ish units
  - duration slices use event-id distance when matching begin/end events exist
  - metadata must say `time_kind = "logical_event_id"`
- If wall-clock timestamps arrive later, add a new time mode. Do not fake it
  now.
- Keep export offline and bounded: caller passes an existing `TraceSnapshot` or
  `&[RuntimeEvent]`.
- Output is stable enough for tests, not a forever wire format.
- Event mapping must be exhaustive so new `RuntimeEventKind` variants force a
  compile update.

## Public API

Home: `tina-tracing`.

Target names:

```rust
let timeline = tina_tracing::TraceTimeline::from_snapshot(&snapshot)
    .with_name("mini-saas smoke")
    .with_capacity_summary(&capacity_summary)
    .with_shutdown_report(&shutdown_report)
    .finish();

tina_tracing::write_chrome_trace_json(&timeline, path)?;
```

Likely types:

- `TraceTimeline`
- `TraceTimelineBuilder`
- `TraceTimelineOptions`
- `TraceTimelineInput`
- `TimelineExportError`
- `write_chrome_trace_json(...)`
- `to_chrome_trace_json_string(...)` for tests/tools

Keep names user-facing. Avoid "projection", "flatten", or "slab" in public
names.

## Mapping Rules

Emit Chrome Trace Event JSON (`traceEvents` array):

- metadata rows:
  - process/runtime name
  - pid `0` for the Tina runtime
  - one tid per shard using the raw `ShardId`
  - isolate ids in event args; do not pretend isolates are OS threads
  - trace completeness and missing shards
- instant events for ordinary runtime facts:
  - `Full`
  - `Closed`
  - `Timeout`
  - `Rejected`
  - late reply / deferred reply rejection
  - restart / child lifecycle
  - shutdown / stopped
  - protocol facts
- duration slices where Tina has paired facts:
  - handler turn: `HandlerStarted` to matching `HandlerFinished` /
    `HandlerPanicked` / `HandlerReportedFailure`
  - runtime call: `CallDispatchAttempted` to `CallCompleted` / `CallFailed` /
    `CallCompletionRejected` / `CallCancelled`
  - deferred reply: `DeferredReplyCaptured` to sent/rejected/dropped
- counters:
  - pressure/capacity facts when present in `FactObserved` or supplied report
  - missing counter sources should not be invented
- flow/correlation:
  - include `event_id`, `cause_id`, `call_id`, `slot_id`, shard, isolate, and
    reason fields in `args`
  - add Chrome flow events only when the cause/call link is unambiguous

All typed reasons stay typed strings. Do not collapse to `"error"`.

## Implementation Shape

- Add a `timeline` module to `tina-tracing`.
- Add `serde_json` as a normal dependency of `tina-tracing`.
- Build JSON with structured values, not string concatenation.
- Reuse the stable-name helpers from `tina-tracing::events` where possible.
- Pair handler spans with a per-isolate stack.
- Pair call spans by `CallId`.
- Pair deferred reply spans by `DeferredSlotId`.
- If an end event has no begin, export it as an instant with
  `unmatched = "missing_begin"`.
- If a begin event has no end, export an instant/short slice with
  `unmatched = "missing_end"`. Do not panic or invent an end.
- Sort output by:
  1. logical timestamp
  2. event id
  3. emitted subevent order
- Partial traces must export with an explicit metadata event naming missing
  shards.
- Optional report inputs are explicit builder methods:
  - capacity/pressure summary
  - shutdown/lifecycle report
  - fairness/load report
  Missing reports mean "not supplied", not zero.
- Unknown/unsupported future data should be impossible for event kinds because
  mapping is exhaustive. Optional reports may be skipped only with explicit
  metadata.

CLI/example:

- Add one tiny example command under `tina-tracing/examples/export_timeline.rs`.
- It should run a small Tina workload, collect a `TraceSnapshot`/trace, write
  `target/tina-traces/<name>.trace.json`, and print the path.
- Do not require Chrome/Perfetto in CI.

Docs:

- Add a short section to `docs/tina-user-guide/19-tracing.md`:
  - trace is canonical
  - timeline is a view
  - logical time means order, not wall-clock latency
  - command to produce and open the JSON

## Required Proof

Unit tests:

- export an empty trace: valid JSON with metadata
- export a partial `TraceSnapshot`: JSON says which shards are missing
- handler start/finish becomes one duration slice
- handler panic becomes a duration slice with panic terminal truth
- unmatched handler begin/end exports visibly and does not panic
- call dispatch/completed becomes one duration slice with call kind/id
- call failed/cancelled/completion rejected keep distinct typed reasons
- deferred captured/sent/rejected/dropped keep slot id and call id
- child lifecycle/restart events appear with child shard/id/generation/ordinal
- `FactObserved` protocol facts appear as typed instants
- output ordering is deterministic
- JSON parses with `serde_json`

User-shaped test:

- run a small live/local system that does:
  - one successful call
  - one rejected/full/closed pressure fact
  - one child spawn/restart or stop
- construct or capture one partial `TraceSnapshot` and prove the exported file
  names missing shards
- export timeline JSON
- parse it and assert the visible names users need exist

Golden-ish test:

- do not pin the whole JSON blob if that makes harmless ordering fields painful
- pin a small stable subset: metadata, one duration, one counter/instant, one
  typed reason

Verification commands:

```bash
cargo test -p tina-tracing -- --nocapture
cargo test -p tina-runtime trace -- --nocapture
cargo run --example export_timeline -p tina-tracing
cargo fmt --all --check
cargo clippy -p tina-tracing --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc -p tina-tracing --no-deps
```

## Traps

- Do not imply wall-clock latency from event-id durations.
- Do not make the exporter own trace retention.
- Do not drop partial-trace truth to make the file prettier.
- Do not make timeline output part of replay/hash stability.
- Do not create a daemon or background writer.
- Do not collapse `Full`, `Closed`, `Timeout`, `Rejected`, and
  `CallerCancelled` into one error color/name.
