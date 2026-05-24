# Phase 134: Config And Budget Manifest

## Status

- Future implementation phase.
- Runs after Phase 120 if it wants the refreshed skeleton.
- One PR.

## Grug Truth

Bounded services have many knobs.

If users cannot see the knobs in one place, they will guess. Guesses become
stupid-high caps or invisible production failures.

## Goal

Make boundedness copyable.

Ship a structured manifest for:

- mailbox caps
- pending reply/call caps
- pool caps
- body byte caps
- DNS/connect policy
- retry/backoff budgets
- deadlines
- shared scopes
- bridge in-flight caps
- event/log sink caps
- replay-affecting config

## Starting Facts

- Capacity reports and discovery lines exist.
- Service pressure reports exist.
- `ReplayCase` carries config when users provide it.
- Systems still configure many caps by local constants.

## Does Not Include

- no automatic tuning
- no memory magic
- no global cross-shard budget
- no config file format war
- no hiding runtime validation behind defaults
- no "unbounded" unless explicit and loud

## Decisions

- User-facing names:
  - `ServiceBudgetManifest`
  - `BudgetSurface`
  - `BudgetManifestReport`
  - `BudgetValidationError`
- Manifest is data first. Builders may help, but the report must be printable
  and diffable.
- Every budget has:
  - name
  - kind
  - unit
  - cap or explicit unbounded policy
  - mode
  - owner/shard label when known
  - whether it affects replay
- Manifest validation rejects:
  - zero caps where zero would mean broken service
  - duplicate names
  - invalid surface names
  - missing required caps for copied service skeleton
  - expired `unbounded_for_now`
- Wrong weight choices are still user bugs. Reports must make them visible.
- Manifest is not a global registry. It is explicit data a service passes into
  setup. Runtime/config adapters may read it, but no hidden singleton owns the
  service.
- Manifest names must match capacity/report names. A mismatch is a testable
  service bug, not "close enough."
- Secrets do not live in the manifest. Paths/env var names may, secret values
  may not.
- The manifest has a schema version so saved replay/capture artifacts can say
  which shape they used.

## Implementation

### Rock 1: Manifest Vocabulary

Add a small module in `tina-runtime`:

- `ServiceBudgetManifest`
- `BudgetSurface`
- `BudgetKind`
- `BudgetUnit`
- `BudgetManifestReport`
- `BudgetValidationError`

Support conversion from existing capacity surfaces where possible.

### Rock 2: Runtime/Service Integration

Make the copied service path accept a manifest:

- mailbox capacity
- listener/session capacity
- pool capacity
- body capacity
- shared scopes
- event sink cap
- DNS/connect policy

Do not require every old API to take the manifest. First form may be explicit
adapter methods that build existing configs.
Add a consistency checker that compares the manifest against a running service
report/capacity summary and returns typed missing/extra/mismatched surfaces.

### Rock 3: Replay And Capture

Include manifest summary in:

- live replay capture metadata
- saved replay case config/projection
- timeline/export metadata when Phase 130 is present

Changing replay-affecting manifest values must change or invalidate replay.
Non-replay-affecting display metadata may change without invalidating replay,
but the report must say it was ignored.

### Rock 4: Systems

Update one canonical service and one small system:

- no scattered capacity constants without manifest entry
- README prints manifest and pressure report
- tests assert manifest names match capacity report names

## Required Proof

- Duplicate surface names fail validation.
- Zero/invalid caps fail validation.
- Explicit unbounded-with-expiry validates before expiry and fails after.
- Secret-looking values are rejected or redacted in display output.
- Manifest can build existing runtime/service configs.
- Service pressure report surfaces line up with manifest surfaces.
- Missing/extra/mismatched report surfaces fail the consistency checker with
  typed rows.
- Replay capture includes manifest metadata.
- Changing a replay-affecting budget changes or invalidates the replay case.
- Changing non-replay display metadata does not change replay, and the report
  says it was non-replay-affecting.
- A cheap-model-style copied service can find all caps in the manifest, not by
  grep across handlers.
