# Phase 148 Review

## Plan Review 1

Findings:

- [P2] The plan could become benchmark theater if it only chases p50. It now
  makes wall-clock numbers evidence, not the main gate. Stage counts,
  allocation counts, leak truth, final-current zero, and typed pressure are the
  harder checks.
- [P2] "Reduce HTTP turn count" could hide suspension truth. The plan now only
  allows protocol-local folding when no user policy boundary is crossed, and it
  forbids hidden callbacks, hidden retries, hidden pipelining, and body-pressure
  lies.
- [P2] Linux evidence could be hand-waved. The plan now requires Linux/x86 rows
  or a concrete environment blocker, and keeps perf-check platform-scoped.
- [P2] Whole-service proof could duplicate existing systems. The plan names
  `mini_saas_api` as the primary service and sharpens it instead of creating
  another half-service.
- [P3] Strict latency gates would be flaky and stupid. The plan says loose
  "obviously worse" ceilings only; p50 is recorded for evidence.
- [P3] Allocation cleanup could accidentally force a public `HeaderMap`
  migration. The plan keeps that out unless explicitly justified before code.

Decision:

- Plan is implementation-ready. It is intentionally not a production
  performance claim.

## Plan Review 2

Findings:

- [P2] Hotpath stage counts were visible but not durable. Current
  `perf_record.sh` records `perf-compare` and `perf-process` rows, not
  `hotpath` rows, so a stage-count regression could disappear after logs roll
  away. The plan now requires recording hotpath rows into Phase 148 history and
  checking stage/process-allocation fields with loose thresholds.
- [P2] The old done condition allowed "no improvement, but blocker named" to
  merge. That is a planning/audit outcome, not an implementation phase. The
  plan now requires at least one measured HTTP turn or allocation improvement;
  if none is found, stop and hand back the blocker instead of merging.
- [P2] `mini_saas_api` pool coverage could still be inferred from generic 503s.
  That is weak user-shaped proof. The plan now requires direct notify/outbound
  pool activity fields such as attempted notify ops and acquired/released/
  retired leases.
- [P2] Linux evidence was too easy to hand-wave. The plan now requires a manual
  non-required Ubuntu workflow that uploads/prints JSONL rows, with Linux rows
  recorded in history or attached as workflow artifact. If the session cannot
  run the workflow, the missing external proof must be named.
- [P3] The plan named `make proof-long-soak` before requiring the target. It now
  explicitly requires adding the Makefile target and checking it with
  `make -n proof-long-soak`.
- [P3] Dry-run parser proof needed a stable sample input. The plan now requires
  a phase-local `perf_sample.txt` carrying compare/process/hotpath sample rows.

Decision:

- Plan is stronger and still gruglike enough: build evidence, reduce measured
  cost, prove soak/load truth, do not claim production performance.

## Implementation Review

Findings:

- [Fixed] Dirty perf evidence could lie. `perf_record.sh` originally recorded
  only `git rev-parse --short HEAD`, so a PR run from a dirty tree looked like
  clean main. The recorder now suffixes `-dirty` when tracked or untracked
  changes are present. The checked-in Phase 148 rows were regenerated with
  `git_sha=0194ebe-dirty`.
- [Fixed] Empty or warming perf history could abort `perf_check.sh` under
  `set -euo pipefail`. The history grep pipelines now tolerate no matches and
  print warming/no-history verdicts.
- [Fixed] The new HTTP response-head extra-capacity path used plain addition.
  It now uses checked addition so pathological capacities fail loudly instead
  of wrapping.
- [Confirmed okay] The HTTP allocation cleanup is narrow: coalesced buffered
  responses reserve head + body once. It does not change wire bytes,
  keepalive/close truth, or body-pressure accounting. The public
  `encode_response_head` API remains unchanged.
- [Confirmed okay] `mini_saas_api` soak proof now uses direct service facts:
  `notify.attempted`, `outbound.acquired`, `outbound.released`, and
  `outbound.retired`. Pressure counters can be zero in a healthy serial run,
  so the test no longer guesses from generic 503s.
- [Needs follow-up, not blocker] One short soak run timed out while several
  release/debug builds were compiling in parallel. A clean rerun passed with
  no timeout and `body_io_error=0`. Keep the strict `ops_timeout == 0` guard;
  if this repeats without heavy host contention, treat it as a real service or
  runtime bug.
- [Fixed after deeper review] The opt-in long soak did not print the p50/p99
  or final-current-zero proof promised by the plan. It now prints max
  per-round p50/p99, `rss_delta_kb=unknown`, `final_current_zero`, the last
  capacity/terminal report, and asserts the important current fields are zero.
- [Fixed after deeper review] The manual Ubuntu workflow uploaded rows but did
  not print them, and it installed unused rustfmt/clippy components. It now
  installs nightly minimal, runs `make perf-record`, prints the JSONL rows, and
  uploads the same file.
- [Fixed after third review] Dirty perf evidence still missed untracked files.
  The recorder now treats tracked, staged, and untracked changes as dirty. A
  probe with an untracked `.tina_perf_dirty_probe` file emitted
  `git_sha=3372f89-dirty`.
- [Fixed after third review] The coalesced response encoder still called
  `reserve_exact` after reserving head + body capacity. This was probably
  harmless but made the optimization less direct. The redundant reserve is gone,
  and the exact-capacity encoder test still passes.

Measured evidence:

- Phase 145 fixed-body process allocations were about 1858-1871 per sampled
  row. Phase 148 recorded about 1478-1484 after presizing coalesced buffered
  writes.
- Hotpath stage ceilings are loose and named. Current recorded rows include:
  close `stage_count=28`, keepalive `stage_count=100`, fixed-body
  `stage_count=34`.
- Phase 148 history now contains 49 rows: 7 compare, 36 process, and 6
  hotpath rows.
- Linux/x86 rows were not run locally. The manual Ubuntu workflow is present
  and uploads `.intent/phases/148-native-performance-linux-turn-soak/perf_history.jsonl`.

Verification run:

- `cargo fmt --all --check`
- `git diff --check`
- `scripts/perf_record.sh --dry-run --read-from .intent/phases/148-native-performance-linux-turn-soak/perf_sample.txt`
- `touch .tina_perf_dirty_probe && scripts/perf_record.sh --dry-run --read-from .intent/phases/148-native-performance-linux-turn-soak/perf_sample.txt | rg '"git_sha":"[^"]+-dirty"' && rm .tina_perf_dirty_probe`
- `cargo test -p tina-http encode_response -- --nocapture`
- `make -n proof-long-soak`
- `TINA_LONG_SOAK_SECONDS=0 cargo test --manifest-path examples/systems/mini_saas_api/Cargo.toml --test long_soak -- --ignored --nocapture`
- `cargo test --manifest-path examples/systems/mini_saas_api/Cargo.toml --test smoke -- --nocapture`
- `cargo test --manifest-path examples/systems/mini_saas_api/Cargo.toml --test soak -- --nocapture`
- `cargo test --release --manifest-path examples/systems/perf_native/Cargo.toml --test hotpath -- --nocapture`
- `cargo test --release --manifest-path examples/systems/perf_native/Cargo.toml --test perf -- --nocapture`
- `cargo test --release --manifest-path examples/systems/mini_saas_api/Cargo.toml --test perf -- --nocapture`
- `cargo test -p tina-http --tests`
- `make perf-record`
- `make perf-check`
- `make proof-fast`

Decision:

- Phase 148 is done enough to review. It improves one real HTTP allocation
  source, records the evidence, adds Linux collection plumbing, and sharpens
  whole-service soak proof. It still does not claim production performance.
