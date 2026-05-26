# Native Perf Rows

Small release-mode performance rows for native Tina designs against bounded
Tokio designs.

This is not a production benchmark suite. It is alpha evidence:

- same op count
- same worker count
- bounded capacity on both sides
- pressure and leak truth printed beside timing
- median of five measured samples after warmup
- allocation counts for work done inside the load worker op
- semantic match labeled as `exact` or `partial`

Run:

```sh
cargo test --release --manifest-path examples/systems/perf_native/Cargo.toml --test perf -- --nocapture
```

Or from the repo root:

```sh
make perf-compare
```

## Findings

What felt good:
- The harness makes obvious rows easy to print and grep.
- `PerfComparisonReport` keeps mismatch truth next to ratios.
- Splitting `host_enqueue` from `observed_admission` stopped one bad
  comparison from hiding inside the word "send."

What felt rough:
- Very fast rows need nanosecond fields; microsecond p50 can round to zero.
- Tina observed admission and `call_blocking` allocate more than the equivalent
  Tokio mpsc/oneshot patterns.
- The HTTP rows still count only load-worker allocations; server-thread
  allocation accounting needs a process/sample-level probe later.

Tina capability pulled:
- Release-mode proof output, pressure summaries, host-side ingress costs, and
  basic HTTP/1 service costs.

Suggested follow-up:
- Add in-isolate call/fanout rows so host ergonomics cost and service-runtime
  cost are measured separately.
- Add repeated-run / historical tracking before any public performance claim.
- Add process-level allocation/RSS probes for HTTP and WebSocket rows.
- Investigate why Tina host call and observed admission allocate ~4x Tokio in
  the current local row.

Verdict:
- keep
