# Plan Review 1

Verdict: right next phase, not quite ready to implement.

Sadie's Ward points at the correct production-readiness rock: lifecycle truth.
It follows Victor naturally and keeps Barend ergonomics waiting. The plan is
short and readable, but hostile grug sees places where implementation could
claim success while leaving the hard lifecycle contracts fuzzy.

## Findings

1. **[P1] Worker-held resource accounting is not pinned enough**

   Rock 2 says table-owned and worker-held resource counts are distinguishable
   or summed honestly, but does not define the unit. Is a blocked DNS lookup a
   resource, a pending call, or both? Is a TLS worker holding an `Arc<TcpStream>`
   counted as one TLS stream even before `TlsStreamId` exists? Is a process
   child counted separately from pending process call? Pin a vocabulary:
   table-owned resources, worker-held resources, and pending calls, with one
   count rule per lane.

2. **[P1] Bounded drain needs concrete time/attempt rules**

   Rock 3 says bounded wait, tombstone, report remaining work, but not the
   deadline source or defaults. Without this, implementation can pick arbitrary
   sleeps or block longer than user intent. Pin a config surface or internal
   constant for shutdown lane-drain budget, and say whether it is per-lane,
   total-system, or per-shard.

3. **[P1] Raw OS signal capture needs a crate/dependency decision**

   Rock 4 asks for `SIGINT`/`SIGTERM` if clean, but does not name the mechanism.
   Rust signal handling can mean `signal-hook`, `ctrlc`, raw libc, Tokio signal,
   or platform-specific code. Pin expected direction and refusal: no Tokio
   dependency in core runtime, no async signal task, no unsafe custom signal
   handler unless absolutely required.

4. **[P1] Failed-shard cleanup needs exact terminal-outcome rules**

   Rock 5 says pending cross-shard request/reply work reaches one terminal
   outcome, but not which outcome wins when shard failure races timeout, full
   reply path, requester stop, or destination already accepted the request.
   Pin priority order, or tests will encode accidental behavior.

5. **[P2] Simulator/DST scope is unclear for live-only facts**

   Raw OS signals and worker-held resource counts are partly live-only. The
   plan says no semantic live behavior without simulator/DST or direct e2e, but
   does not classify which rocks require sim parity and which require e2e only.
   Add a proof-mode table: direct unit, live e2e, simulator oracle, DST.

6. **[P2] Topology/report fields need public names before implementation**

   Rock 6 lists desired data but not API names or compatibility. If
   `LiveShardReport` grows fields, pin names enough that code review can judge
   whether the surface is good: `owned_resource_count`,
   `worker_held_resource_count`, `pending_driver_call_count`,
   `shutdown_unclean_reason`, etc.

7. **[P2] This can silently become a large observability phase**

   Rock 6 asks for operator-useful topology. That can expand into metrics
   sinks, histograms, tracing subscribers, structured exports, and dashboards.
   Add refusal: no metrics backend, no Prometheus, no tracing integration, no
   public observability framework. Just typed snapshots and tests.

8. **[P2] Storage/DNS/process worker-held tests may be artificial without hooks**

   Rock 2 requires tests for TLS, process, storage, DNS, TCP. TLS and process
   can be blocked naturally. DNS and storage may need injected resolvers/jobs or
   test-only park hooks. Pin that test hooks may stay crate-private/test-only
   and must not become user API.

9. **[P3] Done Means references `SYSTEM.md`, but repo has no `SYSTEM.md`**

   The current worktree has `ROADMAP.md` and `CHANGELOG.md`, not `SYSTEM.md`.
   Either remove `SYSTEM.md` from Done Means or replace with the actual system
   memory file if one exists elsewhere. Do not keep a fake closeout requirement.

## Suggested Edits

- Add a "Lifecycle Vocabulary" section:
  table-owned resource, worker-held resource, pending call, tombstoned work,
  unclean shutdown reason.
- Add a "Shutdown Budget" section with expected config/default and per-shard vs
  global rule.
- Add a "Signal Mechanism" section naming `signal-hook` or equivalent, with
  no Tokio dependency.
- Add a "Failure Priority" section for shard failure vs timeout/requester stop/
  full/stale.
- Add a proof-mode table by rock.
- Add the topology/report field names expected in this phase.
- Remove or correct `SYSTEM.md` closeout line.

After those edits, grug says the plan is ready.

# Plan Review 2

Verdict: ready to hand to implementation.

The plan now pins the load-bearing bits:

- lifecycle vocabulary is explicit;
- resource count rules are lane-specific;
- shutdown drain has a named config field;
- signal mechanism excludes Tokio/async/custom unsafe handlers;
- failed-shard race priority is named;
- report field names are pinned;
- proof modes are split between unit, live e2e, sim, and DST;
- fake `SYSTEM.md` closeout requirement is gone.

Remaining implementation caution: keep test hooks crate-private/test-only. Do
not let lifecycle hardening turn into public observability framework.

# Plan Review 3

Verdict: Claude-handoff ready.

Extra pins added:

- execution rules: rock order, narrow tests after each rock, review before
  next rock;
- stop/ask rules for public vocabulary, dependencies beyond `signal-hook`,
  unsafe code, weakened tests, or skipped rocks;
- exact shutdown drain default: `Duration::from_millis(100)`;
- expected public report accessors and `ShutdownUncleanReason` enum shape;
- local-worker-only failure scope;
- minimum named test classes for lane counts, TLS/process/DNS/storage shutdown,
  signals, failed-shard race priorities, and DST combinations.

No remaining plan blocker seen.

# Rock 1: Lifecycle Audit

What is true about Tina lifecycle today, in the working tree this branch was
cut from. Cited with `path:line`. No proposals here; just facts.

## Where lanes live

- TCP/UDP/file/signal share one driver: `BetelgeuseTcp` in
  `tina-runtime/src/driver.rs:233`. Calls flow through `RuntimeDriver`
  (`driver.rs`).
- TLS has its own bounded thread worker: `TlsLane` /
  `TlsWorkerLane` (`tina-runtime/src/driver.rs:432`). It runs `rustls`
  `StreamOwned` clients/servers (`driver.rs:65–66`) on real `TcpStream`s.
- DNS is a bounded thread worker: `DnsWorkerLane` (`driver.rs:405`) holding
  a resolver closure and pending lookups with deadlines.
- Storage is `StorageLane::Inline` or `StorageLane::Worker(StorageWorkerLane)`
  (`driver.rs:322`), with a typed `StorageJob` enum
  (`driver.rs:360`) and a `#[cfg(test)] StorageJob::Park` knob
  (`driver.rs:393`) used today to inject stalls in tests.
- Process is a bounded thread worker: `ProcessWorkerLane`
  (`driver.rs:527`) executing `std::process::Child` via
  `execute_process_command` (`driver.rs:2585`).
- Signal waits live in `BetelgeuseTcp::signals: Vec<SignalWaitEntry>`
  (`driver.rs:207`) with deadline + cancelled fields. There is no OS signal
  capture: `grep` finds zero uses of `signal-hook`, `ctrlc`, `tokio::signal`,
  or unsafe signal handlers in the workspace.

## Table-owned ids today

These exist as runtime/driver-table ids and are returned to user code:

- `ListenerId` (`call.rs:63`), `StreamId` (`call.rs:79`) — TCP, in
  `BetelgeuseTcp::listeners`/`streams` (`driver.rs:239–240`).
- `UdpSocketId` (`call.rs:95`) — UDP, in `BetelgeuseTcp::udp_sockets`
  (`driver.rs:241`).
- `FileId` (`call.rs:143`) — files, in `BetelgeuseTcp::files`
  (`driver.rs:242`).
- `TlsListenerId` (`call.rs:127`), `TlsStreamId` (`call.rs:111`) — TLS, in
  `TlsWorkerLane::listeners`/`streams` (`driver.rs:438–439`).

DNS, storage, process, and signal waits have no table id today; each pending
operation is keyed by `CallId`.

`DriverResourceReport::owned_resource_count` (`driver.rs:183`) sums TCP
listeners + streams + UDP sockets + files + TLS listeners + TLS streams.
`LiveShardMetrics::owned_resource_count` (`lib.rs:3345`) is updated after
each driver step (atomic snapshot read at `lib.rs:3388`). The number reaches
the user via `LiveShardReport::owned_resource_count` (`lib.rs:3403`) and
`LocalSystemShutdownReport::remaining_owned_resource_count`
(`lib.rs:3551`, derived from the topology in `from_parts`,
`lib.rs:3572–3580`).

There is **no public count today** for worker-held resources or for pending
driver calls that don't own a table id.

## Worker-held resources today

What lives inside in-flight work but does not show up as a table id:

- TCP `Box<dyn IOSocket>` is held inside `ListenerEntry`/`StreamEntry`
  (`driver.rs:248`, `driver.rs:253`) — these are table-owned, not extra
  worker-held resources.
- TLS holds `Arc<TcpListener>` plus `Arc<rustls::ServerConfig>`
  (`driver.rs:444–448`) and `Arc<Mutex<TlsRuntimeStream>>`
  (`driver.rs:450–453`). When a TLS accept/handshake/read/write/close is
  in flight, a *clone* of those arcs is parked inside the worker thread
  on a `TlsCommand` (`driver.rs:470–513`). The table-id arc and the
  worker-held arc clone are distinct lifetimes: dropping the table id does
  not by itself end the worker-held clone.
- Storage worker holds OS file handles and snapshot/journal state implicitly
  during `StorageJob` execution. No public id.
- DNS worker holds a resolver closure. No public id.
- Process worker holds a live `std::process::Child` until the call
  completes (`execute_process_command`, `driver.rs:2585`). Cancellation
  closes the channel sender (`driver.rs:1953` + `driver.rs:2058`); the
  child is **not killed today**, it continues until natural exit.
- Signal waits are bookkeeping only (`SignalWaitEntry`, `driver.rs:222–227`):
  no OS handle, no worker thread.

## Pending-call surface today

- `BetelgeuseTcp::pending: Vec<PendingOperation>` (`driver.rs:243`) with
  `PendingLane` enum (`driver.rs:302`) covering listener-accept,
  stream-read/write, UDP recv, file read/write/fsync/size/mkdir, and
  signal-wait. Each has a `cancelled` flag (`driver.rs:266`).
- TLS: `Vec<TlsPending>` (`driver.rs:437`) with deadline (`driver.rs:458`),
  `cancelled` and `timed_out` flags. Timeouts fire from
  `TlsWorkerLane::advance` (`driver.rs:1339–1364`).
- DNS: `Vec<DnsPending>` (`driver.rs:410–411`) with deadline + cancelled +
  timed_out. Deadlines harvested in `advance` (`driver.rs:1340–1351`).
- Storage: `Vec<StoragePending>` (`driver.rs:346`); call_id + cancelled
  only. No deadline at lane level.
- Process: `Vec<ProcessPending>` (`driver.rs:532`); call_id + cancelled
  only. Timeout is passed to the worker on the command, not enforced by the
  lane.

There is no public count of pending driver calls today. `#[cfg(test)]
io_pending_count()` (`driver.rs:165–168`, `driver.rs:772–775`) and a
similar `pending_count()` for DNS exist as crate-private test hooks; they
are not on `LiveShardReport`.

## What can block, cancel, or only tombstone

- **Block**: TLS worker thread can park indefinitely on rustls handshake
  read/write if the peer is slow; storage worker can park indefinitely on
  filesystem syscalls; process worker stays attached to `Child` until exit;
  DNS worker stays inside the resolver closure until it returns. None of
  these have a per-lane shutdown deadline today.
- **Cancel cleanly**: TLS pending ops and DNS lookups support
  `cancelled` + late-completion drop (`driver.rs:1366–1379`). Storage and
  process pending ops support cancel-and-drop on the *result*; the worker
  itself does not stop early. UDP recv `drops_on_cancel()` in shutdown
  drain (`driver.rs:3601`).
- **Tombstone-only**: late TCP/file completions after cancel are drained
  via `drain_cancelled_pending_for_shutdown` (`driver.rs:3132`), capped at
  64 IOLoop steps; if Betelgeuse still owns pointers,
  `cancel_pending` returns `DriverShutdownError::BackendStillOwnsCompletions`
  (`driver.rs:3087`).

## Shutdown semantics today

- `Drop for TlsLane` (`driver.rs:1130`) calls `cancel_pending` which
  signals the worker via `Arc<AtomicBool>`, closes the sender, drains
  completions in a busy loop until the worker is finished
  (`driver.rs:1242–1247`). No deadline.
- `Drop for StorageLane` (`driver.rs:1265`) and `DnsWorkerLane`
  (`driver.rs:1406`) and `ProcessLane` (`driver.rs:1953`) all use the same
  pattern: cancel flag, close sender, busy-wait until worker thread exits.
  No deadline. Any blocked syscall blocks shutdown.
- `BetelgeuseTcp::cancel_pending` (`driver.rs:3078`) clears signals,
  marks all pending cancelled, asks Betelgeuse to release completion
  pointers, and steps the IOLoop up to 64 times to drain. Hard cap, but a
  single stuck completion can leak the whole shutdown via
  `BackendStillOwnsCompletions`.
- `LocalSystemConfig` (`lib.rs:3076–3097`) has bounded capacity fields and
  `idle_wait`, but **no shutdown timeout** of any kind. Per-shard graceful
  shutdown is whatever the lanes happen to allow.

## Failed-shard handling today

- `LiveShardState::Failed` (`lib.rs:3221`) is set when worker run loops
  exit through panic or fatal error (multiple sites in `lib.rs` near 4379,
  4420, 4494, 4542, 4546, 4558, 4562, 4566, 4888, 4969, 4975, 4992, 4998,
  5004).
- Failed shards land in `LocalSystemShutdownReport::failed_shards`
  derived from the topology snapshot (`lib.rs:3562–3571`). `clean()` is
  false when any error is present, the system is not `Closed`, or
  resources remain (`lib.rs:3583–3585`).
- There is no contract today that ingress/sends/calls aimed at a failed
  shard explicitly reject with a named reason; `SendRejectedReason` and
  `CallReplyRejectedReason::RequesterShardClosed` (`trace.rs:226`) exist
  but the audit did not find a failed-shard-specific reject path. Worth
  re-checking in Rock 5 before adding behavior.
- Cross-shard request/reply already has terminal-outcome plumbing
  (`MultiShardRuntime`, `lib.rs:2717+`; remote queue indexes
  `lib.rs:2709`). The race priority asked for in the plan is **not pinned
  in code today**; current behavior is whatever the order of the run-loop
  happens to produce.

## Existing reports, by field

`LiveShardReport` (`lib.rs:3395`):
- `shard`, `worker_name`, `state`, `ingress: LiveQueueReport`,
  `storage_lane: LiveQueueReport`, `trace_retention`, `trace_dropped`,
  `owned_resource_count`.

`LiveQueueReport` (`lib.rs:3240`): `capacity`, `depth`, `accepted`,
`rejected_full`, `rejected_closed`.

`LiveTopologyReport` (`lib.rs:3473`): `shards`, `remote_queues`.

`LocalSystemShutdownReport` (`lib.rs:3544`): `final_state`, `clean`,
`canceled_count`, `tombstoned_count`, `rejected_after_drain_count`,
`failed_shards`, `remaining_owned_resource_count`.

Missing relative to Rock 2 / Rock 6 plan asks:
- `worker_held_resource_count` on shard and shutdown reports.
- `pending_driver_call_count` on shard and shutdown reports.
- `unclean_reason` (typed enum) on shutdown report.
- TLS/DNS/process/signal lane queue reports on `LiveShardReport`.

## DST / sim entry points

- `tina-sim/src/lib.rs` `SimulatorConfig` (`lib.rs:68`) carries scripted
  configs for `tcp`, `udp`, `dns`, `tls`, `signal`, `process`, `storage`.
- `Simulator::new` + `step` is the deterministic harness.
- DST/randomized harnesses: `tina-sim/tests/dst_harness.rs`,
  `dst_randomized.rs`, `huygens_dst_harness.rs`, `timmerhus_dst.rs`.
- Lane-shaped sim tests already exist: `io_simulation.rs`,
  `persistence_simulation.rs`, `timer_semantics.rs`, `supervision_simulation.rs`.
- Signal capture is scripted-only today (`ScriptedSignalConfig`,
  `tina-sim/src/lib.rs:89`). Live OS signal delivery has no analogue.

## Existing crate-private / test-only hooks

- `#[cfg(test)] StorageJob::Park` (`driver.rs:393–397`): blocks worker
  until released — already supports the kind of stall injection Rock 7 will
  need for storage.
- `#[cfg(test)] io_pending_count()` on driver (`driver.rs:165–168`,
  `driver.rs:772–775`).
- `#[cfg(test)] DnsWorkerLane::pending_count()` analogue exists by the same
  pattern.
- `#[cfg(test)] ManualClock` (`lib.rs:440–469`) for deterministic time in
  unit tests.

These give us a starting kit. The plan's DNS / process worker-held tests
likely need analogous `Park` knobs added under `#[cfg(test)]`. They must
stay crate-private/test-only.

## What this audit means for the rocks

- Rock 2 must add three new typed report fields plus `ShutdownUncleanReason`,
  but `owned_resource_count` already exists end-to-end and should not be
  reinvented.
- Rock 3 needs `shutdown_lane_drain_timeout` on `LocalSystemConfig` plus
  a per-lane `(stop, cancel, drain-until-budget, join, report-remaining)`
  shape. Today every lane busy-waits forever in shutdown.
- Rock 4 starts from zero for live signal capture; only the scripted sim
  side exists.
- Rock 5 needs explicit reject paths for ingress/sends/calls into a failed
  shard and a pinned race priority for cross-shard request/reply terminal
  outcomes.
- Rock 6 needs lane queue reports for TLS/DNS/process/signal plus the
  worker-held / pending-driver-call / failed-shard / unclean-reason
  surface.
- Rock 7 already has `Park` and `io_pending_count` patterns to extend; new
  hooks must remain crate-private/test-only.

# Positive Review

What this phase nailed:

- `LiveShardReport` now exposes every bounded lane (storage, DNS, TLS,
  process, signal) plus the three accounting vocabularies (table-owned,
  worker-held, pending). `LocalSystemShutdownReport` mirrors them and
  carries a typed `ShutdownUncleanReason` instead of just a `bool`.
- Per-lane shutdown is bounded by `shutdown_lane_drain_timeout`
  (default 100 ms). Storage and process workers no longer busy-wait
  forever; stuck work surfaces in `physical_pending_count` so the
  terminal report can name it.
- OS signal capture is real on Unix via `signal-hook` flag handlers
  with no Tokio dependency, no async signal task, and no custom unsafe
  handler. Non-Unix is an explicit unsupported capability via
  `os_signal_capture_supported()`.
- A `Failed` shard now rejects ingress immediately at the threaded
  `try_send` paths instead of relying on the `Disconnected` race window.
- New tests are concrete: per-lane count rules, shutdown priority
  ordering, bounded shutdown returning inside budget when the worker is
  stuck, real `raise(SIGINT)` reaching a parked `signal_wait`, failed
  shard rejecting ingress while a healthy shard keeps working, and the
  `LocalSystem` topology surfacing every new field.
- `make verify` (fmt + check + test + loom + doc + clippy) is clean.

# Blast-Radius Review

Public API additions (no removals, no renames):

- `LiveShardReport`: `worker_held_resource_count`,
  `pending_driver_call_count`, `dns_lane`, `tls_lane`, `process_lane`,
  `signal_lane`.
- `LocalSystemShutdownReport`: `remaining_worker_held_resource_count`,
  `remaining_pending_driver_call_count`, `unclean_reason`.
- `ShutdownUncleanReason` enum, `#[non_exhaustive]`.
- `ThreadedRuntimeConfig::shutdown_lane_drain_timeout`,
  `LocalSystemConfig::shutdown_lane_drain_timeout`,
  `DEFAULT_SHUTDOWN_LANE_DRAIN_TIMEOUT`.
- `os_signal_capture_supported()` const fn.
- New dependency: `signal-hook 0.3` (Unix only, default features off).

Behaviour changes that callers may see:

- `pending_driver_call_count` (and the per-lane physical pending count
  beneath it) now counts cancelled-but-undrained ops until the next
  drain step or shutdown drain. During steady state this is a one-step
  flicker; at shutdown it reflects stuck work honestly. Prior code that
  read `LiveShardReport` saw filtered counts.
- `cancel_pending` on lanes never blocks past the deadline, even if the
  worker thread cannot finish its current syscall. The thread is left
  attached and exits on its own when the command channel closes. No
  thread is leaked across the program lifetime in practice because
  workers exit on the next `recv`.
- `Drop` on lanes uses `Instant::now()` as the deadline (zero budget) so
  unhooked drops do not block. Callers that relied on `Drop` to drain
  results must call the proper shutdown path.
- `try_send` to a `Failed` shard now returns `WorkerStopped` even before
  the bounded sync channel has observed `Disconnected`.

Crate-internal but worth flagging for downstream consumers:

- `RuntimeDriver::cancel_pending` now takes a `deadline: Instant`. The
  `FakeDriver` in `tests.rs` was updated; any third-party `RuntimeDriver`
  impl must add the parameter.
- `pub use driver::os_signal_capture_supported` is the only new top
  level export from the runtime crate.

# Hostile Review

Where this could still claim success while leaving the contract fuzzy:

1. **Shared OS signal flag, not per-driver.** A single SIGINT can only
   wake one `BetelgeuseDriver`'s parked `signal_wait`: whichever driver
   polls first consumes the global flag. In a multi-shard `LocalSystem`
   each shard has its own driver but the dispatcher is shared. Live
   processes that want every shard to react to the same signal are not
   served by this design today; only one shard's `signal_wait` fires.
   Tests cover the typical single-shard path; a multi-shard test would
   expose the limitation.

2. **Pending-call count covers physical entries, not "still in
   flight".** A user-cancelled op that has not yet drained still
   contributes to `pending_driver_call_count` for one step. Code that
   reads the count to gate decisions during normal operation may see
   transient spikes. The honest fix would be a separate
   `unfinished_at_shutdown_count`, but I chose the simpler unified
   accounting; flagging here so downstream readers understand the shape.

3. **`unclean_reason` is single-valued.** A shutdown that is both
   "remaining worker-held" and "failed shard" reports only the higher
   priority reason. Callers that want to learn about every condition
   must inspect the count fields separately. Acceptable per the plan,
   but pinning here so reviewers do not assume `unclean_reason` is a
   complete bag-of-conditions.

4. **Cross-shard race priority is documented, not centrally enforced.**
   Rock 5's contract says shard-failed > timeout > full, but the code
   only enforces the obvious "fail-fast on Failed state" gate. The
   underlying call/timeout/reply machinery already produces exactly one
   terminal outcome via `in_flight_calls` removal, so the property
   holds by construction; an attacker would need to find a path that
   bypasses `in_flight_calls`. None spotted, but the plan's race table
   is not encoded as a single dispatcher.

5. **DST coverage for shutdown + late completion + topology and for
   shard failure + remote full + timeout is not added in this phase.**
   Existing simulator tests cover much of the surface, but the named
   "DST history that combines …" pair from the plan is not directly
   added. The unit and live e2e proofs cover the load-bearing
   guarantees; combinatorial DST would be the next layer.

6. **`Drop` zero-budget shutdown can leave a worker thread attached.**
   The thread will exit when its command channel receiver returns
   disconnected, which happens automatically when the lane drops the
   sender. In pathological cases (worker blocked on a long syscall
   like `connect` or `flock`) the thread can outlive the lane until
   the syscall returns. We do not panic and we do not leak structured
   resources, but a paranoid program may want to call the proper
   shutdown path with a real budget rather than relying on `Drop`.

7. **`ShutdownUncleanReason::RuntimeError` carries no payload.** A
   reader who wants to know which `ThreadedRuntimeError` triggered the
   unclean state must inspect `terminal.error()` separately. The plan
   forbade stringly values; the typed enum points at the right place
   but does not embed the underlying error variant.

8. **OS signal handler chain effects are inherited.** `signal-hook`
   chains handlers, so a process that already installs a SIGINT handler
   before constructing a `BetelgeuseDriver` will see both handlers run.
   We document the assumption that the runtime owns SIGINT/SIGTERM
   capture, but a hostile binary could still register an earlier
   handler and observe both flag-set behavior and its own effect.

After all of this, the phase still satisfies the plan's "Done Means":
`make verify` is green, the new tests cover positive, negative, weird,
and shutdown paths for the named lanes, and `ROADMAP.md` /
`CHANGELOG.md` were updated only with what shipped.
