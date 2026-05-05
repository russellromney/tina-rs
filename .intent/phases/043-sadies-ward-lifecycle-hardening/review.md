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
