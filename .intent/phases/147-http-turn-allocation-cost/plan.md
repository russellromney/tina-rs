# Phase 147: HTTP Turn And Allocation Cost

## Status

- Implemented on `codex/phase-147-http-turn-allocation-cost`.
- Phase 146 removed the obvious fresh-read-buffer and clone-before-write waste.
- Current truth: Tina HTTP/1 still allocates more than Axum and close/keepalive
  rows are still slower/noisier.
- Rock 0 evidence shape is implemented locally.
- HTTP hotpath probes are implemented locally.
- First allocation cleanup removes benchmark-client request-format allocation
  noise and presizes common HTTP/1 request/response encoders.
- Small no-metrics buffered HTTP/1 responses now write head+body in one TCP
  write. Large buffered bodies still split to avoid copying giant bodies, and
  metrics-enabled responses still split to keep body-pressure truth exact.
- HTTP body-pressure proof row is implemented locally: declared-too-large
  requests produce typed `full`, service-pressure surfaces, and drained final
  current.
- Evidence is recorded in `perf_history.jsonl` and `commits.txt`.

## Grug Truth

Phase 146 made the dumb buffer waste go away.

Now measure and fix the next dumb things:

- perf history must say platform and bytes, or numbers lie;
- HTTP hotpath must show worker turns, or we guess;
- common HTTP requests should not heap more than they need;
- keepalive should not pay avoidable per-request setup;
- faster code must still show `Full`, `Closed`, `Timeout`, cancel, and leak truth.

Do not claim production performance.

## Goal

A user can run Tina's native perf rows and see:

- platformed history rows;
- whole-process allocation count and bytes;
- HTTP/1 close/keepalive/fixed-body stage reports;
- at least one real HTTP allocation/turn improvement;
- pressure truth still attached.

## Do Not Change

- HTTP wire semantics;
- parser strictness;
- request/response public constructors unless needed and source-compatible;
- `CallOutcome` / rejection / timeout meaning;
- trace stable tags except append-only if needed;
- body-pressure accounting;
- keepalive reuse/retire truth;
- simulator determinism.

## Rock 0: Fix Perf Evidence Shape

Implement now:

- `perf-process` prints process allocation count, process allocated bytes, and RSS.
- `perf_record.sh` stores `platform`, `arch`, `profile`, allocation bytes, and
  process allocated bytes.
- `perf_check.sh` compares only rows for the current platform/arch/profile.
- Move the default history file to this phase.
- Add a tiny parser test/proof using sample `perf-compare` / `perf-process`
  lines so history output cannot silently drop fields again.

Proof:

- `scripts/perf_record.sh --dry-run --read-from sample` emits platformed rows.
- `make perf-check` still works with the new history file.

## Rock 1: HTTP Hotpath Turn Probes

Add release-mode hotpath probes for:

- `http1_close_request`;
- `http1_keepalive_sequential`;
- `http1_fixed_body_close`.

Rules:

- Use public paths: real listener, real socket client, real service isolate.
- Use a `TraceObserver` to timestamp worker events.
- Report turn count and named stages. Names describe observed boundaries, not
  guesses.
- Keep HTTP probe overhead out of comparison rows.

Proof:

- hotpath test prints `hotpath_http1_*` rows;
- each HTTP report has non-empty stages and a bounded turn count;
- the report says whether cost looks like allocation, turn count, socket floor,
  or semantic cost.

## Rock 2: Common Header/Request Allocation Cleanup

Fix the common path only. No redesign of HTTP.

Targets:

- request parser `HeaderMap` capacity / tiny path allocation;
- response encoder capacity sizing;
- request encoder capacity sizing;
- avoid avoidable `String` / `Vec` churn in the perf request client.

Rules:

- keep fallback for many headers;
- keep duplicate `Content-Length`, `Transfer-Encoding`, `Host`, and control-byte
  tests;
- do not replace correctness with a benchmark-only shortcut.

Proof:

- parser/encoder tests still pass;
- many-header heap fallback still works;
- perf process allocations decrease, or evidence names the remaining source.

## Rock 3: Keepalive Per-Request Cost

Look for and fix only clear keepalive waste:

- per-request buffer allocation inside `KeepaliveConnection`;
- repeated request encoding allocation that can be reused safely;
- unnecessary drains that shift large buffers on partial writes.

Rules:

- no hidden pipelining;
- no unbounded queue;
- no change to retire/reuse/closed truth.

Proof:

- keepalive tests pass;
- keepalive perf row improves or names the next cost.

## Rock 4: Pressure Row Stays Attached

Add or keep one HTTP-shaped overload/perf proof row:

- service mailbox `Full`, or body cap `Full`, or keepalive pool `Full`.

Report must include submitted/completed/full/closed/timeout/cancelled/late,
high-water, and final-current where available.

Proof:

- final current returns to zero after shutdown;
- faster paths still emit typed terminal facts.

## Required Verification

```sh
cargo fmt --all --check
git diff --check
cargo test -p tina-http --tests -- --nocapture
cargo test --release --manifest-path examples/systems/perf_native/Cargo.toml --test perf -- --nocapture
cargo test --release --manifest-path examples/systems/perf_native/Cargo.toml --test hotpath -- --nocapture
make perf-record
make perf-check
make proof-fast
```

## Done

- Phase 146 missed evidence fields are fixed.
- HTTP turn cost is measured through public paths.
- At least one HTTP allocation/turn cost improves with proof.
- Small buffered response coalescing is bounded and pinned by small/large tests.
- If remaining cost is still high, the next bottleneck is named from evidence.
- HTTP overload still prints pressure/final-current truth.
