# Eiffel Memory-Tier Runner

Wraps an existing Eiffel comparison and runs it under a series of
process-level address-space caps (`RLIMIT_AS`). Reports duration and
exit status per tier so we can see whether Tina-shaped services
plateau under constrained memory while Tokio-shaped services grow
hidden buffers or fail less visibly.

## Usage

```bash
cargo run --manifest-path examples/eiffel_mem_run/Cargo.toml -- \
  [comparison-manifest] [tiers-mb-comma-separated]
```

Defaults:
- `comparison-manifest` → `examples/eiffel_real_io_chat/Cargo.toml`
- `tiers-mb` → `512,256,128`

Example:

```bash
cargo run --manifest-path examples/eiffel_mem_run/Cargo.toml -- \
  examples/eiffel_mini_keyspace/Cargo.toml 512,256,128
```

## Platform notes

- **Linux:** `RLIMIT_AS` is a real address-space cap. Allocations and
  mappings beyond it fail.
- **macOS / non-Linux Unix:** the runner does *not* apply the cap. The
  reasons are practical: macOS reserves substantial address space at
  process startup and `setrlimit(RLIMIT_AS, ...)` with sub-GB values
  causes child spawn to fail with `EINVAL` for many normal binaries.
  The runner still spawns each tier and times it, but the numbers are
  baseline-equivalent. Real memory-tier runs should use Linux +
  cgroups, Docker, or Fly.

The runner prints which mode it's in at the start of every run.

## What this runner taught us

Same caveat as `eiffel_cpu_run`: the existing comparisons assert a
fixed scripted output and do not yet expose
`accepted/full/closed/timeouts` counts under load, so the most this
runner can tell us about Tina-vs-Tokio behaviour under a memory cap is
"both still produced the expected output" or "one or both crashed."

The runner is in place for when comparisons grow load drivers and
overload metrics. Until then, treat the output as a process-survival
gate, not as a memory-shedding measurement.

## Cross-cutting notes

See [`examples/FINDINGS.md`](../FINDINGS.md) for the cross-cutting
ergonomic findings that this runner contributed to.