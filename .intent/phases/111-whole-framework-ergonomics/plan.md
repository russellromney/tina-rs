# Phase 111: Whole-Framework Ergonomics

## Status

- IDD implementation phase.
- Runs after several 103-109 slices land enough real user proof.

## Grug Truth

The nouns are good. There are too many of them.

Tina needs a copied path for whole services, not only good local helpers.
Ergonomics may remove ceremony. It must not hide `Full`, `Closed`, `Timeout`,
cancel, capacity, or trace truth.

## Goal

Make Tina feel coherent end to end:

- one obvious prelude
- one service skeleton
- one config/budget manifest shape
- one way to call, defer, cancel, drain, report, and shut down
- fewer raw generic/turbofish call sites
- better compiler errors
- better copied examples

## Non-Goals

- No async/await cosplay.
- No hidden retry.
- No hidden queues.
- No flow macro that hides suspension/failure.
- No broad renaming churn unless it removes real confusion.

## Rocks

### Rock 1: Noun Cleanup Into Code

Clean up the public noun surface:

- group imports into prelude tiers
- add aliases only where they remove repeated generic noise
- remove superseded helpers from docs
- mark escape hatches clearly
- make internal/public names harder to confuse

The deliverable is code/docs/specimen changes, not an audit artifact.

### Rock 2: Copied Service Skeleton

Create one blessed skeleton:

- config manifest
- runtime setup
- HTTP/HTTPS listener
- DB/outbound bridge or pool
- request-scoped cancellation
- health/readiness
- shutdown
- topology/capacity summary
- DST seed/replay hook

This skeleton should be boring enough for LLMs to copy.

### Rock 3: Fluent But Honest Workflows

Add small helper chains for common flows:

- defer request and reply later
- defer cancelable work after bounded admission
- race/join with named branches
- recurring tick with missed-tick policy
- bounded pressure handling

Each helper expands to normal Tina messages/effects. Failure policy remains in
source.

### Rock 4: Error And Diagnostic Polish

Improve user-facing messages:

- wrong handler lane
- non-`Send`
- non-`'static`
- reply mismatch
- missing capacity/config
- unsupported extension path

Pin the message shape with compile-fail tests where Rust allows it.

### Rock 5: Specimen Rewrite Pass

Rewrite a small selected set using the blessed path:

- `mini_saas_api`
- `system_realtime_rooms`
- `system_job_queue`
- one bridge-heavy specimen
- one protocol-heavy specimen

Delete stale README guidance and move resolved pain to history/changelog.

## User Proof

Run the "cheap model proof":

- give the skeleton and docs to a fresh model/session
- ask it to build a small service with HTTP + DB + outbound + shutdown
- record what it got wrong
- fix docs/helpers until the mistakes are compile errors or copied-path fixes

## Required Proof

- Selected systems still pass smoke/load proof.
- Compile-fail tests cover new diagnostics.
- Docs contain a one-page "which noun do I use?" guide.
- No helper hides `Full`/`Closed`/`Timeout`/cancel outcomes.
- At least one system gets meaningfully shorter without losing trace/capacity
  truth.

## Done Means

A new Tina developer or LLM can build a normal service by copying one path, and
the compiler/runtime catch the common wrong turns loudly.
