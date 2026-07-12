# Changelog

This file records completed work.

## Unreleased

### Authoritative child-restart observation

- Child-restart waiters now match the complete parent address identity
  (shard, isolate id, and generation). Stale-generation and same-id
  foreign-shard addresses can no longer claim a live parent's replacement
  event; threaded and deterministic multi-shard owners retain the same typed
  waiter API and replacement-address result. Dropped or timed-out restart
  waiters release their bounded observation slots and cannot consume a later
  fact intended for a live waiter.

### LocalSystem host-control parity

- `LocalSystem` and `LocalMultiShardSystem` now forward typed terminal-result
  observation and cloneable runtime shutdown control. Shared app hosts can
  retain `Arc` owners, observe exact `stop_with` values, and obtain cached
  terminal truth without exposing or unwrapping the lower threaded runtime.

### Typed clean-shutdown check

- Added `LocalSystemTerminalReport::ensure_clean` and
  `UncleanShutdownError`, preserving the full shutdown accounting when a
  successfully observed terminal report still proves failed or leaked work.

### Bounded shared-runtime shutdown

- Added `ThreadedShutdownHandle::request_and_wait_report`, which retries
  bounded shutdown admission and waits for terminal truth under one total
  timeout. Partial multi-shard progress and the final request failure remain
  visible through typed `ShutdownAndWaitError` outcomes.

### Fallible threaded runtime startup (#296)

- Added typed `StartupError` and `ThreadedRuntimeConfigError`. Startup failures
  carry field, shard, platform source, and panic message instead of panicking
  inside the constructor.
- Added fallible `try_*` constructors to `ThreadedRuntime` and
  `ThreadedMultiShardRuntime`, and `try_build` to both `LocalSystem` builders.
  The panicking constructors now delegate to the fallible path and keep their
  old signature.
- Construction now waits for a worker-ready handshake: success means config
  validation, I/O loop creation, dispatcher bootstrap, and worker startup all
  completed. Multi-shard failures shut down and join the partially started
  topology; a worker blocked indefinitely in user startup code may outlive the
  timeout error (documented on `StartupError::WorkerHandshakeTimeout`).

### Canonical bridge-host construction

- `BridgeHost` now accepts only a fallibly started `LocalSystem` through
  `BridgeHost::from_app`; the panic-on-startup `BridgeHost::new` constructor
  has been removed.
- Tokio/Tower bridge examples and specimens configure ingress and worker wait
  through `LocalSystem::single_shard(...).try_build()` and propagate
  `StartupError` from production entry points.

### Split-service set / scope event helpers

- `CallGroup::start_cancelable_service_event`,
  `CallJoinSet::start_cancelable_service_event`, and
  `CallSelectSet::start_cancelable_service_event` take a domain-event
  translator and wrap the split-service envelope internally — the same
  shape as `then_service_event` for ordinary calls.
- `RequestScope::cancel_into_service_event_effect` is the cancel twin:
  translators return domain events, not `ServiceMessage::Event(...)`.

### Envelope-free example cohort

Migrated remaining example call sites off manual `ServiceMessage`
construction onto `then_service_event` / `reply_service_event` /
`call_request` / `call_cancelable_request` / `send_event` /
`register_split_service` and the set/scope helpers above. Specimens and
systems that still name `ServiceMessage` only do so in type aliases for
HTTP listener wiring (`HttpListener<…, ServiceMessage<…>>`), not in
application effect construction.

### Single-lane service authoring

- `#[tina_runtime::isolate(event = Event)]` defines an event-only service
  without a placeholder request type or request handler.
- `#[tina_runtime::isolate(request = Request, reply = Reply)]` defines a
  request-only service without a placeholder event type or event handler.
- Runtime, threaded-runtime, and simulator registration return only the usable
  `EventServiceHandle` or `RequestServiceHandle`; application call sites do not
  name the internal `ServiceMessage` envelope or `Infallible` lane.
- Multi-shard runtime, threaded, and simulator owners now mirror those service
  shapes with `register_{split,event,request}_service_on`. `LocalSystem` and
  `LocalMultiShardSystem` delegate the same capability-typed registrations,
  and all five multi-owner/facade surfaces expose `try_send_event` without a
  public `ServiceMessage` envelope.
- `LocalSystem` and `LocalMultiShardSystem` now expose the two host-call shapes
  that their registration APIs require: `call_blocking` for ordinary roots and
  `call_blocking_request` for request/split service capabilities. Both preserve
  `CallOutcome` terminal truth; backend-specific host-wait tuning remains on
  the lower threaded owners.

### Concurrency-charged parked callers

- Added `ConcurrencyPendingReplies`, a bounded owner that composes
  `ConcurrencyLimit` with deferred reply slots. Local permits stay private and
  move-only: intentional replies count as completions; caller-gone sweeps,
  drains, admission rollbacks, and owner drop retire them exactly once.
- Added optional auxiliary RAII guards for multi-budget requests without a
  sidecar table. `system_api_gateway_limits` now uses the combined owner for
  its local parked-work cap while retaining its atomic multi-scope capacity
  reservation.
- Added one combined pressure/lifecycle report and simulator coverage for
  completion, duplicate-key refusal, and concurrency `Full` behavior.

### Split-service continuation events

Added focused continuation helpers that keep split-service domain code out of
the raw `ServiceMessage` envelope. `TypedCall`, `SleepCall`, `IsolateCall`,
`CancelableCall`, and `CancelCallBuilder` now expose `then_service_event`;
`IsolateCall` also exposes `then_service_event_with_request` for an
already-captured `RequestContext`.
The request-deferred typed-call and isolate-call adapters expose
`reply_service_event`, preserving their must-answer authority while wrapping
the resulting domain event internally. Representative applied examples and a
live runtime test cover timer, typed I/O, isolate-call, cancelable-call,
cancel-acknowledgement, and deferred-reply delivery.

### Four split-service API helpers (#292)

The 2026-07-09 examples-canonicalization sweep left four small gaps where
split-service call sites hand-wrapped around a missing helper
(`examples/FINDINGS.md`). All four are additive, each proven by migrating
the exact call-site that surfaced it:

- `tina_sim::Simulator::register_split_service` mirrors
  `tina_runtime::Runtime::register_split_service`; `specimen_multi_turn_request_context`'s
  sim path no longer hand-wraps `SplitServiceHandle::from_address(sim.register(...))`.
- `tina_runtime::RequestPendingCancelableInsertError::reply` consumes the
  matching one-use request-effect permit returned with a `Full`/`DuplicateKey`
  admission rejection, so a split-service `handle_request` arm can answer
  typed without reopening the generic request-effect escape. `system_job_queue`'s
  `Queue::submit` drops its `is_full()` pre-check + panic; the Full path now
  replies `QueueReply::Busy`.
- `tina_runtime::call_cancelable_request` is the request-lane sibling of
  `call_cancelable`. Migrates the hand-wrapped `WorkerMsg::Request(...)`
  dispatch sites in `specimen_request_scope_fanout` and `system_job_queue`.
- `tina_runtime::ThreadedMultiShardRuntime::call_blocking_request` mirrors
  the single-shard `ThreadedRuntime::call_blocking_request`. Migrates
  `system_session_auth` off raw `ServiceMessage::Request` envelopes sent
  through plain `call_blocking`.

### Betelgeuse re-vendor and provenance (#286)

Re-vendored `vendor-betelgeuse/` to upstream tip (`6d1f137`) and closed the
historical provenance gap: the vendor-base commit is now reconstructed and
recorded (`97f4f40a`, confirmed two independent ways), with the tina patch set
replayed one family per commit so each is directly exportable as an upstream
PR. Upstream delta since our base was trivial (a darwin restyle, adopted, plus
README commits crediting tina-rs). Also found and fixed undocumented ledger
drift: the ~1000-line simulated I/O backend is now a documented family, a
claimed patch family with no code footprint was corrected, and dead
worker-park residue (a never-called blocking `kevent` path) was excised — the
darwin backend is now structurally incapable of sleeping. Upstreaming
assessment and Pekka Enberg coordination plan live in the PR body; deliberate
darwin connect simplifications (EALREADY/getsockopt-EINTR drops vs upstream)
are documented in VENDOR.md with a registered one-line hardening option.

### 0.1.0 release-readiness wave (external review P0/P1 fixes)

An external 0.1.0 review found two release blockers and two live-boundedness
gaps. All four fixed, each behind an independent adversarial review:

- **Publishable crate graph** (#283). Every internal `path` dependency now
  carries a `version` requirement, publish metadata (description, repository,
  keywords) is complete workspace-wide, and a `packaging` CI job runs
  `cargo package --no-verify` on the zero-prerequisite crates so the graph
  can't silently regress. Publication order documented. Open decisions
  recorded in the PR: the vendored Betelgeuse dependency gates `tina-runtime`
  and its dependents, and crates.io already has an unrelated `tina` crate --
  both resolved by the planned Tinio rename (`tinio-*` package names,
  `tinio-betelgeuse` for the fork).
- **Real copied-service path** (#281). `system_copied_service_path` previously
  built its report from constants without running a service, and its own smoke
  test failed; it is now one real split-service isolate on a live
  `ThreadedRuntime` -- bounded admission with typed `Full`, a ledger step, a
  leak check that reads the scope's actual post-shutdown state. Two
  fake-coverage companion crates deleted. New `systems-examples` CI job gates
  the copied path and `mini_saas_api` on every PR, and `make verify-examples`
  now includes `examples/systems/*`.
- **Bounded live trace retention** (#282). `ThreadedRuntimeConfig` and
  `LocalSystemConfig` defaults moved from `TraceRetention::Full` (memory grows
  with uptime) to `Bounded(16_384)` with honest `trace_dropped` accounting.
  `Full` stays the explicit choice for tests/sim/replay; streaming capture is
  unaffected (the trace observer fires before retention). A new test pins the
  bound under ~20k-event load.
- **Bounded timer lane** (#284). Driver timers moved from an unbounded `Vec`
  with a linear min-scan (quadratic under synchronized deadlines) to a
  `BTreeMap` keyed `(deadline, insertion_order)` -- O(log n) harvest with the
  same-deadline FIFO tie-break preserved exactly (zero golden/DST changes).
  Per-shard capacity (default 262,144, configurable, zero rejected) refuses
  over-cap arms with a typed `CallError::TimerFull`; a per-advance harvest
  budget (1,024) keeps a synchronized batch from monopolizing a shard without
  reordering delivery.

### CI

- Cut `make verify` CI time via layered caching and a faster test runner:
  `Swatinem/rust-cache` (target/registry cache keyed on `Cargo.lock` +
  rustc, exact hit under `--locked`) plus `sccache` via the GitHub Actions
  cache backend (per-compilation-unit cache that survives lockfile bumps;
  requires `CARGO_INCREMENTAL=0`, since sccache doesn't cache incremental
  artifacts). Expected: warm runs drop from ~10-17 min to ~2-4 min.
- Dropped the standalone `check` step from `verify` -- `cargo clippy
  --all-targets` already type-checks everything `check` did, so running both
  back-to-back recompiled the workspace twice for no extra coverage.
- Switched `make test` to `cargo nextest run` (per-test-binary isolation,
  parallel by default) plus a separate `cargo test --workspace --doc`
  pass, since nextest cannot execute doctests. Nextest's isolation also
  fixes the aggregate-run flakiness the trybuild compile-fail suites
  (`admission_compile_fail`, `flow_macro_compile_fail`) saw under a single
  `cargo test` process.
- Split the `verify` CI job into parallel jobs -- `static` (fmt, doc;
  ubuntu-only), `clippy` (both OSes), `test` (nextest + doctests; both
  OSes), `guards` (loom, race-surface-guard, rail-inventory-guard, cost
  smoke; both OSes) -- backed by new `verify-static` / `verify-guards`
  Makefile targets so wall-clock is the slowest single job instead of the
  sum. clippy keeps its own both-OS matrix because it only lints code it
  compiles for the target, and the workspace has macOS-only cfg blocks.
  `main` has no branch protection rule today, so this rename can't break an
  existing required check; if branch protection is added later the gating
  job names are listed in `verify.yml`.
- Documented the local sccache setup (`RUSTC_WRAPPER=sccache
  CARGO_INCREMENTAL=0`) in the Makefile header for developers who want the
  same compile cache locally.

### Test Hardening

- Drove the registry's deferred `Closed` and `Full` downstream outcomes through
  the live `handle_call → defer(call(service)) → continuation → reply_to` path,
  not just the pure outcome-to-reply mapping table. A service that stops mid-call
  yields `Closed → RouterReply::Internal`; a service whose mailbox rejects the
  mediated send yields `Full → RouterReply::Full`. Only the timeout arm had live
  coverage before.
- Added a real-path bridge e2e (`tina-rpc-tokio`): the tokio bridge drives the
  production `tina_rpc::Client` isolate against a real `tina-rpc` server over a
  loopback TCP socket and awaits a byte-exact reply. The existing bridge suite
  substitutes a `ClientStub`, so the production client isolate was never
  exercised over the wire.
- Documented `BridgeGuard` in `tina-tokio-bridge` as send-only: it delegates
  only `handle` and must never be a `call()` target. A clean `handle_call`
  passthrough is not expressible (the inner isolate's `CallContext` cannot be
  reconstructed from the guard's), so the invariant is stated rather than
  silently relied upon.
- Switched the streaming length-prefix math in the tina-rpc connection and
  client read loops to `checked_add`, matching the sibling `frame::decode`.
  Defense-in-depth parity: `body_len` is already capped at `max_frame_size`, so
  no overflow is reachable on 64-bit.
- Corrected the stale `expected_trace_hash` in the `specimen_replay_dst` README
  to match the code constant and the actual run.
- Made the HPACK panic-containment gate load-bearing. The `hpack_block_is_sound`
  gate in front of `hpack::Decoder::decode` closes a pre-auth remote DoS, but
  its wiring was untested: under the test profile's `panic = "unwind"` the
  surrounding `catch_unwind` produced the same error a gate rejection does, so
  deleting the gate left every test green while silently reintroducing the abort
  under `panic = "abort"`. Two fixes: the `hpack_headers` fuzz target now drives
  the real production decode entry instead of a private copy of the gate, so
  removing the gate aborts the process under the fuzzer (and the fast-literal
  path, which runs first on every inbound block, finally gets coverage); and a
  deterministic unit test asserts the panic shapes are rejected *before* decode
  via a gate-wiring counter that moves only when `catch_unwind` actually catches
  a panic. Deleting the gate now fails a normal `cargo test`.
- Folded the fuzz seed corpus and documented panic shapes into deterministic
  unit tests so the panic-containment property has per-PR regression coverage:
  the HPACK panic inputs through `decode_headers_block`, the chunked-decoder cap
  invariant across every split feed, the h2 DATA/HEADERS payload views on
  padded/truncated input, and the rpc frame decode on a truncated length prefix.
- Extended the HPACK soundness differential past 2 bytes to all 3-byte
  size-update blocks plus deterministic deep-continuation anchors — where the
  real panic trigger lives — asserting walker-accepts implies decode-no-panic
  and walker-rejects on the panic shapes.
- Added a weekly (and manual) CI job that builds every fuzz target and runs each
  for 60 s, so the fuzzers get continuous execution without gating each PR; the
  per-PR shim check is now `--locked`.
- Pinned the requester-stop cancel ordering on both engines. When a requester
  with several in-flight driver calls stops, the runtime cancels them in
  ascending call-id (insertion) order; a `Runtime`↔`Simulator` parity test
  creates four concurrent in-flight sleeps, stops the requester, and asserts
  both engines emit `CallCompletionRejected{RequesterClosed}` strictly ascending
  and agree. The prior `swap_remove` sweep emitted `1, 4, 3, 2` — the test fails
  if reverted to it.
- Hardened the simulator's timer harvest against a completion for a call the
  table no longer tracks: it panicked in `requester_registration_index` before
  reaching `deliver_completion_at`'s quarantine, unlike the live runtime. The
  lookup now returns `Option` and an untracked completion flows to quarantine
  (trace + drop) so the shard survives. Known-call ordering is unchanged, so no
  golden or DST hash moves. A sim unit test drives an unknown timer through the
  real `step → harvest_timers → deliver_completion_at` sink.
- Added a runtime unit test that drives a genuinely-unknown completion through
  the real `advance_driver → pending_completions → deliver_completion` sink
  (stronger than the existing direct-`deliver_completion` call) and a sim
  analogue for the carried-completion purge: a stopped requester's pending timer
  is purged, not quarantined.
- Added a multi-shard `WorkerUnresponsive` test: a wedged handler on one shard
  makes a host-control call return `WorkerUnresponsive`, asserting only the
  deterministic lower bound (`elapsed >= control_call_timeout`); the healthy
  shard still answers.
- Documented `ThreadedRuntime::send_and_observe`'s intentionally-unbounded
  `recv()` (a wedged worker can hang the host thread, by design) and pinned it
  with a test that proves it stays blocked past the control-call timeout, then a
  gate release lets it finish.
- Added a debug-only tripwire counting `call()` rejections that resolve as
  `UnsupportedMessage` — the signature of the default `handle_call` — so the
  "answers `call()` but only implements `handle`" bug class surfaces without an
  e2e test. Allocation-free and trace-free (no golden-hash or allocation-pin
  movement); compiled out in release. A test confirms it fires for a handle-only
  target and not for a proper `handle_call` isolate.
- Deflaked three timing tests by replacing wall-clock races with deterministic
  mechanism checks rather than widening budgets. The `host_burst` overflow test
  uses a non-draining mailbox so a burst past capacity provably overflows
  (exactly capacity admit, the rest `MailboxFull`, every run). The multi-shard
  park test widens `idle_wait` to 10 s against a 10 ms repoll so a pending timer
  must produce a repoll wakeup (the choice, not a rate), with the determinism
  proven in a `has_pending_runtime_work` unit test. The shutdown-under-flood
  test asserts completion via a watchdog thread (a regression deadlocks, not
  runs slow) instead of a 3 s stopwatch.

### RPC Dispatch Fix

- Fixed tina-rpc request/reply dispatch, which was broken end-to-end: every
  routed service call came back as a wire `Error(Internal)` instead of a
  `Reply`. The runtime delivers `call()` traffic to `handle_call`, but
  `Registry` and `SingleService` implemented only `handle` (the old
  implicit-reply-slot model), so calls hit the default `handle_call` and were
  rejected with `UnsupportedMessage`. `SingleService` now answers synchronously
  through `handle_call`; `Registry` captures the caller's `RequestContext`,
  defers through the downstream service call, and answers on completion via
  `reply_to`. The wire protocol, outcome mapping, and backpressure are
  unchanged.
- Added end-to-end coverage that drives real `call()` traffic through the
  runtime — host-call to registry and service, plus a full TCP roundtrip
  through the connection isolate — asserting a `Reply` comes back. Every
  existing tina-rpc unit test drove isolates via `handle` directly, which never
  exercised `handle_call`; that gap is what let dispatch regress unnoticed.
- `RegistryMsg` is no longer `Clone`: its internal continuation now carries a
  move-only `RequestContext`.

### Examples Sweep

- Compiled every out-of-workspace example crate (specimens, systems,
  extensions) against current main and fixed the two that had drifted:
  `specimen_replay_dst` now propagates the `Result` that `shrink_replay_case`
  returns, and `specimen_rpc` renames its `#[service]` method argument off the
  `payload` name the generated request constructor now reserves.
- Ported `specimen_multi_turn_request_context`'s readiness service to
  `tina::flow!`: the probe-then-db chain is the one hand-written linear
  multi-step `RequestContext` continuation left in the example corpus, so the
  macro now writes the continuation enum and dispatcher it used to spell out by
  hand. Behavior is unchanged (`ready` / `not_ready` on every timeout path).
- Documented a tina-rpc bug that `specimen_rpc` surfaces end-to-end: a
  registry-routed service call comes back as wire `Error(Internal)` instead of
  a `Reply`, because `Registry` and `SingleService` answer via `handle`
  returning `Effect::Reply` (the old implicit-reply-slot model) but the runtime
  now delivers `call()` traffic to `handle_call`, whose default rejects with
  `UnsupportedMessage`. Every tina-rpc unit test drives these isolates by
  calling `handle` directly, so nothing caught it. (The library fix landed in
  the RPC Dispatch Fix above; the specimen now round-trips `ok=1`.)
- Swept phase-number scars (bare `0NN` phase tags and `.intent/phases/…`
  narrative pointers) out of example READMEs and one source comment, restating
  each as the design fact instead.
- Removed the dead `examples/eiffel_outbound_fetch/` directory, a rename
  leftover that held only a stale `Cargo.lock` (no manifest, no source).

- Added a `serve` mode to the mini SaaS example: `-- serve [--addr HOST:PORT]`
  binds the HTTP server, prints one startup line (bind address, shard count,
  capacity summary), and runs until SIGINT/SIGTERM on the runtime's
  `signal_wait` rail, then drains through the existing shutdown choreography
  and exits `0`. This is the copyable run-forever entrypoint; the
  `smoke`/`pressure`/`soak` modes are the verification harness.
- Removed the mini SaaS controller's compatibility-forward continuation
  variants left over from the `tina::flow!` port; the flow continuations are
  now the only notify path.
- Split the example's `tina_impl.rs` into focused modules (controller,
  harness, serve, shutdown) with no behavior change.
### Docs On-Ramp

- Added `tina-runtime/examples/hello_world.rs`: the smallest runnable program —
  one isolate on `ThreadedRuntime`, a blocking call, a reply, and shutdown. The
  first-isolate guide chapter now ends with the same program so a newcomer can
  run something on page one.
- Rewrote `task_dispatcher` and `tcp_echo` onto `ThreadedRuntime` +
  `DefaultThreadedMailboxFactory`, removing the hand-rolled mailboxes and manual
  `step()` pumping. `task_dispatcher` now learns a spawned worker's address with
  `spawn_observed(...).then(...)` instead of parsing runtime trace events. The
  deterministic trace-shape proof for `tcp_echo` moved into an in-file `#[test]`
  on the explicit-step runtime.
- Restructured the request/reply chapter to lead with the default reply shapes
  (`reply`, `defer(work).reply`, `tina::flow!`) before the lower-level tools.
- Added glossary entries for `rail`, `first-form`, `fact`, `battery`, and
  `specimen`, and pointed the README quickstart at `hello_world` first.

### Runtime Hardening

- Folded the runtime's three parallel in-flight-call structures (in-flight
  calls, translators, pending isolate calls) and their index maps into one
  `CallTable` keyed by call id, with each call's translator stored inline. This
  removes the "missing translator" / "already consumed" panics that a driver
  accounting bug could trip to kill the whole shard thread.
- A driver completion for a call the runtime no longer tracks is now quarantined
  — traced as a new event and dropped — instead of panicking, so one buggy
  driver can no longer tear down unrelated isolates on the same shard.
  Wrong-message-type delivery still panics, since continuing past type confusion
  is worse than aborting. The simulator mirrors both the storage and the
  quarantine behavior so replay parity holds.
- Bounded the host control plane: `ThreadedRuntime` host calls (and the
  multi-shard equivalents) now wait at most a configurable `control_call_timeout`
  (default 30s) for the worker to answer, returning the new
  `ThreadedRuntimeError::WorkerUnresponsive` instead of hanging forever behind a
  wedged or runaway handler.

### Continuation Flow Authoring

- Added `tina::flow!`, which generates explicit continuation enums and
  dispatch methods for fixed multi-step request handlers without changing the
  runtime effect contract.
- Added authority-focused compile-fail coverage and a live runtime test for a
  generated flow carrying `RequestContext` through `CallOutcome`, including
  shadowed-request, duplicate-name, and renamed-crate expansion coverage.
- Ported the mini SaaS `POST /items/{id}/notify` path to the generated flow
  surface, and documented the pattern in the user guide.
### Vocabulary Consolidation

- Renamed `Effect::Call` to `Effect::Io` for runtime-owned I/O effects.
- Renamed `Isolate::Call` to `Isolate::Io` for runtime-owned I/O effects.
- Renamed the isolate macro `call = ...` option and `isolate_types!` `call:` key
  to `io`.
- Removed `sequence()`; use `batch()`.
- Removed `reply_to_request()`; use `reply_to()` with `RequestContext::into_deferred`.
- Removed public `Context::take_reply_slot()`; use `Context::take_request_context()`.
- Removed deprecated `SpawnObservedBuilder::reply()`; use `SpawnObservedBuilder::then()`.
- Removed `TimerInterval`, `MissedTickPolicy`, and `IntervalDelay`; use `RecurringTick` and `RecurringCatchUp`.
- Removed public `tina_runtime::wait_list::WaitList`; use `SharedWork`.
- Renamed `request_effect_after_wait_park` to `request_effect_after_shared_wait`.
### Fuzzing

- Added `ws_frame` and `h2_payload` fuzz targets covering the hand-rolled
  WebSocket frame parser and HTTP/2 DATA/HEADERS padding/priority stripping —
  two wire-facing parsers previously only covered by inspection. Both ran
  millions of executions clean. The gRPC frame reassembler stays covered by
  inspection (length-guarded, decode delegates to prost); see fuzz/README.md.

- Added a `fuzz/` crate (excluded from the workspace) with coverage-guided
  targets for the hand-rolled decoders: chunked bodies, HTTP/1 request and
  response heads, the RPC frame codec, and HTTP/2 frame headers.
- Closed an attacker-reachable, pre-auth panic in HTTP/2 HPACK decoding. A
  truncated or over-long dynamic-table-size-update integer made the `hpack`
  crate unwrap a failed decode and panic inside `decode` — a process abort on
  `panic = "abort"` builds, i.e. a remote DoS. The decoder is now gated behind
  a structural soundness walker that rejects exactly the inputs `hpack` would
  fault on, under every panic strategy. A fuzz target proves the walker never
  admits a panicking block, and an exhaustive unit test over all short blocks
  checks the walker agrees with the real decoder in both directions.

### CI Dependency Hygiene

- Pinned the nightly toolchain in `rust-toolchain.toml` (with rustfmt and
  clippy components) so upstream nightly changes cannot fail CI under
  unrelated PRs; the weekly fresh-resolve canary now also runs the latest
  nightly so toolchain drift is still caught on purpose.
- Fixed nightly toolchain drift: replaced same-type `drain(..).collect()`
  with `mem::take` (runtime scope teardown, HTTP/2 client teardown) and an
  atomic max `fetch_update` loop with `fetch_max`, so the unpinned nightly
  clippy gate is green again.

- Committed the root workspace `Cargo.lock` and switched normal workspace CI
  commands to `--locked`, so pull-request failures are tied to Tina changes
  instead of surprise dependency resolution drift.
- Kept independent example workspaces unlocked for now and documented that
  policy at the local `verify-examples` sweep.
- Added a weekly/manual fresh-resolution canary that deliberately removes the
  lockfile, resolves current crates.io state, and checks the workspace so
  ecosystem drift is still visible on purpose.

### Adversarial Review Fix Wave

- Recorded the 2026-06-08 adversarial review and per-track findings under
  `.intent/review/`, then merged the full fix wave so the review record and
  code state agree on `main`.
- Fixed RPC macro arg hygiene for trait parameters named `encoding` or
  `payload`, and tightened the split request-call safety rail so string
  literals cannot hide forbidden call-authority use from compile-fail tests.
- Removed remaining hot-path quadratic scans in runtime stopped-entry
  collection and promoted-slot sweeping.
- Made uncertain process-kill cleanup cancel and join stdout/stderr drain
  threads instead of dropping blocked drain handles.
- Hardened HTTP/1 keepalive client truth: a non-chunked response that sends
  bytes beyond `Content-Length` now retires the pooled socket instead of
  silently truncating and reusing a desynchronized connection.
- Fixed HTTP/1 keepalive chunked peer-close handling so truncated chunked
  responses now fail with `Closed` instead of succeeding with partial body data.
- Fixed bridge admission-slot leaks by reserving runtime-owned continuation
  overflow delivery for bridge/self continuations when the ordinary bounded
  mailbox is saturated. The docs now state the real priority semantics: FIFO
  within the overflow lane, not FIFO relative to older ordinary ingress.
- Hardened live multi-shard proof truth. Cross-shard trace invariants now fail
  closed when an event id is ambiguous across shards instead of proving a cause
  by id alone, and the proof harness has explicit ambiguity regressions.
- Fixed HTTP/2 and gRPC symmetry gaps: streamed client responses now check
  `Content-Length` at `END_STREAM`, client-streaming gRPC decode enforces the
  service-owned message-count cap, the server's initial SETTINGS advertises
  configured stream/window/frame caps and disables push, buffered uploads return
  window credit during the upload instead of deadlocking above the initial
  window, and client/server stream lookup uses bounded id-to-slot maps instead
  of per-frame linear scans.
- HTTP/2 server request/response funnel teardown now cancels owned body sources
  after successful completion as well as reset/error paths, so no source
  isolate is left running after the stream has terminally resolved.
- HTTP/2 streamed-response abandonment is explicit: if the caller leaves a
  stream open between pulls, the idle timer sends local `RST_STREAM(CANCEL)`;
  if the peer has already sent `END_STREAM`, the same timer reaps the local
  stream slot without sending a reset. Each delivered body chunk arms at most
  one no-op idle timer, and gRPC trailers-only responses continue to surface
  their final status from the response header block.

### Real Protocol Performance

- gRPC hot path follow-through: native unary calls now use
  `Http2ClientMsg::SubmitGrpcUnary`, a compact HTTP/2 client request shape that
  emits fixed gRPC headers directly instead of rebuilding a public
  `Http2ClientRequest` + `HeaderMap` per call. `GrpcClient::unary_template`
  validates/stores the method path once, and `GrpcPreframedUnary` lets hot
  repeated calls reuse an already length-prefixed request body through a shared
  outbound body. The normal dynamic `unary_request` still encodes the protobuf
  message per call; the preframed path is explicit for fixed-payload probes,
  health checks, and command messages.
- Finite gRPC server-streaming responses can now use
  `GrpcRouter::server_streaming_buffered` with
  `GrpcBufferedServerStreamingResponse`. Small fixed message streams are framed
  once and returned as a shared buffered HTTP body, avoiding the old per-call
  response-source isolate pool. `GrpcBufferedStreamLimits` makes the
  service-owned message count and framed-body byte cap explicit, with
  overflow returning `ResourceExhausted` instead of streaming partial messages.
  `HttpResponseBody::Shared(Arc<[u8]>)` is a first-class known-length body;
  HTTP/2 keeps it shared through response admission and DATA framing.
- Measured macOS/aarch64 release rows after the gRPC hot-path pass:
  `grpc_h2c_unary_warmed` load-worker allocations are 56 / 32 ops
  (down from 88 in the compact-only pass, and far below the old dynamic
  request shape); `grpc_h2c_unary_pooled_concurrent` is also 56 / 32 ops with
  p50/p90 around 660/793 µs in the final sample; finite server-streaming is
  376 load-worker allocations / 32 ops with p50/p90 around 1271/1557 µs. The
  whole-process gRPC rows still show thousands of allocations, so this is real
  movement, not a production-performance claim.
- HTTP/2 DATA payload ownership: a new `into_data_payload(frame) -> (Vec<u8>,
  usize)` moves the unpadded payload out of the owned frame and returns the
  flow-control wire length; the old cloning `data_payload` is removed so a
  handler cannot pick it by accident. Server and client DATA handlers use the
  owned path. Padded DATA still validates padding and preserves wire length;
  flow-control accounting is unchanged.
- HTTP/2 buffered responses are consumed by value: `enqueue_response` takes
  `HttpResponse` by value and *moves* the buffered body into `PendingResponse`
  instead of cloning it. `max_response_body_bytes` is still validated before the
  body is stored or sent; Stream/ChunkedStream/WebSocket bodies are unchanged.
  A new `push_data_frame` helper frames a DATA payload straight into the
  outbound queue (header + slice), so a multi-frame buffered response copies
  each chunk once. Measured: 3075 -> 3011 whole-process allocations over 64
  warmed h2c responses (one fewer per response) on macOS/aarch64.
- Streaming/gRPC DATA: server `flush_response_stream` drains the consumed prefix
  straight into the framed buffer (one copy, not two). The HTTP/2 client request
  body is an owned `Vec` + cursor with direct DATA framing, replacing the
  per-byte `VecDeque<u8>` drain; consumed/finished buffers are compacted/dropped
  so a long request body does not stay resident. A new public unary gRPC perf
  row (`grpc_h2c_unary_close`, `GrpcRouter` behind the real `Http2Listener`)
  improves through the same server-side path (5599 -> 5548 process allocations).
- WebSocket single-event delivery: the connection owner now emits exactly one
  session-rich app event per wire event (`SessionText`/`SessionBinary`/
  `SessionClose`/`SessionClosed`/`SessionPressure`, `SessionOpen` +
  `SessionAccepted` on open). It no longer also emits the legacy
  `Text`/`Binary`/`Close`/`Open`/`Closed`/`Pressure` duplicate, so the payload
  is moved into a single delivery instead of cloned. Ping echoes its payload
  into the pong from a borrowed slice (`encode_server_frame_from`) and moves the
  owned payload into the app `Ping` notification -- no clone. Examples,
  specimens, and tests use the session-rich path. Measured: a 64-message
  WebSocket session drops from 133 to 67 app-handler turns (2.08 -> 1.05 per
  message), and `websocket_text_round_trip` from 4691 to 3865 process
  allocations.
- New proofs: `perf-ws-turns` (a hotpath probe asserting WebSocket app turns
  stay below the pre-dedup `2*N`) and the `grpc_h2c_unary_close` perf row,
  pinned in the perf test's label list.
- Structural HTTP/2 allocation pass (beyond the leaf copies above). Inbound
  frames are decoded with a borrowed view (`try_decode_frame_meta` +
  `data_payload_view` / `headers_payload_view`): the server read loop takes its
  buffer out and processes DATA and HEADERS straight from a borrowed slice, with
  no `Frame { payload: Vec }` per inbound frame (only a streaming chunk, which
  must outlive the buffer, still copies; control frames keep a cheap owned
  copy). The buffered response is coalesced — HEADERS + every DATA frame +
  trailers are framed into one queued buffer, so a response is one outbound
  slot and one TCP write instead of one per frame (frame boundaries, peer max
  frame size, END_STREAM, and flow control unchanged on the wire). The response
  header block is pre-sized and content-length is formatted into a stack buffer
  (the perf allocator counts each `Vec` growth realloc). Measured on
  macOS/aarch64: `perf-h2-alloc` 3075 -> 2434 (48.05 -> 38.03/request, ~20.8%);
  `http2_h2c_steady_state_small` whole-process allocations 1570 -> 1249
  (~20.4%) and allocated bytes 426776 -> 234072 (~45%); the `grpc_h2c_unary_close`
  row rides the same server path (5599 -> 4964, ~11.3%). Steady-state p50
  improved (209 -> 182 us); the `connection_setup` rows are connect-bound and
  noisy, with allocation counts the trustworthy signal. The residual is now the
  inbound HPACK request model and the per-request runtime service call, not
  framing — documented in the phase notes.
- Not a production performance claim: macOS/aarch64 local/alpha, Linux/x86_64
  evidence still pending.

### Protocol Perf Rows And Byte-Path Cost

- `examples/systems/perf_native` gains Tina-only native protocol rows
  (`comparison_baseline=none`): HTTP/2 h2c (`http2_h2c_close_request`,
  `http2_h2c_keepalive_sequential`, `http2_h2c_steady_state_small`) and
  WebSocket (`websocket_open_close`, `websocket_text_round_trip`,
  `websocket_steady_state_small`). Each drives the real Tina server isolate over
  a raw socket client, the same shape the HTTP/1 rows use. Rows carry a
  setup-vs-reuse `kind` so connection setup cost is never silently folded into
  steady-state service cost, and the perf test pins the label list and the
  setup/reuse classes so a row cannot quietly disappear.
- The buffered HTTP/2 response path now builds each DATA frame straight into the
  outbound queue (a new `push_frame_header` writes the 9-byte header, then the
  body slice is appended) instead of cloning each chunk into a `Frame` and
  re-encoding it. A buffered response body chunk is copied once instead of
  twice; measured at exactly one fewer allocation per response (3139 -> 3075
  allocations over 64 warmed h2c responses on macOS/aarch64). The wire output is
  unchanged and the per-frame bounded-queue admission is preserved, so no
  replay-visible fact moves.
- New proof: the `perf-h2-alloc` ceiling inside `hotpath_probes_report_and_stay_bounded`
  pins the post-rewrite allocation count (whole-process counting lives in the
  single-test hotpath binary so a parallel test thread cannot contaminate it),
  and `http2_multi_frame_response_marks_end_stream_only_on_last_data_frame`
  asserts multi-frame body integrity, a single terminal `END_STREAM`, and that
  HEADERS does not claim `END_STREAM` while a body follows.
- `websocket_capacity_fill_probe` is a deterministic capacity-fill pressure row:
  an over-cap echo raises a typed `SessionPressure` on the connection and closes
  the session without writing the frame. It proves typed pressure via the public
  send/report path without sleeping on a slow client.
- `scripts/perf_record.sh` records the new `native` row family and defaults its
  history to `.intent/phases/152-protocol-perf-byte-path/perf_history.jsonl`.
- Not a production performance claim: rows are local/alpha, the tails are wide
  under one single-shard worker, and the buffered-response body clone, inbound
  `data_payload` clone, streaming/chunked framing, and gRPC request body copies
  are named as remaining cost rather than hidden.

### Explicit-Step I/O Purity

- The Phase 151 readiness-driven worker park has been removed. Betelgeuse is
  back to one progress primitive, `step()`, and Tina's live workers again use
  explicit stepping plus bounded idle re-poll. This preserves the
  completion/event architecture at the cost of the known HTTP tail regression.
- Linux io_uring socket reads/writes again use `MSG_DONTWAIT`; send paths still
  include `MSG_NOSIGNAL`. There is no hidden cross-thread I/O wake channel in
  the threaded runtime.
- `idle_repoll_interval` / `idle_wait` once again drive the single-shard idle
  park policy: pending runtime-owned work gets the shorter bounded re-poll, and
  fully idle workers use the longer wait.
- Mailbox-owned readiness remains as the safe part of Phase 151: `Mailbox`
  still has required `is_empty()`, and the scheduler scan skips expensive
  `recv` calls on quiet isolates. The empty-to-nonempty wake hook experiment was
  removed with the I/O park.

### Scheduler, Turn, and Tail Performance

- Tail-aware hotpath rows: reports now carry p90/p99 alongside p50, the
  p90-over-p50 / p99-over-p50 / range-over-p50 per-mille ratios, a
  scheduler-gap threshold/count and max gap, and a `traced` flag. Each key path
  emits a traced and an untraced `*_tail` row so observer overhead is visible
  and never confused with runtime cost. `perf_record.sh` records the new
  fields; old rows still parse.
- Bounded worker hot-drain: the shard worker drains in an explicit burst capped
  by rounds and elapsed time, re-polling the command queue between bursts so a
  flood of always-progressing local work cannot hide a command or shutdown. A
  pending-work-aware park services a pending timer or I/O at a short re-poll
  interval while a fully idle worker parks longer; defaults preserve prior
  behaviour.
- Bounded backend completion drain: the driver-advance layer no longer drains
  every ready completion in one unbounded batch. Completions are delivered in a
  deterministic FIFO up to a per-step budget; the remainder carries over,
  preserving order, failure truth, and accounting.
- Host-call fast lane: `call_blocking` routes through a typed worker command
  instead of a boxed closure, cutting one host allocation per call (warmed
  `hotpath_call_blocking` host allocations 2 -> 1, process 6 -> 5) while
  preserving Full/Closed/Timeout/Rejected truth and bounded admission.
- Linux/x86 evidence: warmed `call_blocking` p50 improved on a dedicated-CPU
  cloud box (about 25.7us -> ~14us across runs), with HTTP rows steady. A
  controlled single-machine sweep showed the HTTP hot path is dominated by the
  worker re-polling the I/O loop on a timer rather than by request work —
  shrinking that re-poll interval cuts HTTP p50 about 5x. Phase 157 keeps the
  explicit re-poll architecture despite that cost. All numbers are local/alpha
  evidence on a single machine, not a production performance claim.

### Structural HTTP/Runtime Performance

- Moved perf history defaults to Phase 149 and added hotpath counters that
  separate trace-event stages from handler turns, backend calls, service calls,
  successful completions, and rejected completions.
- Added steady-state HTTP/1 keepalive comparison and hotpath rows that reuse
  warmed connections, so request cost after session setup is visible beside
  connect/accept rows.
- Added a narrow runtime terminal completion action for backend calls:
  `Message`, `StopRequester`, and `Noop`. Native HTTP/1 uses it for successful
  full TCP write-close completions, removing the extra `WroteClose` handler
  turn while preserving partial-write and failure paths.
- Added trace and pressure vocabulary for terminal completion actions and
  terminal-action-on-failure rejection, with live and simulator delivery paths
  preserving failure truth.
- Extended `perf_native` rows to include steady-state keepalive small/fixed
  HTTP/1 comparisons against Axum/hyper. Local macOS/aarch64 evidence remains
  alpha evidence, not a production performance claim.

### Native Performance Rows And Soak Truth

- Moved perf history to Phase 148 and taught `perf_record.sh` /
  `perf_check.sh` to record and compare `hotpath` rows, including
  `stage_count` and process allocations. The checker still uses loose,
  matching platform/arch/profile gates; ordinary verify does not fail on
  shared-machine p50 wobble.
- Added a manual Ubuntu `perf` workflow that runs `make perf-record` and
  uploads the Phase 148 JSONL evidence, so Linux/x86 rows can be collected
  without making normal CI performance-sensitive.
- Presized coalesced HTTP/1 buffered response writes so the connection reserves
  head + body capacity once instead of growing the buffer after encoding the
  head. Wire behavior and body-pressure accounting are unchanged.
- Tightened native hotpath proof with named HTTP stage-count ceilings and
  process-allocation evidence for every hotpath row.
- Strengthened `mini_saas_api` soak proof with direct notify/outbound-pool
  activity fields (`notify.attempted`, `outbound.acquired`,
  `outbound.released`, `outbound.retired`) instead of inferring pool coverage
  from generic pressure or errors.
- Added `make proof-long-soak`, an ignored opt-in long soak for
  `mini_saas_api` that defaults to 10 minutes and supports
  `TINA_LONG_SOAK_SECONDS=3600` for one-hour runs.

### HTTP Hot-Path Evidence And Allocation Cleanup

- Added Phase 147 HTTP hotpath probes for native HTTP/1 close, keepalive, and
  fixed-body paths. The reports now include `stage_count` so turn-heavy paths
  are visible instead of guessed from latency alone.
- Tightened perf history rows: `perf-process` now includes process allocated
  bytes, and `perf_record.sh` / `perf_check.sh` store and compare by
  platform, architecture, and release profile.
- Removed benchmark-client request-format allocation noise from the native
  perf specimen and presized common HTTP/1 request/response encoder buffers.
  This is still local alpha evidence, not a production performance claim.
- Coalesced small no-metrics buffered HTTP/1 responses into one TCP write,
  while keeping large buffered responses split and preserving exact body
  pressure accounting when metrics are enabled. Local hotpath evidence moved
  fixed-body close from 33 to 28 observed stages and four-request keepalive
  from 111 to 91 observed stages; the generic close row remains noisy.
- Added the terminal `TcpWriteClose` runtime rail for small TCP close
  responses and tightened it after hostile review so it obeys ordinary
  close-wins truth: sibling pending reads are rejected as `ResourceClosed`, the
  terminal write-close call completes normally, and in-flight call state is
  reclaimed in live runtime and simulator paths.
- Added an HTTP body-pressure perf probe that drives `max_body_bytes` overload,
  records typed `full` pressure, projects `BodyMetrics` into service-pressure
  surfaces, and proves final current drains back to zero.

### Protocol Chaos And Byte Replay

- Made protocol bad-peer behaviour proof-shaped. Every bad-peer story now
  reduces to one typed `ProtocolChaosReport` (family, byte tallies, peer and
  terminal action, app delivery count, close/reset/status, the typed
  `ProtocolFact` sequence, elapsed budget, and unsupported facts), with a
  `ProtocolChaosCase`/`ProtocolChaosExpectation` to assert it. The existing
  `BadPeerOutcome` is unchanged and folds into a report via
  `ProtocolChaosReport::from_bad_peer`.
- Added a pure WebSocket session engine and a hermetic compliance corpus to
  `tina-proof-harness`: valid text/binary, valid fragmented text (including a
  codepoint split across frames), invalid UTF-8 across fragments, reserved
  bits without an extension, oversized control and message frames, masking
  direction, and ping/pong and close-handshake edges. Each case names the
  exact app messages, so valid data is proven to reach app code once and
  malformed bytes are proven never to.
- Added `ProtocolByteReplayCase`: save a bad-frame case as ordered byte
  chunks, reproduce it, and shrink it down to the minimal chunk set that still
  triggers the bug, refreshing the expected facts for the smaller case. The
  expected facts are pinned as a count plus a stable fingerprint over the
  typed `ProtocolFact` values — never debug text. Unsupported facts or an
  over-budget case fail closed and never pass as an exact replay.
- Added hermetic HTTP/2 and gRPC bad-peer probes that map malformed framing to
  typed facts and outcomes — invalid frame size, duplicate pseudo-header, DATA
  after stream close, RST_STREAM mid-body, GOAWAY with active streams,
  flow-control window exhaustion, gRPC missing `grpc-status` (defaulted to
  UNKNOWN), and oversized gRPC messages — instead of a bare "connection
  closed".
- Added `LiveReplayFact::Protocol(ProtocolFact)` so a live capture can save
  protocol facts beside capacity facts; its display names the protocol family.
  A capture mixing protocol and capacity facts fails replay if either family
  diverges, and `classify_protocol_facts` tells a real divergence apart from a
  live-only simulator-coverage gap (`UnsupportedProtocolFact`).
- Updated `make proof-fast` to run the bounded protocol corpus, `make
  proof-bad-peer` to print typed report lines with `--nocapture`, and `make
  proof-soak` to repeat the corpus at a higher count. Documented when to reach
  for the proof harness versus a local test in the systems README.

### Outbound Connect And Session Managers

- Added unresolved outbound endpoints (`HttpEndpoint`, `Http2Endpoint`,
  `GrpcEndpoint`, `WebSocketEndpoint`) that resolve into the existing
  low-level targets at a chosen address, preserving Host/authority, SNI,
  trust roots, and ALPN truth. The resolved targets stay as escape hatches.
- Added `ConnectPolicy` over bounded runtime DNS and TCP/TLS connect, with
  address-family ordering and a Happy Eyeballs policy. It validates before
  first use (no zero attempt caps, no zero deadlines, no concurrency above
  the total cap) and exposes stable budget surfaces.
- Added the `ConnectAttempts` connect helper: it classifies a DNS result
  (Full/Closed/Timeout/Failed distinct from any connect failure), admits a
  bounded candidate set through `BoundedItems`, races attempts via a
  `CallGroup`, cancels losers when a winner appears, and tombstones any loser
  that completes late so it can never become a user success. Typed
  `ConnectReport` keeps the ordered attempt list with per-attempt family and
  reason, the winner, and the cancelled-loser / late-completion counts.
- Added `WebSocketClientManager`: a reconnecting WebSocket client over a
  bounded connection pool with one current session, bounded reconnect, a
  generation guard that drops stale-session replies, bounded retained
  closed/stale reports, drain-on-shutdown, and per-session pressure. A live
  closed-port reconnect-storm test proves the path is bounded and leaks no
  sessions or attempts.
- Added fixed-endpoint `Http2ClientPool` and `GrpcClientPool`: round-robin
  over healthy endpoints, a per-connection in-flight stream cap, a
  pre-connect waiter cap, idle/stale retire, and `NoHealthyEndpoint` when
  every endpoint is down. HTTP/2 transport truth (reset/GOAWAY/ALPN) stays
  separate from gRPC status truth.
- Wired manager/pool/connect caps into budget manifests with a live pressure
  join; a stale or missing manifest row fails the consistency check.
- Added the outbound-clients user-guide page (endpoint → policy → manager).

### Local Performance Evidence

- Added `tina_proof_harness::PerfReport`, a small wrapper over load/soak
  reports that prints local-machine timing, pressure, capacity-surface counts,
  leak truth, platform/profile/git metadata, nanosecond fields for fast rows,
  optional allocation counts, and an explicit `comparison_baseline=none` field.
- Added `make perf`: it runs the existing portable runtime cost rows in release
  mode, proof-harness perf report tests, native Tina-vs-bounded-Tokio rows
  (`host_enqueue`, `observed_admission`, `host_request_reply`,
  `service_request_reply_chain`, HTTP/1 close, HTTP/1 keepalive, and HTTP/1
  fixed body), and a whole-service `mini_saas_api` HTTP + SQLite +
  outbound-pool load run that prints both grep-friendly and JSON perf lines.
- Added `make perf-compare` for the native comparison rows only, plus
  `tina_proof_harness::PerfComparisonReport` for median-of-five
  ratio/semantic-match output.
- Wired the `mini_saas_api` soak path to attach live `/debug/capacity`
  observations to its `LoadReport`, so the perf row carries capacity surfaces
  instead of timing without boundedness truth.
- Added owned-buffer TCP/TLS runtime calls (`tcp_read_buf`,
  `tcp_write_owned`, `tls_read_buf`, `tls_write_owned`) so hot paths can move
  reusable buffers through effects and get them back instead of allocating a
  fresh read buffer or cloning pending write bytes per call. The simulator
  supports the same calls, and live/sim TCP tests pin the public helper shape.
- Moved native HTTP/1 server, one-shot client, and keepalive client read/write
  paths onto the owned-buffer calls. The server keeps its body-pressure,
  keepalive, chunked, and WebSocket semantics; request-body chunk delivery also
  avoids the common `drain(..).collect()` path.
- Tightened HTTP encoder allocation paths by writing content lengths and
  chunked frame headers directly into the output buffer instead of allocating
  temporary strings.
- Extended load/perf output with p90 latency fields, moved perf history to the
  Phase 146 evidence file with `TINA_PERF_HISTORY_FILE` override, and made
  `perf-check` require enough local history plus an absolute tolerance before
  failing tiny-row comparisons.

### Request Scope End-To-End

- Added `ScopedRequestReport`, the request-level aggregate that wraps a
  `ScopeCancelReport`, the post-removal `RequestScopeSetCapacityReport`,
  late-result counts, ignored-timer counts, and any `UnsupportedScopeRow`s —
  one typed value for "the request went away; here is what was cancelled, what
  had settled, how much capacity came back, and what could not be cancelled."
- Added `ScopedTimerSet` / `ScopedTimer`: a bounded tombstone timer for request
  deadlines. Plain `sleep` is not cancelable, so cancelling tombstones the
  ticket and a late physical fire is reported as `IgnoredLate` and skipped,
  never a pretended physical cancel.
- Added `tina_http::scope` adapters that register HTTP rails into a request
  scope: `scoped_request_body_pull`, `scoped_websocket_send` / `_report` /
  `_close`, `scoped_grpc_unary`, the generic `scoped_operation`, and the
  protocol-honest `cancel_response_source`. A scoped WebSocket operation is a
  single send/report/close, never the whole session.
- Added `system_scoped_request_tree`: one streaming route, one tombstoned
  deadline, one cancelable child, one report. A mid-body client disconnect
  cancels the child, the timer fires late and is ignored, and the scope slot is
  reclaimed, with sim/replay agreement on the scope-set surface.
- `mini_saas_api` now owns a request scope on its notify path: the outbound
  keepalive request call registers as a cancelable child, the scope set and
  per-request child cap are declared as `request.scope_set` /
  `request.scope_child_cap` budget rows and joined with live pressure, and the
  set returns to zero in-use under load. Shutdown runs an owner-stop scope
  sweep (`DrainScopes`) that cancels any still-pending child rail and reports
  unreleased capacity; a focused proof drains a notify held mid-outbound and
  shows its parked child cancelled.

### Hostile Review Fixes

- Tightened the bounded fanout rails after review: `BroadcastTracker`
  construction now stays behind `BroadcastTargets`, request-sized examples map
  `BoundedItems` into effects only after admission, and the boundedness guide
  teaches that safer copied path.
- Made overload hidden-buffer assertions reject malformed weighted reports that
  declare a weight cap without a real current/high-water observation.
- Made the live replay bugbox system smoke prove the saved bugbox file exists
  and clean it up after the run.

### Overload Bugbox Replay

- Added overload-focused DST helpers: `capture_overload_run`,
  `save_overload_bug`, `replay_overload_bug`, and bounded capacity assertions
  (`check_no_hidden_buffering`, `assert_no_hidden_buffering`,
  `check_overload_visible`, `assert_overload_visible`). They sit on top of the
  existing live trace-to-sim replay machinery and keep unsupported live facts
  fail-closed.
- Added unit coverage for two overload-shaped saved cases (broadcast/slow-peer
  and pool waiters), unsupported-fact rejection, saved bugbox hints, and hidden
  buffering assertions.
- Updated `system_live_replay_bugbox` and the user guide to use the overload
  bugbox names when pressure facts are the thing being captured.

### Bounded Broadcast Fanout

- Added `tina_runtime::broadcast`: `BroadcastTargets`, `BroadcastTracker`,
  `BroadcastReport`, and `broadcast_observed` for the common
  room/session/pubsub shape. A service must build a bounded target list before
  it can create many observed-send effects, so request-sized fanout has to pass
  through a service-owned cap.
- Updated `examples/specimen_real_io_chat` to use the broadcast helper. The
  specimen now distinguishes the client-requested burst from
  `max_broadcast_targets`; targets over the cap are counted as visible `Full`
  before they become runtime effects, while admitted targets still report
  `Accepted` / `Full` / `Closed` through ordinary continuation messages.
- Documented the bounded-broadcast copied path and the mailbox sizing rule:
  fanout owners must provision continuation slots for admitted targets, not for
  arbitrary request size.

### Service-Owned Bound Rails

- Added `BoundedItems`, `BoundedEffects`, and `bounded_batch` to make
  request-sized loops pass through an explicit service-owned cap before they
  become many runtime effects. The helpers reject zero caps, stop at the first
  over-cap item/effect, and preserve order for admitted work.
- Added `assert_service_owned_bound(...)` for specimen and system smoke tests:
  a copied assertion that catches unbounded declarations, missing observations,
  zero caps, and observed work that exceeds the configured cap.
- Updated `specimen_dynamic_worker_pool` and
  `specimen_sharded_fanout_read` to use bounded effect rails and bound
  assertions, and documented the "what is the max in-flight work, and did the
  service choose it?" review rule.

### Unix-Domain Sockets On The Per-Shard I/O Rail

- Moved Unix-domain sockets off their private blocking worker thread and onto the
  per-shard Betelgeuse completion rail — the same substrate TCP and TLS already
  ride, on the shard thread, with no `std::os::unix::net` and no worker. Bind,
  accept, connect, read, write, and close are now completion-backed, with the
  same lane discipline as TCP (one accept/read/write lane each, `ResourceBusy`
  on duplicates, close-wins cancellation, tombstoned shutdown).
- Added the narrow Unix-domain support the substrate lacked directly to vendored
  Betelgeuse (`bind_unix` / `connect_unix` plus socket-file lifecycle: a stale
  socket file is cleared before bind, and a listener's socket file is unlinked on
  close — only ever a socket inode, never a regular file or symlink at a
  user-supplied path). Accept, recv, send, and close already worked at the
  substrate's family-agnostic socket layer.
- Every runtime-owned rail now self-classifies in the capability report as
  completion-backed, fallback-worker, justified-blocking-lane, simulator-scripted,
  or unsupported. DNS (platform resolver) and process spawn/wait carry written
  reasons for staying bounded blocking lanes; the storage rename/remove/readdir/
  metadata worker is named a fallback, not a general storage lane. The Unix rail
  is now reported completion-backed, not poll-backed.
- Added a static rail-inventory guard (`scripts/rail_inventory_guard.sh`, run by
  `make verify`) that fails the build if a runtime-owned rail adds a worker
  thread, a blocking std socket, or blocking `std::fs` work without being listed
  in `.intent/runtime-rail-inventory.txt` and classified in the capability
  report. A runtime test keeps the file inventory and the capability classes in
  sync.

### TLS On The TCP Rail

- Moved native TLS off per-operation worker threads and onto the runtime's own
  Betelgeuse TCP rail: the runtime now owns a rustls connection (sans-I/O) per
  TLS stream and drives handshake/read/write/close on the shard thread as TCP
  completions arrive. No TLS worker thread, no second socket stack.
- A Tina TLS client and server can now share one runtime — previously the single
  TLS worker deadlocked both sides of one handshake. The `specimen_native_https`
  Tina side now runs an HTTPS client and server together in one runtime.
- Public `tls_*` call signatures are unchanged. The TLS layer owns its socket
  exclusively and serializes its internal reads/writes, so a single `tls_*` call
  can interleave I/O without a self-inflicted `ResourceBusy`. `tls_lane_capacity`
  is now the shard-total cap on in-flight TLS ops. The HTTPS listener's accept is
  completion-driven (a real "wait" deadline, not a busy-wait poll).
- Preserved the security posture (cert validation, SNI/name check, DER-root
  policy), the clean-`close_notify`-vs-truncation distinction, and the
  cancellation/close-wins/timeout-tombstone outcomes. Simulator TLS is unchanged.

### Live Durability On The Per-Shard I/O Rail

- Moved the live runtime's durability reads/writes/fsync/size onto the per-shard
  Betelgeuse completion rail. Journal appends, snapshot commits, snapshot loads,
  journal replays, and parent-directory syncs now ride the same reactor the shard
  already runs for its sockets, instead of a separate storage worker thread.
- Kept a thin bounded off-shard fallback worker only for the operations the I/O
  rail has no opcode for — rename, remove, readdir, and metadata (plus internal
  recursive directory creation and torn-tail truncation). The capability report
  names that fallback explicitly (`storage_metadata_fallback`) and now reports the
  durability family as completion-backed rather than lane-backed-blocking.
- Preserved recovery semantics: append-before-apply ordering, torn-tail, checksum,
  duplicate/out-of-order index, `CommitUncertain`, and `StorageFull`/`StorageClosed`
  produce the same typed outcomes as before, with the pwrite completion harvested
  before fsync is submitted. Concurrent writes to the same journal/snapshot path
  are serialized so they cannot interleave their offsets (matching the old
  single-worker behavior); distinct paths still overlap. The explicit-step oracle
  keeps its synchronous inline path unchanged.

### Config And Budget Manifest

- Added `tina_runtime::budget`: a `ServiceBudgetManifest` that declares a
  service's bounded surfaces (mailboxes, pools, body bytes, lanes, shard pairs,
  protocol sessions, connect attempts, bridge in-flight, event sinks, pending
  replies/calls, request scopes) in one place. Each `BudgetSurface` carries a
  stable name, kind, unit, cap or explicit unbounded policy, capacity mode,
  owner/shard labels, replay impact, and source.
- `manifest.validate()` rejects bad caps before runtime startup with typed
  `BudgetValidationError`s — duplicate/empty/whitespace names, zero caps (which
  would deadlock a queue, fake EOF on a byte budget, or disable a rail),
  bounded/unbounded mode conflicts, policy-rejected or expired unbounded modes,
  secret-looking printable fields, and missing required surface kinds. No silent
  clamping, no hidden unbounded default.
- Added config adapters that build manifest rows from existing configs:
  `LocalSystemConfig`, `ThreadedRuntimeConfig`, `MultiShardRuntimeConfig`,
  HTTP/1 server/client/pool configs, HTTP/2 server/client limits,
  `WebSocketLimits`, and the SQLite bridge install config. The mapping is exact
  (one field, one row); adapters never invent caps. Time deadlines are
  deliberately not surfaced — the unit vocabulary is count and weight, not time.
- Added consistency checks (`compare_capacity_summary`,
  `compare_service_pressure`, `compare_manifest`) returning typed
  missing/extra/cap/unit/mode/replay-impact rows, and `manifest.report(...)`,
  which joins declared caps with observed `cur`/`high`/`full` facts from the
  live pressure report. Observed numbers always come from runtime reports, never
  guessed config.
- Added `manifest.replay_export()`: a stable hash over the replay-affecting
  surfaces plus the list of display-only surfaces it ignored. Changing a
  replay-affecting cap changes the hash; changing a display-only cap does not.
- Migrated `mini_saas_api` to declare every cap once in a budget manifest and
  read caps back from it. The service validates the manifest before binding,
  joins it with the live pressure report at shutdown, pins the manifest hash
  into its live-replay fact, and a `tests/budget.rs` suite proves the documented
  caps are exactly the manifest rows and every live surface has a row.

### Race-Surface Honesty

- Drew the explicit line between what replay proves and what it does not: the
  simulator is single-threaded and proves logical interleavings (message,
  timer, and completion order) with byte-for-byte replay; it does not, and
  cannot, catch physical memory-ordering races on the live parallel runtime.
  Updated the README, the DST user guide, the roadmap north star, and the Odin
  review memo to stop implying replay reaches the physical substrate.
- Enumerated and verified the custom shared-memory race surface in
  `.intent/SYSTEM.md` and `.intent/race-surface-allowlist.txt`: the SPSC mailbox
  and `SharedCapacityScope` are the only first-party lock-free structures.
  Cross-shard transport is `std::sync::mpsc`; the remaining atomics are
  single-writer handshake cells, metrics counters, or id generators.
- Added a loom model for `SharedCapacityScope` reserve/admit/release/high-water
  behavior, proving the cap holds and counters stay conserved under every short
  interleaving.
- Added a `make verify` guard that fails when a new synchronization primitive
  (`UnsafeCell`, `unsafe impl Send|Sync`, or atomic) appears in core code
  outside the reviewed allowlist, so a new primitive cannot land without review
  and a model.

### Hard Shard Pinning

- Made `configured_core` a real OS pin instead of advisory intent: on Linux a
  shard worker pins itself with `sched_setaffinity` over the process's allowed
  affinity mask and reports `AffinityStatus::Applied` with the observed core,
  read back inside the worker via `sched_getcpu`. `configured_core` is an OS CPU
  id checked against the allowed mask, not an index into `0..num_cpus`.
- A requested core outside the allowed mask reports `AffinityStatus::Failed`
  with a reason and the worker keeps running unpinned — never a silent mis-pin.
  Platforms without a hard pin (macOS and others) report
  `AffinityStatus::Unsupported`; default `None` stays `NotRequested` and makes
  no affinity call. Helper-lane threads are never pinned.
- Retired `AffinityStatus::AdvisoryOnly` as an outcome `configured_core` can
  produce; the variant is kept only as a reserved slot for a possible future
  intent-only knob.

### Copied Service Ergonomics And Workflow Helpers

- Added the Phase 120 copied-service path: canonical system specimens for a
  production-shaped Tina service, companion/smoke proof crates, a "which noun do
  I use?" guide, refreshed systems/finding docs, and cleaned WebSocket app-control
  examples away from stringly bootstrap/tick messages.
- Added `RunCapture` / live-replay proof-harness helpers and task-shaped bug
  workflow wrappers so copied services can capture a run, save a bug, replay it,
  and shrink it without stitching together lower-level DST APIs.
- Added fairness/load assertion helpers for common user claims and expanded
  bounded workflow helpers with join-all / select-next shapes over named
  cancelable calls while preserving branch identity, capacity, cancellation, and
  late-reply truth.

### Cross-Shard Child Ownership

- Made cross-shard observed children owned within the local multi-shard runtime:
  parents can spawn onto another shard, learn the typed child address, stop or
  clean up children through bounded remote control, and observe lifecycle state
  without trace spelunking.
- Added child lifecycle reports, remote child-control trace facts, replay/DST
  projection support, threaded/explicit runtime query paths, and a
  `specimen_cross_shard_child_ownership` example.
- Preserved bounded shard-pair pressure and stale-address truth: remote child
  stop/restart/cancel races report typed outcomes instead of relying on a hidden
  registry or unbounded owner queue.

### Trace Timeline Export

- Added `tina_tracing::TraceTimeline` and Chrome Trace JSON export from
  `TraceSnapshot` / runtime traces using logical event-id time. Timeline export
  is an offline visual view; `RuntimeEvent` remains canonical replay truth.
- The exporter includes shard/isolate metadata, partial-trace truth, handler and
  call slices where pairs exist, deferred-reply spans, child lifecycle and
  protocol facts, pressure/capacity counters, cause/call ids, deterministic
  ordering, and visible unmatched begin/end events.
- Added `tina-tracing/examples/export_timeline.rs`, tracing docs, and timeline
  tests covering empty/partial traces, typed failures, protocol facts, child
  lifecycle, JSON validity, and deterministic output.

### Fairness And Load Behavior

- Added `LagObservation` to `tina_runtime::FairnessReport` for copyable
  `progress_gap_turns` report lines. It stays honest about the unit: handler
  turn progress folded from the trace, not wall-clock scheduler latency.
- Extended `tina-proof-harness::load` with Phase-121 names
  (`LoadProfile`, `LoadRunReport`), end-of-run `LoadObservation`,
  `SurfacePlateau`, trace hash, late-result count, and surface leak
  reporting. Surface lines now quote sloppy names, carry unavailable
  surfaces explicitly, and preserve count/weight/shared-weight axes instead
  of collapsing them into one lossy number. Existing `LoadRun`/`LoadReport`
  call sites keep working.
- Updated `specimen_hot_key_fairness` so Tina prints trace-derived fairness
  progress, progress-gap lag observations, and a stable trace hash while
  asserting cold shards process their admitted work instead of comparing raw
  hot/cold turn counts as if differently sized workloads should have identical
  progress.
- Tightened the proof-harness `ReconnectStorm` scenario so `count` means the
  total number of connection attempts. Closed-port storm tests now prove
  aggregate connection errors deterministically instead of depending on kernel
  listen-backlog timing.

### Native WebSocket Client Session

- Added a native bounded WebSocket client session to `tina-http` with explicit
  `ws://` and `wss://` targets, native HTTP/1.1 upgrade validation, client
  frame masking, pull-shaped `Receive`, typed send/report calls, TLS trust-root
  truth, protocol-close facts, and visible pressure counters.
- Hardened WebSocket client terminal behavior: connect failure returns typed
  `Connect`, failed handshakes preserve the pending connect reply,
  receive-before-connect returns `NotConnected`, close remains reportable, and
  call-only messages delivered by `try_send` increment
  `wrong_lane_messages` instead of disappearing as uncounted no-ops.

### Live Trace Replay Capture

- Added the blessed live-weirdness to simulator-replay workflow:
  `capture_live_run(...)`, `LiveReplayCapture`, source metadata,
  truncation/unsupported-fact truth, saved-case round trips, and
  `shrink_captured_replay(...)`. Captures now carry seed, config, explicit
  history, expected trace shape, replay facts, and source completeness so live
  evidence cannot quietly masquerade as exact replay when facts are missing.
- Updated the live replay bugbox system specimen to exercise live capture,
  saved-case read/write, exact replay, fail-closed unsupported facts, and
  fact-preserving shrink output.

### Hostile Review Fixes

- Made SQLite and SQLx bridge installed addresses callable capabilities
  (`CallAddress`) instead of raw sendable addresses. The copied bridge helper
  path now uses `call_typed`, and doctests prove bad `SendAddress` usage does
  not compile.
- Made `CallGroup` record a loser reply that arrives after a first-success
  winner but before the loser cancel outcome. Real races can queue that reply;
  services should not panic on it.
- Added call-shaped startup replies to the plain HTTP listener, matching the
  HTTPS listener shape for `Ready`, `AlreadyStarted`, and bind errors while
  preserving the old send-shaped `Start` path for existing tests/specimens.
- Added `Http2ClientReport::wrong_lane_messages` so HTTP/2 call-only messages
  delivered through `try_send` are visible in release builds instead of
  disappearing as uncounted no-ops.

### Ergonomic Obvious Fixes

- Added `SharedCapacityReservation` and `SharedCapacityScope::charge(...)` so a
  request can reserve multiple shared budgets all-or-nothing. Failed later
  charges roll back earlier charges before returning the refusing
  `SharedScopeFull`. `system_api_gateway_limits` now uses this instead of
  hand-written in-flight/body rollback.
- Added `CallGroup::start_cancelable(...)`, the copy path for first-success
  races. It reserves the generation token, builds the cancelable continuation,
  stores the handle, and returns the effect only after the group accepted the
  branch. `ergonomics_playground`, `specimen_cancellation_chain`, and the live
  `call_group` tests now use it.
- Added `UnixWriteAll` and `UnixReadToEof`, mirroring the TCP loop helpers for
  Unix-domain streams, including `Ok(0)` stuck-write detection.
- Added the unified `FileCopyBounded::next_effect(...)` /
  `FileCopyBounded::advance(...)` copy-pump path and migrated the local I/O
  specimen away from manual `next_leg` dispatch.

### Roadmap And Specimen Bookkeeping

- Updated `ROADMAP.md` so completed Wave A/post-122 work no longer appears as
  active implementation work: native HTTP/2/gRPC client parity, local I/O/codec
  and Unix IPC, admission/rate policy, production resource lifetime, durable
  local outbox/work recovery, and supervision/fairness are now treated as
  landed work with follow-up edges.
- Refreshed the current evidence snapshot to match the shipped protocol,
  persistence, bridge-extension, and supervision surfaces before the next
  ergonomics pass.

### Runtime Supervision And Fairness

Make owned work fail loudly and let a host prove progress without trace
spelunking. These are typed reports over facts the trace already records, plus
new typed outcomes for failure and cross-shard observed spawn (spawn +
learn-address; cross-shard supervision/ownership is a follow-on).

- **`spawn_observed(child).on_shard(shard)`** — spawn an observed child on
  another (local, in-process) shard and learn its address back through the same
  `.then(...)` continuation. The child constructor is `Send` and ships to the
  target shard, which registers it and replies with its address; the owner's
  continuation waits on the owner shard until the reply lands, and a
  `ChildStarted` fact records the learned address. Same-shard `spawn_observed`
  is byte-for-byte unchanged; the `Send` bound only appears on `.on_shard`.
  Isolates that never spawn cross-shard keep `SpawnObservedRemote = Infallible`
  and cannot construct the effect. Proven live (`MultiShardRuntime`) and in the
  deterministic simulator. (First sub-phase: spawn + learn address; cross-shard
  stop/restart/address-change remain follow-on work.)

- **`tina::Effect::Fail` (and `tina::fail()`)** — a handler can fail loudly
  without unwinding. The isolate stops and any in-flight caller settles
  visibly, exactly like a panic, but the runtime records a distinct
  `HandlerReportedFailure` fact and routes it through the same supervision
  policy: a supervised child restarts per its parent's policy and budget; an
  unsupervised one just stops. Panic and reported failure never collapse into
  one outcome. Wired through both effect erasers, the live dispatcher, and the
  simulator, so live and replayed runs agree; new trace tags are append-only.
- **`tina::Effect::StopChildren` (and `tina::stop_children()`)** — an explicit
  supervised shutdown. An owner stops every child it owns; each child stops
  through the normal path so its callers settle and a `ChildStopped` fact names
  it under the owner. The owner keeps running — pair with `stop()` for a full
  owner-then-children shutdown. Plain `Effect::Stop` is unchanged and never
  cascades, so children meant to outlive their parent still do.
- **`tina_runtime::SupervisorReport`** — a typed terminal supervision summary,
  folded from the trace for one owner (mirrors `PressureSummary::from_events`).
  Names children by stable ordinal with their latest incarnation, counts restart
  triggers / attempts / completions / skips / rejections, counts children the
  owner closed via supervised shutdown, and reports a distinct halt reason
  (budget exhausted vs supervisor stopped). Composes with the pressure and
  capacity readers over the same event slice. (Same-shard children only: a
  cross-shard observed child records its `Spawned` on the child's shard, so it
  is not reflected here.)
- **`tina_runtime::FairnessReport`** — the progress-count slice of fairness:
  per-isolate handler-turn and sleep-completion counts folded from the trace,
  plus a typed `StarvationWarning` (with a round-count-free `starvation_by_gap`
  form) that names the victim and the hot isolate rather than hiding a progress
  gap. Progress is turns taken and sleeps completed (deterministic), not a
  wall-clock promise. A proof runs a self-flooding hot isolate beside a
  steadily-ready neighbor and a recurring timer: round-robin keeps the neighbor
  within one turn of the flooder and the timer keeps firing under load.
  Ready-turn lag, timer lateness, and remote-drain-yield counts are **not** in
  this report yet (they need instrumentation the trace does not carry); the
  remote-flood-vs-local-command fairness proof shipped earlier.
### Durable Local State And IPC

A local service can record work before doing it, restart, and resume or report
the truth — without an exactly-once claim or a durable mailbox.

- **`tina_runtime::DurableOutbox`** — a bounded, restart-survivable record of
  local work. `enqueue` reserves a stable `WorkId` and frames a durable journal
  record; a full outbox returns `OutboxFull` carrying the original work back. The
  outbox is a sync state machine: it produces the bytes to append and consumes
  the append result, so Tina still owns the `journal_append` / `journal_replay`
  I/O and the outbox stays testable without a filesystem. First form is
  at-least-once: after recovery, recorded-but-not-completed work may run again.
- **Record-before-apply is a type rule.** `apply` requires a `RecordedWork`,
  which only a successful durable record (or recovery of still-pending work)
  produces — so apply-before-record cannot be written. `apply` consumes the
  token, so the same work cannot be applied twice. A failed append returns the
  original work in `AppendFailed`; `abandon` reclaims a staged slot when you
  decide not to record. Marking work complete is idempotent by `WorkId`
  (`AlreadyCompleted`), not a silent success. Compile-fail proofs pin both the
  apply-before-record and double-apply diagnostics.
- **Recovery names the tail.** `DurableOutbox::recover` replays the journal into
  a fresh outbox plus a `RecoveryReport`: pending work as ready-to-apply tokens,
  the ids already completed, and a `TailStatus` separating a clean tail, a
  repaired truncated tail, and an uncertain commit. A corrupt tail is rejected
  by name as `RecoveryError::CorruptTail`; over-capacity replay is rejected as
  `RecoveryError::OverCapacity`, keeping replay bounded. Completed work is listed
  as completed, never resumed as pending. `OutboxShutdownReport` names pending,
  abandoned, completed, and failed work at shutdown — no silent drop.
- **Journal compaction bounds growth.** `DurableOutbox::recover_compacted`
  rebuilds the outbox and also returns a compacted journal image — only the
  still-pending enqueues, re-indexed from 1, with completed and stale records
  dropped and `WorkId`s preserved. `tina_runtime::persistence::commit_file_atomic`
  swaps it in one durable step (temp + fsync + rename + parent-dir fsync),
  returning `CommitUncertain` when only the final fsync is unconfirmed.
- **Commit fences make uncertain recovery turnkey.**
  `persistence::{raise,clear}_commit_fence` + `commit_fence_present`, and
  `CommitConfidence::from_fence_present`, let a service flag a commit whose final
  durability step was interrupted, so the next recovery reports
  `TailStatus::UncertainCommit` instead of silently clean.
- **`ResumeQueue` drains pending work.** `RecoveryReport::into_resume` yields a
  queue whose `next_apply` applies the next pending item through the outbox,
  oldest first, skipping already-completed ids — the resume loop in one call.
- **Codec ordering integrity proven.** A length-delimited frame buffered before
  an oversize one is still delivered intact; only the oversize frame is rejected.
- **Specimen.** `examples/specimen_webhook_outbox` runs the full enqueue → send →
  mark-sent → restart → recover → compact → resume flow, comparing the durable
  outbox against a hand-rolled flat-file outbox.

### Runtime Fixes

- **Process group cleanup holds the Linux leader pid until descendant cleanup.**
  `process_run` now peeks child exit with `waitid(WNOWAIT)` on Linux, so a
  truncated stdout/stderr drain can kill the owned process group before the
  leader pid is reaped and possibly recycled. The non-Linux path stays
  best-effort and documented.
- **Completion-pressure truth documented.** `MailboxFull` at call-completion
  delivery is ratified as a distinct trace-only terminal class, and the
  recently-cancelled cause ring is documented as bounded best-effort attribution
  with an observable eviction counter.

### Ecosystem Hooks And Async Boundary

Public extension seams so third-party crates can grow the ecosystem without
private runtime access, plus docs that classify every async path.

- **`tina_codec::SyncCodec`** — an open sync-codec extension trait (the sealed
  `Framer` stays for the built-ins). Both built-in framers also implement it, so
  generic code drives a built-in or a custom codec. Codecs stay sync, bounded,
  and replayable; Tina still owns I/O.
- **`tina_runtime::ServicePolicy`** — an open admission/rate-policy trait
  returning typed `AdmissionDecision`. A policy decides; it never sends, retries,
  sleeps, or hides a queue. `ConcurrencyLimit`, `KeyedLimit`, and `RateLimit`
  implement it, so generic service code can drive built-in or custom policies
  through one shape.
- **`tina_runtime::RuntimeCapabilityReport`** — a read-shaped view over
  `RuntimeCapabilities` naming, per rail, supported / unsupported /
  simulated-only / cancel-backed / tombstoned / drain-backed, with a
  grep-friendly discovery report. It renames nothing.
- **Bridge author parts and the capacity surface / bounded event sink hooks**
  are the existing `tina_runtime::bridge` vocabulary, `CapacitySurfaceReport` +
  `CapacitySummary::push`, and `BoundedEventSink` — aligned and proven as
  extension hooks, not rebuilt under new names.
- **Five workspace-excluded extension smoke crates under
  `examples/extensions/`** using public APIs only: a custom capacity surface
  that joins a `CapacitySummary`; a custom `SyncCodec` driving a sim service; a
  custom per-key `ServicePolicy` proven replayable; a bounded-worker fake bridge
  proving setup/closer/metrics/pressure/shutdown and caller-timeout honesty
  (`ExternalWorkMayContinue`, never a claim that external work stopped); and a
  compile-fail crate whose `compile_fail` doctests prove an extension cannot
  import a private runtime module, mint a runtime-owned permit, or forge a
  private `BridgePressure` / `ResourceCapability`. Each crate ships a README
  command and a smoke test, and `make verify-examples` now walks them.
- **Docs:** `docs/tina-user-guide/25-extension-hooks.md` (the extension contract
  and where third-party code belongs) and `26-async-boundary.md` (native vs
  bridge vs unsupported, with the common Tokio ecosystem cases sorted).

### Resource Lifetime, Health, And Pool Retirement

Long-lived pooled resources can now age out, retire on a health verdict,
and report their shutdown — without the generic pool pretending it can
close handles it does not own.

- **New pool vocabulary** in `tina::pool`: `ResourceLifetime`
  (`max_idle` / `max_lifetime`), `ResourceHealth`
  (`Healthy`/`Suspect`/`Retire` + `disposition()`), `RetireReason`
  (`IdleTimeout`/`MaxLifetime`/`Unhealthy`/`ForceClosed`),
  `PolicyCheckPoint`, `RetiredResource`, `ResourcePolicyReport`,
  `RefillOutcome`, and `PoolShutdownReport`.
- **`WorkerPool` lifetime sweep.** `WorkerPool::with_lifetime(config,
  resources, lifetime, now)` plus a new `WorkerPoolMsg::Maintain { now }`
  retires idle resources past `max_idle` / `max_lifetime` and returns a
  `ResourcePolicyReport` naming each retired slot and reason. Time is the
  owner's: `now` rides in the message (drive it off a Tina timer), the
  pool never reads a wall clock. A *leased* resource past `max_lifetime`
  is reported old in `over_age_leased`, never stolen back from its
  caller. Idle age is stamped by the first sweep that observes the slot
  idle, so maintenance cadence bounds idle granularity.
- **Owner-driven refill.** The pool marks retired slots and reports them;
  it cannot build an `H`. `WorkerPoolMsg::Refill { resource_id, handle,
  now }` lets the owner install a fresh resource into a retired slot to
  reclaim capacity. Refill refuses a live (idle or leased) slot so a
  resource is never swapped behind a caller's back. `maintain_effect` /
  `refill_effect` are the call-site sugar.
- **Health is the caller's verdict.** A generic pool cannot probe an
  arbitrary handle. `ResourceHealth::Retire.disposition()` maps to a
  release that drops the resource; the pool reports the retire.
- **Typed shutdown report.** `PoolShutdownReport::from_pressure(mode,
  &pressure)` folds a close mode and a post-close snapshot into the
  lifecycle words — drain / force / closed / leased count — with
  `drained()` true once nothing is left out on lease.
- **HTTP/1 keepalive idle retirement.** `KeepaliveConnectionMsg::Maintain
  { now, max_idle }` closes a connection's idle socket proactively and
  replies `KeepaliveOutcome::Maintained { closed_idle }`. The pool slot
  stays leasable; the next request reconnects cleanly instead of
  discovering a dead socket on use. Closes the "no idle-connection
  timeout" gap the module previously named as out of scope. A mid-request
  connection resets its idle clock and is never closed by a sweep.
- **DB bridge pressure stays honest.** SQLite and SQLx bridges already
  project onto the shared `BridgePressure` vocabulary while keeping their
  own truth; this work changes neither and does not fake SQLx pool
  internals. The resource owner matrix records both rows.
- **Docs.** New `docs/resource-owner-matrix.md` — checked-in evidence of
  who owns the close/drain/force/report path for HTTP/1 keepalive, HTTP/2
  client connection, HTTP/2 stream slot, SQLite bridge, SQLx bridge, the
  generic `WorkerPool` handle, and local file/journal rails.
- **Tests.** `tina-runtime/tests/pool_lifetime.rs` (18): idle retire
  names why; never-leased retires on the first sweep; max-lifetime retire
  is not handed to a new caller; max-age wins precedence over idle in the
  reason; an idle-only policy never flags leased resources over-age; a
  health verdict retires; over-age leased is reported not stolen;
  multiple resources retire in one sweep; fill/retire/refill reclaims
  capacity; refill keeps generations monotonic (no ABA after a
  retire/refill); refill resets the age clock; refill serves a parked
  waiter; the `maintain_effect`/`refill_effect` sugar drives the pool;
  drain/force shutdown reports; refill rejections; `Maintain` on a closed
  pool retires nothing; no-policy `Maintain` is a no-op. `tina/src/pool.rs`
  (4): vocab unit tests for `ResourceLifetime` constructors,
  `ResourceHealth::disposition`, and `PoolShutdownReport`.
  `keepalive_pool.rs` (+2): idle sweep closes the socket and the next
  request reconnects; unconnected/freshly-used connections are left
  alone. Existing pool (19), keepalive (27), and bridge tests still pass.
- **Deferred to the durable local state and IPC plan**
  (`.intent/phases/126-durable-work-restartable-state/plan.md`): the
  durable restore/service half — `RecoveryReport`, append-before-apply
  type-state, corrupt/truncated/uncertain-tail outcomes, and durable
  specimens. Those names collide directly with that plan's `RecordedWork`
  / `CommittedWork` / `RecoveryReport`, so building them here would create
  a second design of the same surface.

### Admission And Rate Policy

(Pre-merge tightening — see the "fixes" sections below for the deltas
against the original landing.)

#### Plan-completion pass (round 3)

Closed the remaining gaps against the phase plan's proof/specimen list:

- **`specimen_rate_limited_worker` now paces with `RateLimit`.** The Tina
  side's pacing is a `RateLimit<()>` token bucket driven by `ctx.now()`
  (`Admitted` → process one; `RateLimited { retry_after }` → sleep, then
  ask again) instead of a hand-rolled `sleep(RATE_WINDOW)` + `SingleCallGate`.
  The bounded mailbox is still the backpressure surface and the Tokio-side
  parity holds.
- **New `examples/specimen_idempotent_retry`.** A runnable outbound-edge
  relay that uses `FullHandling::retry_backoff` for bounded, caller-owned
  retry against a flaky downstream, with the idempotency key named on the
  `Deliver { idempotency_key }` message. Proves: exactly-once charge across
  retries, visible budget exhaustion, no charge on exhaustion.
- **tina-sim replay proof.** `tina-sim/tests/admission_replay.rs` drives a
  `RateLimit` isolate off the simulator's virtual `ctx.now()` and shows the
  decision trace is byte-identical across runs and *independent of seed*,
  with `retry_after` equal to the exact token window under sim time.
- **API-shape proof for explicit retry.** A test pins that
  `FullHandling::shed()` never schedules a retry and that the only path to
  `RetryAfter` is an explicitly-constructed `retry_backoff(Backoff)`; the
  admission policies themselves expose no retry method.
- **`system_api_gateway_limits` gains the body-bytes dimension.** Each
  request now charges two shared weighted budgets — `gateway.in_flight`
  (request weight) and `gateway.body_bytes` (body size) — admitted only if
  both have room, with the in-flight charge rolled back if the body budget
  is full. A new smoke test drives the body-bytes-bound case and asserts the
  typed `Full` names `gateway.body_bytes`.

#### Pre-merge hardening (round 2)

- **Gate-tagged permits.** Every `ConcurrencyLimit` / `KeyedLimit` gets
  a process-unique gate id; permits carry it. Releasing a permit on a
  *different* limit instance returns `WrongGate` (with the permit handed
  back) instead of silently decrementing the wrong slot. Closes the
  cross-instance soundness gap (`ConcurrencyLimit` now issues a
  `ConcurrencyPermit` wrapper for this; `KeyedReleaseError` is generic
  over `K`).
- **Shared-scope composition.** `ConcurrencyLimit::with_shared_scope`
  charges a `SharedCapacityScope` alongside the local gate (two-phase,
  with rollback on shared-budget full). The `ConcurrencyPermit` owns the
  `SharedLease`; releasing or dropping it releases both. The capacity
  surface is decorated with the scope columns.
- **`PressureAction` on all three policies.** `KeyedLimit` and
  `RateLimit` now honor shed/degrade/close/wait on their hard-full path
  (`RateLimit`'s per-key rate decision still always returns
  `RateLimited { retry_after }`, which is more useful than a generic
  degrade).
- **Report polish.** `AdmissionReport` gains an `evicted_count` field
  (telemetry, *not* counted as a rejection), `Display` for
  `AdmissionReport` and `AdmissionFailure` (grep-friendly lines;
  `AdmissionFailure: std::error::Error`), and `surface` is now
  `Cow<'static, str>` so per-route/per-tenant names built at runtime
  work without leaking.
- **Tests.** Cross-instance release rejection (both limits), shared-scope
  composition + permit-drop release, `PressureAction` on keyed/rate,
  `close()` with live state for both keyed and rate, mode round-trip,
  evict-during-grant, and high-N churn/stress correctness for the
  `live_keys` field. Lib admission tests: 36 (was 20); integration
  proofs: 10 (was 8).

#### Pre-merge fixes (round 1)

- `KeyedLimit` and `RateLimit` track `live_keys` as an `O(1)` field
  instead of scanning all slots on every `report()`.
- `KeyedLimit::try_admit` and `RateLimit::try_admit` take `&K` instead
  of `K`. The hot path (existing key) is allocation-free even for
  `K = String`; the key is only cloned on the new-slot allocation path.
- `RateLimit::forget_key` renamed to `evict_key_for_capacity` with an
  explicit doc that it is a policy-owned lever, not a request-path
  helper, plus an `evicted_count` telemetry counter.
- Fixed a double-count: `ConcurrencyLimit.report().full_count` now
  counts only decisions that surfaced as `Full(_)`; the underlying gate
  view is exposed separately as `gate_full_count()`.
- Fixed `PressureAction::Close` to be sticky for concurrency, keyed, and
  rate-table pressure. Once a pressure decision returns `Closed`, the
  policy stops admitting future work until it is rebuilt.

#### What landed originally

- `tina_runtime::admission` ships three policy types over the existing
  capacity primitives. `ConcurrencyLimit` wraps `LocalPermitGate` with a
  typed `AdmissionDecision`; `KeyedLimit<K>` bounds per-key concurrency
  with fixed-capacity slot storage and a move-only `KeyedPermit<K>`;
  `RateLimit<K>` is a per-key token bucket whose decisions are pure
  functions of `(rate, burst, now, key history)`. None of them invent a
  second capacity product — every report projects onto
  `CapacitySurfaceReport` so existing discovery/`CapacitySummary`
  assertions keep working.
- The shared decision shape (`AdmissionDecision::{Admitted, Full,
  RateLimited, Wait, Degrade, Closed, TimedOut}`) makes every overload
  path typed. `PressureAction` lets a `ConcurrencyLimit` declare what
  happens on full — shed, degrade, close, or hint a bounded wait. The
  policies never retry on their own; pair with the existing
  `FullHandling` if retry-with-backoff is the right answer.
- Rate-limit math is integer-only (nano-tokens); identical
  `(config, now, key history)` inputs produce identical decision
  sequences across runs. Per-key storage is a fixed-capacity `Vec<Option<...>>`;
  the first form does not silently evict a key to make room for a new
  one. The `examples/systems/system_tenant_rate_limiter` specimen drives
  the cold-tenant-progresses-while-hot-tenant-is-limited proof and
  asserts byte-identical `retry_after` across two runs.
- Compile-fail proofs cover `KeyedPermit` move-only release (no double
  release) and the private-field invariant that user code cannot forge a
  permit via a struct literal.

### Phase 117 Local I/O, Codec, And IPC Parity

Wave A local I/O / codec / IPC parity
(`.intent/phases/117-local-io-codec-ipc-parity`). Supported targets are
Linux and macOS; Windows waits on a `betelgeuse` Windows backend.

**Core**

- `tina_runtime::file_loops` adds `FileReadChunks`, `FileWriteAll`, and
  `FileCopyBounded` — bounded state-machine helpers over the existing
  `file_read_at` / `file_write_at` rails. Per-step trace truth is
  preserved (no hidden read-whole-file path), the helpers refuse a
  zero per-call budget at construction, and terminal reports name
  whether the loop ended by `Done`, `Eof`, `CapReached`, `Error`, or
  `StuckWrite` along with bytes transferred, final offset, and the
  total requested (stable across partial-write drains). `FileCopyBounded`
  dispatches read/write legs via `next_leg`; calling the wrong leg is a
  caught programmer error.
- Unix-domain socket rails added to `tina-runtime` and `tina-sim`:
  `unix_bind` / `unix_accept` / `unix_connect` / `unix_read` /
  `unix_write` / `unix_close_listener` / `unix_close_stream`. New
  distinct `UnixListenerId` / `UnixStreamId` resource types, new
  `CallKind` variants `UnixBind`…`UnixStreamClose` appended to the
  stable trace-hash mapping (tags 42..=48; existing tags preserved and
  pinned by a golden test).
- **Live OS-backed Unix lane** (`tina-runtime/src/driver/unix.rs`): on
  Unix platforms a single worker thread owns every `UnixListener` /
  `UnixStream` as a non-blocking socket and drives a bounded poll loop;
  the runtime assigns ids and enforces accept/read/write lane
  discipline (`ResourceBusy`) and `InvalidResource` synchronously.
  Closing a listener removes its socket file and refuses parked
  connects; closing a stream cancels its pending ops and wakes the
  peer with EOF. On non-Unix platforms the lane reports typed
  `CallError::Unsupported`, and the runtime capability table names
  `unix` accordingly — no cfg-silent omission.
- Simulator implements the full Unix-domain byte-stream pair model with
  symmetric accept/connect parking: a connect to a bound-but-not-yet-
  accepting listener parks and resolves against the arriving accept (and
  vice versa); a connect to an unbound path is typed `NotFound`. Reads
  block until the peer writes; writes append to peer inbound under a
  configurable cap (kernel-style short writes); stream close wakes the
  peer with EOF; listener close refuses parked connects with a typed
  error.

**Codec battery**

- New `tina-codec` crate with `LineFramer`, `LengthDelimitedFramer`, a
  sealed `Framer` trait, and `FrameDecision::{NeedMore, Frame,
  Malformed, Full}`. Both framers are pure sync state machines living
  on the caller's isolate. Bounded buffers: lines and frames over the
  configured cap are rejected before allocation (`Full`); embedded NUL
  bytes in the line framer are typed as `Malformed` when opted in; the
  length-delimited framer rejects oversized declared bodies before any
  body byte enters the buffer. `LengthPrefix::decode_length` peeks an
  announced length without owning a parser. The crate is pure-`std` and
  portable. Compile-fail fixtures pin that `Framer` stays sealed and the
  decoded frame stays typed bytes, not stringly.

**Specimen**

- `examples/specimen_local_io_codec_ipc` ships flows in one binary:
  file-ingest and bounded file-copy (via `FileReadChunks` /
  `FileCopyBounded`), admin-socket (line-framed local IPC over the
  simulator Unix pair, with bounded write back-off), framed-keyspace
  (length-prefixed mini-keyspace), and a live-unix smoke that drives the
  real runtime (binding a true socket on Unix). Each flow has a smoke
  command and a bad-input proof (`CapReached`, oversized line, oversized
  declared frame).

**Tests**

- Live `LocalSystem` Unix echo round-trip; focused simulator tests for
  connect/accept parking, wrong-resource typed errors, peer-close-while-
  read EOF settling, and listener-close refusing a parked connect; a
  golden `CallKind` tag-stability test; codec compile-fail fixtures.
### DST Replay Honesty for the Native HTTP/2 Client

The native HTTP/2 client does real outbound socket I/O
(`tcp_connect`/`read`/`write`/`close`). The deterministic simulator's
replay op-alphabet models *app* operations, not a remote peer's live
socket completions, so a captured live client run cannot be silently
re-driven from the op history. Rather than a silent no-op or a fake
replay, a live capture carries a typed
`tina_sim::dst::UnsupportedLiveFact` naming the client socket work, and
`check_captured_replay` fails closed on it.

- **`dst_http2_client.rs`** runs the native client live (h2c connect +
  one GET), captures the run as a `LiveReplayCapture` whose
  `unsupported_facts` name the client socket I/O, and asserts
  `check_captured_replay` reports a `CapturedReplayChange::UnsupportedFact`
  — proving the simulator does not fake the replay. The saved replay case
  round-trips through `write_saved_replay_case` /
  `read_saved_replay_case`, preserving the unsupported fact and the
  explicit op history.

### Streaming gRPC Client

`GrpcClient` now covers server-streaming, client-streaming, and bidi on
top of the HTTP/2 client's streaming bodies — Tina is a native streaming
gRPC client, not only a server. The gRPC status stays first-class on
every shape.

- **`GrpcClient::server_streaming_request(path, &req)`** builds an
  `OpenStream` (one buffered request message, a pulled response). Feed
  each pulled `Http2ResponseChunk` to a `GrpcStreamDecoder` and fold it
  with `decode_stream_chunk` into `GrpcStreamItem`s — `Message(..)` for
  each decoded response message, then exactly one terminal `Status` /
  `Transport` / `Malformed`.
- **`GrpcClient::client_streaming_request(path, source)`** builds a
  `SubmitStreaming` (a streamed request body of gRPC-framed messages, one
  buffered response). The response decodes with the existing
  `decode_unary`. `GrpcClient::frame(&msg)` length-prefixes a message for
  the request `source` (e.g. an `IterBodySource`).
- **`GrpcClient::bidi_request(path, source)`** builds an `OpenStream`
  with a streamed request body and a pulled response, so the two
  directions progress independently.
- **`GrpcStreamDecoder`** reassembles length-prefixed gRPC messages
  across response DATA chunks — a chunk may carry several messages, one,
  or a fragment that spans chunks. It rejects compression and over-cap
  lengths before allocating, and `finish()` flags a truncated trailing
  frame. **`GrpcStreamItem<Resp>`** is the typed fold result;
  `stream_head_status(headers)` reads a trailers-only status from the
  response head.
- **Live proofs** (`grpc_client_live.rs`, dialing an in-tree
  `GrpcRouter`): server-streaming receives all messages then a status;
  client-streaming sends several messages and gets the summed reply +
  status; bidi echoes each streamed request back as a streamed response.
  A compile-fail proof pins that a `GrpcStreamItem` cannot be coerced to
  the response message (the status arm cannot be silently dropped).

### HTTP/2 Client Streaming Response Bodies

The HTTP/2 client can now deliver a response incrementally instead of
buffering the whole body. This is the response half of streaming bodies
and the remaining blocker for server-streaming / bidi gRPC.

- **`Http2ClientMsg::OpenStream(Http2ClientStreamCall)`** opens a stream
  whose response is pulled. The request body is buffered or streamed
  (`Http2ClientRequestBody::{Buffered, Stream}`), so one call shape
  serves both server-streaming (buffered request) and bidi (streamed
  request). The first reply is an
  `Http2ClientOutcome::ResponseStreaming { status, headers }` head — or a
  terminal error outcome if the stream never opened.
- **`Http2ClientMsg::ResponseNext { stream_id }`** pulls the body, one
  `Http2ResponseChunk` per call: `Data(bytes)`, then `End { trailers }`
  on clean END_STREAM, or a terminal `Reset(reason)` / `Closed` /
  `ProtocolError`. One pull outstanding per stream; a second concurrent
  pull is rejected.
- **Credit-on-consume backpressure.** Received DATA is held under the
  per-stream flow-control window and only `WINDOW_UPDATE`-credited as the
  caller consumes each chunk. A slow consumer therefore closes the stream
  window and backpressures the peer — there is no unbounded buffer. The
  shared connection window is credited as DATA is received (batched), not
  held until consume, so one slow stream does not stall the others;
  per-stream backpressure is the only lever.
- **Terminal truth reaches the live channel.** A reset / GOAWAY /
  connection-close settles whichever caller channel is live: the
  `OpenStream` waiter (if the head was never delivered) gets the terminal
  `Outcome`; a parked `ResponseNext` pull gets the terminal
  `ResponseChunk`. The `GrpcFinalStatusReceived` and `Http2StreamClosed`
  facts fire on clean streamed completion exactly as on the buffered
  path.
- **Live proofs**: `OpenStream` GET delivers a head then a pulled body to
  `End` (`http2_client_live.rs`); a 32 KB echo response reassembles
  byte-for-byte across multiple pulled DATA frames; and a peer
  RST_STREAM mid-stream delivers a terminal `Reset` chunk to the parked
  pull (`http2_client_adversarial.rs`).

### HTTP/2 Client Streaming Request Bodies

The HTTP/2 client can now stream a request body from a chunk source
instead of buffering the whole `Vec<u8>` up front. This is the request
half of streaming bodies and the first half of the blocker for streaming
gRPC.

- **`Http2ClientStreamingRequest`** carries `method`, `path`, `headers`,
  and a `source: Address<ResponseChunkMsg, ResponseChunkReply>` — the
  same pull protocol the server uses, so [`IterBodySource`] is a request
  source unchanged. Submit it with the call-only
  `Http2ClientMsg::SubmitStreaming(..)`; it replies with one
  `Http2ClientOutcome` like a buffered submit.
- **Pull with backpressure.** The client sends HEADERS without
  END_STREAM, then pulls one chunk at a time (`ResponseChunkMsg::Next`)
  — never more than one pull in flight per stream, and only when the
  stream's outbound buffer has drained. Body bytes ride out as DATA under
  the existing stream + connection flow-control pacer, so a streamed
  upload backpressures against a slow peer the same way a buffered one
  does. The source ends the body with `Eof` (or `GrpcStatus`, treated as
  end-of-body for a request); the final/empty DATA carries END_STREAM.
  An empty DATA(END_STREAM) carries no payload, so a completed stream
  closes its request half even when the connection send window is
  exhausted.
- **Failure is local.** If the source call fails (`Full`/`Closed`/
  `Timeout`/`Rejected`), the client RST_STREAM(CANCEL)s that stream and
  reports `LocalCancel` — it does not poison other streams. Pre-connect
  streaming submits queue like buffered ones and drain with the typed
  connect-failure outcome.
- **Live proofs** (`http2_client_live.rs`): a multi-chunk streamed POST
  to `/echo` round-trips byte-for-byte (server reassembles the DATA
  frames), and an empty source still closes the request half with a lone
  empty DATA(END_STREAM).

### Native HTTP/2 Client over TLS (h2/TLS)

Wires the HTTP/2 client to the TLS rail, turning the `TlsAlpnMismatch`
placeholder into real h2/TLS. `Http2Target::Tls` now actually connects.

- **Dual-rail connection.** A `ClientStream` enum {`Tcp`, `Tls`} backs
  every IO site. `Http2Target::H2c` uses `tcp_connect` + `tcp_*`;
  `Http2Target::Tls` uses `tls_connect_alpn` (offering the target's
  `AlpnProtocols`) + `tls_read`/`tls_write`/`tls_close`. The HTTP/2
  framing code above is rail-agnostic.
- **Real ALPN negotiation.** A TLS connect that offers `h2` and gets
  `h2` proceeds; the client also defends against a non-`h2` selection.
  An offered-but-unnegotiated ALPN fails the connect with
  `CallError::TlsAlpnMismatch`, surfaced as
  `Http2ClientOutcome::TlsAlpnMismatch`. The `noop()` placeholder and
  the `handle_submit` TLS short-circuit are gone; TLS submits queue
  pre-connect like h2c.
- **Half-duplex IO on TLS.** The runtime TLS lane is single-lane per
  stream (read and write share one lane, one blocking worker), unlike
  TCP's split read/write lanes. A new `pump_io` runs the connection
  full-duplex on TCP (read always armed) and half-duplex on TLS (drain
  writes, then arm a read) so the two directions never collide with a
  `ResourceBusy`. This is correct for request/response (unary); a
  concurrent full-duplex h2/TLS *stream* would need a non-blocking TLS
  reactor in the runtime (split TLS lanes + sans-IO rustls in the poll
  loop) — a future runtime-maturity phase, out of Phase 116 scope.
- **`Http2ClientLimits::tls_io_timeout`** (default 30s) bounds the TLS
  connect/handshake and per-call TLS read/write/close.
- **Live proofs** (`http2_client_tls_live.rs`, hand-rolled rustls +
  HTTP/2 server peer): h2/TLS GET round-trips with `h2` selected; a
  server without `h2` ALPN yields `TlsAlpnMismatch`; an untrusted cert
  fails with a typed non-`Replied` outcome and never panics. The h2c
  suite is unchanged and still green.

### TLS ALPN Rail (runtime + simulator)

Promotes ALPN to a real core hook so "selected protocol is typed runtime
truth" (the plan's Hostile Review Note), not a battery-only placeholder.

- **`tina-runtime` TLS rail carries ALPN.** `CallInput::TlsConnect` and
  `TlsBind` gain `alpn_protocols: Vec<Vec<u8>>` (raw wire bytes, empty =
  no ALPN). `CallOutput::TlsConnected` and `TlsAccepted` gain
  `selected_alpn: Option<Vec<u8>>`. New `CallError::TlsAlpnMismatch`
  (trace tag 28, appended) fires when ALPN was offered but the peer
  negotiated none — distinct from cert/name/handshake failures.
- **Existing helpers unchanged; ALPN-aware variants added.**
  `tls_connect` / `tls_bind` / `tls_accept` keep their signatures (offer
  no ALPN, ignore selected), so the HTTP/1 TLS client and HTTPS listener
  are untouched (no HTTP/1 rewrite). New `tls_connect_alpn`,
  `tls_bind_alpn`, and `tls_accept_alpn` offer ALPN and report the
  negotiated protocol; `into_tls_connected_alpn` / `into_tls_accepted_alpn`
  extract `(stream, selected)`.
- **rustls wiring.** The TLS worker sets `config.alpn_protocols` on
  connect and bind, reads `conn.alpn_protocol()` after the handshake,
  and fails a connect with `TlsAlpnMismatch` when ALPN was offered but
  none negotiated.
- **Simulator mirror.** `handle_tls_connect` threads the offered ALPN
  and reports `selected_alpn` deterministically (the scripted server
  accepts the client's top preference — a pure function of the offered
  list, so saved cases do not replay under an ambient default).
  Server-side ALPN negotiation and scripted ALPN-mismatch are noted as
  future sim work.
- **Runtime proofs** (`driver` tests): a real rustls handshake with
  `h2` offered selects `h2`; a server advertising no ALPN with `h2`
  offered yields `CallError::TlsAlpnMismatch`; no ALPN offered
  negotiates `None` without a mismatch.

Next: wire the HTTP/2 client to use the TLS rail for `Http2Target::Tls`
(turning the client's `TlsAlpnMismatch` placeholder into real h2/TLS).

### Native gRPC Client — Hostile-Review Fixes

A hostile pass on the unary client found three issues, now fixed:

- **Non-200 HTTP status was mislabeled `Malformed(BadFrame)`**, discarding
  the gRPC HTTP-status mapping. `decode_unary` now treats an explicit
  `grpc-status` as authoritative (regardless of HTTP status), and a
  non-200 response *without* a `grpc-status` is synthesized into a typed
  gRPC status per `grpc/doc/http-grpc-status-mapping.md` (404 →
  `Unimplemented`, 401 → `Unauthenticated`, 403 → `PermissionDenied`,
  429/502/503/504 → `Unavailable`, else `Unknown`). A 200 with no
  `grpc-status` stays `Malformed(MissingTrailers)`. Six unit tests pin
  the branches plus the mapping table.
- **`GrpcTarget` was exported but never consumed.** It is now
  load-bearing: `GrpcTarget::http2_connection::<S>()` builds the
  connection isolate and `GrpcTarget::limits()` feeds `GrpcClient::new`.
  The live test and the next caller construct through it.
- **`unary_request` did not validate the method path.** It now
  `debug_assert!`s an absolute (`/`-prefixed) path, catching a relative
  path that would produce an invalid `:path` pseudo-header.

New live test `unknown_method_returns_unimplemented_status` proves an
unrouted method surfaces as `Status(Unimplemented)` (the server answers
trailers-only).

### Native gRPC Client — Unary First Form

The plan's second-half keystone: Tina is now a native gRPC *client*, not
only a server. `GrpcClient` is a thin, stateless wrapper over an
`Http2ClientConnection` isolate — no Tokio, no hidden queue or runtime.

- **`GrpcClient` + `GrpcTarget` + `GrpcUnaryOutcome`.** The unary path
  encodes one `prost` message, submits it as one HTTP/2 stream, and
  decodes the reply into a typed outcome where the gRPC status is
  first-class:
  - `Ok(Resp)` — OK status *and* a decoded response message.
  - `Status(GrpcStatus)` — a non-OK gRPC status. A normal caller
    outcome, never collapsed into a successful response.
  - `Transport(Http2ClientOutcome)` — HTTP/2 transport failure before a
    status was seen (closed, reset, protocol error, ALPN mismatch).
  - `Malformed(GrpcError)` — reached the gRPC layer but not well-formed
    (non-200 HTTP status, missing `grpc-status`, undecodable/oversized
    message). `#[non_exhaustive]`.
  Request size is capped on encode (`EncodeTooLarge`) before anything
  reaches the wire; the response message is capped on decode.
- **`ProtocolFact::GrpcFinalStatusReceived`.** Paired with the server's
  `GrpcFinalStatusSent` (trace tag 9, appended). The HTTP/2 client
  connection emits it when a stream completes carrying a `grpc-status`
  (in trailers, or headers for a trailers-only response) — gRPC status
  is runtime truth, not a private counter.
- **Live proofs** (`tina-http/tests/grpc_client_live.rs`): unary OK
  returns the decoded message; a non-OK status is `Status(NotFound)`,
  not a success; the received status is emitted as a
  `GrpcFinalStatusReceived` fact; an oversized request is rejected
  before the wire.
- **Compile-fail proofs** (doc-tests on `GrpcClient`): the unary helper
  cannot accept a stream of messages (only a single `prost::Message`);
  a `GrpcUnaryOutcome` cannot be treated as the response message,
  forcing the caller to handle the status arm.
- **Specimen updated.** `examples/specimen_grpc_counter` now drives its
  own server with the native `GrpcClient` (the copied path), exercising
  an OK call, a non-OK `PermissionDenied` status, and a client
  cancellation — and proving the connection survives the cancel.
  `run_smoke()` no longer uses `grpc_unary_call_h2c_blocking`.
- **Docs updated.** The HTTP/gRPC user guide and the `tina-http` crate
  doc now describe the native client path and demote
  `grpc_unary_call_h2c_blocking` to a test-only convenience. The "native
  gRPC is server-first" framing is gone.

Deferred (named honestly, dependency-ordered):

- **Streaming gRPC** (server-streaming / client-streaming / bidi)
  depends on HTTP/2 client *streaming bodies*, which the client does not
  have yet (it buffers). That is the next slice.
- **h2/TLS gRPC** depends on the TLS ALPN runtime rail (also a later
  slice); a TLS target resolves to `TlsAlpnMismatch` today.
- **Tina-client → tonic-server interop** would need tonic as a
  `tina-http` dependency (it is only a specimen dev-dependency today);
  deferred rather than pulling tonic into the battery. Tina-client ↔
  Tina-server gRPC is proven, and the existing tonic-client ↔
  Tina-server test still passes after the shared-code split.

### Native HTTP/2 Client — Plan Audit Follow-ups

A re-read of the Phase 116 plan against the shipped code surfaced three
proof items that were required but unproven, plus two intentional
design divergences worth recording.

- **Protocol-fact emission is now proven** (the plan's "protocol facts
  emitted for stream lifecycle" item). The client emitted
  `Http2StreamOpened` / `Http2StreamClosed` / `Http2StreamReset` facts,
  but no test asserted it. New live tests capture facts from the
  runtime trace (`complete_trace` →
  `RuntimeEventKind::FactObserved`):
  - `client_emits_outbound_open_and_close_lifecycle_facts` — a
    happy-path GET emits an outbound `Http2StreamOpened` and a matching
    `Http2StreamClosed`.
  - `client_emits_inbound_reset_fact_on_peer_rst` — a peer RST_STREAM
    emits an inbound `Http2StreamReset(RefusedStream)`.
- **GOAWAY "lets admitted streams settle" half is now proven.** The
  earlier test only covered the refusal half (`last_stream_id = 0`). New
  `goaway_above_stream_id_lets_admitted_stream_settle_then_blocks_new_admission`:
  a GOAWAY whose `last_stream_id` covers the in-flight stream lets that
  stream complete normally, while a *subsequent* submit is refused
  `Closed` — proving both the settle path and the post-GOAWAY admission
  gate.

Two intentional divergences from the plan's literal sketch, recorded so
they are decisions and not oversights:

- **No `Admitted` outcome variant.** The plan's outcome list included
  `Admitted` alongside `Full` / `Closed` / etc., implying a two-phase
  admit-then-await API. This slice uses a single-reply model instead:
  one `Submit` call yields exactly one terminal `Http2ClientOutcome`
  when the stream finishes. This is Tina-idiomatic (one reply per call)
  and still supports concurrency — a calling isolate issues `Submit` as
  a non-blocking call effect and gets the outcome later, so many
  requests can be in flight at once (proven by
  `concurrent_streams_do_not_cross_replies`). The two-phase
  `Admitted { stream_id }` shape is not needed for that and is not
  implemented.
- **Response/request bodies are buffered, not chunk-delivered.** The
  plan's "response DATA arrives in bounded chunks" / "request DATA
  streaming is bounded" proof items are met on the *bounded* axis
  (response cap + inbound window credit; outbound flow-control pacing)
  but the caller still sees one buffered `Vec<u8>` per body, not a
  chunk stream. Streaming chunk-source bodies (mirroring the server's
  `IterBodySource`) are deferred with the rest of the streaming work.

### Native HTTP/2 Client — Testing Gaps Closed

Closes the testing gaps a third pass identified — claims that were
true structurally but unproven, plus a stale comment and a few
low-risk hardening items. No behavior change to the client beyond two
dev-only `debug_assert`s; everything here is proof and polish.

- **Concurrency / no-crossing proof.** New
  `concurrent_streams_do_not_cross_replies` submits two requests from
  separate host threads (via `std::thread::scope` sharing `&runtime`).
  The hand-rolled peer replies in reverse order with stream-id-tagged
  bodies; each caller asserts the body matches the stream id in its
  own outcome. This is the first test where two streams are genuinely
  in flight at once — the earlier "multiple streams" test was
  sequential.
- **`Full` admission + peer concurrency cap proof.** New
  `peer_max_concurrent_streams_one_yields_full_for_the_excess_submit`:
  the peer advertises `SETTINGS_MAX_CONCURRENT_STREAMS = 1` and holds
  the first stream; of two concurrent submits, exactly one is admitted
  and `Replied`, the other is rejected `Full`. Proves both the `Full`
  outcome (previously unexercised live) and that the client honors the
  peer's advertised cap.
- **Caller-cancel proof.** New
  `caller_cancel_returns_local_cancel_and_keeps_connection_alive`:
  `Http2ClientMsg::Cancel { stream_id }` on a held stream returns
  `LocalCancel` and a follow-up GET on the same connection still
  succeeds. The `LocalCancel` path and the `Cancel` message were
  wired in round 1 but had no live test.
- **Real end-to-end flow-control proof.** New
  `large_upload_paces_through_real_window_updates`: a 128 KB POST
  against a peer that drains DATA and credits `WINDOW_UPDATE`
  incrementally. Forces the outbound pacer to park on the 65535-byte
  window and resume on credit, asserting `flow_control_parks > 0`.
  (The in-tree server test stays at 32 KB because the server's
  response path parks whole — the documented KNOWN LIMITATION.)
- **Foreign-server interop proof.** New
  `foreign_server_happy_path_get_returns_replied`: the client GETs a
  hand-rolled, non-Tina HTTP/2 server (independent framing code) and
  receives the body unchanged. Proves the client does not depend on
  Tina-server framing quirks.
- **Compile-fail proofs.** Two `compile_fail` doc-tests on
  `Http2Target` pin that an `H2c` target cannot name `server_name` /
  `trust_roots` and a `Tls` target cannot omit them. (The unary-vs-
  streaming and gRPC-status-handling compile-fail proofs the plan also
  names are gated on the gRPC client slice.)
- **Stale test comment removed.** `http2_client_live.rs`'s header
  claimed a `max_concurrent_streams = 1 → Full` test that did not
  exist. The header now accurately lists what each test file covers
  and points at the adversarial file for the rest.

Low-risk hardening:

- `Http2ClientConnection::new` `debug_assert`s `max_concurrent_streams
  >= 1` (a zero cap silently rejects everything).
- `connection_fact_id()` `debug_assert`s `self_isolate_id` is set, so a
  fact emitted before the first handler turn (which would tag it with
  connection id 0 and break replay correlation) is caught in dev/test.
- In-source note on `complete_stream` documenting that a gRPC
  trailers-only response lands `grpc-status` in `headers`, not
  `trailers`, so the future gRPC wrapper checks both.

The adversarial test binary now has 9 cases; the live binary 8; two
new compile-fail doc-tests. Full `tina-http` suite green;
`cargo fmt --check` and `cargo clippy -- -D warnings` clean.

### Native HTTP/2 Client — Spec-Compliance Round 2

Closes the bugs a second hostile pass found in the previously-untested
inbound-misbehavior paths, plus two more the review surfaced. Each fix
is pinned by an adversarial live test against a hand-rolled,
deliberately-misbehaving HTTP/2 peer
(`tina-http/tests/http2_client_adversarial.rs`).

- **RST_STREAM on stream 0 is now a connection-level protocol error.**
  `handle_rst_stream` rejected the illegal frame as a silent no-op
  before (the stream-id-0 lookup just missed). RFC 9113 §6.4 requires
  a connection-level PROTOCOL_ERROR; the client now fails the in-flight
  stream with `Http2ProtocolError::BadStreamId` and tears the
  connection down.
- **GOAWAY payload is parsed and acted on.** The branch was six lines
  that set a flag and ignored `last_stream_id` / error code. The client
  now refuses every stream it opened with id `> last_stream_id` (those
  were not processed by the peer, so the caller can safely retry):
  `Closed` for a clean `GOAWAY(NO_ERROR)`, `Reset(reason)` for an
  error-coded GOAWAY. Previously those streams hung until the socket
  dropped.
- **Removed `Http2ClientOutcome::FlowControlBlocked`.** Same lying-API
  problem as `Timeout`: the variant was advertised but no code path
  constructed it. Both land back when a real stream-level deadline
  mechanism does. The exhaustive `outcome_surface_excludes_unimplemented_variants`
  test now guards the whole outcome surface.
- **`try_send` of a call-only message no longer drops silently.**
  `Http2ClientMsg::Submit` / `::Report` delivered via `try_send` have
  no reply channel; `Submit` would silently drop the request body. The
  connection now `debug_assert!`s on the misuse (catches it in
  dev/test) and the type doc states the call-only contract. Release
  builds still no-op rather than killing the connection over a stray
  send.
- **PING frame errors are now correctly classified.** A PING on a
  non-zero stream id is `BadStreamId` (PROTOCOL_ERROR); a wrong-length
  PING is `BadFrameLength` (FRAME_SIZE_ERROR). They were collapsed into
  `BadFrameLength` before.
- **Server-side KNOWN LIMITATION noted.** `http2/server.rs`'s
  `queue_or_send_response` parks a buffered response whole until the
  full body fits the send window — a response larger than ~64 KB
  deadlocks a strict peer (including the native client) that does not
  pre-credit its receive window. This is a pre-existing server bug, not
  a client regression; it is now documented in-source and queued for a
  future "server response streaming" slice (the mirror of the client's
  `flush_outbound_data` pacer).

New adversarial tests, all passing:

- `server_rst_stream_maps_to_typed_reset`
- `server_rst_stream_on_stream_zero_is_connection_protocol_error`
- `server_goaway_below_stream_id_refuses_unprocessed_stream`
- `malformed_inbound_frame_does_not_panic_and_fails_stream_typed`

### Native HTTP/2 Client — Hostile-Review Fixes

Follow-up to the first-form client below. Addresses the issues a
hostile self-review surfaced before this slice is presented as
production-shaped:

- **Outbound flow control (C1).** `admit_stream` no longer blasts the
  request body straight into the write queue. Body bytes now sit in a
  per-stream `outbound_body` VecDeque and are drained by a new
  `flush_outbound_data` round-robin pacer that respects both the
  stream and connection send-windows. `flush_outbound_data` is
  re-entered from `handle_window_update` (peer credit arrived) and
  from `apply_setting`'s `SETTINGS_INITIAL_WINDOW_SIZE` branch (peer
  resized the initial window). A new `Http2ClientReport.flow_control_parks`
  counter records each window-blocked iteration.
- **Write reentrancy (C2).** The client now tracks `write_in_flight`
  alongside `pending_write`. `write_more` is a no-op while a
  `tcp_write` is awaiting completion, and a new `maybe_write_more`
  helper is the *only* place callers nudge the writer. Without this
  the driver's per-stream `lane_has_pending` check would return
  `CallError::ResourceBusy` for back-to-back writes and the
  connection would die.
- **Route-key trust-root collision (C3).** `Http2Target::route_key`
  now hashes the trust-root byte content instead of just counting
  entries. Two TLS targets with the same authority/server_name/ALPN
  but distinct roots now produce distinct keys, so a future Phase 119
  pool cannot share a connection across security boundaries.
- **DATA-before-HEADERS / DATA on closed stream (C4 + C5).** DATA
  arriving before HEADERS on a stream is now a connection-level
  `Http2ProtocolError::DataBeforeHeaders` GOAWAY (RFC 9113 §8.1).
  DATA on an unknown / closed stream is a stream-level
  RST_STREAM(STREAM_CLOSED), not a connection-level kill (RFC 9113
  §6.9.1). The connection survives the misbehaving stream.
- **Removed unreachable `Http2ClientOutcome::Timeout` (H1).** No path
  in the client today returns a `Timeout` outcome — caller deadlines
  arrive through `CallOutcome::TimedOut` at the host. Removing the
  variant prevents silent advertising of behavior that does not
  exist. A new test pins the present variants so re-adding `Timeout`
  without wiring a real stream-level deadline breaks compilation.
- **Outbound queue cap is now enforced (H3).** `Http2ClientLimits.
  connection_outbound_queue_capacity` was dead config. Admission now
  rejects with `Http2ClientOutcome::Full` and bumps a new
  `outbound_queue_full` counter when the write queue is at the cap.
  A new `pre_connect_submit_capacity` knob bounds the
  before-connect submit queue.
- **Outbound caller-cancel path (L7).** `Http2ClientMsg::Cancel
  { stream_id }` is now wired: it sends RST_STREAM(CANCEL), emits an
  outbound-direction `Http2StreamReset` fact, replies to the
  original submitter with `LocalCancel`, and increments
  `locally_cancelled` in the report.
- **Body-cap error is now correctly labeled (M1).** A response body
  that exceeds `max_response_body_bytes` returns
  `Http2ProtocolError::BodyTooLarge { cap_bytes }`, not
  `HeadersTooLarge`.
- **Outbound HEADERS too large (M2).** A request whose encoded
  HEADERS block does not fit one frame is rejected without consuming
  a stream id and bumps a new `request_too_large` counter (not the
  generic `protocol_errors` — the peer is innocent here). Surfaced
  as `Http2ProtocolError::OutboundHeadersTooLarge`.
- **Stream id exhaustion fails closed (M3).** After client stream id
  space exhausts (2^31 streams), `stream_id_exhausted` is set and
  subsequent admissions return
  `Http2ProtocolError::StreamIdExhausted` instead of silently
  reusing the last id.
- **Trailer-block validation (L3).** A trailer HEADERS that carries
  any pseudo-header now fails with
  `Http2ProtocolError::InvalidTrailerPseudoHeader`; a `content-length`
  trailer fails with `ContentLengthMismatch`. Both are connection-level
  protocol errors per RFC 9113 §8.1.
- **Removed `AlpnProtocols::wire()` (L1).** The
  `#[allow(dead_code)]` helper was a footgun. It lands back when the
  TLS rail actually consumes it.
- `Http2ClientReport`, `Http2ClientReply`, `Http2ClientMsg`, and
  `Http2ProtocolError` are now `#[non_exhaustive]`. `Http2ClientLimits`
  is intentionally exhaustive so callers can construct it with
  struct-update syntax — new fields go through a major-version bump.

New / extended tests:

- `h2c_post_body_is_echoed_back_byte_for_byte` — POST round-trip now
  asserts the exact bytes echoed by `/echo`, not just status 200.
- `h2c_multiple_streams_share_one_client_connection` — three
  sequential GETs on one isolate, asserts
  `opened_streams == closed_streams == 3` and zero protocol errors.
- `response_body_above_cap_returns_typed_body_too_large` — pins the
  new `BodyTooLarge { cap_bytes }` outcome label.
- `h2c_post_large_body_paces_through_flow_control_window` — 32 KB
  POST through the outbound flow-control pacer.
- `tls_target_route_key_distinguishes_distinct_root_sets` — pins the
  trust-root hashing fix from C3.
- `outcome_does_not_include_timeout_variant` — compile-shape proof
  that `Timeout` is gone from the outcome surface until a real
  stream-level deadline lands.

### Honest gaps still standing

These are genuine future work, not unproven behavior. (The
GOAWAY/RST/malformed-frame paths that were listed here are now fixed
and pinned by adversarial live tests — see "Spec-Compliance Round 2"
above.)

- **Stream-level deadlines.** Until they land, the client has no
  per-stream timeout: a caller enforces its own deadline via
  `call_blocking_with_host_timeout` (surfacing as `CallOutcome::TimedOut`
  at the host) and the connection keeps the stream slot until the
  response, a reset, or connection close. The `Timeout` and
  `FlowControlBlocked` outcome variants are deliberately absent until
  this mechanism exists.
- **Server response streaming.** `http2/server.rs` parks a buffered
  response whole until it fits the send window (see the in-source
  KNOWN LIMITATION). A response over ~64 KB deadlocks a strict peer.
  This is a server-side fix — the mirror of the client's
  `flow_control_parks` pacer — and is its own slice. The client's
  outbound flow control is proven by `h2c_post_large_body_paces_through_flow_control_window`.
- **Native gRPC client.** Not in this slice; the HTTP/2 client carries
  response trailers visibly so the gRPC wrapper can read `grpc-status`.

### Native HTTP/2 Client — First Form

Builds on the module split below. Adds the native HTTP/2 client
isolate the plan called for, plus the typed
`Http2Target` / `AlpnProtocols` surface so authority, SNI, trust
roots, and ALPN protocol selection are typed inputs, not
string/byte bags.

- `Http2ClientConnection<S>` is a Tina isolate over one TCP stream.
  It sends the client preface and SETTINGS, opens odd-numbered
  streams, enforces `max_concurrent_streams`, applies peer SETTINGS
  (initial window, max frame size, max concurrent streams,
  `ENABLE_PUSH`), tracks connection/stream flow-control windows
  with `WINDOW_UPDATE` credit flushing, and reports lifecycle
  through `ProtocolFact::Http2StreamOpened` / `Http2StreamClosed` /
  `Http2StreamReset` with `direction: Outbound` for client-initiated
  streams. Each request is one `Http2ClientMsg::Submit` call; the
  connection captures the caller's `RequestContext` and replies
  later with one typed `Http2ClientOutcome`.
- `Http2ClientOutcome` covers every reason a Tina-owned client
  stream can end in: `Replied(Http2ClientResponse)`, `Full`,
  `Closed`, `FlowControlBlocked`, `Timeout`, `Reset(Http2ResetReason)`,
  `LocalCancel`, `ProtocolError(Http2ProtocolError)`, and
  `TlsAlpnMismatch`. The enum is `#[non_exhaustive]` so streaming
  bodies and the live TLS ALPN path land as new arms without
  breaking match sites.
- `Http2Target::H2c { authority, addr }` and
  `Http2Target::Tls { authority, addr, server_name, trust_roots, alpn }`
  are typed shapes. `H2c` cannot carry SNI or trust roots; `Tls`
  must carry both. `Http2Target::route_key()` folds authority,
  TLS/root/ALPN policy into a stable key the pool work in Phase 119
  will read.
- `AlpnProtocols::h2()` / `AlpnProtocols::none()` is the named ALPN
  config; no raw `Vec<Vec<u8>>` ALPN bag. This is the API the
  runtime TLS rail will accept once the ALPN extension lands. Today
  the runtime TLS rail does not yet plumb ALPN bytes, so the client
  surfaces a typed `Http2ClientOutcome::TlsAlpnMismatch` for any
  `Http2Target::Tls` target — no silent h2c fallback, no generic
  IO error. Live HTTPS/2 with `h2` selection is a follow-up that
  lands the ALPN bytes through `CallInput::TlsConnect` and reads
  `selected_alpn` out of `CallOutput::TlsConnected`.
- Live proofs in `tina-http/tests/http2_client_live.rs`:
  - `h2c_get_round_trip_returns_typed_replied_outcome` — Tina HTTP/2
    client GETs the existing Tina HTTP/2 server's Counter service
    and receives `Http2ClientOutcome::Replied` with a 200 status
    and a non-empty body.
  - `h2c_post_body_is_round_tripped_through_data_frame` — POST with
    a buffered body crosses one HEADERS + one DATA frame and the
    server replies with 200.
  - `tls_target_returns_typed_alpn_mismatch_without_touching_tls_rails`
    — `Http2Target::Tls { .. }` resolves to the typed
    `TlsAlpnMismatch` outcome without ever calling `tls_connect`.
  - `tls_target_route_key_distinguishes_from_h2c_route_key` and
    `request_method_and_path_round_trip_through_targets` pin the
    target/request shapes for the future pool layer and gRPC client.
- `tina-http`'s full test suite passes (libs + every integration
  binary); no `cargo fmt` / `cargo clippy` warnings.

### Honest Deferrals After This Checkpoint

These items are named in the Phase 116 plan and are still future
work after this slice. Each is a real gap, not a label.

- TLS ALPN rail: `CallInput::TlsConnect` / `TlsBind` do not yet
  accept `AlpnProtocols`, and `CallOutput::TlsConnected` /
  `TlsAccepted` do not yet carry a `selected_alpn`. The client
  surface already takes the named `AlpnProtocols::h2()` config, but
  the bytes are not plumbed through rustls yet. Live HTTPS/2 with
  `h2` selection unblocks once that lands.
- Native gRPC client (`GrpcClient::unary` / `server_streaming` /
  `client_streaming` / `bidi`): not implemented in this slice. The
  HTTP/2 client now carries response trailers visibly so a later
  `GrpcClient` wrapper can pull `grpc-status` out of
  `Http2ClientResponse::trailers` and emit a paired
  `ProtocolFact::GrpcFinalStatusReceived`. `grpc_unary_call_h2c_blocking`
  remains the test-only blocking helper; production users still
  copy the existing bridge path for now.
- HTTP/2 client streaming request and response bodies. Today's
  client buffers both under explicit caps. The typed outcome enum
  pins where streaming variants land.
- `Http2ClientMsg::Cancel { stream_id }` and a paired
  `cancel_call`-shaped cancellation that emits outbound
  RST_STREAM(CANCEL). The connection already maps a peer RST_STREAM
  into `Http2ClientOutcome::Reset(reason)`.
- DST replay coverage: the simulator does not yet return a typed
  unsupported fact for live HTTP/2 client socket work, and no
  saved replay case exercises the client. The lifecycle facts the
  client emits reuse existing names so future replay plumbing is
  mechanical.
- Connection reuse beyond "one isolate carries many admitted
  streams": idle eviction, max lifetime, and health policy are
  Phase 119, the resource maturity slice the plan names. The
  `Http2Target::route_key()` shape is in place so pool work can
  consume it without re-keying.
- HTTPS/2 client compile-fail proofs. The typed `Http2Target`
  variants already make a roots-less TLS target structurally
  impossible at the type level, but no `compile_fail` doctest pins
  the gates explicitly.

### Native Protocol Client Parity — Server-Only Module Split

Checkpoint 1 of the native HTTP/2 / gRPC client work
(`.intent/phases/116-native-protocol-client-parity`). The single-file
`tina-http/src/http2.rs` is split into a module tree so the upcoming
native HTTP/2 client can share frame/header/error helpers with the
server without copy-paste.

- `tina-http/src/http2/` becomes the module root, with
  `mod.rs` re-exporting the previous public surface
  (`Http2Listener`, `Http2Connection`, `Http2ConnectionMsg`,
  `Http2ConnectionReply`, `Http2ConnectionReport`, `Http2Limits`,
  `Http2ListenerMsg`, `Http2Outcome`, `Http2ProtocolError`,
  `Http2ServerConfig`, `Http2StreamReport`, `Http2StreamState`)
  under their existing paths.
- `http2/frame.rs` owns frame encode/decode, the standard frame
  builders (`settings_frame`, `rst_stream_frame`, `goaway_frame`,
  `window_update_frame`, `headers_frame`, `data_frame`), padded /
  PRIORITY payload extractors, the wire-level constants
  (`CLIENT_PREFACE`, `FLAG_*`, `FRAME_*`, `PRIORITY_PAYLOAD_LEN`,
  `DEFAULT_WINDOW`, `READ_CHUNK`, `WINDOW_CREDIT_FLUSH_THRESHOLD`),
  and `add_window`.
- `http2/headers.rs` owns HPACK encode/decode and pseudo-header
  validation (`HeaderBlock`, `decode_headers_block_with`,
  `encode_response_headers`, `encode_response_trailers`,
  `validate_request_headers`, `SETTINGS_*`,
  `DEFAULT_HEADER_TABLE_SIZE`, `MIN_/MAX_MAX_FRAME_SIZE`).
- `http2/errors.rs` owns `Http2ProtocolError`, the wire error-code
  constants (`ERR_*`), and `classify_h2_reset`.
- `http2/server.rs` keeps the connection/listener isolates plus the
  in-source server tests. It compiles only against the shared
  helpers through `super::frame::*`, `super::headers::*`,
  `super::errors::*`; no duplicate definitions remain.
- The frame/header/error modules are internal (`pub(super)` items,
  not re-exported from `mod.rs`). The public HTTP/2 surface is
  unchanged — no new client types, no ALPN edits, no behavior
  change. The existing HTTP/2 server tests pass after the move:
  `cargo test -p tina-http --lib` (139 cases),
  `cargo test -p tina-http --test http2_live` (32 cases), and
  `cargo test -p tina-http --test grpc_live` (34 cases). The only
  call-site change is `try_decode_frame`, which now takes
  `max_frame_size: usize` instead of `&Http2Limits` so the frame
  module does not depend on the server config struct.

Honest deferrals — remaining slices of phase 116 (named in
`.intent/phases/116-native-protocol-client-parity/plan.md`):

- Native HTTP/2 client connection isolate (`Http2ClientConnection`)
  with bounded stream-slot admission and typed admit/reset/timeout
  outcomes; pooled-by-authority reuse.
- Native gRPC client (unary, server-streaming, client-streaming,
  bidi) over the client connection, with received `GrpcStatus` as a
  protocol fact.
- Typed `AlpnProtocols::h2()` / `none()` on the TLS rail and
  selected-ALPN truth in TLS connect/accept output. The current
  TLS rail has no ALPN; the deferral is named, not hidden.
- Specimen updates that replace `grpc_unary_call_h2c_blocking` with
  the copied client-isolate path.

### HTTP/2 And Multi-Shard Fairness Hardening (second pass)

- HTTP/2 request `content-length` is now truthful for buffered,
  streaming, and gRPC paths. The header is parsed once during HPACK
  decode; invalid decimal values, empty values, equal duplicates, and
  conflicting duplicates are rejected. Inbound DATA that overruns or
  underruns the declared length resets the stream with
  `RST_STREAM(PROTOCOL_ERROR)` before extra bytes reach service code.
  `END_STREAM` on `HEADERS` with a non-zero declared length is
  rejected before dispatch.
- HTTP/2 known-length streaming responses (`HttpResponseBody::Stream`)
  track remaining `content-length` per stream. A source that over-produces
  resets visibly before the extra byte is queued for outbound; a source
  that EOFs early resets rather than sending `END_STREAM` with a short
  body. Chunked unknown-length responses are unaffected.
- HTTP/2 duplicate pseudo-headers (`:method`, `:path`, `:scheme`,
  `:authority`, `:status`) reject with `InvalidPseudoHeaders` before
  assignment instead of silently overwriting the prior value.
- HTTP/2 `CONTINUATION` is now named (`FRAME_CONTINUATION = 0x9`) and
  any occurrence is rejected as `UnexpectedContinuation`. `PRIORITY`
  validates stream id (must be nonzero) and payload length (must be 5)
  before accepting. Unknown extension frames still follow the
  ignore-unknown rule, so core strictness does not turn unknowns fatal.
- `ThreadedMultiShardRuntime` worker loop services local commands after
  every bounded remote-inbound drain pass, not only when the drain
  delivered zero envelopes. `Run` and `Shutdown` commands no longer wait
  behind a sustained remote inbound flood. Ordinary cross-shard throughput
  is unchanged.

### Phase 115 Core / Ecosystem Reorg

Architecture cleanup before Wave A: docs draw the core-vs-batteries
line; oversized core files split along real module boundaries.

Docs and layering:

- New `docs/tina-user-guide/23-core-and-batteries.md` draws the line between
  Tina core (model crates, runtime/simulator) and official batteries
  (`tina-http`, bridge crates, proof harness). Lists the six "official
  battery rules" — bounded admission, typed outcomes, close/drain report,
  pressure/capacity report, replay support or honest unsupported truth, no
  hidden Tokio/runtime queues — and names the three prelude tiers
  (`tina::prelude`, `tina_runtime::prelude`, battery preludes).
- New `docs/tina-user-guide/24-battery-authoring.md` gives a twelve-item
  authoring checklist for first- and third-party batteries plus a "known
  hook gaps" table that names where existing first-party batteries still
  reach past clean public hooks (HTTP/TLS rails, bridge lifecycle, body
  streaming/source lifecycle, AWS/sqlx/reqwest/Tokio-owned worker copy,
  per-battery replay declarations).
- Updated `docs/README.md` and `docs/tina-user-guide/README.md` so the
  "learn core" and "choose batteries" reading orders are now distinct.
- Added a "Layering" stanza to the Phase 116, 117, and 118 plan outlines so
  Wave A work uses the new core-vs-batteries language and does not invent
  private runtime hooks inside battery code.

No-behavior module splits:

- `tina/src/lib.rs` (3287 → 454 lines, target <1,200 ✅) split into
  `mod address` (address/id/generation types, `Outbound`,
  service-shaped addresses), `mod context` (`Context`, `CallContext`,
  `RequestCall`, `RequestContext`, defer-through traits,
  deferred-reply slots, `CallHandle`/cancellation/`Deadline`,
  `MessageCaller`, `CallRouting`, `DeferredSlotRegistry`),
  `mod effect` (closed `Effect` enum and every constructor: `noop`,
  `fact`, `reply`, `reject`, `send`, `send_to`/`send_event`,
  `spawn`/`spawn_observed`, `stop`/`stop_with`, `restart_children`,
  `batch`/`sequence`, `reply_to`/`reply_to_request`), and
  `mod isolate` (`Isolate`/`CallableIsolate`,
  `Mailbox`/`TrySendError`, `RestartPolicy`/`ChildRelation`/
  `RestartDecision`/`RestartBudget` family, `Shard`/`SingleShard`,
  `ChildDefinition`/`ChildRef`, `SpawnObserved` family,
  `RestartableChildDefinition`).
- `tina-sim/src/dst.rs` (4261 → 1180 lines in `dst/mod.rs`,
  target <1,200 ✅) split into six submodules: `discovery`
  (`DiscoveredConstants`, `discover_constants`), `invariants`
  (`InvariantViolation`/`InvariantSuite` and the per-invariant check
  functions, `contains_visible_pressure`, `assert_projection_eq`),
  `projection` (`TraceShape`, `RuntimeEventKindName`,
  `TraceProjection`, `TraceProjectionError`, `ProtocolReplayMismatch`,
  `project_trace_shape`, `replay_config_hash`/`encode_*` family),
  `replay_case` (`UnsupportedLiveFact`, `LiveReplayFact`,
  `CapacityReplayFact`, `LiveReplayCapture`, `SavedReplayCase` and
  on-disk format, `CapturedReplayChange`, `LiveReplayReport`,
  `CapturedReplayMismatch`, `ReplayMismatch`,
  `check_replay_case`/`check_captured_replay`/`observe_replay_case`),
  `shrink` (`ShrinkConfig`, `ShrunkFailure`, `delete_shrink`,
  `ShrinkReport`, `shrink_replay_case`), and `sweep` (`SweepFailure`,
  `SweepSuccess`, `sweep_seeds`).
- `tina-runtime/src/call.rs` (4656 → 2562 lines in `call/mod.rs`,
  target <1,200 partial) converted to `call/mod.rs` with per-rail
  submodules: `tcp` (`tcp_bind`/`accept`/`connect`/`read`/`write`/
  close), `udp`, `tls`, `dns`, `signals`, `process`, `files`
  (file + filesystem-path), `persistence` (snapshot/journal), plus
  shared `types` (newtype IDs, file/process/path/persistence data
  shapes), `time` (`sleep`, `sleep_then`, `SleepCall` plus all its
  defer-through impls), `pending` (`CancelableCall`,
  `PendingCancelableCall`/`Ticket`/`Set` family), `cancel`
  (`CancelCallBuilder`, `cancel_call`, `call_cancelable`,
  `call_with_handle`, `call_handle_call_id`), and `groups`
  (`WorkTicket`, `CancelableWork`, `CancelableWorkSnapshot`,
  `AdmitWorkError`). The `call/mod.rs` core still holds the
  closely-coupled `CallInput`/`CallOutput`/`CallError` enums and the
  `RuntimeCall`/`TypedCall` family.
- `tina-runtime/src/lib.rs` (5231 → 641 lines ✅) — extracted four
  submodules from the giant `impl<S, F> Runtime<S, F>` block: `mod
  host_call` (`try_send`/`try_send_event`, `observe_*`,
  `set_trace_retention`/`set_trace_observer`, `has_in_flight_calls`,
  `trace`, `pressure_summary`, plus the test-only lineage / child /
  supervisor snapshots), `mod remote` (cross-shard transport types
  `QueuedRemoteEnvelope` / `QueuedRemoteSend` / `RemoteCallReply`
  family plus `dispatch_local_send_with_context` /
  `harvest_remote_*` / `complete_remote_isolate_call`), `mod
  registration` (every `register_*` / `supervise` / `try_supervise`
  API plus the registered-address bookkeeping and the
  `spawn_isolate` / `record_child` /
  `enqueue_bootstrap_message` / `enqueue_entry_message` /
  `recv_entry_message` family), and `mod dispatch` (the biggest
  bin: `step`, `step_with_remote`, `execute_effect` and friends,
  `dispatch_call` / `dispatch_driver_call` /
  `dispatch_observed_send` / `dispatch_isolate_call` /
  `dispatch_cancel_call`, `harvest_isolate_call_timeouts`,
  `record_cancelled_call` / `recently_cancelled_cause` /
  `close_deferred_slot_for_call_with_reason` /
  `complete_isolate_call` / `deliver_isolate_call_outcome`,
  `advance_driver` / `deliver_completion`, the `stop_entry*` and
  restart family, `push_event` / `enforce_trace_retention` /
  `compact_trace_prefix*`, plus every `Erased*` adapter type
  (`ErasedMailbox` / `MailboxAdapter` / `AnyMailboxAdapter`,
  `ErasedHandler` / `HandlerAdapter` / `SendableHandlerAdapter`,
  `ErasedSpawn` / `SpawnAdapter` / `RestartableSpawnAdapter`,
  `ErasedSpawnObserved` / `SpawnObservedAdapter`, `ErasedEffect`,
  `ErasedSend`, `ErasedMessage`, `RegisteredEntry`,
  `RegisteredAddress`, `SpawnOutcome` / `SpawnObservedOutcome`,
  `ChildRecord` / `SupervisorRecord`, `ChildRecordSnapshot` /
  `SupervisorRecordSnapshot`, and the `erase_effect{,_sendable}`
  helpers). Constructors (`new`, `with_betelgeuse_io_loop`,
  `with_clock*`) and the const accessors (`shard`,
  `trace_retention`, `trace_dropped`,
  `cancelled_call_cause_evictions`) stay in `lib.rs`. Crate-internal
  references continue to resolve through narrow `pub(crate) use
  dispatch::{...}` re-exports.
- `tina-sim/src/lib.rs` (6,787 → 1,644 lines ✅ <2,000) — extracted
  `mod sim_impl` carrying every non-constructor method on
  `Simulator<S>` (`register`, `register_with_mailbox_capacity`,
  `supervise`, `try_send`, `advance_time`,
  `advance_to_next_timer`, `step`, `step_with_remote`,
  `run_until_quiescent[_checked]`, `replay_artifact`,
  `durable_image` / `load_durable_image`, `observe_new_events`,
  `execute_effect` plus reject/reply/push_event family,
  `register_entry` / `spawn_isolate` / `record_child` /
  `enqueue_bootstrap_message`, restart/supervise lineage handling,
  `checked_registered_address` and the address-book lookups, the
  full `dispatch_call` / `dispatch_backend_call` /
  `dispatch_observed_send` / `dispatch_isolate_call` family, and
  every TCP / UDP / TLS / file / process / persistence /
  `TimerEntry` resource-state helper). The free `call_kind` /
  `fault_selector` helpers and the spawn-adapter types
  (`SpawnObservedAdapter`, `SpawnAdapter`,
  `RestartableSpawnAdapter` plus their `ErasedSpawn` /
  `IntoErasedSpawn` / `ErasedRestartRecipe` /
  `IntoErasedSpawnObserved` impls) moved alongside them. The
  Simulator struct, constructors, and immutable accessors stay in
  `lib.rs`. Phase audit doc
  (`.intent/phases/115-core-ecosystem-reorg/audit.md`) recorded
  the visibility decisions.

All moves are private re-exports, so the public API is unchanged.
The Runtime/Simulator fields and the private support types (the
full `Erased*` adapter family, the registered-address /
spawn-outcome / child-record / supervisor-record family, the
remote-envelope vocabulary, the resource-state structs, the
`Pending*` and `InFlightCall` / `StoredTranslator` /
`PendingIsolateCall` families) all became `pub(crate)` per the
audit so the submodules can name them; nothing newly public.

Deferred to a follow-on cleanup (intentionally named so they can't
be silently dropped):

- `tina-runtime/src/dispatch.rs` is ~3,250 lines and
  `tina-sim/src/sim_impl.rs` is ~5,270 lines. Both are above the
  comfortable per-file ceiling and are the next obvious targets for
  a sub-bin split (the runtime audit's `mod dispatch` was one
  conceptual bin; we kept it together so this PR stays an honest
  move-only refactor). The sim split into `mod simulator` / `mod
  resources` / `mod calls` follows the runtime pattern: visibility
  audit recorded; sub-bins are mechanical method moves once a
  reviewer wants smaller files.
- `tina-runtime/src/call/mod.rs` (2,562 → 1,779 lines) — extracted
  `mod io` carrying the closed-set runtime call vocabulary
  (`CallInput`, `PersistenceTraceInfo`, `CallOutput`, `CallError`,
  `SendOutcome`, `CallOutcome<T>` plus the `SendOutcome::from_rejected`
  helper). The remaining `mod.rs` keeps the type-erasure machinery
  (`RuntimeCall<M>`, `RuntimeCallKind<M>`, `RuntimeCallable`,
  `RuntimeCallParts<M>`, `ErasedCall`, `IntoErasedCall<M>`) and the
  typed-future family (`TypedCall<T>`, `ObservedSend<T>`,
  `IsolateCall<T, R>`, the deferred / request variants, and the
  builder constructors). Public API unchanged: `mod.rs` re-exports
  via `pub use io::*;` like the other call submodules.
- The oversized test homes (`tina-runtime/src/tests.rs`,
  `tina-runtime/tests/local_system.rs`,
  `tina-sim/tests/io_simulation.rs`) — splittable by test name
  only.

### Phase 123 Adversarial Hardening

- Hardened HTTP/1 keepalive response handling for chunked replies: keepalive
  clients now decode chunked response bodies, surface malformed or over-cap
  chunked bodies as typed parse failures, and retire the connection after
  chunked delivery so stale bytes cannot contaminate a later request.
- Tightened HTTP parser/WebSocket strictness: chunked size lines reject
  forbidden leading whitespace, chunked length accounting uses checked
  arithmetic, protocol-relative HTTP/1 targets are rejected, and WebSocket
  frames reject non-minimal extended lengths plus 127-form high-bit lengths.
- Hardened the native HTTP/2 server: DATA/HEADERS PADDED and PRIORITY flags
  are parsed correctly, SETTINGS now applies peer flow-control/frame-size
  facts before ACK, forbidden HTTP/1 connection-control headers and missing
  authority reject, and rapid reset churn produces `ENHANCE_YOUR_CALM`.
- Fixed `tina-rpc-tokio` bridge cancellation accounting so a stale
  cancellation guard cannot double-release a bounded admission slot.
- Added reserved cross-shard terminal reply lanes so remote `Full`/`Closed`
  replies are not silently dropped behind ordinary remote traffic.
- Unified bridge timeout/capacity truth across SQLx, AWS, reqwest, and Tokio
  bridge paths with separate caller/external/late-work accounting.
- Bounded runtime/process shutdown and journal repair paths: process cleanup,
  snapshot temp cleanup, truncated journal append repair, and append-side tail
  validation now settle with visible outcomes.
- Hardened runtime hot paths and ownership truth: bounded trace retention is
  amortized constant-time, buffered trace observers count drops, restart
  budgets can be windowed, stopped restart entries are collected, cancelled
  call cause-ring overflow is visible, and `PendingReplies::take()` is counted.
- Tightened simulator/proof/macro surfaces: deterministic per-tag simulator
  fault streams, explicit isolate and RPC service macro crate path overrides,
  core `Infallible` expansion, duplicate RPC request-id rejection, async Tokio
  bridge drain, and SQLx ambiguous-commit outcomes with completed step
  records.

### Phase 112 Protocol Facts To Replay

- New replayable fact effect. `Effect::Fact(I::Fact)` rides handler turns
  the same way other effects do; the runtime and simulator translate it
  into `RuntimeEventKind::FactObserved { fact: RuntimeFact }`. Stable
  effect-kind tag `13`; stable runtime-event tag `36`. Existing tags
  are unchanged.
- `tina::Isolate::Fact` associated type. Ordinary isolates default to
  `Fact = Infallible` (macros and `isolate_types!` set it automatically)
  and pay nothing. Protocol isolates opt in with `fact = ProtocolFact`
  in the `#[tina_runtime::isolate]` / `isolate_types!` form.
- `tina_runtime::RuntimeFact` carries the canonical replay shape with
  one family today (`Protocol(ProtocolFact)`) and a stable family tag
  `1`. The `IntoRuntimeFact` trait is the registration boundary: an
  isolate whose `Fact` type does not implement it will not register.
- `tina_runtime::ProtocolFact` vocabulary: `Http2StreamOpened`,
  `Http2StreamClosed`, `Http2StreamReset`, `Http2FlowControlFull`,
  `HttpBodyHighWater`, `WebSocketSlowPeerClosed`,
  `WebSocketSessionClosed`, `GrpcFinalStatusSent`. Each variant carries
  typed connection / stream / session / direction / reason fields and
  no stringly-typed substitutes.
- Real emission points in `tina-http`:
  - HTTP/2 connection isolate emits stream-open/close/reset and
    flow-control facts at the point each protocol fact becomes true
    (header dispatch, RST_STREAM, body-cap and flow-control rejects);
  - HTTP/1 connection isolate emits slow-peer and session-close facts
    on the WebSocket lane;
  - the native gRPC trailer send emits `GrpcFinalStatusSent` and a
    paired `Http2StreamClosed`.
  Blocking host helpers (`grpc_unary_call_h2c_blocking`) intentionally
  do not emit replay facts; the docs say so explicitly.
- Simulator parity. `Effect::Fact` executes identically in
  `tina_sim::Simulator`, so saved DST cases observe the same facts as
  the live runtime in the same order. A typed
  `ProtocolReplayMismatch::UnsupportedProtocolFact` arm names live-only
  physics gaps without faking a pass.
- Projection presets. `TraceProjection::protocol_facts()`,
  `http2_streams()`, `websocket_sessions()`, and `grpc_status()`
  produce fail-closed projections (every other event kind is named
  `ignored`, and an unknown kind still errors). One saved replay proof
  is locked in via `tina-sim/tests/protocol_fact.rs`.
- Compile-time rails. Three new `trybuild` fixtures pin that wrong
  fact wiring fails to compile: an ordinary isolate emitting a
  `ProtocolFact`, a protocol isolate emitting the wrong fact enum, and
  an isolate whose `Fact` type lacks `IntoRuntimeFact`.

### Phase 110 Workflow Pending Ergonomics

- `tina_runtime::sleep(d)` now returns a `SleepCall` wrapper. It forwards
  `.then(...)` and `.then_with_request(...)` unchanged and adds
  `.then_event(|| Msg::Wake)` so the user enum no longer needs a
  `SleepReply`-shaped field for plain "wake me later" timers.
  `then_event` is sleep-only: a non-timer `TypedCall<()>` (TCP close,
  file ops, etc.) must keep using `.then(...)` so its error path stays
  visible. A compile-fail doctest pins that rule.
- `PendingReplies::park_request(key, RequestCall<'_, I>)` and
  `PendingReplies::park_call(key, CallContext<'_, I>)` replace the copied
  `try_insert(qid, call.into_request_context().into_deferred())`
  ceremony. Admission is checked before caller authority is consumed, so
  `Full` / `DuplicateKey` (and for `park_call`, `NoCaller` /
  `CrossShardUnsupported`) return the original caller unchanged. Success
  returns a `ParkTicket<K>` whose private slot/generation identity makes
  forged tickets a compile error and stale-key ABA a runtime error.
- `RequestCall::try_capture` and `CallContext::try_into_request_context`
  added so park helpers can hand caller authority back without a panic.
- `GuardedPendingReplies<K, R, G>` is the new sibling type that pairs a
  parked caller with one RAII `G` guard. It removes the `PendingReplies +
  HashMap<K, Guard>` sidecar pattern and proves the guard is dropped
  exactly once on normal reply, drain, caller-gone sweep, and failed
  admission.
- `WaitList<K, R>` is the new many-callers-per-key parking lot in
  `tina_runtime::wait_list`. Hard global cap, optional per-key cap, FIFO
  per key, ticketed `reply_one`, and `reply_all_clone` /
  `reply_all_with` / `close_all_clone` / `close_all_with` /
  `drain_all_with` for multi-waiter replies.
- `CancelableWork<K, Q, R>` in `tina_runtime::call` is the natural-key
  bounded store for `PendingCancelableCall` tokens. Unlike
  `PendingCancelableCallSet`, multiple live entries may share one key.
  Admission returns a `WorkTicket<K>` so a stale completion against a
  reused slot cannot remove a newer entry.
- Every new helper exposes the same capacity surface
  (`capacity / len / high_water / full_rejects / capacity_report`) and
  takes `.named("service.helper")` so dashboards stay grep-friendly.
- System migrations: `ergonomics_playground`, `system_cache_with_fill`,
  `system_api_gateway_limits` (deletes its `HashMap<qid, SharedLease>`
  sidecar), and `system_soak_http_db` (deletes both HTTP and DB
  sidecars) now use the new helpers. Their existing smoke tests still
  pass.

### Phase 109 Typed Config And Protocol-State Safety

- Added the first split event/request service rail. The
  `#[tina::isolate]` / `#[tina_runtime::isolate]` macros now accept
  `event = Event, request = Request, reply = Reply` and generate an isolate
  whose mailbox envelope is `tina::ServiceMessage<Event, Request>`.
  `handle_event` receives fire-and-forget mailbox facts; `handle_request`
  receives caller authority through `RequestCall` and returns
  `RequestEffect`, so ordinary `noop()` no longer type-checks on the copied
  request path.
- Added capability handles for the split surface:
  `tina::ServiceEventAddress<Event, Request>`,
  `tina::ServiceRequestAddress<Event, Request, Reply>`, and
  `tina_runtime::SplitServiceHandle<Event, Request, Reply>`, returned by
  `Runtime::register_split_service` and
  `ThreadedRuntime::register_split_service`.
- Added `tina::send_event(...)` and `tina_runtime::call_request(...)`.
  The copied path now rejects the two common lane mistakes at compile time:
  requests cannot be sent as events, and events cannot be called as requests.
- Added host/runtime companions:
  `Runtime::try_send_event`, `ThreadedRuntime::try_send_event`,
  `ThreadedRuntime::send_event_and_observe`,
  `ThreadedRuntime::send_event_observed_until`, and
  `ThreadedRuntime::call_blocking_request`, so threaded tests and setup code
  do not need to unwrap split handles into raw `ServiceMessage` addresses.
- Changed split-service raw request-on-event handling from silent `Noop` to a
  visible `Reject(UnsupportedMessage)` effect. The request handler is still
  not run; the trace now records the wrong-lane escape hatch.
- Added positive runtime/threaded proof in `tina-runtime/tests/safety_rails.rs`
  and trybuild diagnostics for split-lane mistakes, invalid split macro
  options, missing split handlers, private internal events, and a split request
  handler that ignores/fakes caller authority (`noop`, `let _`, `drop`,
  partial branch, double consume, forged `RequestEffect`).
- Migrated `examples/systems/system_cache_with_fill` to split public requests
  plus private internal fill events and `call_blocking_request`.
- Migrated `examples/systems/system_lock_manager` to split public requests plus
  private internal lease-expiry events and `call_blocking_request`.
- Confirmed `examples/systems/system_job_queue` as the cancelable deferred
  admission proof: `defer_cancelable(...).try_admit(...)` only returns the
  child effect after `PendingCancelableCallSet` accepts the token.
- Documented the split event/request copied path in
  `docs/tina-user-guide/04-request-reply.md`.

### Phase 108 Proof Harnesses And Replay Ops

- Added the `tina-proof-harness` crate with reusable load/soak,
  bad-peer, and live-replay helper modules. The harness reports typed
  pressure/failure facts instead of forcing tests to scrape logs.
- Added `examples/systems/mini_saas_api/tests/soak.rs` and wired the
  Mini SaaS system through the load/soak harness with capacity and
  lifecycle summaries.
- Added `examples/systems/system_realtime_rooms/tests/bad_peer.rs` so
  the WebSocket/realtime-room path proves a real bad-peer/slow-peer
  close shape through the shared harness.
- Added `examples/systems/system_live_replay_bugbox`, a small system
  that demonstrates the live-capture -> saved replay -> simulator
  workflow and records the expected user-facing command/output shape.
- Added Makefile proof targets for the fast/slow harness split so
  cheap model sessions and humans can run the same smoke/soak/replay
  commands without inventing private drivers.

### Phase 107 Observability And Capacity Product

- Added `tina_runtime::service_pressure` and `ServicePressureReport`
  shapes for copied service-level pressure summaries, including
  measured and explicitly unavailable surfaces.
- Added `SharedCapacityScope` for shard-local user-defined weighted
  budgets. Reports expose current/high-water/full/released counts and
  owner-stop release behavior; weights remain user-defined and honest
  rather than pretending to be memory accounting.
- Added `BoundedEventSink`, a capped log/metric/event sink with drop
  policy, high-water, dropped counts, and drain snapshots, so
  observability does not become a hidden unbounded queue.
- Expanded capacity assertion/discovery helpers in `tina-runtime` and
  updated `mini_saas_api` to emit/assert compact topology and capacity
  summaries.
- Added `examples/systems/system_api_gateway_limits` and
  `examples/systems/system_soak_http_db` as user-shaped proofs for
  shared weighted capacity and CI-friendly pressure discovery lines.
- Recorded the simulator honesty boundary: the new out-of-trace live
  surfaces report `Unavailable` in simulator paths until a future
  adapter carries those facts into replay.

### Phase 105 Request-Scoped Cancellation

- Added `tina_runtime::scope` with `RequestScopeId`, bounded request
  scope storage, scoped call handles, and typed scope cancellation
  reports.
- Wired request-scope cancellation through Tina-owned waits so a
  request can cancel child calls/timers/rails it owns, reclaim bounded
  storage, and keep late completions visible instead of mysterious.
- Kept the external-work boundary honest: bridges may stop waiting and
  reclaim caller capacity, but Tina does not claim a database/AWS/HTTP
  SDK operation stopped unless the bridge can prove that terminal fact.
- Added runtime and simulator tests for scope admission, fill/cancel/refill,
  owner-stop cleanup, late completion truth, and admission-failure paths.
- Added `examples/specimen_request_scope_fanout`, showing one request
  fanning cancellation through multiple child operations with typed
  results and visible cleanup.
- Updated request/reply and lifecycle/shutdown docs with the request-scope
  cancellation model and its "cancel wait vs cancel external work" boundary.

### Phase 104 Production Client / Bridge Breadth

- Extended `tina-aws-bridge` beyond the S3 first form with DynamoDB,
  SNS, Secrets Manager, and broader SQS surfaces, each with typed
  request/response/error shapes, explicit caps, timeouts, metrics, and
  operation/service tracing fields.
- Added AWS classifiers for success/transient/fatal/caller-timeout style
  outcomes without hiding retry/idempotency policy inside the bridge.
- Added hermetic bridge tests and request-shape examples for DynamoDB,
  SNS, Secrets Manager, and SQS so copied use does not require a real AWS
  account.
- Tightened supplied-client/bridge ownership docs and bridge convention
  language around timeout ownership, close behavior, worker-terminal
  metrics, and caller-observed late-result truth.
- Added `examples/systems/system_webhook_relay` as a production-shaped
  bridge consumer with retry/dead-letter-style policy and typed capacity
  facts.
- Updated `system_bounded_object_lane` to record the real bridge-backed
  object-lane lessons while keeping default tests hermetic.

### Phase 103 Protocol Parity Finish

- Audited the native HTTP/2, gRPC, and WebSocket stacks for the
  "Tina replaces the protocol, not the runtime" claim and aligned the docs
  with what `tina-http` actually proves today.
- HTTP/2: confirmed typed `Http2ProtocolError` covers bad preface, oversized
  frame/headers/body, malformed pseudo-headers, flow-control, window
  overflow, bad stream id, unsupported frames, and request trailers, plus
  `Http2Outcome` (marked `#[non_exhaustive]`) names the six lifecycle
  categories the plan called out: `Replied`, `Full`, `Closed`,
  `FlowControlBlocked`, `Timeout`, `ProtocolError(Http2ProtocolError)`,
  `StreamReset(u32)` (peer-initiated), and `LocalCancel(u32)` (locally
  initiated). `Http2ConnectionReport` carries the per-connection counters
  for opened/closed/reset streams, connection/stream full, flow-control
  blocked, GOAWAY sent, and late replies after close.
- HTTP/2 live tests now also assert the wire error code on every GOAWAY
  and on each RST_STREAM the server emits: oversized frame ->
  FRAME_SIZE_ERROR, oversized header block -> PROTOCOL_ERROR, inbound
  flow-control violation -> FLOW_CONTROL_ERROR, stream cap exceeded ->
  ENHANCE_YOUR_CALM, refused-after-peer-GOAWAY -> REFUSED_STREAM. Unit
  tests pin the typed-error -> wire-code mapping and the Http2Outcome
  vocabulary so future drift breaks compile or assertion, not silently.
- gRPC: `GrpcRouter` ships unary, server-streaming, client-streaming, and
  bidirectional streaming routes with typed `GrpcStatus` trailers, declared
  message caps, deadline-to-`DeadlineExceeded` mapping, content-type
  rejection, identity-encoding-only handling, and h2c tonic interop. Live
  tests cover the full peer-reset cancellation path through both response
  source and accepted service call sides, malformed frame final status,
  declared message cap rejection, and concurrent gRPC streaming modes
  sharing one HTTP/2 connection without cross-talk.
- WebSocket: extracted `tina_http::WebSocketMemberTable` with `admit`,
  `broadcast_text`/`broadcast_binary`, `fanout`, `shutdown_close`,
  `remove_peer`, and `record_send_outcome`, plus the typed
  `AdmitOutcome::{Admitted, AlreadyMember, Full}` and
  `SendOutcomeAction::{Ok, RemovedSlow, RemovedClosed, RemovedProtocol,
  RemovedTimeout, Stale}` enums. Counters live on
  `WebSocketMemberTableReport`. The table preserves explicit admission,
  fanout pressure, slow-peer eviction, and close reports; idle eviction,
  the recurring liveness tick, and shutdown sequencing remain in the room
  isolate that owns the table.
- Migrated `examples/systems/system_realtime_rooms` to the new helper. The
  smoke and bad-peer proofs still pass; the room's typed counters
  (joined, left_idle/peer/slow/shutdown, presence ticks, shutdown_close_*)
  now mirror the table's bookkeeping rather than reaching into a private
  `BTreeMap`.
- Documented the WebSocket helper in `tina-http`'s module docs, in
  `examples/specimen_websocket_room/README.md`, and in
  `examples/systems/system_realtime_rooms/README.md`.
- ROADMAP "Native service protocols" row now names the typed HTTP/2 errors,
  the four gRPC modes, the WebSocket browser proof, and the WebSocket
  member-table helper, and lists what stays future work (native HTTP/2
  client, HTTP/2 TLS ALPN/mTLS, gRPC reflection/interceptors/load
  balancing, native broad WebSocket client, `permessage-deflate`,
  web-framework ergonomics, and full WebSocket-bytes simulator replay).
- **Deferred from this phase, named follow-up.** Rock 5 ("Simulator
  Facts") did not ship. The protocol facts the plan named — stream
  opened/closed/reset, flow-control full, body high-water, WebSocket
  slow-peer close, and server-side gRPC final status sent — surface as
  bounded counters on `Http2ConnectionReport`, `BodyMetrics`,
  `WebSocketMemberTableReport`, and typed `GrpcStatus` trailers, not as
  `RuntimeEventKind` variants on the trace stream. The blocking
  `grpc_unary_call_h2c_blocking` helper observes received trailers, but it
  is not a Tina client service and does not emit replayable runtime facts.
  Protocol facts do not round-trip through `tina-sim` replay yet, which
  means a protocol bug cannot be replayed from a trace today.
- **Deferred from this phase, named follow-up.** The plan's Required
  Proof line "At least one DST replay case for a protocol
  pressure/lifecycle bug" is satisfied by pre-existing
  `tina-http/tests/dst_simulator.rs` cases
  (`slow_body_multichunk_inbound_replays_deterministically`,
  `service_full_with_concurrent_peers_replays_deterministically`,
  `shutdown_mid_request_replays_deterministically`), so the phase 103
  minimum is met; no new DST replay case was added in this phase. A
  protocol-fact-driven DST replay rides with the Rock 5 follow-up.
- Both deferrals are tracked in `ROADMAP.md` under "Protocol facts as
  runtime/simulator trace events" and in
  `.intent/phases/103-protocol-parity-finish/plan.md` under "Deferred
  to follow-up". Phase 103 is marked shipped on the strength of Rocks
  1–4 (protocol parity, helper extraction, client-side parity docs)
  and the pre-existing DST coverage; Rock 5 is not silently dropped.

### Phase 106 Lifecycle, Health, And Topology

- Added `tina_runtime::lifecycle` with typed service lifecycle vocabulary:
  `Lifecycle` (Starting / Ready / Degraded / Draining / NotReady / Stopped),
  `ReadinessReason` with stable wire tokens via `as_token()` (Starting,
  IngressStopped, DependencyClosed/Full/Timeout/Error("dep"), Custom),
  `Readiness` with `legacy_body()` for the `ready\n` /
  `not_ready reasons=<csv>\n` wire format, and `Health` pairing the typed
  state with an optional `ServicePressureReport` snapshot.
- Added `ServiceTopology` and `TopologyComponent` so services answer "what
  is running" with one greppable report naming isolates, bridges, pools,
  listeners, addresses, the shard label, and the current lifecycle state,
  backed by the existing `ServicePressureReport` for bounded-surface
  capacity facts. No global registry; each service constructs and threads
  the report explicitly.
- Added `ShutdownChoreography` plus typed step / outcome / close vocabulary
  (`ShutdownStep`, `ShutdownStepReport`, `StepOutcome`,
  `ServiceShutdownReport`, `ResourceCloseReport`, `ResourceKind`,
  `CloseAdmission`, `CloseOutcome`). The helper records ordered shutdown
  steps with elapsed and outcome; recordings whose ordinal precedes the
  highest already-recorded step become `StepOutcome::OrderingViolation`
  so bad sequences are visible, not hidden. `record_close` folds typed
  resource-specific reports (keepalive pool, bridge, listener) into the
  same step kind while preserving the resource details.
- Refreshed `examples/systems/mini_saas_api` into the canonical lifecycle
  skeleton: replaced the stringly-typed `ready_reasons(&[...])` helper
  with `Readiness` + `ReadinessReason` (wire body unchanged), added a
  typed `ServiceTopology` to `RunReport::topology` naming the main
  listener, notify listener, controller isolate, SQLite bridge, and
  outbound keepalive pool, wrapped the host shutdown sequence in a
  `ShutdownChoreography` recording every step
  (`StopIngress` → `DrainInFlight` → 4 × `CloseResource` → `StopOwner`)
  with per-resource close reports, and exposed the typed report on
  `RunReport::shutdown_report` and `RunReport::health_pre_shutdown`.
  Smoke tests assert against the typed surfaces in addition to the
  legacy wire strings.
- Updated `examples/systems/system_metrics_shipper` as the worked
  non-HTTP proof: the host shutdown drives `ShutdownChoreography` with
  `DrainInFlight` (the shipper's `Stop` handshake) → `CloseResource
  sink.isolate` → `StopOwner` (runtime shutdown), populates a typed
  `ServiceTopology` (shipper / sink / flush_tick), and emits a typed
  `Health` in `Lifecycle::Stopped`. Added `lifecycle_for_drain_stage`
  mapping `DrainStage::{Open,Draining,Stopped}` to
  `Lifecycle::{Ready,Draining,Stopped}` so a service-owned drain
  handshake reports state in the same vocabulary as a host-driven
  service. The smoke test pattern-matches on the typed report.
- Added DST-style ordering proofs in `tina_runtime::lifecycle::tests`:
  every backwards step pair produces a typed
  `StepOutcome::OrderingViolation`, and identical step sequences produce
  byte-identical `ServiceShutdownReport::summary_line` and
  `discovery_lines` across runs.
- Signal-driven shutdown composes through the new helper. Added
  `tina-runtime/tests/lifecycle_signal_driven_shutdown.rs` proving the
  pattern end-to-end: a spawned "signal handler" thread fires
  `ThreadedShutdownHandle::request_shutdown` and the main thread wraps
  the cross-thread teardown in `ShutdownChoreography` producing a clean
  typed report. A second test proves a late "signal" landing after
  `StopOwner` is recorded as a typed `OrderingViolation` rather than
  starting a second shutdown.

Hostile-review fixes:

- Bug: `Readiness::not_ready` / `Readiness::degraded` with an empty
  reasons list could emit the ambiguous `not_ready reasons=` body in
  release mode (the previous `debug_assert!` did not fire). Added a
  public `READINESS_UNKNOWN_REASON` (`Custom("unknown")`) constant and
  routed both constructors through an `ensure_some_reason` fold so the
  wire body is always parseable. Tightened `Readiness::legacy_body` so
  the HTTP shape keys off `self.ready` alone: a degraded service still
  answers `ready\n` so existing clients keep sending traffic while the
  typed report carries the degradation detail.
- Proof: added a typed
  `Vec<Lifecycle>` `lifecycle_transitions` field on both
  `mini_saas_api::RunReport` and `system_metrics_shipper::ShutdownReport`
  and assert the canonical
  `[Starting, Ready, Draining, Stopped]` sequence. Closes the plan's
  "service starts NotReady, becomes Ready, enters Draining, then
  Stopped" required proof which was only implied across separate fields.
- Proof: added `stuck_child_close_produces_typed_timeout_in_terminal_report`
  covering the plan's "shutdown with stuck child returns a timeout
  report" requirement at the full-sequence level (clean step + stuck
  step + remaining clean steps; asserts `clean=false`, exactly one
  `Timeout`, retained close report, summary line, discovery lines).
  Added four `pool_shutdown_to_close_report` unit tests in
  `mini_saas_api` covering clean drain, timed-out drain, connection
  failures, and already-closed pool — the production conversion path.
- Coverage: smoke tests now assert each `TopologyComponent::kind`
  (`listener`, `bridge`, `pool`, `isolate`, `timer`) per-name instead
  of substring matching, and assert `Health::summary_line()` carries
  the typed state and service label. Added unit coverage for
  same-ordinal step repetition (the multi-`CloseResource` case),
  every non-`Clean` `CloseOutcome` propagating through `record_close`,
  and an `unknown` fallback test for the empty-reasons fix.
- Cleanup: `mini_saas_api::build_startup_summary` now calls
  `ServicePressureSurface::discovery_line()` instead of duplicating the
  measured/unavailable format inline. Tightened the DST ordering test
  to walk every distinct-ordinal pair (clean + violation) without
  confusing variable scaffolding. Added a doc note that
  `ServiceShutdownReport: PartialEq` includes wall-clock durations so
  equality-based DST tests should zero them and use
  `ShutdownChoreography::with_started_at`.

### Phase 102 Host Control Ergonomics

- Added address-routed `ThreadedMultiShardRuntime::call_blocking(addr, msg,
  timeout)` so host/test/control-plane code can call sharded services without
  a driver isolate or explicit shard argument.
- Tightened `ThreadedRuntime::call_blocking` and the multi-shard mirror so
  host-control command admission is bounded and honest: full worker command
  queues return `ThreadedRuntimeError::CommandFull` instead of waiting behind
  worker scheduling before the caller timeout starts.
- Added cloneable `ThreadedShutdownHandle` for single- and multi-shard threaded
  runtimes. Host code can request shutdown, wait for the terminal report, and
  share that control path across threads without `Arc::try_unwrap(runtime)`.
- Added `tina-runtime/tests/host_control_ergonomics.rs` coverage for normal
  replies, `Full`, `Closed`, `Timeout`, unsupported calls, unknown shard,
  command-full admission, idempotent shutdown, multiple waiters, cached
  terminal reports, and shutdown while command queues are saturated.
- Migrated copied host-control docs and system/specimen call sites to the new
  `call_blocking` / `shutdown_handle` shapes where they remove driver or
  unwrap ceremony without hiding service shutdown policy.

### Phase 101 Mailbox-First Service Ergonomics

- Added service-local helper state for repeated mailbox-first patterns:
  `tina::time::RecurringTick`, `tina_runtime::LocalPermitGate`,
  `tina_runtime::DrainState`, and `tina_runtime::FullHandling`.
- Added `register_with_capacity_and_bootstrap` and threaded/multi-shard mirrors
  so startup remains an explicit mailbox message while registration can prefill
  that first message before exposing the address. Failed prefill returns a typed
  bootstrap error and leaks no registered address.
- Kept the helpers Tina-shaped: they compute decisions, permits, drain state,
  and reports. They do not mutate user state in callbacks, resend messages,
  close resources, or run hidden retries.
- Migrated system/specimen code including metrics shipping, bounded object
  lanes, and service shutdown paths to the copied helper shapes.

### Phase 100 Compile-Time Safety Rails

- Added capability-typed addresses `tina::SendAddress<M>` and
  `tina::CallAddress<M, R>` so the wrong path becomes a compile error instead
  of a runtime `CallRejectedReason::UnsupportedMessage`. Wrapped raw
  `Address<M, R>` is the explicit escape hatch.
- Added `tina_runtime::ServiceHandle<M, R>` and `SendOnlyServiceHandle<M>`,
  plus `Runtime::register_service` / `register_service_send_only` and threaded
  mirrors that return the capability-typed handle.
- Added the `tina::CallableIsolate` marker trait with a stable
  `#[diagnostic::on_unimplemented]` phrase. The `#[tina::isolate]` and
  `#[tina_runtime::isolate]` macros emit the impl automatically when the
  block defines `fn handle_call(...)`. Registering a non-callable isolate
  through `register_service` is now a compile error rather than a service
  whose every caller silently sees `UnsupportedMessage`.
- Added the `send_only` macro flag: forces `Reply = ()` and rejects an
  authored `handle_call`. Send-only services register through
  `register_service_send_only` which exposes only the `.send` lane.
- Added capability-typed `tina_runtime::call_typed` and
  `ThreadedRuntime::call_blocking_typed`. The older `call` /
  `call_blocking` keep working with raw `Address` for low-level interop.
- Added `tina::send_to(SendAddress, msg)` as the capability-typed companion
  to `tina::send`.
- Added the user-shape proof matrix in `tina-runtime/tests/safety_rails.rs`
  (positive fixtures) and compile_fail doctests pinned to `SendAddress`,
  `CallAddress`, `call_typed`, and `register_service` (negative fixtures).
- Migrated `system_cache_with_fill` to `register_service` /
  `call_blocking_typed` so the public call lane is type-tagged at every
  caller boundary. Migrated `system_realtime_rooms` to hold the room as a
  `ServiceHandle` and to type-tag the gateway's call lane as a
  `CallAddress`. Stamped `impl tina::CallableIsolate` on hand-rolled
  `isolate_types!` isolates that define `handle_call`.
- Documented the rails and the review rule
  ("could this runtime rejection be a type error?") in
  `docs/tina-user-guide/21-compile-time-safety-rails.md`.
- Deferred: the wire-enum split (separate `InternalMsg` and `PublicCall`
  types so calling an internal continuation by accident becomes a compile
  error). The cancelable-admission rail shipped separately in Phase 097.

### Phase 099 Production Service Skeleton Refresh

- Refreshed `examples/systems/mini_saas_api` as the copied production-shaped
  service skeleton: native HTTP routes, controller isolate, SQLite bridge pool
  shape, outbound keepalive webhook, health/readiness, capacity/pressure
  report, live-replay fact, and graceful shutdown.
- Migrated the skeleton to current service vocabulary: `call_ctx.defer(...)`,
  current pressure/capacity reports, `DrainState`, capability-typed service
  handles where applicable, and host-control helpers where they reduce ceremony.
- Added smoke and pressure modes plus docs that name what is reusable Tina API
  versus specimen-local service policy.

### Phase 097 Cancelable Deferred Admission

- Added `PendingCancelableCallSet<K, Q, R>` and `PendingCancelableTicket` for
  bounded cancelable multi-turn calls that must store caller authority before
  dispatching child work.
- Added `DeferredCancelableCall::try_admit(...)` as the blessed copied path:
  the child effect is returned only after bounded storage accepts the pending
  token. `Full` and duplicate-key admission errors return the token so the
  service can still answer or reject the original caller.
- Kept removal keyed by `(key, ticket)` so stale completions cannot remove a
  reused natural key, and added tests for full, duplicate, refill, cancel,
  owner-stop drain, stale completion, and capacity cleanup.
- Migrated `system_job_queue` to the new helper, proving cancel-while-running
  and worker-failure paths with one bounded pending set.

### Phase 081 Bridge Convention Audit

- Audited `tina-tokio-bridge`, `tina-tower-bridge`, `tina-reqwest-bridge`,
  `tina-sqlite-bridge`, `tina-sqlx-bridge`, `tina-rpc-tokio`, and
  `tina-aws-bridge` for install/config/closer/metrics/tracing/late-result and
  supplied-client vocabulary.
- Added the bridge convention table to the user guide and tightened stale docs
  where bridge metrics or traces overclaimed caller-terminal truth.
- Kept the result as convention plus small fixes, not a new bridge framework:
  bridge timeouts still mean Tina stopped waiting unless the backend proves
  stronger cancellation, and worker-terminal metrics are named honestly.

### Phase 095 Call Context Defer Ergonomics

- Added `CallContext::defer(work).reply(...)` so multi-turn call handlers start
  from explicit caller authority without making ordinary continuations carry
  hidden request context.
- Added `then(...)` and `then_with_request(...)` names for ordinary runtime
  continuations, deprecated the old ordinary `reply(...)` /
  `reply_with_request(...)` builder spellings, and added a `ReplyAbandoned`
  diagnostic hint pointing at `call_ctx.defer(work).reply(...)`.
- Migrated the multi-turn request-context specimen and selected
  `mini_saas_api` call sites, and added runtime/specimen tests for successful
  deferred replies, full/closed/timeout outcomes, unsupported call paths, and
  caller-visible timeout handling.

### Phase 094 WebSocket Usable Server

- Added the user-shaped WebSocket server layer on top of the first-form
  WebSocket rail: public session handles, bounded session send, bounded
  room/fanout helpers, slow-peer policy, and copyable service docs.
- Added TCP/TLS/browser-shaped WebSocket proofs plus a room specimen so the
  WebSocket path is no longer only a frame/protocol primitive.
- Left the larger production-replacement claim explicit: compliance matrix,
  Autobahn-style classification, load/soak, production observability, native
  client decision, and live trace-to-sim replay remain follow-up work.

### Phase 057 Native gRPC Service Stack

- Added native gRPC over Tina's HTTP/2 h2c path with `prost`, typed
  `GrpcStatus`, trailers, unary service shape, request/response caps,
  timeout/cancel/status mapping, and live tests.
- Extended the first gRPC stack with initial server-streaming and
  client-streaming routes on native HTTP/2, plus a small h2c specimen helper.
- Kept the scope honest: true bidirectional streaming, production pooled Tina
  gRPC client, tonic/grpcurl interop scripts, TLS ALPN, and richer HTTP/2
  streaming substrate remain follow-up work.

### Phase 056 Native HTTP/2 Service Stack

- Added a native HTTP/2 first form in `tina-http`: frame parsing/encoding,
  client preface/settings, headers/data/reset/goaway handling, bounded stream
  state, flow-control accounting, and Tina-owned TCP/TLS I/O.
- Kept the scope narrow and honest: HTTP/2 is service-shaped and bounded, not a
  full hyper/tonic clone. gRPC, broad web-framework ergonomics, and advanced
  HTTP/2 feature parity remain follow-up work.
- Added live tests for handshake, request/response, concurrent streams, body
  caps, flow-control pressure, reset/goaway, malformed frames, TLS transport,
  and trace/DST projection support.

### Phase 092 AWS Bridge Follow-Ups

- Extended `tina-aws-bridge` with a bounded SQS first follow-up:
  `install_sqs`, `SqsWorker`, typed send/receive/delete requests,
  typed responses/errors, message body and receive-count caps, explicit
  visibility timeout, empty receive as success, metrics, pressure reports,
  and close/drain lifecycle truth.
- Kept retry and idempotency caller-owned. Bridge timeout still means Tina
  stops waiting; already admitted SQS SDK work may finish late and continues
  occupying bounded bridge capacity until terminal truth is observed.
- Added fake-local SQS tests for happy send/receive/delete/empty receive,
  cap rejection, full/closed pressure, timeout/late-result metrics,
  close/drain operation kinds, typed SDK queue errors, supplied-client retry
  ownership, and config validation.

### Phase 091 Timer Vocabulary

- Added replay-safe timer helper vocabulary in `tina::time`: interval,
  backoff, retry-delay, debounce, and throttle state helpers.
- Kept helpers as state, not schedulers. User code still emits
  `sleep(delay).then(...)`, so the runtime owns time and simulator replay stays
  honest.
- Added timer semantics tests and migrated `specimen_periodic_batcher` to the
  copied interval/backoff shape.

### Phase 085 Race And Join Helpers

- Added bounded `CallGroup` support in `tina-runtime` for first-success race
  patterns over named calls.
- Kept loser cancellation visible and bounded: no hidden retry, no hidden
  unbounded result collection, and no magic `select!` clone.
- Migrated the cancellation-chain specimen to the helper and added tests for
  winner selection, loser cancellation, full/closed/timeout paths, dropped
  callers, and capacity cleanup.

### Phase 082 Capacity Modeling Round 2

- Added weighted capacity reporting, shared HTTP body-byte capacity scopes,
  explicit unbounded-for-now policies, and stronger capacity discovery /
  assertion helpers.
- Updated HTTP body streaming metrics and specimens so users can tune from
  observed high-water/full counts instead of guessing giant caps.

### Phase 088 AWS Bridge First Form

- Added `tina-aws-bridge`, an opt-in S3-only bridge around the AWS Rust SDK with explicit config, bounded mailbox and in-flight admission, capped `PutObject` / `GetObject`, `HeadObject`, `DeleteObject`, typed S3 request/response/error enums, metrics, and examples.
- Pinned the cancellation truth: bridge timeout lets Tina stop waiting, but already accepted SDK work may finish late and continues occupying bounded bridge capacity until it reports terminal truth. `S3Closer::close_and_drain` reports remaining in-flight operation kinds at deadline.
- Added fake-local S3 tests for happy put/get/head/delete, body caps, full/closed pressure, timeout/late-result metrics, close/drain lifecycle, typed SDK errors, supplied-client ownership, and zero-cap config validation.

### Phase 089 Live Trace To Sim Replay Workflow

- Added live replay capture tooling in `tina-sim`: `LiveReplayCapture`,
  `LiveReplayReport`, `TraceProjection`, `UnsupportedLiveFact`,
  `SavedReplayCase`, and `CapturedReplayMismatch`.
- Made live-to-sim replay fail closed. A captured case cannot pass while
  unsupported live facts remain, and projected comparison rejects event kinds
  that were not explicitly included or ignored.
- Added the user workflow for "bug in a box": capture typed live inputs/config/
  topology/pressure facts, save a replay case, run the simulator projection,
  and get a mismatch that names the missing fact or divergent trace shape.

### Phase 090 Resource Lifecycle Unification

- Added a compact lifecycle matrix to the shutdown guide, naming close
  admission, close resource, cancel, drain, terminal proof, and honest
  `not Tina-owned` boundaries for runtime, pool, HTTP body, keepalive, and
  bridge surfaces.
- Fixed `KeepaliveConnectionMsg::Stop` on the call-shaped path so callers see
  `CallOutcome::Replied(KeepaliveOutcome::Stopped)` instead of
  `Rejected(UnsupportedMessage)`.
- Added `shutdown_keepalive_pool(...)` and
  `KeepalivePoolShutdownReport` as the copied keepalive shutdown path:
  close pool admission, wait for `Drain` leases to return before stopping
  connections, count requested/stopped/timed-out/rejected/already-closed, and
  name any non-stopped connection by slot. If admission close fails or `Drain`
  times out with leases remaining, the helper leaves connections running and
  reports that terminal truth.
- Follow-up: bridge close/drain terms still deserve a separate audit before any
  common bridge lifecycle helper is introduced.

### Phase 070 Small Sharded Ergonomics

- Added `ShardBatch<T>` and `GroupByOwnerError::CapExceeded` to
  `tina_runtime::sharded`.
- Added `ShardPlacement::group_by_owner_bytes` and
  `ShardPlacement::group_by_owner_str` for bounded grouping of keyed items by
  owner shard. Output follows `placement.shards()` order; empty shards are
  omitted; too many items returns a typed cap error.
- Added integration tests proving placement-order preservation,
  non-contiguous shard ids, empty input, cap exceeded, duplicate-key grouping,
  and byte-identical live/sim mapping.
- Added `hot_key_pressure_first_attempt_only_no_retry_loop` test to
  `sharded_primitives.rs`, making explicit the first-attempt-only pressure
  report shape when the caller does not retry.
- Updated `docs/tina-user-guide/10-service-patterns.md` with group-by-owner
  snippet, hot-key pressure paragraph, and caller-owned retry paragraph.

### Phase 078 Host-Side Ergonomics

- Expanded docs for `ThreadedRuntime::call_blocking` with full example,
  usage guidance, and a warning that it is host/test only and must never
  be called from inside an isolate handler.
- Added trace query helpers to `RuntimeTraceExt`:
  `count_matching` / `any_matching`, `count_spawned` / `any_spawned`,
  `count_call_dispatched` / `any_call_dispatched`.
- Migrated `tina-http/tests/client_bad_input.rs` and
  `tina-http/tests/client_against_native.rs` from fake host Driver
  isolates to direct `call_blocking` calls, removing ~130 lines of
  ceremony while preserving `CallOutcome` visibility.

### Phase 086 Call Context Reply Obligation

- Split send and call handling at the public isolate boundary. Plain
  `handle(...)` messages have no caller; `handle_call(...)` receives a typed
  `CallContext` that must be replied, rejected, or promoted into a
  `RequestContext`.
- Replaced warning-only abandoned replies with immediate caller truth:
  unused call authority rejects as
  `CallOutcome::Rejected(CallRejectedReason::ReplyAbandoned)` and reclaims
  capacity instead of leaving the caller in timeout purgatory.
- Added explicit `CallRejectedReason` vocabulary for unsupported call messages,
  abandoned replies, and handler panics, with live/runtime/simulator trace
  coverage.
- Migrated runtime, simulator, bridge crates, docs, and specimens to the new
  call-shaped service dispatch. Multi-turn services now carry
  `RequestContext` deliberately through continuation messages and finish with
  `reply_to_request(...)`.
- Updated request/reply docs with the blessed multi-turn pattern so readers do
  not copy the old "runtime magically keeps caller context" bug.

### Phase 084 Child Lifecycle / Join / Supervision Usability

- Added typed child observation through `ChildRef<M, R>` and
  `spawn_observed(...)`, so a parent can spawn a child and receive the child's
  typed address/generation as ordinary message data.
- Kept the first form honest: child refs are not liveness promises, stale
  generations remain visible, host-side typed child-start observation is
  deferred until spawn events carry enough type truth, and cross-shard child
  ownership remains out of scope.
- Added live and simulator proofs for observed spawn success, zero-capacity
  rejection, invalid construction, parent-delivery failure, and parent use of
  the returned child address.
- Updated supervision docs/specimens away from Boot-message and trace-spelunking
  patterns where the new child ref is the clearer copied shape.

### Phase 080 HTTP Body Chunked Symmetric

- Added a shared incremental HTTP/1 chunked-transfer decoder.
- The native HTTP client now decodes chunked responses into bounded buffered
  response bodies, charging decoded bytes against `HttpLimits::max_body_bytes`.
- The native HTTP server now accepts chunked request bodies through the existing
  streaming pull model when inbound streaming is enabled, while still rejecting
  chunked requests loudly when streaming is disabled.
- Added integration coverage for client chunked decode, server chunked request
  streaming, HTTPS chunked request parity, malformed/truncated chunked wire, body
  cap enforcement, and body-metric accounting.

### Phase 079 Cancellation Round 2

- Applied cancellation truth to HTTP body sources: when a connection abandons a
  streaming response, `ResponseChunkMsg::Cancel` is delivered to the source so
  it can release files, downstream calls, and pending slots.
- Added cancellation paths for known-length and chunked streaming responses and
  made duplicate cancels harmless.
- Added a cancellation truth table to the lifecycle guide that names what cancel
  means on Tina-owned rails, bridge/external work, pools, body sources, and
  caller-owned request flows.

`ResponseChunkMsg::Cancel` is a new typed variant sent to chunk sources
when the HTTP connection abandons the wire mid-stream. Sources can
release files, downstream calls, and pending slots. `IterBodySource`
handles it with `stop()`.

The connection isolate sends `Cancel` on every wire-death path:
`Read(Err)`, `Wrote(Err)`, `handle_wrote(0)`,
`handle_stream_chunk(Timeout|Full|Closed)`, peer EOF, and header
deadline. It also defensively cancels in `begin_close()`. Duplicate
cancels are harmless — the source either already stopped or drops the
late message.

`body_io_error_count` still increments on truncation; cancel is an
additional typed signal, not a replacement for the metric. Integration
tests prove both known-length and chunked paths.

Added a cancellation truth table to
`docs/tina-user-guide/14-lifecycle-and-shutdown.md` that names what
cancel means on every surface Tina exposes.

### Phase 077 DB Pool Consumers

- Made the database bridges report pool-shaped pressure instead of leaving DB
  concurrency as bridge folklore.
- Added `PgPressureReport` / `PgMetricsHandle::pressure_report` for the SQLx
  bridge, separating Tina admission `Full`, SQLx pool-acquire pressure,
  per-attempt timeout, SQL errors, and lane high-water truth.
- Added `SqlitePressureReport` / `SqliteMetricsHandle::pressure_report` for the
  serial SQLite bridge, making the one-connection / one-in-flight shape visible
  through the same pool vocabulary.
- Added admission tests proving the pressure reports match the installed bridge
  capacity instead of caller-supplied stale config.

### SQLx/Postgres bridge

- Added `PgPressureReport` and `PgMetricsHandle::pressure_report` so
  callers can observe the bridge as a pool: `capacity`, `leased`,
  `available`, `waiters`, `full_count`, `timeout_count`, `high_water`.
- Added admission tests proving distinct outcomes at each boundary:
  `PgError::Full` (Tina admission), `PgError::PoolAcquireTimeout`
  (SQLx pool pressure), `PgError::Timeout` (bridge deadline), and
  SQL errors (lane stays held until completion).

Added `tina-sqlx-bridge`, a bounded Postgres bridge around
`sqlx::PgPool`. The bridge owns the Tokio/SQLx side; Tina callers see
typed `Full`, `Closed`, per-attempt timeout, pool-acquire timeout,
SQLx/decode errors, and worker-terminal metrics.

The public first form now includes:
- `PgWorker`, `PgConfig`, `PgPoolConfig`, `InstalledPgBridge`,
  `PgCloser`, `PgMsg`, `PgRequest`, `PgResponse`, `PgValue`, `PgRow`,
  `PgError`, `PgMetricsHandle`.
- `Execute`, `FetchOne`, `FetchMany`, and `Transaction` request shapes
  with bounded row caps.
- typed helper calls (`execute_call`, `fetch_one_call`,
  `fetch_many_call`, `transaction_call`) and `PgOutcomeExt::classify`.
- opt-in DB-side cancel-on-timeout via a sidecar pool and
  `pg_cancel_backend`, documented as best-effort rather than guaranteed
  query death.
- wider value support for bool, integers, floats, text, bytea, UUID,
  JSON/JSONB, NUMERIC, DATE, TIMESTAMP, and TIMESTAMPTZ, including
  typed NULL helpers so NULL does not silently infer the wrong type.

The Postgres counter specimen now drives the bridge with host
`ThreadedRuntime::call_blocking`, keeping the example as a SQL script
instead of a fake Driver isolate.

### Phase 063 Native Database First Form

Added `tina-sqlite-bridge`, a serial SQLite bridge around one
`rusqlite::Connection` on one blocking std thread. The first form is
deliberately small: one connection, `max_in_flight = 1`,
`pending_reply_capacity = 1`, autocommit only, explicit pragmas and
busy timeout, bounded buffered rows, late-result truth, metrics, and
typed errors for admission, timeout, response cap, busy/constraint/I/O,
SQLite, and internal faults.

Polish added `execute_call` / `query_call`, row/value accessors, common
Rust value conversions without silent `u64` wrapping, and a classifier
for caller-owned retry. `specimen_sqlite_counter` now uses the bridge
and the host `call_blocking` path; demo modes cover constraint,
timeout, closed, invalid, and retry surfaces.

- Added `SqlitePressureReport` and `SqliteMetricsHandle::pressure_report`
  so callers can observe the bridge as a pool: `capacity`, `leased`,
  `available`, `waiters`, `full_count`, `busy_count`, `high_water`.
- Added admission test proving the serial pool shape:
  `pressure_report_reflects_serial_pool_shape`.

### HTTP body streaming and backpressure

Native HTTP/1 now has production-shaped server response streaming:
`BodyMetrics`, `BodyPressureReport`, `IterBodySource`, typed
`HttpResponse::stream_known_length`, typed
`HttpResponse::stream_chunked`, and response-side chunked
transfer-encoding. The connection isolate pulls chunks on demand and
tracks high-water / body I/O error counters instead of buffering the
whole response.

The body-streaming specimen demonstrates the blessed shape:
`IterBodySource` plus `stream_known_length` for fixed length, and a
chunked route for unknown length. Follow-up phases later closed the basic
HTTP/1 gaps: request-side chunked bodies, client-side chunked response
decoding, and source cancellation on abandoned wire. Periodic metric emission
remains future capacity/observability polish.

### Native HTTPS first form

Native HTTP can now run over rustls-backed TLS through Tina-owned rails:
`HttpsListener`, typed startup (`HttpsReady` /
`HttpsStartupError`), `HttpTarget::Https`, `TlsTrustRoots`,
`HttpHostPolicy`, and transport-shaped client errors that preserve TLS
name/cert/handshake/I/O truth. Tests cover listener startup failures,
client Host/SNI/default-host behavior, untrusted roots, pool-over-HTTPS,
and simulator TLS replay. `specimen_native_https` compares a Tina HTTPS
listener with a Tokio + tokio-rustls version over the same scripted
client.

### Capacity modeling first form

Added Tina capacity vocabulary:
`tina::capacity::{CapacityMode, CapacityPolicy, CapacitySurfaceReport}`
and `tina_runtime::capacity::{CapacitySummary, SurfaceAssertion,
format_discovery_line}`. Worker-pool waiters and pending replies can
emit capacity reports with high-water and full counts, and docs now
show the tuning loop: unknown -> measured -> fixed. This is count-first
and policy-first; weighted capacity, shared shard-local budgets, and
explicit unbounded-for-now modes remain future work.

### Deadline and PendingCallSet

Added `Deadline` with explicit runtime/simulator clock truth:
`Context::now()`, `Context::deadline_after(after)`,
`Deadline::from_instant(now, after)`, and
`remaining_or_zero(ctx.now())`. There is no ambient
`Deadline::after()` shortcut, so simulator/replay code does not inherit
hidden live-clock behavior.

Added bounded `PendingCallSet<K, R>` for caller-owned `CallHandle`
storage. It uses fixed-capacity storage, rejects duplicate keys, and
keeps cleanup explicit (`remove`, `drain`, `sweep_terminal`) so it does
not hide ABA bugs. Cancellation, pool-cancel, and backpressure specimens
now use the shape.

### Bounded pool vocabulary

Added the first Tina pool primitive and vocabulary: `WorkerPool`,
`PoolLease`, `AcquireOutcome`, `ReleaseOutcome`, `CloseMode::{Drain,
Force}`, `ReleaseDisposition::{Reuse, Retire}`, pool pressure reports,
FIFO waiters, stale lease detection, and typed acquire/release helper
effects. Force close retires outstanding leases; late release is typed,
not silent. Follow-on work turned HTTP keepalive into the first real
consumer.

### Phase 064 Service Bootstrap And Fanout Ergonomics

Round-4 specimen follow-up shipped the boring helper wins and recorded
design notes for the dangerous ones. Landed pieces include
self-address-at-registration for single-shard runtimes, bounded
pending-reply drain helpers, visible-input scatter/gather conventions,
and specimen cleanups using `stop_with(report)` / `observe_result`
instead of host side-channels. The design notes deliberately left
pipeline sugar, generic scatter/gather, flat reqwest continuation
sugar, work-settled helpers, and cross-isolate paired registration
unshipped until a real caller proves the shape.

### Phase 062 Specimen Round 2 Ergonomics

Round-2 specimen work produced and applied the low-risk helpers:
`ThreadedMultiShardRuntime::observe_result`, `HostBurstOutcomes` and
`try_send_outcome`, `send_observed_until`, `SingleCallGate`, and
`ReqwestOutcomeExt::classify`. Sharded, rate-limited, and retrying
outbound HTTP specimens were migrated so the helpers are the copied
shape. Self-address multi-shard parity, generic scatter/gather, and
reqwest flat continuation sugar remain evidence-gated.

### Phase 076 Server-Side HTTP/1.1 Keepalive

`tina_http::HttpListener` / `HttpConnection` can now serve multiple
sequential requests on one TCP/TLS stream. Opt in with
`HttpLimits::keepalive_idle_timeout = Some(d)`; `None` keeps the
legacy one-request-per-connection behavior.

What stays explicit:
- HTTP/1.0 default close, explicit `Connection: close`, parse errors,
  service-call errors, and short known-length streaming responses all
  force close after the response.
- Between requests, the connection waits up to
  `keepalive_idle_timeout` for the next request head. Stale head
  deadlines are generation-tagged and ignored.
- No pipelining: per-request reset drops read-ahead bytes between
  iterations.

Nine integration tests in `tina-http/tests/server_keepalive.rs` cover
sequential reuse, close intent, idle timeout, default one-shot behavior,
slow-loris timeout on a later request, per-request body caps, and mixed
GET/POST traffic on one socket. The final test drives the native
listener through `tina_http::build_keepalive_pool` and asserts one
server `TcpAccept` across the whole script. `specimen_outbound_http`
now uses the same pooled keepalive client shape against a
keepalive-enabled Tina listener.

### Phase 073 Pool Consumers

`tina-http` now ships a real keepalive pool consumer of the
`WorkerPool` vocabulary. One TCP (or TLS) connection serves many
sequential requests; the pool exposes acquire / release / retire /
close and a pressure report.

What got shorter:
- An integration-test client that issues three requests against the
  same origin shares one TCP accept on the server side. (The
  user-facing wins of keepalive — fewer handshakes per workload —
  apply when there is a keepalive-capable server to talk to. A
  user-facing specimen is gated on the planned server-side keepalive
  work.)

What stayed explicit:
- Origin keying. `OriginKey` carries scheme + `SocketAddr` + (HTTPS:
  SNI server name + the DER trust roots stored verbatim — no
  fingerprint hash, no collision risk). A pool's connections are
  pre-bound to one `HttpTarget` at registration so cross-origin
  reuse is impossible at the connection-isolate level: the only
  way to reach a different origin is to register a different pool.
- Caps. `PoolConfig::new(capacity, max_waiters)` sizes resources
  and parked callers; the pool isolate's mailbox size is a separate
  bound (size to `>= max_waiters + burst`). The connection isolate's
  mailbox sizes only its own continuations.
- Reuse vs retire. The connection's `KeepaliveOutcome::Request`
  carries `must_retire`. Set when the server returned `Connection:
  close`, the peer EOF'd, or any connect/write/read error fired.
  **Recommended consumer pattern: always release `Reuse`.** The
  connection isolate self-heals — it has already dropped the bad
  transport and will reconnect on the next request. Releasing
  `Retire` permanently removes the slot and drains pool capacity if
  done on every must_retire; reserve it for the rare "the
  connection isolate itself is suspect" case (panic, observed
  protocol violation).
- Deadlines. `KeepaliveConnectionMsg::Request { request_timeout }`
  is the wall-clock budget enforced inside the connection via a
  generation-tagged `sleep`. Pass `Deadline::remaining_or_zero(now)`
  from a caller-owned deadline to propagate budgets honestly across
  hops. No hidden retry, no hidden queue.
- Close modes. `CloseMode::Drain` blocks new acquires and lets
  outstanding leases return normally; `Force` retires outstanding
  leases — late releases get `PoolClosed`.
- Shutdown. `WorkerPool` does not own connection-isolate lifetime.
  `build_keepalive_pool` returns a `KeepalivePoolHandles` bundle
  with the pool address *and* the connection addresses; send
  `KeepaliveConnectionMsg::Stop` to each connection after pool
  close so the underlying transports are closed and the isolates
  exit. Without this, transports leak past pool close.

New surface in `tina_http`:
- `OriginKey`, `KeepaliveConnection`, `KeepaliveConnectionMsg`,
  `KeepaliveOutcome`, `KeepaliveConnAddr`, `KeepalivePoolHandles`,
  `build_keepalive_pool`.
- `encode_keepalive_request` — sibling of the unchanged
  `encode_request` for the keepalive caller (omits the
  `Connection: close` header). The original `encode_request`
  signature is unchanged; this is a pure addition.

Sixteen integration tests live in `tina-http/tests/keepalive_pool.rs`,
plus an HTTPS-keepalive smoke (`keepalive_tls_smoke.rs`) and an
isolate-level DST replay (`dst_keepalive.rs`):

- sequential reuse counted via TCP accept count
- cross-origin isolation (two pools, two accept counts)
- stale server-close → `must_retire = true`, reconnect on next call
- acquire `Full`, acquire-call-timeout & acquire-cancel reclaim of
  waiter capacity
- request timeout marks must_retire; **always-Reuse** keeps pool
  capacity stable; an explicit-Retire test shows the pool removes
  the slot when asked
- Drain blocks new acquires; Drain-with-parked-waiters settles
  waiters as Closed; Force marks outstanding leases stale; Stop
  after Force closes the underlying transport
- HTTPS keepalive shares one TLS handshake across three sequential
  requests
- multi-slot pool (capacity 2) serves two concurrent acquires with
  two TCP accepts; `available` returns to 2 after release
- `OriginKey` distinguishes SNI / trust-root identity
- `request_timeout` correlates with wall-clock duration (proves the
  variable controls the failure)
- `PoolPressureReport` shape across capacity / `Full` / closed
  transitions
- DST replay: two passes against the same scripted TCP config
  produce identical trace fingerprints

The legacy capacity-1 `HttpConnectionPool` is kept as the first
form for one-request-per-connection admission; it remains useful
for callers that do not need keepalive.

### 066A hostile-review fixes

Round of correctness, vocabulary, and proof fixes on top of 066A
following an independent review. The first-form `CallHandle` /
`cancel_call` API surface is unchanged for the common path; the
deltas below tighten classification, close half-work, and add the
plan-mandated proof tests.

**Cross-shard cancel — typed rejection, not silent wrong-result:**
- `CallHandleShared` now stamps `shard_id` on dispatch alongside
  `call_id`. `dispatch_cancel_call` checks the stamp and returns
  the new `CancelOutcome::WrongShard` if the cancel runs on a
  different shard than the originating one.
- `CancelOutcome::NotDispatched` removed — the only reachable path
  to it (cancel batched ahead of its own `call_cancelable` effect
  in one isolate handler) was both rare and a wrong-result hazard
  (cancel returned `NotDispatched` while the call still ran). The
  cancel-dispatch path now `expect`s the `call_id` stamp; user code
  that batches cancel before call panics loudly instead.

**Cause-aware late-reply classification:**
- The recently-cancelled ring now stores `(CallId, CancelCause)`,
  not just `CallId`. Late callee replies for cancelled / timed-out /
  owner-stopped / runtime-stopped calls surface with the matching
  cause-specific rejection reason instead of the generic
  `NoPendingCall` / `CallerClosed`.
- Two new helper enums of variants: `CallReplyRejectedReason` and
  `DeferredReplyRejectedReason` each gain `CallerTimedOut`,
  `OwnerStopped`, `RuntimeStopped`. Stable hashing tags and
  `tina-tracing` string names extended.
- Timeout, owner-stop, and shutdown all record into the ring with
  their respective cause; previously only explicit `cancel_call`
  did, leaving the timeout path classifying late replies as
  generic `NoPendingCall`.

**Runtime shutdown wires `RuntimeStopped`:**
- `cancel_in_flight_calls_for_shutdown` now emits
  `CallCancelled { RuntimeStopped }` for pending isolate calls and
  transitions any caller-held `CallHandle` to `Cancelled` state. A
  user holding a handle while the runtime shuts down sees
  `state() == Cancelled`, not a forever-`Pending` lie.
- `CancelCause::CallerTimedOut` and `CancelCause::RuntimeStopped`
  were dead variants in 066A; now both are emitted by the runtime.

**Owner-stop late-reply distinct from explicit cancel:**
- Owner-stop cleanup now closes captured deferred slots with
  `DeferredReplyRejectedReason::OwnerStopped`. Previously it
  conflated owner-stop with `CallerCancelled`, which broke the plan's
  "trace facts must distinguish" rule (owner-stop and explicit
  cancel must stay distinct in the trace).

**`PressureSummary` extension is additive:**
- New counters `reply_rejected_caller_timed_out`,
  `reply_rejected_owner_stopped`, `reply_rejected_runtime_stopped`.
- `Display` format now appends the new keys after the pre-066
  `path_full=` / `shard_closed=` entries — pre-066 positional
  matchers continue to find their fields at the same offsets.

**API surface tightening:**
- `CallHandleShared::set_call_id`: reverted to a loud
  `assert!`-on-double-stamp instead of the 066A first-stamp-wins
  + `eprintln!`. The two `set_call_id` sites in the runtime are
  mutually exclusive; the path is dead defense and should panic.
- `CallHandleShared::set_shard_id` (new) follows the same shape.
- `CallHandle` `must_use` message clarifies that drop is a no-op
  and the call runs to completion.

**Tests added (Plan Rocks 2/3/5 proof items):**
- `late_reply_after_cancel_surfaces_specifically_as_caller_cancelled`
  (runtime + sim) — asserts the *specific* `CallerCancelled` reason
  and asserts the absence of the generic `CallerClosed` /
  `NoPendingCall`. Catches a regression that removes the cause-aware
  ring without breaking the test (the previous assertion accepted
  either, so the ring could have been silently removed).
- `double_cancel_returns_already_cancelled` (sim) — drives the
  `AlreadyCancelled` outcome through dual handles wrapping the same
  shared cell via `runtime_internal`. User code can't reach this
  with a `!Clone` move-only handle; the runtime can (e.g. owner-stop
  then a queued cancel effect dispatches).
- `late_reply_after_timeout_surfaces_as_caller_timed_out` (sim) —
  pins the new `CallerTimedOut` rejection reason and asserts the
  trace does NOT bleed `CallerCancelled` into a timed-out scenario.
- `owner_stop_cancels_pending_handles_and_classifies_late_replies`
  (sim) — full Rock 5 proof: 3 pending calls, owner stops, each
  handle transitions to `Cancelled`, trace records 3 ×
  `CallCancelled { OwnerStopped }` plus 3 ×
  `DeferredReplyRejected { OwnerStopped }`, no late reply reaches
  the owner's translator.
- `cancel_admit_cycle_does_not_leak_capacity` (sim) — Rock 3 proof:
  32 cancel/admit cycles produce 32 dispatches and 32
  cancellations, no `Full` rejection appears.
- `cross_shard_cancel_is_rejected_with_wrong_shard` (sim) — H1
  regression test: a handle stamped with a foreign shard id returns
  `CancelOutcome::WrongShard`, not silent `AlreadyCompleted`.

**Existing tests rewritten:**
- The 066A `late_reply_after_cancel_is_visibly_rejected` test
  accepted either `NoPendingCall` *or* `CallerCancelled`, which let
  the cause-aware ring be silently removed. Replaced with a
  specific-reason assertion.
- The 066A `double_cancel_returns_already_cancelled` test punted on
  exercising the `AlreadyCancelled` outcome with a comment about
  move-only handles. Now actually tests it.
- Three `tests/deferred.rs` and one `tests/consumer_api.rs` test
  updated to assert the new `CallerTimedOut` rejection reason
  instead of the old `CallerClosed`.

**Tokio side of `specimen_cancellation_chain` invariant relaxed:**
- 066A asserted `replies_after_cancel == 0` strictly; that holds for
  the current worker shape (`sleep().await; i`) but tripwires any
  refactor that adds an await between sleep and the reply path.
  Now asserts the loose `replies_before + after <= FANOUT`.
- Tina side of the example now polls the trace until rejection
  count converges before snapshotting, with a 2s safety budget.
  Removes the slow-CI flake where workers' late replies hadn't
  arrived by the time `result.wait` returned.

### Cancellation: first-form `CallHandle` and `cancel_call`

**Breaking trace surface changes** (downstream consumers — lint
your matches):
- `PressureSummary::Display` format adds a `cancelled=N` field. Log
  scrapers matching the old `reply[no_pending=N path_full=N
  shard_closed=N]` shape need updating.
- Stopping an isolate with pending calls used to settle as
  `CallCompletionRejected { RequesterClosed }` when the worker's late
  reply arrived. It now emits `CallCancelled { OwnerStopped }` at the
  moment of stop, and the worker's late reply hits
  `CallReplyRejected { NoPendingCall }` (or
  `DeferredReplyRejected { CallerCancelled }` for captured slots).
  Trace assertions on the old shape fail loudly.

API additions:

- `tina::CallHandle<R>` (move-only, `!Clone`, `#[must_use]`) plus
  `tina_runtime::call_cancelable(addr, msg, t).then(...)` returning
  `(Effect, CallHandle<R>)`. The old `call_with_handle(...)` spelling
  remains as deprecated compatibility pending a later scrub.
- `tina_runtime::cancel_call(handle).then(...)` closes the wait,
  reclaims caller-side capacity, and emits
  `RuntimeEventKind::CallCancelled { call_id, cause }`. Late callee
  replies surface as `CallReplyRejected { NoPendingCall }` or
  `DeferredReplyRejected { CallerCancelled }` — visible truth, not
  silent loss.
- `CancelOutcome` (`Cancelled` / `AlreadyCompleted` / `AlreadyCancelled`
  / `NotDispatched`) is `#[must_use]`. `CancelCause` distinguishes
  `CallerCancelled` / `CallerTimedOut` / `OwnerStopped` /
  `RuntimeStopped`.
- Stopping an isolate with pending calls now proactively cancels them
  with `CallCancelled { OwnerStopped }` instead of waiting for late
  replies to bounce as `CallCompletionRejected { RequesterClosed }`.
- New trace shapes: `CallKind::CancelCall`,
  `CallReplyRejectedReason::CallerCancelled`,
  `DeferredReplyRejectedReason::CallerCancelled`. Stable hashing,
  `tina-tracing` event names, and `PressureSummary`
  (`reply_rejected_caller_cancelled`) all extended.
- `PressureSummary::Display` format gained a `cancelled=N` field —
  log scrapers that match the old shape need updating.
- `examples/specimen_cancellation_chain` rewritten to use the new
  shape; the host now counts rejected late replies from the trace
  and the `Report` reflects that truth instead of hardcoding zero.
- Simulator parity: `tina-sim` mirrors the dispatch and owner-stop
  paths; `tina-sim/tests/cancel_call.rs` pins the public behavior
  deterministically.
- Bounded `PendingCallSet` (Rock 4) and `Deadline` value (Rock 1)
  deferred to follow-up phases; design notes recorded in
  `.intent/phases/066-cancellation-and-deadline-model/`.

### DST As A First-Class Dev Mode

- Added a "bug in a box" replay-case shape in `tina_sim::dst`:
  `ReplayCase`, `ReplayReport`, `ReplayConfig`, plus `assert_replay_case`
  / `check_replay_case` / `ReplayMismatch`. Cases pin name, seed, full
  `SimulatorConfig`, declared mailbox capacities, scenario, history,
  expected event count, and `stable_trace_hash`. Failures name the
  next decision, not just numbers, and include the case history so a
  coding agent reading only the panic can see what the case did.
- `ReplayConfig` carries the full `SimulatorConfig` (seed overridden by
  `case.seed` at run time) and a `BTreeMap<&'static str, usize>` of
  per-isolate mailbox capacities. `ReplayConfig::mailbox(role)` panics
  loudly on a missing role so the runner cannot quietly inherit a
  literal. `check_replay_case` debug-asserts that `case.name`/
  `case.seed` match `case.history` so the two source-of-truth fields
  cannot drift.
- Added `sweep_seeds` for hand-cranked deterministic seed search.
  Failures return a `SweepFailure` whose `failing_case` has refreshed
  expected count/hash and is ready for `assert_replay_case`. All
  helpers accept `FnMut` runners so stateful runners are allowed.
- Added `shrink_replay_case` plus `ShrinkReport`. The shrunk case
  preserves name/seed/config/scenario/invariant, refreshes its
  expected count/hash, and prints a pasteable Display form with a
  review step.
- Rewrote `docs/tina-user-guide/08-simulation-and-dst.md` around the
  workflow (history → run twice → sweep → save bad seed → shrink →
  commit) with a copyable test skeleton, bug-report shape, and an
  explicit note that `Display` output is pasteable as readable lines
  for bug reports while the `case()` function itself is what to copy
  for code.
- Upgraded `examples/specimen_replay_dst` to be the copyable specimen:
  one saved `ReplayCase` with `Tick`/`Drain` ops where every op is
  load-bearing (deleting one changes the trace hash), declared
  mailbox capacities, runner, sweep demo, shrink demo, and
  same-seed/different-seed regression tests.
- Added `tina-sim/tests/saved_replay_cases.rs` with one
  service-shaped saved case (`burst overflow under local-send delay`)
  that pins a real `SendRejected{ reason: Full }` mailbox-pressure
  fact with exact `full_rejections` / `accepted_sends` counts
  alongside its event count and trace hash.
- Migrated the existing `remote_full_burst_is_known_edge_contract_and_replays`
  case in `tina-sim/tests/timmerhus_dst.rs` from `assert_replays` +
  bare `History` to `assert_replay_case` + `ReplayCase` so the new
  shape is the way for new DST tests, not just an alternative
  parallel API. Followed up by migrating
  `live_sim_projection_matches_topology_failure_history` and the four
  saved-seed cases in `tina-sim/tests/portable_service_dst.rs`
  (`portable_service_dst_replays_whole_service_history`,
  `portable_service_dst_replays_observed_send_full_before_persistence`,
  `baobab_dst_replays_observed_send_persistence_and_requester_stop`,
  `baobab_dst_replays_pressure_shard_failure_and_topology_truth`),
  each now pinning event count + `stable_trace_hash` alongside the
  service-projection invariants. The shrink tests still use
  `delete_shrink` + bare `History` because they exercise shrink
  itself, not pin a saved trace shape. Random-history tests in
  `dst_randomized.rs` and harness-exercising tests in
  `dst_harness.rs` stay on `assert_replays` for the same reason.
- Added ergonomic helpers driven by real-use friction:
  `ReplayCase::new(...).expecting(count, hash)` builds the case and
  the bundled `History` from one set of name+seed inputs;
  `ReplayCase::simulator_config()` returns a `SimulatorConfig` with
  `case.seed` already set so the runner is one line;
  `ReplayConfig::with_faults(...)` and `with_mailbox(role, cap)`
  replace nested struct literals; `observe_replay_case` plus
  `ReplayReport::pinned_constants()` are the blessed discovery path
  for first constants; `discover_constants` runs a batch of cases
  sharing the same `Op`/runner and prints one pasteable
  comment-headed block per case so adding or perturbing several
  related cases at once is a single `cargo test --ignored` away;
  the user guide gains a "Pick Your Op Alphabet" section that names
  the mental move.

### Phase 065 Observability First Form

- Added `tina-tracing`: an adapter crate that turns `RuntimeEvent`s and
  `LiveTopologyReport`s into structured `tracing::Event`s under stable
  targets (`tina_runtime::trace`, `tina_runtime::live`) without flattening
  typed reasons (`Full`, `Closed`, `CallerClosed`, `ReplyPathFull`,
  `MailboxFull`, `BudgetExceeded`, …).
- Added a live `TraceObserver` hook on `tina-runtime` and `tina-sim`: one
  synchronous in-line callback fired before retention, wired through
  `ThreadedRuntime`, `ThreadedMultiShardRuntime`, `LocalSystem*Builder`,
  and the `Simulator` setter. `ThreadedRuntimeConfig` stays pure data
  (`Copy + Eq`); the observer is a separate constructor parameter.
- Added byte-identical-trace property tests on both runtime and simulator
  proving a noop observer leaves the deterministic event record
  unchanged. `TraceRetention::Off` plus an observer is the new
  stream-only mode.
- Added new bridge tracing emission for `tina-sqlite-bridge`,
  `tina-tokio-bridge`, `tina-tower-bridge`, and `tina-reqwest-bridge`
  behind each crate's optional `tracing` Cargo feature. Targets follow
  the `tina_<bridge>.bridge[.call]` shape; shared `reason` strings
  (`Full`, `Closed`, `Timeout`) reuse runtime vocabulary where the
  concept matches; bridge-specific fields stay bridge-shaped
  (`request_kind`, `method`, `status`, `outcome`, `rows_changed`,
  `row_count`, `elapsed_ms`, `scope`, `detail`).
- `tina-rpc-tokio`'s pre-existing spans (`tina_rpc.bridge.call` with
  `service`, `method`, `correlator`, `result_kind`) were left
  untouched in this phase and remain on the previous shape.
  Documented in `19-tracing.md` as the one bridge whose vocabulary
  has not been harmonised yet.
- Added `docs/tina-user-guide/19-tracing.md` with the field/level/reason
  vocabulary table for runtime and bridge events; one runnable example
  (`specimen_tracing_demo`) wires the live observer end-to-end.
- Left OpenTelemetry / Prometheus mappers, span timing for runtime
  calls, cross-bridge correlator alignment, and `tina-rpc-tokio`
  vocabulary harmonisation as future observability work.

### Phase 055 Codebase Module Split

- Split the giant runtime, driver, simulator, and support files into smaller
  modules without changing public behavior, trace vocabulary, or API shape.
- Preserved the existing test/verify surface while making future feature work
  easier to review file-by-file instead of re-reading one massive module.
- Kept the split intentionally boring: module extraction only, no semantic
  cleanup mixed into the move.

### Phase 051 Ecosystem Bridge Adapters

- Completed the first bridge tranche around Tina's bounded core:
  `tina-rpc-tokio`, `tina-tower-bridge`, and `tina-reqwest-bridge`, with
  `tina-tokio-bridge` kept as the generic host/lifecycle foundation.
- Added bridge docs and specimens that name the two-runtime shape,
  preserve `Full`/`Closed`/`Timeout` outcomes, keep deadlines and ingress caps
  explicit, and document weakened DST/replay guarantees at the Tokio boundary.
- Added bridge ergonomics polish: friendlier Tower service aliases, Tower
  `Service` re-export, reqwest `install`, `send_request`, layered
  `ReqwestCallOutcome`, opt-in `flatten_outcome`, and
  `ReqwestOutcomeExt::classify` for caller-owned retry loops.
- Left SQLx/Postgres, AWS SDK, smol, common bridge setup extraction, and bridge
  crate folder layout as future bridge/database work.

### Phase 061 Bounded Deferred Replies and Service Fanout

- Added typed deferred reply capture so a service can accept a call, hold the
  caller reply slot across later messages, and answer after fanout or worker
  completion without hiding the pending capacity.
- Added bounded pending-reply accounting, explicit full/closed/rejected trace
  outcomes, and runtime/simulator proofs for slot capture, reply, drop,
  caller-close, requester-shard close, stop cleanup, and panic cleanup.
- Tightened the public API so deferred reply capture derives its reply type
  from the running isolate context, with a runtime type guard retained at the
  erased boundary.
- Added Specimen scatter/fanout examples that use deferred replies as ordinary
  Tina state instead of side-channel host polling.

### Phase 059 Specimen Ergonomics Harvest

- Added typed isolate result waiters through `stop_with(...)` and
  `observe_result(...)`, retiring several `Arc<Mutex<_>>`/atomic host
  side-channel patterns in specimens.
- Added per-call reply aliases and first-form TCP loop helpers so common TCP
  continuation enums read closer to the runtime call that produced them.
- Added capacity diagnostics and pressure-report conventions for examples and
  tests, including reusable mailbox budget naming.
- Added small HTTP router ergonomics, stateful-router support, and bridge
  specimen structure cleanup so examples show the Tina-shaped code first.

### Phase 058 Tina RPC Usability Layer

- Added a typed Tina RPC service layer on top of the framed-call seed:
  service-handler topology notes, generated service dispatch, typed client
  stubs, and a `#[tina_rpc::service]` authoring surface.
- Added `tina-rpc-tokio` so Tokio callers can await Tina RPC calls through a
  bounded bridge without pretending cancellation, full, or timeout disappear.
- Added RPC usability tests and Specimen typed-RPC notes that keep capacity,
  serialization limits, local-vs-wire outcomes, and retry policy explicit.

### Phase 053 Sharded Service Primitives

- Added sharded placement primitives with stable key ownership over an
  explicit ordered shard list, visible placement reports, and owner-side
  wrong-shard validation.
- Added first-form sharded table/counter patterns, service-table helpers,
  reply adapters, bounded scatter/gather vocabulary, partial aggregate
  outcomes, and hot-key pressure reporting.
- Added live and simulator/DST proofs for placement determinism, wrong-shard
  rejection, closed targets, aggregate timeout, and bounded collector pressure.

### Phase 052 Tina Framed Calls First Form

- Added a Tina-native framed request/reply probe with length-prefixed TCP
  frames, service/method names, request ids, bounded in-flight calls, typed
  full/closed/timeout/error outcomes, client state machine, and registry.
- Added simulator and live proofs for framed-call behavior, including visible
  overload and close/cancel semantics.
- Added Specimen RPC comparison coverage so the first-form RPC surface is tested
  as code people actually read, not only as crate internals.

### Phase 048 Native HTTP Service Stack

- Added Tina-owned HTTP/1.1 first-form support: parser/framing, request and
  response types, connection/listener isolates, bounded limits, visible
  overload, graceful close paths, and small routing helpers.
- Added native HTTP client and bounded pool first forms, plus examples showing
  when Tina can own HTTP directly instead of using Tokio/Axum as the edge.
- Added parser-level DST and documented the remaining larger slices:
  production streaming bodies and full listener/connection simulator replay.

### Phase 047 Specimen Ergonomics Harvest

- Added the first Specimen comparison suite discipline and harvested its obvious
  Tina papercuts into primitives instead of leaving them as example folklore.
- Added bounded observation handles, stable trace/fingerprint support, easier
  single-shard defaults, mailbox/reply-slot sizing guidance, sequenced-call/TCP
  helper docs, bridge lifecycle cleanup, and runtime surface alignment.
- Updated Specimen findings/READMEs so resolved pain moved out of the active
  complaint list and remaining pain became future work.

### Runtime: TCP/UDP close cancels pending lanes instead of failing with `ResourceBusy`

- `tcp_close_stream`, `tcp_close_listener`, and `udp_close_socket` no
  longer fail with `CallError::ResourceBusy` when a read/write/accept/
  recv is pending. Close cancels the pending op and closes the
  resource. The pending caller's continuation never fires (silent
  cancel — same shape as isolate-stop with pending calls).
- New `CallCompletionRejectedReason::ResourceClosed` trace variant
  keeps each silent cancellation observable.
- Live driver pushes cancelled call ids onto `cancelled_by_close`;
  the runtime layer drains them via the new
  `RuntimeDriver::take_cancelled_by_close` hook and drops matching
  `in_flight_calls` plus translators. Without this the worker would
  spin on ghost calls.
- Simulator gets a matching `cancel_backend_calls_for_resource`
  helper that drains its pending queues, in-flight calls, and
  translators. `run_until_quiescent` no longer hangs after
  close-while-pending.
- Tests previously pinning `ResourceBusy` for close-while-pending now
  assert the clean-cancel-and-close behavior. `examples/FINDINGS.md`
  is updated to mark the issue fixed.

### README Rewrite and Forward Roadmap Phases (Native DB / HTTP/2 / gRPC)

- Rewrote the project `README.md` to match the conventions of mature
  framework READMEs (Tokio, Mbanugo's Tina/Odin, Seastar): descriptive
  lead, property bullets, one canonical TCP-echo example, an
  architecture section with ASCII diagram and crate table, a
  deterministic-simulation section as one section among several, a
  quickstart, a documentation table, an honest status/limits paragraph,
  and a prior-art table that names the Rust neighbors (madsim, turmoil,
  ambitious, joerl, lunatic, glommio, monoio, loom, shuttle).
- Added forward-roadmap phases 055 (native database, Postgres via
  `postgres-protocol` plus SQLite via `rusqlite`), 056 (native HTTP/2),
  and 057 (native gRPC), with an "adopt-don't-rebuild" discipline note
  naming the sync codec crates Tina borrows (`httparse`,
  `postgres-protocol`, `rusqlite`, `hpack`, `prost`, `rustls`,
  `tungstenite`).

### Phase 048a Native HTTP Service Stack — Server First Form

- Added a new workspace crate `tina-http` containing the HTTP/1.1
  server first form: `parse` module wrapping `httparse` with typed
  `RequestParseError` variants (`BadRequestLine`, `HeadersTooLarge`,
  `UnsupportedTransferEncoding`, `InvalidContentLength`, `BodyTooLarge`,
  `UnsupportedRequestTarget`, `UnsupportedHttpVersion`,
  `HeaderReadTimeout`); `connection` module hosting an
  `HttpConnection<S: Shard>` isolate that reads, parses, accumulates a
  `Content-Length` body, calls a service isolate via `tina_runtime::call`,
  writes the response, and closes; `listener` module hosting an
  `HttpListener<S: Shard>` isolate that binds, accepts, and spawns one
  connection per accept; `types` module with `HttpRequest`,
  `HttpResponse`, and `HttpLimits` (including `header_read_timeout`).
- Pinned the connection isolate's `CallOutcome` -> HTTP status mapping
  with an exhaustive match on every `CallError` variant: `TargetFull`
  -> 503, `Timeout` -> 504, `TargetClosed` -> 500, every other variant
  -> 500. Adding a new `CallError` in `tina-runtime` is now a compile
  error in `tina-http`.
- Added a slow-loris guard: an in-flight `sleep(header_read_timeout)`
  fires concurrently with the head-read; if parsing has not completed
  by the deadline, the connection isolate stops and the runtime drops
  the stream. Documented the runtime's `tcp_close_stream`-while-read-
  pending limitation in `examples/FINDINGS.md`.
- Fixed the listener `Stop` race: a queued `Accepted(Ok)` arriving
  after `Stop` took the listener now closes the orphan stream instead
  of panicking on `self.listener.expect(...)`. Removed the dead
  `build_close_child` helper.
- Fixed RFC 7230 §6.1 parsing: `Connection: close, keep-alive` (in any
  order) now correctly reports `connection_close = true`. Fixed RFC
  7230 §3.3.2 parsing: conflicting `Content-Length` values now map to
  `400 Bad Request` (was incorrectly `411 Length Required`).
- Switched the parser's per-call header buffer to a stack-allocated
  array for the common case (`max_headers <= 64`), with a heap fallback
  for larger configurations.
- Switched the connection isolate's partial-write loop to a
  `drain(..count)` + `clone()` pattern (matching `tcp_echo.rs`) instead
  of slicing-with-offset, bounding total response-write copies to
  O(N) for an N-byte response.
- Added an `specimen_native_http` paired Tokio-vs-Tina comparison: axum
  on the Tokio side, `tina-http` on the Tina side, identical scripted
  client, asserts byte-equivalent outcomes. The Tina HTTP server runs
  with no Tokio runtime in the process — first Specimen comparison where
  Tina speaks the wire protocol itself.
- Added the 048 plan's "Slices", "User-Facing Shape (First Form)",
  "Crate Placement", and "Coordination With 047" sections, plus an
  honest split of rock 5 into 5a (typed-mapping overload visibility,
  shipped in 048a) and 5b (admission limits + metrics + wire-level
  Full coverage, deferred to 048b alongside the connection pool).
- Filed three new entries in `examples/FINDINGS.md` capturing real
  pain surfaced by 048a: the `#[tina_runtime::isolate(shard = S)]`
  macro does not accept a generic shard parameter (forces hand-rolled
  `Isolate` impls); `tcp_close_stream` rejects with `ResourceBusy`
  while a `tcp_read` is pending on the same lane (no
  `tcp_cancel_read` primitive exists, blocking the slow-loris path's
  ability to write 408 before close); wire-level `CallOutcome::Full`
  is not deterministically constructible on a single shard with the
  current API.
- 41 tests in `tina-http`: 25 unit (parser determinism, parser
  edge-cases, response encoder, exhaustive `CallError` mapping); 4 DST
  parser-replay (parser purity, corpus fingerprint stability,
  fingerprint sensitivity to limit changes, error->status mapping
  fingerprint); 6 bad-input integration (malformed line, oversized
  headers, chunked transfer encoding, oversized Content-Length,
  absolute-form target, peer close mid-request — all with follow-up
  request assertion to prove listener uncorrupted); 5 pressure
  integration (multi-read body with trace assertion, graceful
  shutdown, slow-loris timeout via deadline trace event, stop-race
  regression, wire-level 504 via a service that never replies); 1
  happy-path smoke. Plus the paired `specimen_native_http` comparison.

### Phase Baobab Production-Readiness Rails

- Added an executable readiness matrix in `tina-runtime/tests/readiness_matrix.rs`
  covering runtime-owned rails, bridge ingress, replay/DST, affinity, cost
  reporting, cancellation, backpressure, `io_uring` non-claim, and
  platform-gated Glommio comparison rows.
- Extended the canonical portable service harness with a Baobab user-service
  gauntlet that composes a TCP listener/session, Tina-owned timer, DNS, bounded
  process execution, runtime-owned file I/O, journal append, cross-shard
  isolate call, and terminal shutdown/report checks through `LocalSystem`.
- Added a live multi-shard Baobab service proof: one worker shard fails, sibling
  persisted work still completes, and calls into the failed shard surface typed
  closed/failure truth.
- Expanded portable service DST with saved-seed histories for observed-send +
  persistence + requester stop, pressure + shard failure + topology truth, and
  deletion shrinking over a requester-stop history.
- Added Baobab DST histories for persistence restart/corrupt/truncated recovery
  and bridge timeout/retry/shutdown behavior.
- Upgraded `make portable-runtime-cost` from shape-only rows to local timing
  rows over real Tina smoke paths for local send, live ingress, cross-shard
  send, isolate call, plus local TCP loopback, while keeping unmeasured
  TLS/bridge rows explicit and labeled "not benchmark".
- Hardened the Baobab TCP service proof and cost smoke so both use framed or
  accumulated TCP reads instead of assuming one read is one request.
- Folded the Baobab readiness gate into the single `make verify` command:
  readiness matrix, portable service, LocalSystem rail/backpressure e2e tests,
  service DST, bridge cancellation model/e2e, and cost smoke.

### Phase Portable Local Runtime Completion

- Added a canonical public-path portable service harness using
  `LocalMultiShardSystem`: configure budgets, register router/workers, route
  by key to shard-owned workers, perform journal append before reply, shut down,
  assert terminal topology/report truth, and replay durable journals.
- Fixed isolate-call continuation semantics in both the live runtime and
  simulator: runtime-owned call completions and observed-send completions now
  preserve the original isolate-call context, so a service can receive a call,
  do runtime-owned I/O/persistence, and reply afterward.
- Added direct live and DST proofs for observed-send continuation: accepted
  audit send outcomes can drive the original call reply, accepted audit sends
  eventually run the target side effect, and full audit sends return a typed
  failure without mutating the audit target or losing the caller reply.
- Added visible placement/backpressure proofs: wrong key-to-shard routing
  rejects before work runs, unknown shard registration returns
  `ThreadedRuntimeError::UnknownShard`, and busy retry uses a Tina-owned timer
  before returning a typed rejection.
- Completed the user-facing budget manifest path with builder knobs for DNS,
  TLS, process, signal, and shutdown drain timeout, plus terminal topology
  assertions that the configured shape survives shutdown reporting.
- Added service-level DST in `tina-sim`: saved-seed whole-service histories
  over cross-shard call, observed-send continuation, journal append, worker
  stop/closed outcomes, observed-send full before persistence, replay equality,
  invariant checks, and deletion shrinking.
- Added `make portable-runtime-cost` as an explicit cost-smoke command and
  folded the service harness, budget manifest, service DST, bridge cancellation
  model, and cost smoke into the project verification path. The cost command is
  labeled "local machine / not benchmark" and makes no speed claim.

### Phase Blue Whale

- Added `AffinityStatus` and shard/core ownership reporting to live topology:
  `LiveShardReport` now exposes worker name, worker thread id, configured
  core, optional observed core, and affinity status. (This first shipped as
  advisory reporting only; hard pinning landed later — see Hard Shard Pinning
  above.)
- Added `configured_core` to `LocalSystemConfig` and
  `ThreadedRuntimeConfig`. Multi-shard local systems treat it as the core for
  the first shard in stable order, with later shards on contiguous OS CPU ids.
- Added `PreallocationConfig` for setup-time runtime-owned metadata reserves:
  isolate entries, child records, supervisors, trace events, in-flight calls,
  translators, isolate-call metadata, driver-completion scratch, and per-step
  round scratch. User payloads, erased reply/message boxes, durable buffers,
  and backend-owned completion slots remain explicit non-claims.
- Added `remote_inbound_drain_budget` so a live destination shard harvests a
  bounded number of remote envelopes before giving local runtime work a turn.
  Cooperative isolate fairness remains one delivery chance per isolate per
  runtime step; Tina still does not preempt a synchronous handler.
- Exposed that fairness budget in `LiveShardReport`, added builder helpers for
  `LocalSystem`, and made low-level `ThreadedRuntime` reject zero budgets
  before starting a worker.
- Tightened fake-driver contract tests with a TCP-ish pending resource path
  that proves pending-call and table-owned resource reporting clear on cancel.
- Added a checked Blue Whale/Seastar principles table as a Rust test covering
  per-core ownership, thread pinning, bounded queues, preallocation, allocator
  locality, backend shape, NUMA, scheduling groups, DST/replay, and Tina's
  non-`await` user model.
- Added combined e2e coverage for advisory core ownership, preallocation
  posture, bounded remote drain budget, and live cross-shard isolate-call
  behavior. `make verify` passes.

### Phase Sadie's Ward

- Added typed worker-held and pending-driver-call accounting alongside the
  existing table-owned count: `LiveShardReport::worker_held_resource_count`
  and `pending_driver_call_count`, plus
  `LocalSystemShutdownReport::remaining_worker_held_resource_count`,
  `remaining_pending_driver_call_count`, and `unclean_reason`.
- Added `ShutdownUncleanReason` (`#[non_exhaustive]`) with a deterministic
  priority order: runtime error > failed shards > not-closed > worker-held
  remaining > pending-call remaining > table-owned remaining.
- Added `shutdown_lane_drain_timeout` to `LocalSystemConfig` and
  `ThreadedRuntimeConfig` (default `DEFAULT_SHUTDOWN_LANE_DRAIN_TIMEOUT`,
  100 ms). Per-shard shutdown drains lane workers up to that budget, then
  returns; stuck work surfaces in the terminal report rather than blocking
  shutdown forever. The Betelgeuse TCP shutdown drain replaces its
  64-step constant with the same deadline.
- Added Unix `SIGINT`/`SIGTERM` capture via `signal-hook` flag handlers
  (no Tokio dependency, no async signal task, no custom unsafe handler);
  flagged through to runtime-owned signal completions parked by
  `signal_wait`. Added `os_signal_capture_supported()` so non-Unix is an
  explicit unsupported capability instead of a silent no-op.
- Added `LiveShardReport::dns_lane`, `tls_lane`, `process_lane`, and
  `signal_lane` so every bounded lane capacity is reachable from the
  topology snapshot.
- Hardened threaded `try_send` (single-shard and multi-shard) so a
  `Failed` shard rejects ingress immediately with `WorkerStopped` instead
  of relying on the bounded sync channel to observe `Disconnected`.
- Changed live `trace()` to return a `TraceSnapshot`: default observation now
  keeps retained events even after a shard failure, while `complete_trace()`
  remains the strict all-shards-or-error path. Terminal reports retain partial
  trace instead of going blind when shutdown/failure is exactly what the user
  needs to inspect.
- Added `shutdown_report()` on the low-level threaded runtime owners so users
  can get terminal error, topology, resource counts, and retained trace
  together instead of losing report shape on driver shutdown failure.
- Added per-lane unit tests for the count rules, a unit test that the
  storage and Betelgeuse TCP shutdowns return inside their budget when a
  worker is stuck, a real `raise(SIGINT)` test that reaches a parked
  `signal_wait`, a live `LocalSystem` test that a failed shard rejects
  ingress while a healthy shard keeps running, low-level tests that retained
  trace survives sibling worker failure, DST combining timeout/remote-full/
  closed-target outcomes, and a live `LocalSystem` topology test that exposes
  every new field through the public API.

### Phase Jan de Quay

- Added native bounded live DNS support behind `dns_lookup`, with visible
  `DnsFull`, `DnsClosed`, timeout, queued cancellation, and tombstoned
  already-started resolver work.
- Added native rustls-backed TLS support behind `tls_connect`, `tls_read`,
  `tls_write`, and `tls_close`, with `TlsStreamId`, one pending operation per
  TLS stream, certificate/name/handshake/I/O/full/closed/timeout outcomes, and
  simulator TLS scripts for semantic replay.
- Added richer runtime-owned path operations: `path_metadata`,
  `rename_replace`, `remove_file`, `read_dir`, and `sync_parent`, with typed
  missing/unsupported/uncertain/I/O outcomes where platform behavior matters.
- Added runtime-owned shutdown notification through the signal rail so a live
  runtime can deliver `"shutdown"` to waiting isolates before the worker stops.
  Raw OS signal capture remains a non-claim.
- Updated `RuntimeCapabilities` so DNS, TLS, path/storage, process, UDP, and
  signal rails report their actual supported/lane-backed/poll-backed/
  completion-backed/tombstoned/drained shapes.
- Expanded `LocalSystem` e2e coverage for DNS, TLS, runtime-owned file/path
  operations, shutdown notification, and composed resource workloads.
- Expanded simulator/DST resource histories over DNS, TLS, path, signal,
  process, UDP, and TCP combinations, with replay and delete-shrink coverage.

### Phase Funkishus

- Added `RuntimeCapabilities` for runtime-owned resource families, including
  support status, execution shape, cancellation shape, shutdown shape, lane
  capacity, and durability support.
- Added runtime-owned UDP helpers: `UdpSocketId`, `udp_bind`,
  `udp_send_to`, `udp_recv_from`, and `udp_close_socket`.
- Implemented live UDP in the Tina driver with nonblocking runtime-owned
  sockets, visible truncation, same-resource receive lane ownership,
  `ResourceBusy` close/duplicate-receive behavior, and requester-stop
  cancellation.
- Added scripted simulator UDP bind/send/recv/close, loopback, truncation,
  receive capacity pressure, completion capacity pressure, and cancellation.
- Added DNS call vocabulary and typed helpers while keeping live DNS honestly
  unsupported on the current substrate. Added scripted simulator DNS success,
  failure, timeout, and bounded-lane full behavior.
- Added bounded local process execution with command-plus-args, null stdin,
  bounded stdout/stderr capture, timeout kill/reap, lane full/closed outcomes,
  and simulator parity for exit/failure/timeout/kill-uncertain paths.
- Added signal wait call vocabulary with simulator-first signal injection,
  timeout/failure/full/cancel behavior, and typed live `Unsupported` without
  installing process-global handlers.
- Kept TLS as an adapter-only capability; native TLS remains an explicit
  non-claim.
- Added a composed live proof where one `LocalSystem` service uses UDP,
  process execution, and journal append before committing durable state.
- Expanded DST over DNS/process/UDP/signal histories with replay, common trace
  invariants, and deletion shrinking.

### Naming Polish Before Funkishus

- Renamed the canonical live owner from `LocalApp` to `LocalSystem`, with
  matching `LocalMultiShardSystem`, `LocalSystemState`,
  `LocalSystemTerminalReport`, `LocalSystemShutdown`, and builder names.
- Renamed the user-facing live threaded runner from
  `BetelgeuseBackedRuntime` to `ThreadedRuntime`, with matching
  `ThreadedMultiShardRuntime`, `ThreadedRuntimeConfig`,
  `ThreadedRuntimeError`, `ThreadedTrySendError`, and
  `ThreadedSendObservedError`.
- Kept Betelgeuse as the named backend/driver implementation detail where the
  code is specifically talking about the completion backend.

### Phase Timmerhus

- Added first-class live topology reporting for the canonical local app path:
  `LiveTopologyReport`, `LiveShardReport`, `LiveQueueReport`,
  `LiveRemoteQueueReport`, and `LiveShardState`.
- Added `LocalSystem::topology()` and `LocalMultiShardSystem::topology()` so users
  can inspect shard ownership, worker names, lifecycle state, ingress capacity,
  remote queue capacity, storage-lane capacity, and honest pressure counters
  without scraping logs.
- Added terminal topology snapshots to `LocalSystemTerminalReport` so graceful
  shutdown and failed worker termination remain visible after the app owner is
  consumed.
- Kept queue pressure honest: exact depth is `None` unless exact by
  construction, measured counters are `Some(_)`, and unmeasured storage-lane
  counters are `None` instead of fake zeros.
- Named live shard worker threads as `tina-shard-{id}` and tracked
  per-shard lifecycle as `Running`, `Stopped`, or `Failed`.
- Added per-shard and source/target remote-queue metrics for
  threaded multi-shard runtimes.
- Added user-shaped live tests for topology before/after shutdown, bounded
  ingress full, bounded remote queue full, and one failed worker while another
  shard continues and then stops cleanly.
- Added Timmerhus DST coverage: a replayable topology/failure history, true
  live-vs-simulator projection comparison, mutation-after-rejection absence,
  common trace invariant checks, and deletion shrinking for the failing
  topology model.
- Hardened Timmerhus tests so normal tests pin known negative/edge contracts
  directly (`Closed` direct ingress after stop, bounded remote `Full`), while
  DST sweeps prove the random histories actually exercise `Full`, `Closed`,
  timer, and panic rocks instead of drifting back to happy paths.

### Phase Stuga

- Added `tina_sim::dst` with reusable `History`, `DstRun`, replay assertion,
  deletion shrinking, shrink reports, trace invariant suite, persistence-image
  replay helper, visible-pressure helper, and semantic projection comparison.
- Refactored randomized single-shard and multi-shard DST tests onto the shared
  harness and added an optional `TINA_DST_LONG=1` long seed sweep.
- Added harness self-tests proving replay equality, deletion shrinking,
  causality failure detection, and accepted settled-send fixtures.
- Added simulator-only `ScriptedStorageFaultConfig` for deterministic
  journal/snapshot failure, truncate, corrupt, and commit-uncertain durable
  image faults.
- Reworked persistence and TCP cancellation matrices into history-shaped DST
  runs using shared replay and invariant checks.
- Reworked bridge ingress model DST to use shared histories and deletion
  shrinking while keeping it explicitly model-only.
- Added live-vs-sim projection comparison helper and used it for oracle,
  simulator, and Betelgeuse runner parity checks.

### Phase Johan Rudolph Thorbecke

- Added a bounded live storage lane for snapshot/journal persistence work so
  persistence helpers no longer execute synchronously inside the shard worker
  on the preferred live path.
- Added `BetelgeuseBackedRuntimeConfig::storage_lane_capacity` plus
  `LocalSystem` single-shard and multi-shard builder knobs for that bounded lane.
- Added `CallError::StorageFull` and `CallError::StorageClosed` as named
  runtime-owned storage admission/lifecycle outcomes.
- Kept direct explicit-step runtime storage inline while using the bounded
  storage lane on live single-shard and multi-shard worker paths.
- Added storage-lane proofs for bounded full rejection without sleep-as-proof,
  cancellation swallowing late completions, and shutdown skipping buffered work
  that never started.
- Added `local_app_end_to_end_service` proof: multi-shard `LocalApp` ingress,
  cross-shard service routing, journal append before state apply, shutdown,
  fresh-app recovery, and recovery trace visibility.
- Added a composed live TCP plus persistence proof where a `LocalApp` service
  accepts a real TCP client, journals the payload before replying, shuts down,
  and replays the durable journal.
- Added `LocalAppTerminalReport::summary()` as trace-derived terminal
  accounting for completed, failed, rejected, abandoned, journaled, and
  recovered work.
- Pinned terminal summary accounting as zero-allocation over retained trace.
- Tightened storage-lane capacity to mean total accepted pending work, then
  proved a user-shaped `LocalApp` storage overload path where one journal
  append succeeds, the next returns `StorageFull`, and replay sees only the
  accepted record.
- Added a full live thread-per-core service proof: real TCP ingress on one
  shard, cross-shard durable journal append on another shard, cross-shard ack
  back, client reply after persistence, shutdown, and journal replay.
- Added randomized DST pressure for single-shard and multi-shard histories:
  delayed sends, timers, stop, panic, mailbox pressure, remote queue pressure,
  stale sends, unknown targets, replay equality, causal trace checks, and
  no-turn-after-stop invariants.
- Added DST pressure for persistence fault matrices, supervision plus
  persistence recovery, TCP cancellation tombstones, bridge ingress timeout
  cancellation, shrinker smoke proof, and live-vs-sim parity over send/stop
  closed-rejection semantics.

### Phase Wim Kok

- Added Tina local persistence helpers: `snapshot_commit`,
  `snapshot_load`, `journal_append`, and `journal_replay`.
- Added durable snapshot metadata with `last_journal_index`, framed journal
  records with monotonic `record_index`, and append-before-apply recovery
  semantics.
- Added explicit persistence trace events for snapshot commit, journal append,
  recovery start/finish/failure, and snapshot/journal failures.
- Added `LOCAL_PERSISTENCE_SUPPORT` so platform durability claims are visible
  instead of implied.
- Added `JournalReplayWarning::TruncatedTail` for valid-prefix replay and
  `CallError::CorruptRecord` for complete records with bad checksums or
  unreplayable record index order.
- Added `CallError::CommitUncertain` for snapshot commits where rename already
  happened but the final durability step could not be proven.
- Added simulator `DurableImage` capture/load support so durable recovery can
  be replayed from deterministic path-to-bytes state.
- Proved live `Runtime`, canonical `LocalApp`, deterministic `tina-sim`, and
  Tokio bridge recovery paths for stateful services.
- Added negative proof that failed journal append is visible and does not
  mutate user state.

### Phase Piet de Jong

- Added `tina_runtime::LocalApp` as the canonical live app owner for local
  Tina services, with single-shard and multi-shard builders.
- Added lifecycle shutdown/reporting types: `LocalAppState`,
  `LocalAppTerminalReport`, `LocalAppShutdown`, and
  `LocalMultiShardAppShutdown`.
- Added `BridgeHost::from_app(app)` so bridge-hosted services can start from
  the canonical `LocalApp` path.
- Added retry policy support with both per-attempt timeout and total policy
  deadline.
- Added direct lifecycle failure proof for worker-side failure surfaced through
  `LocalAppTerminalReport`.
- Added production-shaped local service proofs:
  `llama_http_bridge_service`, `llama_tcp_timer_service`,
  `llama_supervised_worker_service`, and `llama_sim_dst_parity_service`.
- Added a narrow performance/allocation envelope for the preferred app ingress
  path and kept broader performance claims explicit non-claims.

### Phase Jelle Zijlstra

- Added outbound TCP connect to the vendored Betelgeuse native and simulated
  I/O backends.
- Added Tina runtime-owned `tcp_connect(addr)` and trace vocabulary for
  client-side TCP streams.
- Added runtime-owned file I/O to `tina-runtime`: `FileId`,
  `FileOpenOptions`, `file_open`, `file_create`, `file_read`,
  `file_read_at`, `file_write`, `file_write_at`, `file_fsync`, `file_size`,
  `file_close`, and `mkdir`.
- Added deterministic `tina-sim` file behavior for config/snapshot/log-shaped
  workloads, including replay-visible failures for invalid file resources and
  unsupported open modes.
- Proved live and simulated outbound TCP client flows, live and simulated file
  read/write/fsync/close flows, LocalApp-hosted file services, and
  Tokio-bridge-hosted file services.
- Added future roadmap home for visible sequential workflow ergonomics without
  hiding runtime-owned suspension points or failure policy.

### Phase Dries van Agt

- Added `tina-tokio-bridge`, a narrow Tokio/Tower bridge that sends bounded
  `BridgeRequest` messages into a Betelgeuse-backed Tina runtime and waits for
  explicit typed responses with timeout.
- Added Axum integration proof plus bridge overload tests for worker-ingress
  `Full` and target-mailbox `Full`, so the bridge does not degrade bounded
  pressure into silent timeout.
- Added `TraceRetention::{Full, Bounded, Off}` to `tina-runtime`, wired it into
  `BetelgeuseBackedRuntimeConfig`, and proved bounded live trace retention.
- Renamed the live runner surface to backend-honest
  `BetelgeuseBackedRuntime` / `BetelgeuseBackedMultiShardRuntime`.
- Added a runnable `llama_bridge` example showing Tokio caller code crossing
  into Tina without async handlers or hidden unbounded queues.
- Added bridge production-shape helpers: `BridgeHost`, explicit close/health,
  metrics snapshots, bounded retry/reject policy helpers, caller-timeout late
  response accounting, and a preserved/weakened bridge capability table.
- Added bridge compile-fail guardrails for non-`Send` requests and wrong
  response shapes, plus tests for lifecycle, metrics, cancellation, timeout,
  overload, and clean host shutdown.
- Tightened bridge timeout/cancellation semantics: host-registered services
  skip cancelled queued requests before user state mutates, bridge handles can
  map requests into larger service message enums, and `BridgeHost::try_shutdown`
  can be retried after handles are dropped.
- Added `TINA_DRIVER_RUNTIME_CONTRACT` to name Tina's driver-runtime target:
  completion-shaped I/O, bounded commands, explicit cancellation, owned
  shutdown, explicit progress, deterministic simulation, no hidden executor
  tasks, and no claim of being a general async runtime.
- Rewrote the README into a shorter Tina-as-concurrency-primitive story with
  explicit inspiration links and current non-claims.

### Phase Sputnik

- Added the `tina` trait crate as the shared vocabulary layer.
- Added `Isolate`, `Effect`, `Mailbox`, `Shard`, `Context`, `Address`,
  `Outbound`, and `ChildDefinition`.
- Chose a closed `Effect` enum with per-isolate payload types.
- Added docs, compile-fail tests, and downstream-style integration tests for
  the trait surface.

### Phase Pioneer

- Added shared supervision policy types in `tina`, including restart policy,
  restart-budget accounting, and child restart classification.
- Added `tina-mailbox-spsc`, a bounded single-producer/single-consumer mailbox
  implementation.
- Proved mailbox FIFO order, boundedness, explicit `Full` and `Closed` errors,
  and no hidden overflow queue with black-box tests.
- Added Loom coverage for producer/consumer interleavings, close/send races,
  close/recv behavior, wraparound, and slot reuse.
- Added drop-accounting and allocation-accounting tests to keep mailbox claims
  narrow and evidence-backed.
- Documented the DST boundary and the runtime-enforced SPSC contract.

### Phase Mariner

- Added `tina-runtime`, a small in-progress runtime with a deterministic
  event trace and causal links.
- Added single-shard stepping and local same-shard `Send` dispatch in
  registration order.
- Added local same-shard `Spawn` dispatch with runtime-owned mailbox creation,
  deterministic child IDs, and later-round child execution.
- Added runtime-owned direct parent-child lineage for root registrations and
  spawned children, with crate-private proof support for restart-oriented
  follow-up slices.
- Added a typed runtime ingress API so external code can send to registered and
  spawned isolates without holding raw mailboxes.
- Added stop-and-abandon semantics: when an isolate stops, buffered messages are
  drained in FIFO order, dropped, and traced as `MessageAbandoned`.
- Added panic-capture semantics: an unwinding handler panic becomes
  `HandlerPanicked`, then `IsolateStopped`, and the runtime continues the rest
  of the round deterministically.
- Added runtime tests for trace-core behavior, local send dispatch, and
  stop-and-abandon determinism.
- Added runtime tests for panic capture, post-panic abandonment, preserved
  programmer-error panics, and same-round continuation after panic.
- Added runtime tests for spawn dispatch, typed ingress backpressure, cross-shard
  ingress panics, and zero-capacity spawn rejection.
- Added runtime unit tests for direct parent-child lineage, nested spawn edges,
  and lineage survival across stop/panic.
- Added address-liveness semantics: `Address<M>` now includes a generation,
  runtime send traces include target generation, and stale known generations
  fail visibly as `Closed` instead of targeting a current incarnation.
- Added restartable child records: `RestartableChildDefinition<I>` records a
  factory-backed restart recipe, and `Runtime` stores private child
  metadata for future `RestartChildren` execution.
- Added `RestartChildren` execution for direct child records: restartable
  children are replaced with fresh isolate incarnations, non-restartable
  children are skipped visibly, and restart traces now support deterministic
  causal tree branching.
- Added `tina-supervisor` with `SupervisorConfig`.
- Added supervised panic restart in `tina-runtime`: configured parents
  apply `RestartPolicy` and runtime-lifetime `RestartBudget` state when direct
  children panic.
- Added generated-history runtime property tests for deterministic traces,
  causal-link validity, visible send outcomes, and no accidental handling after
  stop.
- Added an assertion-backed task-dispatcher proof package for the single-shard
  runtime, covering `OneForOne`, `OneForAll`, `RestForOne`, budget exhaustion,
  stale-address closure, and repeated-run determinism.
- Added a runnable `task_dispatcher` example that mirrors the tested workload:
  dispatcher-owned task ingress, registry-isolate address resolution, worker
  panic/restart, and later work continuing through replacement workers.
- Extended `runtime_properties.rs` with generated dispatcher workloads and a
  replay-style proof that reconstructs worker completions, panics, stops, and
  replacements from the runtime trace alone.
- Added focused Miri coverage for the SPSC mailbox unsafe slot paths and a
  `make miri` target.
- Added a runtime-owned call effect family at the `tina` boundary:
  `Isolate::Call` associated type and `Effect::Call(I::Call)` variant.
  Trait surface stays substrate-neutral; concrete request/result
  vocabulary lives in runtime crates.
- Added runtime-owned child bootstrap on `ChildDefinition` and
  `RestartableChildDefinition` via `with_initial_message`. The runtime delivers the
  bootstrap message to the new child immediately after spawn (and after
  each restart, for restartable specs), so a parent can hand a child its
  initial kick without test-harness trace introspection.
- Added `tina-runtime`'s first TCP call family on Betelgeuse
  (nightly Rust): `RuntimeCall<M>` carrying a translator from `CallOutput`
  back to `I::Message`, plus `CallInput` covering TCP listener bind,
  accept, stream read, stream write, listener close, and stream close.
  Resources are runtime-assigned opaque ids; raw sockets never escape
  into isolate state.
- Added a Betelgeuse-backed I/O backend in `tina-runtime`:
  caller-owned typed completion slots, synchronous Betelgeuse ops
  (bind / close) finish during dispatch, async ops (accept / recv / send)
  stay in a pending list until their slot has a result, all driven from
  `Runtime::step()` synchronously.
- Pinned tina-rs to nightly Rust via `rust-toolchain.toml` so the Betelgeuse
  substrate's `allocator_api` feature is available; the gate is scoped to
  `tina-runtime` via a crate-level `#![feature(allocator_api)]`.
- Added new runtime trace event kinds for call dispatch attempt, call
  completion, call failure, and rejected-on-stop completion delivery.
- Added focused tests for the call effect path covering invalid resource
  ids and call-id monotonicity, plus a "no call effect" compile-only smoke
  test that shows existing isolates remain ergonomic with
  `type Call = Infallible`.
- Added an assertion-backed live `tcp_echo` integration test: listener
  isolate supervises a restartable connection-handler child spawned via
  `RestartableChildDefinition::with_initial_message`; bytes round-trip end-to-end on
  `127.0.0.1:0` with the runtime reporting the actual bound address; trace evidence is asserted per
  call kind. Separate unit tests prove the connection isolate's
  partial-write retry logic and the `CallCompletionRejected{RequesterClosed}`
  path for a pending `TcpAccept`, plus accepted-stream `peer_addr` reporting.
- Added a runnable `tcp_echo` example mirroring the tested workload with
  inline assertions on echoed payloads.
- Added ordered `Effect::Batch(Vec<Effect<I>>)` at the `tina` boundary and
  runtime support in `tina-runtime` for deterministic left-to-right
  execution with `Stop` short-circuiting later batched effects.
- Added direct batch-semantics tests in `tina-runtime` proving
  left-to-right execution, spawn-plus-send sequencing, and `Stop`
  short-circuit behavior.
- Expanded the live `tcp_echo` proof and runnable example from a one-client
  demo into a small server-shaped workload: listener self-address capture,
  re-armed `TcpAccept`, sequential multi-client handling, bounded overlap,
  graceful listener close/stop, and retained one-client smoke coverage.
- Added a crate-local runtime proof that two accepted stream reads can be
  pending in `IoBackend` at the same time, so the bounded-overlap TCP claim
  is backed by direct runtime evidence rather than only by client-thread
  interleaving.
- Added the first runtime-owned time call verb: `CallInput::Sleep { after }`
  with `CallOutput::TimerFired`, plus `CallKind::Sleep` in the trace vocabulary.
  The runtime samples a monotonic clock once per `step()` and harvests due
  timers against that sampled instant. Equal-deadline timers wake in
  deterministic request order.
- Added a crate-private `ManualClock` seam so timer tests can drive time
  deterministically without brittle wall-clock sleeps, while production
  `Runtime` still uses a real monotonic clock.
- Added focused timer semantics unit tests: single timer wake, no early fire,
  fires exactly once, different-deadline ordering, equal-deadline request-order
  tie-break, and late-completion rejection after requester stop.
- Added a retry/backoff proof workload test: first attempt fails, a
  runtime-owned timer delays a real second attempt, later retry succeeds,
  and the trace proves the backoff `Sleep` completion occurred before the
  retried attempt.
- Added a public-path integration test for the same retry/backoff shape, using
  the shipped monotonic clock rather than the crate-private manual clock seam.

### Phase Voyager

- Added `tina-sim`, the first Voyager simulator crate.
- Added a single-shard virtual-time execution model with deterministic
  event recording against the shipped `tina-runtime` event
  vocabulary.
- Added simulator support for the shipped timer call family:
  `CallInput::Sleep { after }` and `CallOutput::TimerFired`.
- Added replay artifacts containing simulator config, final virtual time,
  and the reproducible event record for one run.
- Added timer-semantics proofs in `tina-sim` covering no-early-wake,
  one-shot wake, different-deadline ordering, equal-deadline request-order
  tie-break, stopped-requester completion rejection, and repeated
  same-config event-record reproduction.
- Added a simulator-backed retry/backoff proof workload and a replay test
  proving that rerunning from the saved config reproduces the same event
  record exactly.
- Made `SimulatorConfig.seed` semantically real for the first narrow seeded
  perturbation surface in `tina-sim`.
- Added `FaultConfig` / `FaultMode` for seeded perturbation over:
  - local-send delivery
  - timer-wake delivery
- Added a small checker surface in `tina-sim`:
  - `Checker`
  - `CheckerDecision`
  - `CheckerFailure`
- Extended replay artifacts to preserve optional checker failure information
  alongside config, final virtual time, and event record.
- Added a deliberate-bug public-path simulator workload proving that a seeded
  local-send perturbation can trip a checker, halt the run, and be reproduced
  exactly from the saved replay artifact config.
- Added a small structural checker proof over simulator event-id monotonicity.
- Fixed two simulator semantic bugs uncovered by the new proof surface:
  - delayed local sends now miss one additional delivery round instead of
    behaving identically to ordinary handler-emitted sends
  - `run_until_quiescent()` now continues while future-visible delayed local
    sends remain pending, instead of stopping early
- Tightened the timer-fault retry proof so its different-seed divergence claim
  is stated honestly: the timer-wake perturbation changes replay-visible
  virtual-time outcome, while the local-send perturbation changes the event
  record and checker outcome.
- Extended `tina-sim` with the shipped single-shard spawn and supervision
  surface:
  - `SpawnSpec`
  - `RestartableSpawnSpec`
  - direct parent-child lineage
  - restartable child records
  - direct-child `RestartChildren`
  - supervised panic restart through `SupervisorConfig`
- Added simulator proofs for spawn/restart parity: later-step child execution,
  same-step spawn ordering, bootstrap re-delivery after restart, repeated
  restart replay, all shipped restart policies, non-restartable skip events,
  stale-address send rejection as `Closed`, budget exhaustion, direct-child
  restart scope, and additive compatibility with existing `Spawn = Infallible`
  timer/fault/checker workloads.
- Extended `tina-sim` with scripted single-shard TCP simulation for the
  shipped call family:
  - `TcpBind`
  - `TcpAccept`
  - `TcpRead`
  - `TcpWrite`
  - `TcpListenerClose`
  - `TcpStreamClose`
- Added explicit simulator config for bounded scripted listeners, peers, and
  pending TCP completion capacity, plus `TcpCompletionFaultMode` for seeded
  delayed-completion and ready-batch reordering perturbation.
- Extended replay artifacts with captured peer-visible TCP output.

### Phase Galileo

- Added additive multi-shard coordinator shells:
  - `tina_runtime::MultiShardRuntime`
  - `tina_sim::MultiShardSimulator`
- Added root supervision routing on multi-shard runtime/simulator shells:
  `supervise(parent, config)` routes to the shard that owns the parent while
  child ownership remains shard-local.
- Added global explicit-step coordination in ascending shard-id order with:
  - global `try_send(addr, msg)` routed by `addr.shard()`
  - explicit root placement by shard
  - destination harvest before each destination shard's handler snapshot
  - next-step-only cross-shard visibility
- Added shared global event-id and call-id allocation across sibling shards.
- Added bounded shard-pair cross-shard transport with deterministic source-side
  `Full` rejection and no hidden overflow queue.
- Added deterministic cross-shard harvest rules:
  - ascending source-shard order per destination
  - FIFO within one shard-pair queue
  - drain-one-channel-to-empty before moving to the next source
- Added explicit source-time vs destination-time semantics for cross-shard
  delivery:
  - source-side `SendAccepted` / `SendRejected` describe transport admission
  - destination harvest records `MailboxAccepted` or destination-local
    `SendRejected` as an observability extension
- Added direct runtime and simulator proofs for:
  - global ingress routing
  - next-step-only remote visibility
  - shard-pair queue overflow
  - stopped/closed remote target rejection
  - unknown remote isolate rejection
  - destination mailbox full on harvest
  - FIFO from one source
  - deterministic multi-source harvest order
- Added a user-shaped two-shard dispatcher/worker workload on the preferred 021
  surface:
  - cross-shard request from coordinator to worker
  - cross-shard reply from worker back to coordinator
  - visible user-path `SendRejectedReason::Full`
  - deterministic repeated-run proof in the live runtime
- Added multi-shard simulator replay support:
  - `MultiShardSimulator::run_until_quiescent()`
  - `MultiShardSimulator::replay_artifact()`
  - `MultiShardReplayArtifact`
  - replay-style proof that rerunning from the saved configs reproduces the
    same multi-shard event record and workload output
- Added direct proof for per-isolate-pair FIFO across one shard pair with
  multiple source isolates and multiple target isolates, in both runtime and
  simulator tests.
- Added direct proof that multi-shard simulator replay works under non-default
  seeded timer/local-send fault config.
- Added direct proof that different non-default seeds can diverge in a
  faulted multi-shard simulator workload.
- Added direct proof that multi-shard scripted TCP echo composes with seeded
  TCP completion faults.
- Added direct proof that multi-shard supervision/restart composes with seeded
  local-send delay.
- Documented the current Galileo boundary honestly: full upstream-style
  peer-quarantine / shard-restarted semantics remain later work, not silently
  bundled into this first multi-shard slice.
- Added simulator proofs for TCP parity and replay: one-client echo,
  bounded-overlap echo, partial read/write drain behavior, invalid-resource
  failures, listener-close cancellation, stopped-requester rejection,
  mailbox-full completion rejection, same-config peer-output replay, both
  TCP fault-surface divergence modes, and checker-backed replay of seeded TCP
  accept reordering.
- Fixed two simulator driver/scheduler bugs uncovered by the TCP proof
  surface:
  - `run_until_quiescent()` and checked replay runs now continue while pending
    TCP calls remain in flight, instead of stopping early when no timers or
    visible messages exist yet
  - seeded TCP delay perturbation now preserves per-resource FIFO by never
    allowing later completions on the same listener/stream to overtake earlier
    ones

### Phase 021 Devex and Call Ergonomics

- Renamed the user-facing runtime crate directory and package surface from the
  transitional `tina-runtime-current` shape to `tina-runtime`.
- Reworked the preferred authoring vocabulary around `Runtime`,
  `RuntimeCall`, `CallInput`, `CallOutput`, `CallError`, `Outbound`,
  `ChildDefinition`, and `RestartableChildDefinition`.
- Added the preferred prelude and isolate authoring macros so common isolates
  do not need the old wall of associated-type boilerplate.
- Added typed runtime-call helpers such as `sleep(...)`, `tcp_read(...)`, and
  `tcp_write(...)` with `reply(...)` as the single public completion combinator.
- Removed the old compatibility-alias plan before public use; Tina kept one
  preferred surface instead of silent equal-peer names.
- Reworked README and tests toward the new syntax and proved the renamed
  surface through runtime and simulator consumer tests.

### Phase Kepler

- Sealed the current explicit-step multi-shard liveness boundary:
  address-local remote failures remain address-local, and there is still no
  shard-down / peer-down / restarted-peer event vocabulary.
- Sealed multi-shard supervision as shard-local: root supervision routes to the
  parent shard, spawned children stay on the parent shard, and supervised
  restarts stay on that shard.
- Added runtime proofs for the sealed rules:
  - `cross_shard_unknown_isolate_does_not_poison_destination_shard`
  - `dispatcher_worker_workload_continues_after_bad_remote_address_on_same_shard`
  - `multishard_supervision_keeps_children_on_parent_shard`
- Added simulator proofs for the same sealed rules:
  - `cross_shard_simulation_unknown_isolate_does_not_poison_destination_shard`
  - `multishard_dispatcher_workload_continues_after_bad_remote_address_on_same_shard`
  - `multishard_simulation_supervision_keeps_children_on_parent_shard`
- Added multi-shard checker support:
  - `MultiShardSimulator::run_until_quiescent_checked()`
  - `MultiShardReplayArtifact::checker_failure()`
- Added checker/replay proofs for the liveness boundary:
  - `multishard_checker_accepts_address_local_remote_failure_then_good_traffic`
  - `multishard_checker_failure_replays_for_address_local_liveness_bug`
- Added a focused allocation probe,
  `multishard_runtime_path_still_has_allocations_so_the_claim_stays_narrow`,
  and narrowed the runtime allocation claim instead of pretending the whole
  multi-shard runtime path is allocation-free.

### Phase Huygens

- Added the first live shard-owned runtime substrate in `tina-runtime`:
  - `ThreadedRuntime<S, F>` for one worker-owned shard runtime
  - `ThreadedMultiShardRuntime<S, F>` for a fixed worker-per-shard runtime set
  - `ThreadedRuntimeConfig` for bounded command ingress and idle wait tuning
  - `ThreadedTrySendError`, `ThreadedSendObservedError`, and
    `ThreadedControlError`
- Defined `ThreadedRuntime::try_send` as bounded handoff only, so it does not
  block after admission waiting for the worker to observe mailbox state.
- Added `ThreadedRuntime::send_and_observe` as the explicit synchronous control
  path for tests/setup that need mailbox `Full` / `Closed` outcomes.
- Added live-threaded TCP echo proof:
  `threaded_runtime_tcp_echo_round_trips_reference_workload`.
- Added live bounded-ingress proof:
  `threaded_runtime_try_send_surfaces_ingress_full_without_blocking_on_worker`.
- Added live single-shard substrate proofs for stopped-target observation,
  runtime-owned timer retry, and local mailbox `Full` trace visibility.
- Added live fixed-shard cross-shard substrate proofs for:
  - request/reply across two OS worker threads
  - remote destination worker queue `Full` observed at the source
  - stale remote address rejection without poisoning later good remote work
- Added sendable erasure for live cross-shard payload transport while keeping
  the explicit-step runtime one-thread-owned.
- Changed shared runtime/simulator event and call id allocation to use a
  cloneable atomic id source so sibling worker runtimes can preserve global
  monotonic ids.
- Added composed Huygens DST harness tests covering supervision + timer +
  local-send perturbation, replayable checker failure, and remote `Full`
  pressure on the explicit-step oracle.
- Documented the Huygens claim boundary: live substrate exists for selected
  workloads, while production hardening, peer quarantine, dynamic shard
  membership, cross-shard child ownership, Tokio bridge work, and broad
  allocation-free runtime claims remain future work.

### Phase Mercury

- Added user-visible observed send outcomes so application code can branch on
  `Accepted`, `Full`, and `Closed` instead of only inspecting trace after the
  fact.
- Added same-shard isolate-to-isolate call with mandatory timeout and typed
  outcomes for reply, target full, target closed, timeout, and requester-stop
  completion rejection.
- Added focused runtime and simulator tests for reply delivery, full/closed
  targets, timeout, late replies after timeout, requester stopped, requester
  mailbox full at completion, and replay determinism.
- Added macro/devex cleanup and a runnable Tokio-vs-Tina semantic comparison
  suite to pressure ergonomics without making Tokio the substrate story.
- Recorded cross-shard call reply transport as not yet claimed; cross-shard
  call rejects deterministically in this slice.

### Phase Betelgeuse

- Added the first live substrate surface as `BetelgeuseRuntime` /
  `BetelgeuseMultiShardRuntime`.
- Kept explicit-step runtime and `tina-sim` as the semantic oracle while
  proving selected workloads on live Betelgeuse-backed runners.
- Added bounded ingress proof, live time/TCP completion semantics, live
  multi-shard bounded send, typed live cross-shard call rejection, and
  oracle/sim/live parity tests.
- Added a narrow Betelgeuse simulated TCP backend with seeded completion delay
  and partial-write pressure.
- Pinned allocation and cost probes for the touched substrate paths and kept
  Tokio as comparison/later bridge rather than the main runtime story.

### Phase Tina TCP Driver Contract

- Moved runtime-owned time/TCP behind a small Tina-owned driver boundary for
  timers, TCP operations, completions, cancellation, shutdown, and wakeups.
- Added native Betelgeuse and simulated Betelgeuse driver adapters under the
  same runtime semantics.
- Added same-resource `ResourceBusy` semantics, bounded pending-operation
  admission, and direct cancellation/late-completion proofs.
- Proved user-shaped workloads on explicit runtime, native Betelgeuse-backed
  runtime, and simulated-driver runtime without adding futures, wakers, async
  handlers, or arbitrary task spawning.

### Phase Parallel Substrate Support

- Polished the simulated Betelgeuse I/O surface as generic substrate support
  rather than Tina-specific magic.
- Added narrow allocation/performance probes for current hot paths.
- Expanded runnable Tokio-vs-Tina comparisons around constrained capacity,
  backpressure, timeout, shutdown, and overload behavior.
- Added only small helper/macro polish that preserved one preferred Tina
  surface.
- Recorded external review prompts and substrate research notes for Tokio
  current-thread, Monoio, Glommio, and Compio.

### Phase Ranger

- Documented the driver capability contract for time/TCP progress,
  cancellation, shutdown, bounded pending work, and deterministic simulator
  compatibility.
- Moved TCP pending ownership to listener/read/write lanes and allowed
  full-duplex same-stream read/write while keeping close and duplicate-lane
  `ResourceBusy` honest.
- Made per-call cancel tombstone the selected call without silently closing
  unrelated resource lanes.
- Added live and simulated proofs for stopped-requester cancellation, explicit
  close, runtime shutdown, late completion swallowing, requester mailbox full
  at completion, and live worker TCP shutdown.
- Pinned TCP read/write allocation counts and recorded Betelgeuse as the
  near-term substrate direction.

### Phase Surveyor

- Treated Tina's live substrate as a Tina-owned implementation over Betelgeuse
  instead of waiting for upstream Betelgeuse to provide Tina-specific
  guarantees.
- Hardened completion-slot ownership so shutdown and cancellation no longer
  depend on dropping slots while a backend may still hold pending completion
  state.
- Added no-leak shutdown/cancel-drain proofs across native and simulated
  Betelgeuse-backed runtime paths.
- Preserved the explicit-step runtime and `tina-sim` as the semantic oracle;
  Surveyor changed live-substrate ownership, not Tina's isolate model.

### Phase Willem Drees

- Added a composed local-production workload in `tina-runtime` with listener
  isolate, connection isolates, bounded worker, supervisor restart,
  runtime-owned TCP, runtime-owned time, and shutdown pressure.
- Proved the workload on live Betelgeuse TCP with real loopback clients,
  observing typed `CallOutcome::{Replied, Full, Timeout}` and trace events.
- Proved the same server shape through Betelgeuse simulated I/O with delayed
  completions and partial writes, driven through the threaded runtime loop.
- Added a `tina-sim` oracle version that replays the bounded TCP flow
  byte-for-byte for observations, peer output, and event kinds.
- Added composed shutdown proof for pending accept, read, write, sleep, and
  isolate-call work.
- Added server-shaped backpressure guards: explicit mailbox/ingress
  capacities, forced worker `Full`, and exact-sized scripted peer output
  buffers so hidden writes cannot be masked.

### Phase Ruud Lubbers

- Added a narrow numerical runtime cost model with allocation probes for
  multi-shard send, isolate call, timer, TCP read/write, two-send batch,
  spawn, restart, repeated trace pressure, live ingress handoff, and
  high-cardinality idle stepping.
- Kept the SPSC mailbox no-allocation proof intact while improving runtime,
  simulator, driver, and coordinator allocation behavior.
- Reused runtime and simulator round-message scratch storage, driver
  completion scratch storage, and preallocated common runtime/simulator
  bookkeeping vectors.
- Changed runtime-created mailboxes to store erased message boxes directly,
  avoiding an extra box/downcast/box cycle for runtime-created mailboxes while
  preserving user-provided typed mailbox registration.
- Reworked multi-shard runtime and simulator coordinator queues into prebuilt
  indexed double buffers, reducing the multi-shard send hot path to
  `1 alloc / 0 realloc` while preserving next-global-step remote visibility.
- Added regression proof for more-than-initial-capacity round-message scratch
  reuse, including a 12-isolate idle-step allocation test.
- Recorded medium follow-up rocks in the roadmap: batch small path, live worker
  command boxing, sizing knobs, trace retention policy, typed fast paths, and
  completion-slot pooling/slabbing.

### Phase Joop den Uyl

- Migrated the composed local-production workload into canonical
  `application_surface` test artifacts for `tina-runtime` and `tina-sim`.
- Added a named local service-capacity pattern in the canonical harness so
  listener, connection, worker, command, backlog, and pending-completion
  capacities are explicit instead of scattered magic numbers.
- Added test-local trace assertion helpers for event existence/counts,
  stopped-and-idle service checks, and terminal send/call outcome invariants.
- Added direct application-surface proofs across live Betelgeuse loopback,
  threaded simulated I/O, explicit-step runtime with simulated I/O, and
  deterministic `tina-sim` replay.
- Added non-TCP porting proofs for bounded worker/router pressure and a
  stateful session/control-plane shape with local audit send.
- Kept helper surface test-local for now; no public application builder,
  router, registry, or macro was added.

### Phase Victor Marijnen

- Made `LocalSystemConfig` the live bounded-shape manifest for ingress,
  shard-pair transport, storage, DNS, TLS, process, signal, trace retention,
  and idle wait.
- Added live local cross-shard isolate calls with requester-shard-owned pending
  state, bounded request/reply transport, typed success/full/closed/timeout
  outcomes, and DST coverage for reply paths.
- Split live source-to-destination remote transport from worker command ingress
  so shard-pair capacity is a real bounded queue, not a soft metric.
- Added native inbound TLS server support with `TlsListenerId`, `tls_bind`,
  `tls_accept`, `tls_close_listener`, existing TLS read/write/close, static
  cert/key scope, total accept/handshake deadline, and negative-path tests for
  invalid key, failed handshake, lane full, timeout, and shutdown.
- Added live resource inventory and terminal shutdown accounting for TCP
  listeners/streams, TLS listeners/streams, UDP sockets, files, and pending
  driver work. Shutdown `clean` now requires no remaining owned resources.
- Added and updated LocalSystem e2e proofs for topology, resource accounting,
  cross-shard calls, TLS server hosting, configured lane capacities, worker
  failure visibility, and shutdown behavior.
- Updated `tina-sim` TLS scripts to model server bind/accept/close outcomes
  deterministically without pretending to test cryptography.
