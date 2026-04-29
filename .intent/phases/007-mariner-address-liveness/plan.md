# 007 Mariner Address Liveness Plan

Session:

- A

## Goal

Make address liveness explicit and testable before restart execution lands.
`Address<M>` should identify one isolate incarnation by shard id, isolate id,
and generation. The runtime must reject stale known generations as `Closed`
instead of accidentally delivering to a current incarnation.

## Context

Slice 006 added stored direct parent-child lineage. The next supervision work
needs a safe target model: restarting or replacing a child must not make old
addresses silently point at the replacement.

The first 007 spec draft used "never reuse isolate ids" as the liveness story.
Review and human feedback pushed this slice to add the better foundation now: a
generation token. This slice still does not implement compaction, id reuse,
restart records, or `RestartChildren`.

## Mapping from spec diff to implementation

- Add `AddressGeneration` to `tina`.
- Extend `Address<M>` with a generation field.
- Keep `Address::new(shard, isolate)` as the initial-generation constructor so
  existing manual/synthetic address call sites stay readable.
- Add `Address::new_with_generation(shard, isolate, generation)` for
  runtime-issued addresses and explicit stale-address tests.
- Add `Address::generation()`.
- Store generation on each `CurrentRuntime` registered entry.
- Make `CurrentRuntime::register` and spawn registration return addresses with
  the registered entry's generation.
- Add generation to erased sends and to send-related trace payloads.
- Match runtime ingress and dispatch on both isolate id and generation.
- Treat known-isolate wrong-generation targets as `Closed`.
- Keep unknown isolate ids as programmer errors. They must not become
  `SendRejected` or another recoverable runtime outcome.

## Phase decisions

- `AddressGeneration::new(0)` is the initial generation.
- This slice keeps `CurrentRuntime` assigning fresh isolate ids monotonically.
  It proves generation-aware lookup now, but does not reuse ids yet.
- Wrong generation on a known isolate id is stale-known and returns/reports
  `Closed`.
- Unknown isolate id remains a programmer error. Runtime ingress unknowns record
  no trace; dispatched unknown sends may retain the preceding handler and
  attempted-dispatch trace before the panic propagates, but they do not record
  `SendRejected`.
- `Address::new` remains available for the initial generation. It is useful for
  tests, examples, and synthetic addresses, but runtime-issued current addresses
  should come from registration/spawn or application-level registries.
- Logical names are user-space. The runtime does not add service names,
  aliases, or refresh APIs.

## Proposed implementation approach

1. Update `tina/src/lib.rs`:
   - add `AddressGeneration`
   - add the generation field and accessors to `Address<M>`
   - add doc text explaining incarnation identity
   - update doc tests/assertions that compare addresses
2. Update `tina-runtime-current/src/lib.rs`:
   - add `generation` to `RegisteredEntry`
   - have register/spawn use explicit generation when returning addresses
   - include target generation in `ErasedSend`
   - include target generation in send trace variants
   - update ingress and dispatch lookup to distinguish exact/current,
     stale-known, and unknown
3. Add `tests/address_liveness.rs` for the slice's black-box proofs.
4. Update existing runtime tests for new trace fields and address equality.

## Acceptance

- Runtime-issued addresses expose their generation.
- `Address::new` produces the initial generation.
- Runtime ingress to stopped current-generation address returns `Closed`.
- Runtime ingress to known isolate id with wrong generation returns `Closed`.
- Dispatched send to stopped current-generation address traces
  `SendRejected { reason: Closed }`.
- Dispatched send to known isolate id with wrong generation traces
  `SendRejected { reason: Closed }`.
- Send trace events include target generation.
- Runtime ingress to an unknown isolate id still panics and records no trace
  event.
- Unknown dispatched send still panics without recording `SendRejected`.
- Repeated identical runs produce the same ids, generations, outcomes, and
  trace events.

## Tests and evidence

- Add focused address-liveness integration tests.
- Run `make fmt`.
- Run `make test`.
- Run `make verify` before final acceptance.

## Traps

- Do not make wrong-generation targets unknown. They are stale-known and must be
  `Closed`.
- Do not add logical names, registry APIs, aliases, or address refresh.
- Do not execute `RestartChildren`.
- Do not implement id reuse or compaction.
- Do not omit generation from send trace target payloads.
- Do not silently change cross-shard semantics.
- Do not turn unknown targets into normal trace events.

## Files likely to change

- `tina/src/lib.rs`
- `tina/tests/sputnik_api.rs`
- `tina-runtime-current/src/lib.rs`
- `tina-runtime-current/tests/address_liveness.rs`
- Existing `tina-runtime-current/tests/*.rs` that assert exact send trace events

## Areas that should not be touched

- `tina-mailbox-spsc`
- Tokio/current-thread driver work
- `tina-supervisor`
- simulator/Voyager code
- README/ROADMAP changes outside the already-pending docs cleanup

## Ambiguities noticed during planning

- Event subject identity still uses shard + isolate id. This is acceptable for
  this slice because id reuse/compaction is not implemented. A future slice that
  actually reuses isolate ids must revisit event subject identity before
  enabling reuse.
