# 029 Review

## Plan Review 1

Verdict: structurally on-shape, not yet ready to hand to implementation.

What looks strong:

- The phase identity is right: Tina's live substrate is now a Tina-owned
  implementation over Betelgeuse, not an upstream-Betelgeuse wishlist.
- The core guarantee is load-bearing and well named: after shutdown/cancel-drain,
  no backend still owns or can write into a Tina completion slot.
- The plan refuses Tokio/Tower/Axum/async-handler drift and refuses to make
  upstreamability block Tina progress.
- It keeps the boundary clean: Betelgeuse is the explicit I/O primitive; Tina
  owns resource ids, completion storage, cancellation tombstones, drain state,
  trace semantics, and lifecycle guarantees.
- It correctly preserves Ranger semantics: TCP lanes, canceled tombstones, and
  requester completion cancellation should survive Surveyor.

Blocking plan findings:

1. **No-hang terminal shape is not pinned enough.**

   The plan says shutdown cannot hang forever without hitting a typed/tested
   terminal error, but it does not say what that error path is, where it lives,
   or what the runtime does with it. Since Surveyor is about removing the leak
   fallback, the alternate terminal state is load-bearing. Pin the intended
   shape before implementation: recommended direction is an internal
   `DriverShutdownError::BackendStillOwnsCompletions`-style result converted by
   the live handle into a typed shutdown/control error, with tests proving both
   the clean drain path and the bounded failure path. If the decision is "Tina
   owns the backend exclusively, so clean drain must always happen," then say
   that and make failure a panic/bug, not a vague typed error.

2. **Native backend claim is too broad for one implementation slice.**

   The plan requires simulated and native Linux/macOS paths to satisfy or gate
   the release guarantee, but it does not decide whether Surveyor must implement
   all three or may close with native explicitly gated behind a documented
   non-claim. That is a big difference. Pin expected direction. Recommended:
   implement and prove the simulated/Tina adapter path first, then either add
   the smallest Betelgeuse release hook used by all backends or explicitly gate
   native `BetelgeuseRuntime` shutdown as not yet no-leak-proved. Do not let the
   phase close with "native audited" if native still has queued/submitted raw
   pointer ownership.

3. **Exclusive backend ownership vs external stepping needs a decision.**

   The plan allows either "external step impossible" or "external step harmless,"
   but the current test surface deliberately shares `SimulatedIO` outside the
   runtime. Surveyor should pick the desired production rule. Recommended:
   production adapter owns the backend exclusively; tests can use an instrumented
   backend handle only through a Tina-owned test seam. External post-shutdown
   stepping should become impossible by construction for the real adapter, not a
   behavior users can rely on.

Useful tightenings:

- Name the likely module boundary, e.g. keep this in `tina-runtime/src/driver.rs`
  unless it grows enough to split `driver/betelgeuse.rs`.
- Add a direct "remove all `mem::forget` leak fallback from Tina shutdown" grep
  proof to Done Means.
- Require preserving the existing stopped-requester oracle parity added by
  Ranger: live and `tina-sim` should still cancel runtime-driver work
  immediately on requester stop.
- Clarify that "completion arena" is allowed only for lifetime safety, not as a
  stealth full allocation-story rewrite.

## Plan Review 2

Verdict: ready to hand to implementation.

The three blockers from Plan Review 1 are closed:

- The no-hang terminal shape is now pinned: clean shutdown requires released
  backend ownership; failed drain returns a typed shutdown/control error such as
  `DriverShutdownError::BackendStillOwnsCompletions` converted through
  `BetelgeuseControlError`.
- Native scope is now honest: simulated/Tina adapter proof comes first; native
  Linux/macOS must either get the same release hook/proof or be explicitly gated
  as not no-leak-proved. "Audited native" is not enough.
- Exclusive backend ownership is now the production rule. External stepping
  behind the adapter is only allowed through Tina-owned test seams.

Useful tightenings also landed:

- `mem::forget` removal is a Done Means grep proof.
- Existing Ranger requester-stop oracle parity must be preserved.
- Completion arena work is scoped to lifetime safety, not a stealth allocation
  rewrite.

Implementation should start with the ownership audit, then the smallest adapter
design that can remove the leak fallback without introducing shutdown hangs.
