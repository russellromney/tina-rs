# Phase 144: Native Performance Evidence

Status: implemented in PR.

## Goal

Answer the alpha-user question:

```text
For basic native service work, how fast is Tina?
Where is the overhead?
Can we improve obvious waste and prove the before/after?
```

This is not an app benchmark and not a SQLite benchmark. Bridges and big
systems can keep perf rows, but the headline phase is native Tina designs
against native Tokio designs for the same small tasks.

## Build

1. Make `make perf` the native headline.
   - Keep runtime cost rows.
   - Keep `mini_saas_api` as `whole_service_specimen`, not the headline.
   - Add native rows:
     - `host_enqueue`
     - `observed_admission`
     - `host_request_reply`
     - `service_request_reply_chain`
     - `http1_close_request`
     - `http1_keepalive_sequential`
     - `http1_fixed_body_close`
   - Each row prints: ops, ok, err, timeout, p50, p99, max, elapsed,
     throughput/sec, capacity surfaces, pressure counts, leak clean, profile,
     platform, git sha.

2. Add paired Tokio designs where semantics are simple.
   - `tokio_mpsc_try_send` vs `host_enqueue`.
   - `tokio_mpsc_ack` vs `observed_admission`.
   - `tokio_mpsc_oneshot_call` vs `host_request_reply`.
   - `tokio_service_chain` vs `service_request_reply_chain`.
   - `axum/hyper` vs native `tina-http` HTTP/1 close, keepalive, and fixed body.
   - Same request count, same concurrency cap, same payload size.
   - If overload semantics differ, mark the row `comparison_baseline=partial`
     and name the mismatch.

3. Add allocation evidence.
   - Reuse the existing allocator-test pattern in
     `tina-runtime/tests/multishard_allocation.rs`.
   - Pin warmed allocation counts for:
     - host enqueue
     - observed admission
     - host call
     - service call chain
     - HTTP/1 close, keepalive, and fixed body client op
   - Separate setup, warmup, steady-state, and shutdown.
   - Do not claim zero allocation unless the test proves it.

4. Improve the worst obvious Tina-local overhead.
   - Preallocate load-harness latency storage so harness Vec growth is not
     counted as Tina runtime cost.
   - Use nanosecond fields for fast rows so sub-microsecond work does not
     round into fake zero-ratio evidence.
   - Name the remaining hot spots instead of weakening semantics:
     observed admission and host calls still allocate more than Tokio.

5. Add performance report tooling.
   - Add `PerfComparisonReport` or equivalent:
     - Tina row
     - baseline row
     - ratio fields
     - semantic_match: exact / partial / none
     - mismatch reason
   - JSON and grep-friendly line output.
   - Release mode for `make perf`.
   - If run in debug mode, report `profile=debug`; do not use it for claims.

6. Document how to run it.
   - `make perf` for local native Tina evidence.
   - `make perf-compare` for paired Tina/Tokio rows.
   - Mention this is local machine evidence. Public claims need repeated runs
     and stable hardware.

## Must Not

- Do not use `mini_saas_api` or SQLite bridge as the framework benchmark.
- Do not compare Tina bounded work against unbounded Tokio work.
- Do not hide overload. `Full`, timeout, dropped/slow peer, and shutdown truth
  stay in the report.
- Do not make a "Tina is faster" claim unless the row has an exact semantic
  match and repeated evidence.
- Do not improve speed by weakening tracing, capacity, cancellation, replay, or
  bounds.
- Do not add a giant benchmark framework. First form is boring local evidence.

## Proof

- `make perf` runs in release mode and emits native Tina rows plus the
  whole-service specimen row.
- `make perf-compare` runs at least three paired rows:
  - send
  - call
  - TCP echo or HTTP/1 keepalive
- Tests assert grep and JSON line shape.
- Tests reject pressure/timeouts/leaks in native comparison rows.
- Native rows run median-of-five measured samples after warmup.
- Allocation counts are included for load-worker op scope.
- Phase README records the hot spots found and follow-up reasons.
- At least one system/specimen README shows how to read the output.

## Done

- A new user can run one command and see native Tina performance for basic work.
- A reviewer can see where Tina is slower/faster than a comparable Tokio design.
- The report includes the Tina truth: bounds, pressure, leak, shutdown.
- Obvious local overhead found by the first run is either fixed or named with a
  follow-up reason.
