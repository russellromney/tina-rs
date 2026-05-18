# Phase 114: Framework Ergonomics After 110-113

## Status

- Ready for implementation after phases 110-113.
- One PR.

## Goal

Make the copied Tina service path read like the user's job, not the
runtime's plumbing.

Users think:

- "let these callers wait for the same work";
- "start this request and track it";
- "close/drain this service";
- "read pressure and trace facts";
- "write a bridge the normal way".

They do not start with:

- deferred slot;
- pending table;
- ticket;
- cancel handle;
- bridge worker terminal outcome.

This phase keeps the precise lower-level nouns. It adds and documents
workflow front doors over the nouns that are already proven by systems.
No hidden queues. No callbacks that mutate state off to the side. No fake
async.

## Inputs From Findings

Roll in these current findings:

- `examples/FINDINGS.md` 21: `WaitList` shipped; now give users the
  front-door name for "many callers wait on one key".
- `system_cache_with_fill` and `ergonomics_playground`: both prove the
  same single-flight/cache-fill shape.
- `system_job_queue`: cancelable request work is safe, but docs should
  lead with "run request" and only then show `PendingCancelableCallSet`.
- `system_metrics_shipper`: batching/drain helpers are good, but the
  copied shutdown/workflow text needs to name the service job first.
- `examples/FINDINGS.md` 26: runtime-call completions are now documented as
  ordinary `handle` messages. Add the regression proof and move the finding
  to Closed.
- `examples/FINDINGS.md` 32/33: bridge author vocabulary exists; make the
  docs show the bridge author's job before the shared traits.

Do not roll in these larger future items:

- scatter/gather builder;
- paired registration;
- shared scope registry;
- DST adapters for scopes/sinks;
- AWS bridge internal state-machine factoring;
- cross-isolate request/event typestate beyond what already exists.

Those are real, but not this cleanup pass.

## User-Facing Naming Rule

Application authors see workflow names first:

- `SharedWork` for callers waiting on one key;
- `CancelableWork` for cancelable request work;
- existing lifecycle helpers for service stop;
- `PressureReport` / `CapacitySummary` for cap facts;
- `TraceQuery` / existing trace helpers for trace facts;
- `BridgeInstall` / `BridgeCloser` only after "install a bridge" is
  shown.

Mechanism names stay available for advanced code:

- `RequestContext`;
- `DeferredReply`;
- `PendingReplies`;
- `PendingCancelableCallSet`;
- `WaitList`;
- `PoolLease`;
- `RuntimeEventKind`.

Docs must introduce the workflow name first, then say which mechanism it
uses.

## Rock 1: `SharedWork`

Add a small public `SharedWork<K, R>` front door in `tina-runtime`.

It wraps `WaitList<K, R>` and keeps the same guarantees:

- fixed global capacity;
- optional per-key capacity;
- FIFO per key;
- typed `Full` and `KeyFull`;
- caller authority returned on rejection;
- ticket cannot be forged;
- capacity report and snapshot;
- no hidden fill state;
- no hidden upstream work.

Preferred copied shape:

```rust
match self.fills.wait(key.clone(), call) {
    Ok(ticket) => {
        if self.fill_in_flight.insert(key.clone()) {
            request_effect_after_shared_wait(&ticket, self.start_fill(key))
        } else {
            request_effect_after_shared_wait(&ticket, noop())
        }
    }
    Err(SharedWorkError::Full { call, .. }) => call.reply(Reply::Busy),
    Err(SharedWorkError::KeyFull { call, .. }) => call.reply(Reply::Busy),
}
```

This is deliberately not a full `SingleFlight` scheduler. The service
still owns:

- whether work is in flight;
- fill generation;
- stale fill policy;
- upstream effect;
- reply value.

`SharedWork` only owns "park callers for this key and reply them later".

Implementation:

- `SharedWork` is a newtype over `WaitList`, not a separate storage engine.
- Keep `WaitList` public.
- Provide `request_effect_after_shared_wait(ticket, effect)` as the
  workflow-named sibling of `request_effect_after_wait_park`.
- Re-export from the same place as `WaitList`.
- Add rustdoc that says when to use `SharedWork` vs raw `PendingReplies`.

Proof:

- unit tests mirror the important `WaitList` facts through the new name:
  full, per-key full, FIFO, stale ticket, closed caller sweep, capacity
  report live count;
- compile-fail: user cannot forge a `SharedWorkTicket`;
- `system_cache_with_fill` uses `SharedWork`;
- `ergonomics_playground` uses `SharedWork` for its cache-fill probe.

## Rock 2: Request Work Copy Path

Make cancelable request work read as "start request" in docs and examples.

Do not rename or remove `PendingCancelableCallSet`.

Do:

- make `CancelableWork<K, Q, R>` the first documented path when many live
  requests may share one natural key;
- keep `PendingCancelableCallSet<K, Q, R>` as the stricter one-entry-per-key
  table;
- document the difference in user words:
  - `PendingCancelableCallSet`: one active request for this key;
  - `CancelableWork`: many active requests grouped by this key.
- add a copied example that starts cancelable work, admits it, handles
  `Full` / `KeyFull`, removes by ticket, and replies to the original
  caller.

If `system_job_queue` still uses the stricter set and that is correct,
leave it. Add a README note: retry/new-attempt semantics require the
many-entry form, not the one-entry set.

Proof:

- `CancelableWork` tests cover many entries under one key, per-key full,
  global full, stale ticket does not remove a newer entry, drain returns all
  tokens, and capacity report counts live entries;
- one system README points to the right choice by user intent.

## Rock 3: Service Pattern Docs

Refresh the user guide so the first copied path is a service workflow, not
a noun glossary.

Update:

- `docs/tina-user-guide/10-service-patterns.md`;
- `docs/tina-user-guide/11-ergonomics-checklist.md`;
- `docs/tina-user-guide/12-outcome-glossary.md`;
- any page that still leads a common workflow with raw `PendingReplies`
  where `SharedWork`, `CancelableWork`, or `RequestContext::defer` is the
  copied path.

Required boxes:

- "Many callers wait for one result" -> `SharedWork`;
- "One cancelable request is running" -> `PendingCancelableCallSet`;
- "Many cancelable requests share a natural key" -> `CancelableWork`;
- "Reply later to the current caller" -> `call.defer(...).reply(...)`;
- "Close/drain on stop" -> existing drain/lifecycle helpers;
- "Write a bridge" -> install, close, drain, metrics, pressure.

Each box must have:

- what user is trying to do;
- preferred helper;
- what stays visible;
- one small copied snippet;
- what not to use.

## Rock 4: Bridge Author Copy Path

Digest phase 113 into a bridge-author page section.

Show the job first:

1. define config and validate caps;
2. install worker and return handles;
3. expose closer;
4. expose metrics;
5. expose pressure;
6. classify outcomes;
7. prove close/drain and late-result truth.

Then map those jobs to:

- `BridgeInstall`;
- `BridgeCloser`;
- bridge-specific `close_and_drain`;
- metrics handle;
- pressure report;
- classifier.

Do not invent a bridge framework. The bridge crates still own their real
messages and worker state machines.

Proof:

- rustdoc for `BridgeInstall` / `BridgeCloser` matches the real trait
  methods;
- one non-AWS bridge README or crate docs links to the bridge-author
  section;
- one AWS bridge README or crate docs links to the same section.

## Rock 5: Runtime Completion Regression And Findings Cleanup

Add a regression for finding 26:

- an isolate receives a call in `handle_call`;
- it returns a runtime call effect whose continuation message is an internal
  event;
- the continuation is delivered to `handle`, not `handle_call`;
- the original caller receives the final reply.

Use a tiny timer or observed-send call so the proof stays hermetic. Name the
test:

```text
runtime_call_returned_from_handle_call_completes_as_event
```

Update findings so they tell the current truth.

Required moves:

- Close or rewrite finding 21 around `SharedWork`.
- Move finding 26 to Closed and cite the regression above.
- Add a short "workflow front doors" note to the active list only if
  something remains open after this phase.
- Move stale solved pain to Closed or history.

Do not leave "build X" in Active when this phase ships X.

## Rock 6: System Rewrites

Migrate exactly these systems:

- `examples/systems/system_cache_with_fill`;
- `examples/systems/ergonomics_playground`;
- `examples/systems/system_webhook_relay` README only, as the bridge-heavy
  pointer to the bridge-author copy path.

Keep rewrites small. The goal is to prove the names make the copied path
clearer, not to redesign the systems.

Each rewritten README must include:

- what got shorter or safer;
- what stayed explicit;
- which helper is now the blessed copied path;
- any remaining rough bit.

## Rock 7: Compile-Fail / Agent-Proof Rails

Add compile-fail tests for the mistakes this phase is trying to prevent:

- cannot forge a `SharedWorkTicket`;
- cannot turn a plain `noop()` into a request effect with the shared-work
  helper;
- the preferred cancelable deferred helper returns the child effect only
  from the `Ok` path after admission; keep the lower-level escape hatch
  documented as advanced and not copied;
- docs compile for the copied snippets.

If a mistake cannot be made impossible yet, docs must say that loudly and
the finding must stay open.

## Required Verification

Run at least:

```sh
cargo fmt --all --check
cargo test -p tina-runtime shared_work --lib -- --nocapture
cargo test -p tina-runtime --test workflow_pending_ergonomics -- --nocapture
cargo test -p tina-runtime --test compile_fail -- shared_work
cargo test -p tina-runtime runtime_call_returned_from_handle_call_completes_as_event --lib -- --nocapture
cargo test --manifest-path examples/systems/system_cache_with_fill/Cargo.toml
cargo test --manifest-path examples/systems/ergonomics_playground/Cargo.toml
cargo test --manifest-path examples/systems/system_webhook_relay/Cargo.toml
cargo clippy -p tina-runtime --lib -- -D warnings
cargo clippy --manifest-path examples/systems/system_cache_with_fill/Cargo.toml --all-targets -- -D warnings
cargo clippy --manifest-path examples/systems/ergonomics_playground/Cargo.toml --all-targets -- -D warnings
cargo clippy --manifest-path examples/systems/system_webhook_relay/Cargo.toml --all-targets -- -D warnings
```

Run broader checks if public exports or docs move:

```sh
RUSTDOCFLAGS="-D warnings" cargo doc -p tina-runtime --no-deps
```

## Done Means

- A new user sees "shared work" before `WaitList`.
- A new user sees "start cancelable request work" before
  `PendingCancelableCallSet`.
- The systems prove the names in real code.
- Findings do not describe shipped pain as current.
- No helper hides overload, cancellation, caller authority, timeout,
  pressure, or trace truth.
