# Phase 060: Betelgeuse Adapter Safety

## Goal

Make Tina's Betelgeuse adapter more paranoid about completion-slot and resource
lifetimes.

Betelgeuse is the canonical portable live substrate for Tina. This phase does
not replace it. It tightens the boundary so Tina can keep making its own
semantic promises even when the backend still owns completion pointers or
finishes work late.

060 answers:

> Can Tina close, cancel, and shut down live resources without ever dropping
> storage the backend may still touch?

Near-grug:

> Betelgeuse good rock. Keep rock. Add bolts.

## Baseline

Already exists:

- `tina-runtime` hides Betelgeuse behind `RuntimeDriver`;
- isolate code never sees raw sockets, files, fds, `IOSocket`, or completion
  slots;
- `Runtime::step` advances the driver, then delivers completions as ordinary
  messages;
- completion slots are heap-allocated so Betelgeuse raw pointers stay stable
  when Tina moves pending entries;
- shutdown reports when the backend still owns completion slots;
- close-cancel paths trace `ResourceClosed` for requester-visible truth.

Sharp edge:

- TCP close currently marks same-stream read/write calls `ResourceClosed`, then
  removes the stream and closes the socket while the backend completion slot may
  still be pending;
- listener close has the same shape for pending accept;
- files may need the same audit for close/removal while read/write/fsync/size
  calls are pending;
- boxed per-operation completions are correct but not the eventual hot-path
  Betelgeuse style.

## Non-Goals

- No replacing Betelgeuse.
- No North Sea / `io_uring` implementation.
- No public API change unless required for truthful reporting.
- No new runtime feature.
- No broad performance claim.
- No completion-slab optimization in the first cut unless the safety work makes
  the shape obvious.
- No userspace TCP.

## Rules

- Tina user semantics stay the same: close wins and pending requester calls
  settle visibly as `ResourceClosed`.
- Backend lifetimes become more conservative: if the backend may still own a
  completion pointer or resource reference, Tina keeps the backing storage alive.
- No hidden retry.
- No hidden queue.
- Terminal reports must keep naming stuck backend ownership.
- Simulator behavior remains the semantic oracle; live driver hardening must not
  invent a different user-visible contract.
- Tests should be meaner than the happy path.

## Rocks

1. **Driver Contract Note**

   Write down the live-driver lifetime contract.

   Must say:

   - backend may own a completion pointer after Tina stops waiting for the
     requester;
   - Tina must keep completion storage alive until backend completion/cancel
     release;
   - Tina must keep resource storage alive when the backend may still touch that
     resource;
   - user-visible close may complete before backend release;
   - stuck backend ownership appears in terminal/resource reports.

   Put this near `tina-runtime/src/driver/mod.rs` docs and link from the I/O
   model docs if useful.

2. **Closing Resource Tombstones**

   Add internal closing/tombstone state for resources that have been closed by
   Tina but may still be referenced by pending backend work.

   First targets:

   - TCP streams with pending read/write;
   - TCP listeners with pending accept;
   - files with pending read/write/fsync/size if audit says Betelgeuse may still
     need the file object.

   Desired internal shape:

   ```text
   Open resource table
   Closing resource table/tombstones
   Pending op records owning/pointing at backend completion slot
   Backend completion drains
   Tombstone drops
   ```

   User-visible shape must remain:

   ```text
   close succeeds
   pending read/write/accept completes to caller as ResourceClosed
   late backend completion is swallowed
   terminal report names stuck backend ownership if it cannot drain
   ```

3. **Close-Cancel Lifetime Tests**

   Pin live behavior for each risky lane.

   Required:

   - close stream while read pending;
   - close stream while write pending;
   - close listener while accept pending;
   - close file while file op pending, if file close/removal can race with
     pending Betelgeuse file completions;
   - late completion after close is swallowed, not delivered as success/error;
   - resource report shows backend-owned pending work until release;
   - terminal shutdown report still names stuck backend ownership.

4. **Hostile Fake Backend**

   Add a tiny fake Betelgeuse-shaped backend or driver test helper that can
   delay completion release after close/cancel.

   It should prove:

   - Tina does not drop completion storage too early;
   - Tina does not drop resource storage too early;
   - cancelling requester interest is separate from backend release;
   - late completion after close cannot revive a closed resource.

   If the real Betelgeuse API is too concrete for a fake backend, write the
   narrowest internal test seam that proves the same contract without changing
   public API.

5. **Completion Slot Allocation Plan**

   Do not optimize blindly, but record the next shape.

   Candidate later work:

   - per-driver completion slab;
   - per-resource read/write/accept slots;
   - reusable completion objects;
   - stable slot IDs in traces/reports;
   - no per-op `Box` on hot TCP/file paths after warm-up.

   This rock can be a design note if safety work is the whole phase.

6. **Docs: Betelgeuse Is Canonical Portable Backend**

   Update wording so Betelgeuse does not read like a disposable placeholder.

   Desired wording:

   ```text
   Betelgeuse is Tina's canonical portable live backend.
   Linux uses Betelgeuse io_uring.
   North Sea means proving and tuning that Linux path.
   A duplicate Tina-owned io_uring backend is evidence-gated.
   ```

## Required Proof

- Existing runtime/TCP/file tests still pass.
- New close-cancel tests prove resource tombstones or equivalent lifetime
  protection.
- Shutdown report tests still show backend-owned pending work truthfully.
- No public API surface changes unless explicitly justified in the review.
- Docs say Betelgeuse is the portable backend, not a temporary embarrassment.

## Done Means

- Closing a resource with pending backend work is lifetime-safe by construction.
- User-visible close/cancel semantics stay unchanged.
- Tina can honestly say it keeps Betelgeuse completion/resource storage alive
  until backend release.
- Future Linux Betelgeuse / North Sea proof work has a clearer driver contract
  to satisfy.
- The next pressure phases can lean harder on live close/cancel behavior.
