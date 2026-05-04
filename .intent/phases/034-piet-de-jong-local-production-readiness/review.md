# 034 Piet de Jong Plan Review 1

Verdict: right phase, not yet ready to execute. The plan aims at the correct
five gaps, but several load-bearing surfaces are still described as goals
instead of pinned implementation shapes.

## Findings

### [P1] Canonical live runner path is not pinned enough

The plan says "one canonical live app runner path" and names lifecycle states,
but it does not say what type owns that path, where it lives, or how it relates
to existing `BetelgeuseBackedRuntime`, `BetelgeuseBackedMultiShardRuntime`, and
`BridgeHost`. For this phase, that is the center of the rock. Pin the intended
public shape before implementation, with 2-3 sketches such as:

- building a single-shard local service;
- building a multi-shard local service;
- shutting down and inspecting terminal state.

This does not need perfect final names, but it needs enough vocabulary that
implementation cannot accidentally ship three live-runner APIs.

### [P1] Bridge boundary mixes Tower and Axum without a source of truth

The plan says "one canonical Axum/Tower service helper" and then asks for Axum
integration tests. That can turn into an Axum-specific API, a Tower-first API,
or both. Pick the core boundary now. Recommendation: Tower `Service` is the
canonical bridge boundary; Axum is a proof/adapter on top of Tower. That keeps
the bridge useful without making Tina chase one HTTP framework.

Also pin cancellation truth more sharply: a synchronous Tina handler cannot be
preempted mid-turn. The bridge can cancel before admission, skip before handler,
or reject/observe the late response after timeout. It cannot make already
running user code disappear.

### [P1] CI/hardening gate is still a category list

The hardening section lists CI, fast/full/stress/loom/miri/doc/compile-fail, but
does not name the actual required gate. That leaves closeout subjective.

Pin:

- the required GitHub Actions job;
- the local command it mirrors;
- which expensive jobs are required, optional, nightly-only, or manual;
- which platforms are claimed for the live runner in this phase.

Without that, "production hardening" can close with a CI file that runs less
than local `make verify`, or with expensive checks that nobody can afford to
run.

### [P2] Performance envelope lacks a measurement method

The plan asks for numbers but not how to get them. Pick a measurement tier:
allocation probes through the existing global-allocator pattern, release-mode
microbenchmarks, Criterion if acceptable, or a custom benchmark harness if that
fits the repo better.

Also pin whether numbers are regression gates or recorded evidence. Some paths
should probably be "measured and recorded" first, not hard CI thresholds yet,
because wall-clock CI flakes are real bad fire.

### [P2] Local-service API completeness can still scope-creep

The plan lists DNS, TLS, UDP, file, process, signal, and durable state as
explicit deferral topics, but it does not state the expected default. Pin the
default now: Piet supports time/TCP/bridge/health/shutdown/test harness as the
local-service core, Jelle Zijlstra owns DNS/TLS/UDP/file/process/signal, and
Wim Kok owns persistence. Everything else is deferred unless a cross-cutting
Piet workload proves it is required now.

This matters because "normal local services" can quietly mean "half of Tokio's
ecosystem." Grug does not want that rock today.

### [P2] Cross-cutting E2E workloads are not named

The e2e section has the right shape, but no named workloads or files. Add named
targets so implementation and review can tell whether the phase is complete:

- a bridge-hosted HTTP service with overload/cancel/shutdown;
- a runtime-owned TCP/time service;
- a supervised child/restart service;
- a simulator/DST parity version where applicable.

Names matter here because this phase is large. Named workloads stop "we tested
something like it" from becoming closeout fog.

## What Looks Strong

- The phase is aimed at the real bottleneck: local production readiness, not
  Gemini/release prose.
- It keeps Tina's important refusals: no async handlers, no arbitrary futures
  inside isolates, no hidden unbounded bridge queues, no broad Tokio replacement
  claim.
- It treats bridge, runner, hardening, perf, and API completeness as one local
  product surface rather than isolated little patches.
- The target claim is narrow enough to be true if the work lands.
- The non-claims are honest and should survive closeout.

## Suggested Plan Fixes Before Implementation

1. Add a "Pinned Intended Surface" section with the live runner owner/type and
   three user-shaped sketches.
2. State that Tower is the canonical bridge boundary and Axum is the first proof
   adapter, unless you deliberately choose the opposite.
3. Add the bridge cancellation truth table, including "running sync handler is
   not preempted."
4. Name the CI gate and classify expensive jobs.
5. Name the performance measurement method and which numbers are gates versus
   recorded evidence.
6. Pin the default I/O support table: support time/TCP/bridge now, send
   DNS/TLS/UDP/file/process/signal to Jelle Zijlstra, and keep persistence in
   Wim Kok unless forced.
7. Name the cross-cutting e2e workloads that must exist at closeout.

After those edits, grug thinks this is ready to launch.
