# 029 Surveyor Betelgeuse Adapter Ownership Plan

## Purpose

Make Tina's Betelgeuse substrate a Tina-owned implementation over Betelgeuse,
not a hope that Betelgeuse itself grows every production guarantee Tina needs.

Ranger settled the core driver shape: runtime-owned time/TCP, bounded queues,
TCP lanes, per-call cancellation, shutdown cancellation, and live/sim/oracle
parity. It also exposed the remaining sharp edge: Betelgeuse backends can hold
raw pointers to caller-owned completion slots, while Tina's production-shaped
shutdown wants a stronger guarantee:

> after shutdown/cancel-drain returns, no backend still owns or can write into a
> Tina completion slot.

Surveyor should make that ownership guarantee true inside Tina's Betelgeuse
implementation layer.

## Framing

Betelgeuse is useful because it has the right primitive shape: explicit step,
caller-owned completions, no hidden async executor, and small backend objects.
But Tina-rs has now grown a richer contract than raw Betelgeuse exposes.

So the phase direction is:

- keep Betelgeuse as the low-level I/O primitive;
- build a Tina-owned adapter/driver layer over it;
- let that adapter own completion storage, sockets, operation identity,
  cancellation, draining, and shutdown rules;
- do not make Tina correctness depend on upstream Betelgeuse adding a perfect
  public API first.

This is not disrespect to Betelgeuse. It is the normal split:

- Betelgeuse: small explicit I/O substrate.
- Tina runtime: isolate scheduling, bounded mailboxes, Effects, tracing,
  supervision, calls, resource ids, cancellation, replay alignment, and
  production-shaped lifecycle guarantees.

## Starting Baseline

Ranger currently has a safe-but-ugly shutdown fallback:

- mark pending TCP ops canceled;
- close all resources;
- drain completions for bounded steps;
- if any completion slot may still be backend-owned, intentionally leak the
  slots rather than dropping boxes under raw backend pointers.

That is the correct safety posture for Ranger, but it is not the desired final
adapter story.

Surveyor should remove the need for that leak fallback by changing ownership,
drain, or backend access rules so Tina can prove completion slots are released.

## Expected Direction

Default path: implement a Tina-owned Betelgeuse adapter inside `tina-runtime`.

Expected shape:

- the adapter owns the `IOLoopHandle`, all sockets, all completion slots, and all
  operation bookkeeping;
- user isolates and higher runtime layers never see Betelgeuse handles;
- tests may use a controlled simulated backend handle, but not one that can step
  behind the adapter after adapter shutdown unless the adapter explicitly permits
  and proves it;
- completion slots live in stable adapter-owned storage until the adapter has
  proof the backend no longer owns their pointers;
- canceled operations become tombstones until the backend releases the pointer;
- shutdown has an explicit drain state with a terminal "all backend ownership
  released" condition.

If this requires tiny vendored Betelgeuse hooks, add them as backend-generic I/O
hooks with no Tina concepts. But do not make upstreamability the phase goal.

## Scope

### 1. Ownership Boundary

Draw the boundary between Tina and Betelgeuse precisely.

The adapter should own:

- completion slots;
- resource ids and Betelgeuse socket handles;
- operation ids and lane identity;
- cancellation tombstones;
- shutdown/drain state;
- any backend-specific "released" accounting.

Betelgeuse should only be asked to:

- arm operations;
- step the backend;
- close low-level resources;
- report completion readiness/results through the existing completion objects
  or a tiny vendored helper if necessary.

### 2. No-Leak Shutdown

Replace Ranger's leak fallback with a real release proof.

Required result:

- no `mem::forget` of pending completion slots in normal Tina shutdown;
- no completion box dropped while the backend may still own its pointer;
- shutdown cannot hang forever without hitting a typed/tested terminal error;
- once shutdown reports complete, later legal backend activity cannot write into
  dropped Tina memory.

The exact shape can be:

- drain-until-empty over a driver-owned backend with no external stepping; or
- backend release accounting; or
- adapter-owned completion arena whose slots outlive all backend ownership; or
- a tiny Betelgeuse cancel/drain hook.

Pick the smallest honest shape after reading the current code.

### 3. Simulated Backend Contract

Make the simulated path prove the ownership model directly.

Required proofs:

- pending accept/read/write shutdown releases completion ownership;
- canceled tombstones drain without requester delivery;
- stepping a controlled simulated backend after adapter shutdown is either
  impossible by construction or proven harmless;
- delayed completions cannot resurrect canceled operations;
- no test relies on sleeps or "probably drained" timing.

### 4. Native Backend Contract

Audit and harden native Linux/macOS backend behavior enough for Tina's claim.

For Linux/io_uring, distinguish:

- queued but not submitted completions;
- submitted completions with kernel CQE still possible;
- retry/requeue paths;
- close-fd cancellation effects.

For macOS/kqueue, distinguish:

- queued completions;
- watched fd events;
- retry/wait paths;
- close/unwatch behavior.

If native backend cannot provide no-leak release without a new hook, pause and
write the hook. Do not preserve a silent leak fallback as "done."

### 5. Driver Surface Cleanup

Keep `RuntimeDriver` small, but make its lifecycle contract explicit.

Likely contract language:

- `cancel(call_id)` cancels requester completion and quiescence pressure for one
  operation;
- `cancel_pending()` begins shutdown cancellation for all operations;
- after `cancel_pending()` returns, the driver may be dropped without leaking
  completion slots or leaving backend-owned pointers to Tina memory;
- `has_pending()` excludes canceled operations for runtime quiescence, but the
  driver still tracks canceled tombstones internally until released.

Do not add public user-facing APIs for this.

### 6. Tina/Odin Alignment

Use original Tina/Odin as design pressure, not as a line-for-line copy.

Surveyor should move Tina-rs closer in one specific way:

- resource and completion ownership belongs to the runtime/substrate layer, not
  user code;
- shutdown and cancellation are part of the runtime contract, not incidental
  cleanup.

Do not try to finish Tina-Odin's arena/no-allocation/trap-boundary story in this
phase. Name those as later production-hardening if they remain.

## Build Order

1. **Ownership audit.** Read Tina's Betelgeuse driver plus vendored simulated,
   Linux, and macOS backends. Record every place a backend stores a raw
   completion pointer.
2. **Adapter design.** Choose the smallest Tina-owned adapter shape that can
   prove release without leaking. Write the decision in `review.md`.
3. **Simulated implementation.** Make the simulated path satisfy the new
   ownership/drain contract first.
4. **Native implementation.** Harden Linux/macOS behavior or add the smallest
   vendored Betelgeuse hook needed for release accounting.
5. **Remove leak fallback.** Delete Ranger's `mem::forget` shutdown escape hatch
   from the Tina driver.
6. **Regression proofs.** Add direct tests for pending accept/read/write
   shutdown, canceled tombstone release, delayed completion after cancel, and
   adapter drop safety.
7. **Verification and review.** Run focused tests and `make verify`. Record
   remaining non-claims in `review.md`.

## Refusals

- Do not build a Tokio bridge.
- Do not add Tower/Axum/Hyper integration.
- Do not make isolate handlers async.
- Do not expose Betelgeuse handles to user isolates.
- Do not make upstream Betelgeuse acceptance block Tina progress.
- Do not hide a remaining leak behind "bounded rare shutdown leak" and call it
  done.
- Do not claim zero-allocation runtime hot paths unless directly measured.
- Do not broaden into arena/envelope redesign unless cancellation ownership
  cannot be solved otherwise.

## Pause Gates

Pause and record a decision if:

- native io_uring/kqueue cannot release completion ownership without a
  meaningful Betelgeuse hook;
- the smallest hook would effectively redesign Betelgeuse;
- no-leak shutdown requires accepting possible infinite hang;
- adapter ownership requires changing public Tina runtime APIs;
- completion arena work starts becoming a general no-allocation runtime rewrite;
- simulated and native release semantics diverge in a way tests cannot explain.

## Proof Bar

Direct proof is required for:

- stopped requester with pending timer/accept/read/write still quiesces;
- runtime shutdown with pending timer/accept/read/write releases completion
  ownership;
- late simulated completion after cancel/shutdown does not deliver a message and
  cannot write into dropped Tina-owned memory;
- same-stream read/write lane behavior from Ranger still holds;
- `make verify` remains green.

Helpful extra proof if feasible:

- a focused Miri or sanitizer-shaped test around adapter drop with pending
  simulated completions;
- an allocation probe showing no shutdown-slot leak on the exercised simulated
  paths.

## Done Means

- Tina has a named Betelgeuse adapter/implementation layer with explicit
  completion ownership.
- Ranger's shutdown leak fallback is gone.
- `RuntimeDriver` lifecycle docs state the release guarantee.
- Simulated and native Betelgeuse paths satisfy or explicitly gate the release
  guarantee.
- Focused tests prove cancel/drain/drop behavior from a user-shaped runtime path.
- `review.md` records remaining production non-claims, especially allocation,
  trap-boundary, and broader adapter work.
- `make verify` passes.
