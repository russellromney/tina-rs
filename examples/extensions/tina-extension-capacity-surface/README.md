# tina-extension-capacity-surface

A **custom capacity surface** built with only public Tina APIs, joining a
normal `CapacitySummary` alongside a runtime-owned surface.

## The hook

The capacity hook is **data, not a trait**. An extension owns some bounded
structure and renders it as a `tina::capacity::CapacitySurfaceReport` using
the same public `count(...)` / `weighted(...)` constructor every runtime
surface uses. The report joins a `tina_runtime::CapacitySummary` through the
public `push(...)` entry point and then appears in discovery, `surface(name)`
lookups, and `any_full()` exactly like a runtime surface.

This crate is the evidence that owned reports are enough — which is why Tina
does **not** ship a `CapacitySurface` trait.

## What it proves

- A custom surface (`RecentSamples`, a bounded ring) joins a `CapacitySummary`.
- A runtime-owned surface (`BoundedEventSink`) joins the same summary.
- Overflowing the custom ring reports `Full`, and `summary.any_full()` sees it.
- The extension only **observes and reports**; it never mutates a runtime queue.

## Run the smoke test

```sh
cargo test --manifest-path examples/extensions/tina-extension-capacity-surface/Cargo.toml
```
