# Tina/Tinio 0.1.0 Ergonomics and Correctness Review

**Date:** 2026-07-09

**Scope:** Repository architecture, public API, representative examples, runtime behavior, documentation, packaging, and launch verification.

**Intent corpus:** Sampled only where needed for orientation; this review deliberately did not read the full `.intent/` directory.

**Additive code review:** See [`tina-0.1.0-rust-code-review.md`](tina-0.1.0-rust-code-review.md) for a focused pass over literal Rust correctness, ownership, unsafe boundaries, error handling, and API evolution.

## Executive summary

Tina has a strong and unusually coherent core: request authority is represented in the type system, overload and cancellation remain observable, live and simulated execution share meaningful semantics, and the project tests failure modes that many early runtimes defer indefinitely. The systems examples show that these ideas compose into credible services rather than only toy actors.

The repository is not ready to publish as `0.1.0` yet. Two issues should block release:

1. The intended crates cannot currently be packaged for crates.io because internal path dependencies omit version requirements.
2. A canonical systems example fails its own smoke path, while the normal verification targets do not exercise systems examples.

The other high-priority risks are unbounded trace retention in live runtime defaults and a timer implementation whose storage and harvesting can grow without a runtime-enforced bound and degrade quadratically under synchronized timer loads.

The API is expressive once its vocabulary is learned, but application code still crosses too many crate boundaries and has multiple ways to express closely related operations. The Tinio rename is an opportunity to establish one application-facing facade before names and import patterns become compatibility commitments.

## Release blockers

### P0: The crate graph is not publishable

Internal dependencies use `path` without a version requirement, for example in [`tina/Cargo.toml`](../tina/Cargo.toml) and [`tina-runtime/Cargo.toml`](../tina-runtime/Cargo.toml). Cargo rejects that graph during packaging:

```text
all dependencies must have a version requirement specified when packaging.
dependency `tina-macros` does not specify a version
```

This was reproduced with:

```sh
cargo package -p tina --allow-dirty --no-verify
```

The customized path-only Betelgeuse dependency also needs an explicit publication strategy. A local-only dependency can be valid during development, but every published crate must resolve entirely from the registry or another declared public source.

**Required before 0.1.0:**

- Add a compatible `version = "0.1.0"` beside each internal `path` dependency intended for publication.
- Decide which workspace crates are public, which remain private, and how the Betelgeuse dependency is distributed.
- Define and test the crate publication order.
- Run `cargo package` for every public crate in CI, including verification of the packaged contents rather than only the workspace source tree.
- Add crate-level READMEs and complete package metadata. In particular, `tina-sim` lacks `description` and `repository`, and `tina-tokio-bridge` lacks `description`.

### P0: The canonical copied-service path fails and is outside the normal gate

[`examples/systems/system_copied_service_path/src/lib.rs`](../examples/systems/system_copied_service_path/src/lib.rs) is presented as a copied-service skeleton, but it currently constructs reports and helper values without running an isolate, runtime, listener, handler, persistence layer, or shutdown sequence. Its smoke test also fails because `run_copied_service_path` leaves leak checking in the `unchecked` state and then asserts that no capacity leaked:

```text
LoadAssertionFailure {
    claim: "no leaked capacity at shutdown",
    observed: "leak was never checked; ... leak=unchecked ..."
}
```

This is more than an example polish issue. A user following the repository's named golden path gets a false picture of the framework and a failing result.

The gap is not caught by the standard example target. [`Makefile`](../Makefile) checks `examples/*/Cargo.toml` and `examples/extensions/*/Cargo.toml`, but not `examples/systems/*/Cargo.toml`; the systems examples are also excluded from the main workspace.

**Required before 0.1.0:**

- Replace this example with a genuinely minimal runnable service using the recommended split request API.
- Make its assertions reflect work actually performed, including capacity/leak accounting.
- Add the documented golden-path systems examples to CI. At minimum, run copied-service smoke plus `mini_saas_api` smoke and pressure tests.
- Keep exclusions intentional, but provide one authoritative command that validates everything promoted as a user starting point.

## High-priority correctness and operability risks

### P1: Live runtimes retain every trace event by default

[`ThreadedRuntimeConfig::default`](../tina-runtime/src/threaded.rs) and [`LocalSystemConfig::default`](../tina-runtime/src/local_system.rs) both select `TraceRetention::Full`. This is useful for deterministic tests, but unsafe as a live default: a healthy long-running service steadily retains all trace events. The `mini_saas_api` serving path constructs `ThreadedRuntime::new`, so the repository's flagship service inherits this behavior.

The framework otherwise treats boundedness as a first-class property. Default-unbounded diagnostic storage conflicts with that contract and makes memory growth dependent on uptime and traffic.

**Recommendation:** default live owners to bounded retention or no in-memory retention, with an observer/export path for operational traces. Keep full retention an explicit test, simulation, or replay choice. Add a long-running test that proves retained diagnostic state remains within its configured bound.

### P1: Timer admission and harvesting are unbounded

The raw timer lane in [`tina-runtime/src/driver/mod.rs`](../tina-runtime/src/driver/mod.rs) stores timers in a `Vec<TimerEntry>` and has no timer capacity in runtime configuration. Harvesting repeatedly scans the vector for the earliest due timer and removes it, making a synchronized batch of `n` due timers approximately quadratic. All due completions are harvested into `pending_completions` before the delivery budget in [`tina-runtime/src/dispatch.rs`](../tina-runtime/src/dispatch.rs) is applied.

This creates three related failure modes:

- A service can admit an unbounded number of timers.
- A large synchronized timer set can monopolize a shard while harvesting.
- The completion burst can allocate in proportion to all due timers even when delivery is budgeted.

**Recommendation:** add a runtime-enforced timer capacity and typed `TimerFull` outcome; store timers in a heap or ordered timer structure; and budget harvesting itself, not only subsequent delivery. Test many same-deadline timers for bounded memory, bounded per-tick work, deterministic ordering, and overload visibility.

## API ergonomics

### P2: Construction failures panic instead of returning startup errors

The local-system builders call `validate().expect(...)`, and threaded construction can panic for invalid zero-valued configuration, I/O loop initialization, thread-spawn failure, or startup-handshake timeout. Configuration and environmental startup failures are recoverable application concerns, especially for a library intended to underpin services.

**Recommendation:** expose `try_build()` and fallible threaded constructors returning a structured `StartupError`. Panic-based convenience constructors can remain, but should delegate to the fallible path and be presented as test/prototype conveniences.

### P2: The application-facing surface is fragmented

The workspace contains many focused crates, while ordinary examples commonly combine `tina::prelude::*` with long lists of `tina_runtime` imports. Closely related capabilities appear at more than one path, including `tina::isolate` and `tina_runtime::isolate`. The runtime root also re-exports a very broad vocabulary without an application-oriented prelude.

The individual tools are good: `flow!`, `PendingReplies`, `CallJoinSet`, `CallSelectSet`, bounded helpers, typed outcomes, request contexts, and `SharedWork` cover real coordination problems. The issue is discovery and composition cost, not missing power.

**Recommendation for the Tinio rename:**

- Make `tinio` the stable application facade and the default dependency in getting-started material.
- Provide a small, curated `tinio::prelude` for the common service path.
- Group advanced APIs under discoverable modules such as `runtime`, `sim`, `replay`, `http`, and `testing` rather than relying primarily on a flat re-export surface.
- Teach one authoritative split request API first. Keep legacy dispatch forms documented as compatibility or advanced mechanisms, not parallel starting points.
- Keep optional batteries separate where their dependencies or operational contracts justify it.

### P2: Several examples weaken the typed-outcome story

[`examples/specimen_worker_pool/src/tina_impl.rs`](../examples/specimen_worker_pool/src/tina_impl.rs) maps every non-success `CallOutcome`, including timeout, closed, and rejected, to `FrontendReply::Full`. This is convenient for a specimen but collapses distinct terminal truths that Tina is specifically designed to preserve.

The boundedness guide also demonstrates a request-sized loop producing a raw batch immediately before explaining why request-sized raw batches are hazardous. The example is understandable locally, but readers tend to copy the first working shape.

**Recommendation:** make golden examples preserve typed terminal outcomes or explicitly name intentional policy coalescing. Demonstrate bounded producers before raw `Effect::Batch`, and reserve the raw form for small statically bounded collections.

## Documentation correctness

Several documentation paths have drifted from the implementation:

- [`docs/tina-user-guide/00-agent-quickstart.md`](tina-user-guide/00-agent-quickstart.md) imports `noop`, `reply`, `send`, and `stop` from `tina_runtime`, where they are not exported.
- [`tina-runtime/src/lib.rs`](../tina-runtime/src/lib.rs) still describes an early single-shard/replies-only-traced state that no longer matches the runtime.
- [`README.md`](../README.md) says `make verify` includes Miri, while the target runs Loom but not Miri.
- `LocalSystemConfig` documentation says each TLS operation owns a worker thread, which does not describe the current shard/Betelgeuse implementation.
- The hello-world path emphasizes duplicate legacy `handle`/`handle_call` dispatch rather than the split API that provides request authority.
- Many Rust examples are ignored doctests, and the Markdown guide snippets are not compiled.

Documentation drift is especially costly here because Tina introduces a precise model and substantial new vocabulary. Incorrect snippets make conceptual difficulty look like user error.

**Recommendation:** compile-test the quickstart and a small set of golden guide programs in CI; minimize ignored doctests; update architecture comments as part of behavior changes; and make the split request API the first path a new user encounters.

## Rename readiness

The rename should happen before 0.1.0, but it is not a single search-and-replace operation. A repository scan found Tina names across crate names, features, environment variables, trace targets, schemas, thread names, example labels, and persisted replay/protocol identifiers. Versioned formats such as `tina-replay-case-v1` and `tina-protocol-byte-replay-v1` require an explicit compatibility decision.

Recommended rename policy:

1. Rename human-facing packages, modules, commands, documentation, environment variables, and new trace targets to Tinio before release.
2. Inventory persisted and externally consumed identifiers separately.
3. Preserve old versioned format identifiers when changing them would break existing artifacts, or ship explicit readers/migrations and tests for both names.
4. Add a temporary CI inventory that rejects unintended new `tina` identifiers while allowlisting deliberate compatibility strings.
5. Perform the rename before publication so crates.io package names and import examples do not immediately require a compatibility era.

## Where Tina shines

### Request authority is real, not aspirational

`RequestCall`, `RequestEffect`, deferred request contexts, and compile-fail suites make forgotten or duplicated replies a type-level problem. This is the project's clearest differentiator. It improves local reasoning without hiding terminal outcomes behind exceptions or generic errors.

### Failure and overload stay visible

Typed rejection, closure, timeout, cancellation, bad-peer behavior, stale-generation handling, lease accounting, and shutdown reports create an honest operational model. The HTTP body accounting, pool lease, bridge admission, and capability examples demonstrate that this discipline extends beyond the core mailbox.

### Simulation and replay reinforce the live model

The simulation surface is not a disconnected test DSL. Trace causality, saved replay cases, shrinking, deterministic scenarios, race guards, and Loom coverage all point at the same concurrency contracts as live execution. That makes Tina unusually strong for debugging rare coordination failures.

### Coordination helpers address real service patterns

The ergonomics playground exercises quote races, debouncing, drain behavior, and single-flight work. The resulting behavior is good: cancellation remains observable, admission limits surface explicitly, and `SharedWork` makes single-flight coordination natural. The remaining friction is mostly token/handle plumbing, manual pending-state management, and vocabulary discovery.

### The flagship service is credible

`mini_saas_api` is a meaningful end-to-end proof rather than a decorative example. Its smoke and pressure paths passed during this review and exercise enough runtime surface to expose integration mistakes. It should become a required release gate after the trace-retention default is corrected.

## Recommended 0.1.0 sequence

1. **Make publication reproducible.** Decide the public crate set and dependency strategy, add registry-compatible versions and metadata, and package every intended crate in CI.
2. **Repair and gate the golden paths.** Replace the copied-service example, compile-test quickstart material, and run selected systems smoke/pressure tests in the normal verification pipeline.
3. **Close live boundedness gaps.** Change trace defaults, bound timer admission and harvesting, and add stress tests around synchronized deadlines and long-lived diagnostics.
4. **Add fallible startup APIs.** Let applications report configuration, thread, I/O, and handshake failures without unwinding.
5. **Establish the Tinio facade.** Rename before publication, curate the application prelude, organize advanced modules, and decide persisted-identifier compatibility explicitly.
6. **Run a release-candidate exercise.** Build a fresh service from the published package artifacts and documentation only; run smoke, pressure, shutdown, saved-replay, and overload scenarios.

## Verification performed

The following checks passed from the reviewed worktree:

```text
cargo test --workspace --locked --no-fail-fast
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo fmt --all --check
scripts/race_surface_guard.sh
scripts/rail_inventory_guard.sh
mini_saas_api smoke and pressure scenarios
ergonomics playground scenarios
```

The following targeted checks failed and produced the release blockers above:

```text
cargo package -p tina --allow-dirty --no-verify
system_copied_service_path smoke test
```

## Launch bar

The core design is worthy of a 0.1.0 release. The release bar should be that the published artifacts, first-run documentation, boundedness claims, and promoted examples are as rigorous as the request model itself. Once the two blockers and live boundedness risks are addressed, the remaining work is primarily API curation and documentation alignment rather than a redesign of Tina's foundations.
