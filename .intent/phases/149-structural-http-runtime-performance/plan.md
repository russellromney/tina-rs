# Phase 149: Structural HTTP/Runtime Performance

## Status

- Planned after Phase 148.
- One PR if possible, but not tiny on purpose.
- Builds on Phase 148 perf history, hotpath rows, Linux workflow, and long soak.

## Grug Truth

Tina HTTP is honest now.

It is not fast enough yet.

Do not chase one pretty p50. Cut real structural cost:

- too many protocol turns;
- too many allocations;
- unclear connect/accept cost mixed with request cost;
- too little Linux evidence.

Keep boundedness, pressure, cancel, trace, and replay truth. No speed lie.

## Current Measured Truth

From Phase 148 on macOS/aarch64 release:

- raw TCP localhost close floor exists and is far below Tina HTTP rows;
- `hotpath_try_send` is cheap;
- `hotpath_call_blocking` still costs multiple runtime events but is no longer
  millisecond-sleep bad;
- HTTP/1 close/fixed-body rows are around 28-34 trace stages;
- HTTP/1 keepalive row is around 91 stages for 4 sequential requests;
- HTTP rows include client connect/accept cost;
- current `stage_count` counts trace-event gaps, not clean handler turns;
- whole-process HTTP allocation rows still show Tina above Axum;
- Linux/x86 collection exists but has not produced repeated committed rows.

Important observed stage smell:

- close/fixed/keepalive reports begin with `host_submit -> call_completed`,
  which includes connect/accept work before the first request is handled;
- keepalive's 4-request row is useful, but it does not isolate steady-state
  per-request cost on an already-open connection.

## Goal

Make native HTTP/runtime perf meaningfully better and easier to trust:

- split event count from handler turn count and backend call count;
- add steady-state keepalive request rows that reuse an open connection across
  measured ops;
- reduce at least one structural turn path, not only one Vec reserve;
- reduce at least one process-allocation row by a visible amount;
- record repeated Linux/x86 evidence or document the exact external blocker;
- keep mini service soak clean after the faster paths.

## Do Not Change

- HTTP wire bytes.
- HTTP/1 keepalive reuse/retire/close truth.
- HTTP body pressure accounting.
- request parser strictness.
- service-call boundary: user app code still runs in an isolate handler.
- `Full` / `Closed` / `Timeout` / cancel vocabulary.
- trace stable hash tags except append-only new facts.
- simulator replay determinism.
- no production-performance claim.

## Rock 0: New Perf History And Baseline

Build:

- create `.intent/phases/149-structural-http-runtime-performance/`;
- create Phase 149 `perf_history.jsonl`;
- create Phase 149 `perf_sample.txt` with compare/process/hotpath rows;
- update `scripts/perf_record.sh` and `scripts/perf_check.sh` default history
  path to Phase 149;
- keep Phase 148 history untouched;
- record a baseline before performance edits.

Proof:

- dry-run parser emits Phase 149 compare/process/hotpath rows;
- `make perf-check` works with warming Phase 149 history;
- checked-in baseline rows are marked with clean or `-dirty` git truth.

## Rock 1: Measure The Right Things

The old `stage_count` is useful but too fuzzy. It counts trace-event gaps.
Add clearer counters to every hotpath row:

- `event_stage_count`: old stage count;
- `handler_turn_count`: count `HandlerStarted`;
- `runtime_call_count`: count backend `CallDispatchAttempted` or equivalent
  call attempts;
- `service_call_count`: count isolate-call dispatch attempts;
- `completion_count`: count successful backend completions;
- `rejected_completion_count`: count completion rejections.

Keep `stage_count` as alias for compatibility if scripts already read it, but
docs must say what it means.

Build:

- extend `HotPathReport` and summary/json lines;
- extend `perf_record.sh` / `perf_check.sh` to record/check the new fields;
- keep process allocations and p50 rows;
- add row labels that separate:
  - close-per-request including connect/accept;
  - keepalive including connect/accept;
  - steady-state keepalive single request on an already-open connection;
  - fixed-body close including connect/accept.

Proof:

- hotpath test fails if any HTTP row omits the new counters;
- scripts parse sample rows with new fields;
- README says which rows include connect/accept and which isolate request cost.

## Rock 2: Steady-State HTTP Rows

Add a real "already connected" workload.

Build:

- add a hotpath probe that:
  - opens one TCP connection;
  - warms with at least one keepalive request;
  - measures N single requests over the same stream;
  - does not reconnect per op;
  - closes at the end;
- add the matching Axum/hyper row;
- record process allocations and turn counters for the steady-state row;
- keep the old connect/accept rows.

Proof:

- steady-state Tina row returns correct body for every measured request;
- steady-state baseline row returns same body;
- row labels are stable:
  - `http1_keepalive_steady_state`
  - `hotpath_http1_keepalive_steady_state`
- row docs say this is request cost after session setup, not connection setup.

## Rock 3: Terminal Completion Action Fast Path

Reduce handler turns only where no user policy boundary is crossed.

Build this narrow runtime shape, not a fake async surface:

- `RuntimeCallCompletion<M>`:
  - `Message(M)` — current behavior;
  - `StopRequester` — stop the requester isolate after the backend completion;
  - `Noop` — consume the completion and do nothing;
- `RuntimeCall::new_with_completion(request, translator)` where translator
  returns `RuntimeCallCompletion<M>`;
- trace a new append-only event when `StopRequester` or `Noop` bypasses message
  delivery;
- closed requester, missing requester, and fallback-message mailbox-full still
  record rejected truth;
- no variant returns arbitrary `Effect<I>` in this phase.

Target first uses:

- HTTP no-metrics small close response:
  - successful full `tcp_write_owned_close` can become `stop()` without a
    `WroteClose` handler turn;
  - failure or partial write falls back to the existing `WroteClose` message
    path;
- any other HTTP-local path only if state mutation is not needed or stays in the
  ordinary handler path.

Not allowed:

- no hidden service response callback;
- no hidden retry;
- no unbounded queue;
- no body-pressure release outside the proven accounting path;
- no "partial write disappeared" behavior.

Proof:

- unit tests for the new continuation shape:
  - success can stop requester without enqueuing a message;
  - success can noop without enqueuing a message;
  - fallback enqueues message;
  - requester closed records rejected truth;
  - mailbox full on fallback records rejected truth;
  - trace shows the terminal completion action as a real fact;
- HTTP tests prove small close response wire bytes still match;
- HTTP hotpath row shows fewer handler turns or fewer event stages;
- `tina-http` body/keepalive/chunked/WebSocket upgrade tests still pass;
- simulator/replay tests are updated only with append-only trace facts.

## Rock 4: HTTP Allocation Cleanup With Real Wins

Attack common allocation sources, not public API churn for fun.

Targets:

- response construction in perf service;
- request/response encoder scratch reuse;
- hotpath stage/report construction (`HashMap<String, _>` and repeated stage
  name strings);
- common small request path internal scratch allocation, without changing the
  public `HttpRequest` shape;
- repeated client test-loop buffers that pollute process rows.

Allowed changes:

- internal small buffers / scratch reuse;
- internal preallocation based on `HttpLimits`;
- add `HttpResponse::with_static_body` or `HttpResponse::with_shared_body` only
  if the perf service can use it and a measured row improves;
- a `Static`/`Shared` response body variant only if it reduces a measured row
  and keeps write/body-pressure truth.

Not allowed:

- no broad `HeaderMap` public migration unless this phase also migrates all
  callers and proves duplicates/bad headers/many headers;
- no borrowed request fields crossing isolate boundaries;
- no hiding response body bytes from body-pressure metrics;
- no allocation claim without process-allocation rows.

Proof:

- at least one HTTP process-allocation row decreases materially;
- exact-capacity encoder tests pass;
- duplicate-header, many-header, malformed-header, body cap, chunked, and
  keepalive tests pass;
- review names any remaining top allocation source.

## Rock 5: Runtime Hot-Path Allocation Cleanup

The runtime path still allocates per call/send in places that are visible in
process rows.

Targets:

- `RuntimeCall` translator boxing where the new terminal completion action can
  avoid one message allocation/delivery;
- in-flight call / translator storage reuse where safe;
- host-call dispatcher/reply pool defaults stay unchanged unless a new row proves
  contention;
- trace observer/projection allocations in instrumented rows.

Rules:

- do not weaken type erasure safety;
- do not make stale call ids possible;
- do not share translator storage across live calls without generation proof;
- do not optimize trace by dropping trace facts.

Proof:

- `hotpath_call_blocking` process allocations stay at or below current ceiling
  and improve if this rock touches it;
- request-context, cancellation, pending-call, and call-group tests pass;
- any storage reuse has stale-generation / ABA tests.

## Rock 6: Linux/x86 Evidence, Repeated

One uploaded artifact is not enough.

Build:

- run the manual perf workflow or equivalent Linux/x86 local command at least
  twice after code changes;
- if GitHub cannot be driven from the session, document the exact blocker and
  leave the workflow command copy-pasteable;
- keep `make verify` free of perf p50 gates;
- keep `perf-check` scoped by platform/arch/profile.

Proof:

- Phase 149 history contains Linux/x86 rows, or `review.md` contains the exact
  external blocker and a link/command for the missing rows;
- macOS rows and Linux rows are not compared against each other.

## Rock 7: Broader Equivalent Workloads

The current rows are too small to answer "can this be efficient?"

Add these rows:

- steady-state keepalive small response;
- steady-state keepalive fixed body;
- many concurrent keepalive clients against one listener;
- HTTP/2 unary request through Tina client/server;
- WebSocket echo round trip;
- mini service `/health` hot row with capacity/report plumbing disabled.

Each row must have an equivalent baseline or an explicit
`comparison_baseline=none` field.

Proof:

- every new row prints:
  - p50/p90/p99;
  - process allocations;
  - RSS delta if available;
  - typed pressure if any;
  - leak/shutdown truth for service-shaped rows.

## Rock 8: Soak After Faster Paths

Fast code must not leak or lie under time.

Build:

- keep `proof-soak`;
- keep `proof-long-soak`;
- add a shorter "perf soak" row:
  - high keepalive reuse;
  - stable RSS-ish delta;
  - final current zero;
  - no transport timeouts.

Proof:

- `mini_saas_api` short soak passes;
- opt-in long soak command remains documented;
- if a soak timeout repeats without host contention, fix the bug before merge.

## Rock 9: Docs And Claims

Update:

- `examples/systems/perf_native/README.md`;
- `examples/systems/mini_saas_api/README.md` if soak output changes;
- `ROADMAP.md`;
- `CHANGELOG.md`;
- Phase 149 `review.md` with before/after rows.

Required wording:

- "local evidence";
- "not production performance claim";
- "steady-state row excludes connect/accept";
- "connect/accept row includes session setup";
- remaining bottleneck after the phase.

## Required Verification

Focused:

```sh
cargo fmt --all --check
git diff --check
scripts/perf_record.sh --dry-run --read-from .intent/phases/149-structural-http-runtime-performance/perf_sample.txt
cargo test -p tina-http --tests -- --nocapture
cargo test -p tina-runtime request_context -- --nocapture
cargo test -p tina-runtime call_group -- --nocapture
cargo test --release --manifest-path examples/systems/perf_native/Cargo.toml --test hotpath -- --nocapture
cargo test --release --manifest-path examples/systems/perf_native/Cargo.toml --test perf -- --nocapture
cargo test --manifest-path examples/systems/mini_saas_api/Cargo.toml --test soak -- --nocapture
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

Optional but important:

```sh
TINA_LONG_SOAK_SECONDS=600 make proof-long-soak
```

Linux:

```sh
make perf-record
make perf-check
```

on Linux/x86_64, or run the manual GitHub `perf` workflow twice.

## Done

- Phase 149 history exists.
- Hotpath rows distinguish event stages, handler turns, runtime calls, service
  calls, completions, and rejected completions.
- Steady-state keepalive rows exist.
- At least one structural HTTP turn path improves.
- At least one HTTP/runtime process-allocation row improves.
- Linux/x86 evidence is collected or precisely blocked.
- Short soak remains clean.
- Claims stay honest.
