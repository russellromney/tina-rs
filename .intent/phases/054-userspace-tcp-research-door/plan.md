# Phase 054: Userspace TCP Research Door

## Goal

Name the userspace TCP door without promising to walk through it.

Tina should not pretend to be Seastar DPDK today.

054 answers:

> What contract would a future kernel-bypass/userspace TCP backend need to
> satisfy, and what proof would make it worth building?

Near-grug:

> leave door. do not build city.

## Baseline

Expected before this matters:

- Betelgeuse native Linux path runs in CI;
- native HTTP/RPC have real pressure cases;
- cost smoke shows where kernel TCP is or is not enough;
- capability reports can tell backend truth.

Do not start this phase until at least one native HTTP/RPC pressure case
suggests kernel TCP is the actual bottleneck.

## Non-Goals

- No DPDK implementation.
- No userspace TCP implementation.
- No packet parser.
- No NIC driver.
- No performance claim.
- No public API promise.
- No launch blocker.
- No code changes required.

## Rules

- Userspace TCP remains later research until measurements justify it.
- Kernel TCP plus `io_uring` is the main path.
- Any future backend must preserve Tina semantics: bounded commands, explicit
  progress, runtime-owned buffers, visible cancellation, visible shutdown, and
  capability truth.
- Platform truth beats ambition.
- "No" is a valid answer if evidence does not justify the work.

## Rocks

1. **Contract Sketch**

   Required questions:

   - what resource ids look like;
   - how packet/socket progress enters Tina;
   - how buffers are owned;
   - how cancellation works;
   - how shutdown works;
   - how capability reports expose backend differences;
   - how simulator would model it.

2. **Need Test**

   Define what evidence would justify userspace TCP.

   Examples:

   - kernel TCP/io_uring cannot meet latency or memory goals;
   - HTTP/RPC cost rows show substrate bottleneck;
   - target workload requires NIC-level control;
   - future deployment explicitly wants DPDK-like shape.

3. **Risk List**

   Name the pain:

   - portability;
   - safety;
   - ops complexity;
   - NIC support;
   - security updates;
   - test matrix;
   - simulation mismatch.

4. **No-Claim Docs**

   Update docs only to say:

   - Tina does not ship userspace TCP;
   - Tina's model could host such a backend later;
   - kernel TCP over Betelgeuse is the real path now.

## Required Proof

- Contract sketch exists.
- Need-test criteria exist.
- No code claims userspace TCP support.
- Roadmap marks this as later research, not planned launch work.
- Output artifact exists, likely
  `docs/research/userspace-tcp.md` or this phase's `research.md`.

## Done Means

- The gap is named honestly.
- Future workers know what would justify the work.
- Tina does not cosplay DPDK.
- The phase may close with "not worth building yet."
