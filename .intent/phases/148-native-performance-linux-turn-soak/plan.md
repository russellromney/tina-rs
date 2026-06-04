# Phase 148: Native Performance, Linux Rows, And Soak Truth

## Status

- Session A plan on `main`.
- Phase 147 is merged.
- Current truth:
  - Tina HTTP/1 has useful local perf evidence.
  - The old worker-loop sleep tax is fixed.
  - Fresh read-buffer and clone-before-write waste are fixed for migrated HTTP/1 paths.
  - Small no-metrics close responses can use terminal `TcpWriteClose`.
  - HTTP/1 still shows too many observed stages.
  - Whole-process allocation rows still show Tina HTTP allocating more than Axum.
  - Linux/x86 perf evidence is not yet recorded.
  - `mini_saas_api` already has the best whole-service perf/soak seam.

## Grug Truth

Do not say "Tina is fast" because we want it.

Make the cost visible. Reduce cost where the code is dumb. Run the same rows
again. Keep pressure, leak, and shutdown truth attached.

No production-performance claim yet.

## Goal

A user can run one native perf suite and one service soak suite and see:

- native Tina-vs-bounded-Tokio rows;
- HTTP/1 stage counts;
- whole-process allocation count and allocated bytes;
- Linux/x86 rows when run on Linux;
- short soak proof in CI-sized time;
- opt-in long soak command;
- `mini_saas_api` whole-service proof with typed capacity and shutdown truth;
- at least one real reduction in HTTP turn count or allocation cost.

## Do Not Change

- HTTP wire behavior.
- HTTP parser strictness.
- keepalive reuse/retire/closed truth.
- body-pressure accounting.
- `Full` / `Closed` / `Timeout` / cancel meaning.
- trace stable tags except append-only if a new fact is required.
- simulator replay determinism.
- performance rows into a production claim.

## Rock 0: Move Perf History To The Next Slice

Phase 147 history exists. Phase 148 needs its own append-only history.

Build:

- create `.intent/phases/148-native-performance-linux-turn-soak/perf_history.jsonl`;
- create `.intent/phases/148-native-performance-linux-turn-soak/perf_sample.txt`
  with one `perf-compare`, one `perf-process`, and one `hotpath` line;
- update `scripts/perf_record.sh` / `scripts/perf_check.sh` default history path
  to Phase 148;
- teach `perf_record.sh` to record `hotpath` rows too, including
  `stage_count`, host/process allocations, platform, arch, profile, and git sha;
- teach `perf_check.sh` to check hotpath stage-count and process-allocation
  rows with loose thresholds, not only `perf-compare` p50 rows;
- keep Phase 147 history untouched;
- add dry-run parser proof if the path or row schema changes.

Proof:

- `scripts/perf_record.sh --dry-run --read-from <sample>` emits Phase 148-shaped
  compare/process/hotpath rows;
- `make perf-check` works with empty/warming Phase 148 history.

## Rock 1: Sharpen HTTP Stage Proof

Phase 147 has stage rows. Make them more useful and harder to cheat.

Build:

- keep public-path probes: real listener, real socket client, real service;
- add per-family stage summaries for:
  - HTTP/1 close;
  - HTTP/1 keepalive;
  - HTTP/1 fixed body;
- record stage count, p50, process allocations, process allocated bytes;
- add loose ceilings for "obviously worse" stage count regressions;
  - use the current Phase 147 stage count plus slack, not a wall-clock p50;
- do not use strict p50 latency gates on shared machines.

Proof:

- hotpath rows print `stage_count`;
- hotpath rows are recorded into Phase 148 history;
- each HTTP row has non-empty named stages;
- close/fixed/keepalive row labels stay stable;
- HTTP stage counts are asserted with named constants:
  - start from the Phase 147 observed counts;
  - give slack for trace noise;
  - tighten a constant only after a measured improvement lands;
- a regression back to millisecond-scale local work fails loudly.

## Rock 2: Reduce HTTP Turn Count Where Honest

Attack measured stage waste, not vibes.

Allowed fixes:

- fold protocol-local continuation work inside `HttpConnection` when it crosses
  no user policy boundary;
- use terminal write-close for more small close cases if partial-write truth
  stays visible;
- remove extra close/read/write turns only when the same `Full`/`Closed`/
  `Timeout`/cancel trace truth remains;
- add a small runtime continuation primitive only if the HTTP-local fix cannot
  remove the measured cost without lying.

Not allowed:

- hidden pipelining;
- hidden retries;
- unbounded response queues;
- dropping body-pressure exactness;
- collapsing service-call boundaries into hidden callbacks.

Proof:

- before/after `hotpath_http1_*` rows are recorded in commits/review;
- at least one HTTP row improves in stage count, or at least one HTTP row
  improves in process allocations / allocated bytes;
- if neither improves, the phase is not done: stop and hand back the exact
  blocker instead of merging a "we looked" slice;
- `tina-http` keepalive, close, body, chunked, WebSocket upgrade, and DST tests
  still pass.

## Rock 3: Kill Remaining Avoidable Allocations

Do not redesign HTTP. Remove obvious small churn.

Targets:

- header/request/response construction that allocates in common empty/small cases;
- trace/projection formatting on hot perf paths;
- host-call/perf harness helper churn that pollutes rows;
- small `Vec`/`String` rebuilds inside repeated HTTP client/server test loops.

Rules:

- do not replace `HeaderMap` publicly unless the whole API migration is justified
  and tested;
- keep many-header fallback;
- keep duplicate-header and bad-header tests;
- keep process allocation rows honest.

Proof:

- process allocation rows decrease for at least one HTTP row, or review names the
  remaining source;
- parser/encoder tests still pass;
- request/response public construction remains source-compatible unless the plan
  is explicitly updated before implementation.

## Rock 4: Linux/x86 Evidence

Mac numbers are useful. Most services run on Linux.

Build:

- make `make perf-record` easy to run on Linux/x86 and store rows with
  `platform=linux arch=x86_64 profile=release`;
- add a documented command for maintainers to refresh Linux rows;
- add a manual, non-required GitHub Actions workflow that runs the perf command
  on `ubuntu-latest` and uploads/prints the JSONL rows;
- do not make ordinary `make verify` fail on p50 wobble.

Proof:

- at least one Linux/x86 row set is recorded in Phase 148 history or uploaded as
  the manual workflow artifact;
- if GitHub workflow execution is unavailable to the session, the workflow file,
  command, and expected artifact shape are still tested locally by dry-run
  parsing, and `review.md` names the missing external proof;
- `perf_check.sh` compares only matching platform/arch/profile rows.

## Rock 5: Whole-Service Soak Proof

Use the service examples we already have.

Primary service: `examples/systems/mini_saas_api`.

Build:

- keep the existing short soak public-front-door workload;
- sharpen pool coverage so the outbound pool is proven by direct pressure/report
  fields, not only "some error happened";
- add a direct notify/outbound-pool activity field to the capacity or soak
  report, such as attempted notify ops plus acquired/released/retired leases;
- attach process allocation/RSS evidence to the whole-service perf row, or emit
  explicit `unknown` fields with a test proving the report shape does not lie;
- add an opt-in long soak command:
  - 10 minutes by default when explicitly requested;
  - 1 hour when `TINA_LONG_SOAK_SECONDS=3600`;
  - never run the long soak in normal verify.
- add the `make proof-long-soak` target named in this plan.

Proof:

- short soak proves useful work, no transport timeout, typed pressure coverage,
  leak-clean shutdown, and clean terminal report;
- short soak proves the notify/outbound-pool lane by direct service/pool facts,
  not by inferring from generic errors;
- long soak command prints ops, p50/p99, capacity surfaces, RSS delta if
  available, and final-current zero;
- README names which command is CI-sized and which is opt-in.

## Rock 6: Keep The User Story Honest

Update docs/examples, not marketing.

Build:

- update `examples/systems/perf_native/README.md`;
- update `examples/systems/mini_saas_api/README.md`;
- update `ROADMAP.md` if this phase closes or sharpens a performance gap;
- update `CHANGELOG.md` only after code/proof lands.

Proof:

- docs show copy-paste commands;
- docs say "local evidence" and "not production performance claim";
- docs name remaining performance gaps.

## Required Verification

Focused:

```sh
cargo fmt --all --check
git diff --check
cargo test -p tina-http --tests -- --nocapture
cargo test --release --manifest-path examples/systems/perf_native/Cargo.toml --test hotpath -- --nocapture
cargo test --release --manifest-path examples/systems/perf_native/Cargo.toml --test perf -- --nocapture
cargo test --manifest-path examples/systems/mini_saas_api/Cargo.toml --test soak -- --nocapture
cargo test --release --manifest-path examples/systems/mini_saas_api/Cargo.toml --test perf -- --nocapture
scripts/perf_record.sh --dry-run --read-from .intent/phases/148-native-performance-linux-turn-soak/perf_sample.txt
make perf-record
make perf-check
make proof-fast
```

Blast radius:

```sh
make check
make proof-soak
make -n proof-long-soak
```

Optional, not a normal PR gate:

```sh
TINA_LONG_SOAK_SECONDS=600 make proof-long-soak
TINA_LONG_SOAK_SECONDS=3600 make proof-long-soak
```

## Done

- Phase 148 history exists and is the default perf history.
- HTTP hotpath rows are sharper and still public-path.
- At least one measured HTTP turn/allocation cost is reduced.
- Linux/x86 row support is real, and Linux rows are recorded or explicitly
  blocked by environment.
- `mini_saas_api` is the whole-service perf/soak exhibit.
- Short soak is CI-sized; long soak is opt-in.
- No leak/capacity/shutdown truth is hidden to win a benchmark.
