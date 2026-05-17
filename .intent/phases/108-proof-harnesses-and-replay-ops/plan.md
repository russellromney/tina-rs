# Phase 108: Proof Harnesses And Replay Ops

## Status

- IDD implementation phase.

## Grug Truth

Unit tests are not enough.

Network services fail under slow peers, resets, bursts, shutdown races, and
weird orderings. Tina's edge is that those facts should become replayable.

## Goal

Build proof harnesses that make Tina bugs cheap to find:

- load/soak harness
- bad-peer harness
- live trace to sim replay workflow
- replay shrink workflow
- real-client e2e gates

## Non-Goals

- No benchmark theater.
- No giant fuzz platform.
- No flaky "sleep and hope" tests.
- No hidden global test server.

## Rocks

### Rock 1: Load/Soak Harness

Add a small harness for repeated requests/sessions:

- concurrency
- duration or operation count
- max latency summary
- pressure summary
- leak check hook
- timeout/failure counts

Use it on at least HTTP or WebSocket plus one bridge/pool path.

### Rock 2: Bad-Peer Harness

Build reusable bad-peer clients:

- half-close
- reset
- slowloris
- stalled writer
- stalled reader
- malformed frame
- reconnect storm
- TLS failure

Each scenario returns typed observed facts, not log scraping.

### Rock 3: Live Trace To Sim Replay

Add a workflow for saving live facts into replay cases:

- seed/config/history/trace hash
- runtime config visible
- pressure facts visible
- helper to discover constants
- helper to shrink and refresh constants

No ambient defaults hidden in replay.

### Rock 4: CI Gate Shape

Add command targets:

- fast PR proof target
- slow soak target
- local bad-peer target
- replay-regression target

Keep commands copyable.

### Rock 5: Specimen Feedback Loop

Systems specimens must have:

- smoke test
- load or bad-peer proof for networked systems
- findings section
- roadmap pointer for discovered rough bits

Make the harness easy enough for cheap model sessions to use.

## Required Proof

- One load/soak test catches pressure without flaking.
- One bad-peer test proves a real close/reset path.
- One live trace is saved and replayed in simulator.
- Shrink helper returns refreshed count/hash.
- CI commands documented.
- Harness failure output says what cap/event/lifecycle fact failed.

## Done Means

A weird production-ish failure can become a saved case that any session can
replay, shrink, and fix.
