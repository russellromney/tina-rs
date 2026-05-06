# Eiffel CPU Contention Runner

Wraps an existing Eiffel comparison and runs it twice — once unloaded
(baseline), once with N busy-spinner threads competing for CPU
(contended). Reports wall-clock duration and exit status for both runs.

This is a macOS-friendly approximation of "CPU quota." For real Linux/Fly
runs, prefer cgroups, `taskset`, or `cpulimit`. Treat the numbers as
discovery, not benchmarks — the runner is meant to surface whether a
comparison still produces correct output under contention, not to
quantify regression precisely.

## Usage

```bash
cargo run --manifest-path examples/eiffel_cpu_run/Cargo.toml -- \
  [comparison-manifest] [spinner-count]
```

Defaults:
- `comparison-manifest` → `examples/eiffel_real_io_chat/Cargo.toml`
- `spinner-count` → `2 * num_cpus - 1`

Example:

```bash
cargo run --manifest-path examples/eiffel_cpu_run/Cargo.toml -- \
  examples/eiffel_mini_keyspace/Cargo.toml 4
```

## What this runner taught us

The current Eiffel comparisons all produce a fixed, asserted output.
None of them yet expose load-shedding metrics, so the contention runner
mostly answers a binary question: did the comparison still pass?

That has been useful three ways:

1. **Cold-cache vs warm-cache dominates wall-clock at this scale.** The
   first untimed `run_target` call exists purely to page the binary in.
   Without that warmup the labelled "baseline" run was roughly 50× the
   labelled "contended" run on the same binary because of dyld/page-cache
   first-touch cost. The runner now does an explicit warmup pass.
2. **Both Tokio and Tina sides survive 4× spinner contention** for the
   tiny scripted comparisons. That tells us nothing surprising yet, but
   the runner is in place for when comparisons grow load drivers.
3. **What we cannot measure here.** Tina's "visible shedding" property
   needs the comparisons themselves to expose accepted/full/closed
   counts under load. The current `SideReport` shapes don't carry that —
   `eiffel_real_io_chat` has the closest approximation
   (`saw_visible_full`), but it isn't surfaced as a CPU-contention
   metric. Adding that is a separate roadmap item.

## Cross-cutting notes

See [`examples/FINDINGS.md`](../FINDINGS.md) for the cross-cutting
ergonomic findings that this runner contributed to.