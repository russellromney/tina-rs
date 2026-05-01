# 021 Single-Shard Devex And Call Ergonomics Plan

Session:

- A

## What We Are Building

Finish the next single-shard Tina surface pass: make ordinary Tina code read
materially closer to Tokio while keeping Tina's explicit message/effect
semantics intact.

Named phase goal:

- **Tokio-level readability, Tina-level semantic honesty**

Concretely, this package pins and proves:

- the preferred public names for the single-shard surface
- the typed runtime-call helper path as the main user path
- plain effect helpers (`send`, `reply`, `spawn`, `stop`, `batch`, `noop`)
- a smaller self-addressing surface (`ctx.me()`, `ctx.send_self(...)`)
- a tiny isolate declaration helper (`tina::isolate_types! { ... }`) if it
  proves it removes obvious slab boilerplate without hiding semantics
- README and public examples that use complete, grounded snippets instead of
  unexplained framework fragments
- user-facing proofs that compile and run through the preferred path

## What Will Not Change

- Tina stays message-driven. Later work still comes back as an ordinary later
  message, not as hidden `await`.
- Handlers stay synchronous.
- Boundedness, explicit failure, runtime-owned I/O/time, and replayability stay
  visible.
- This slice does not guess multi-shard routing/placement ergonomics.
- This slice does not add a Router, Locator, Registry abstraction, or fake RPC
  surface.
- This slice does not add macro magic unless helper APIs leave obvious,
  mechanical repetition behind after the rest of the plan lands.

## How We Will Build It

- Keep one preferred way to do the common path.
  - Preferred path: `tina::prelude::*`, plain effect helpers, typed call
    helpers such as `sleep(...).reply(...)`, and the concise public runtime
    names.
  - Lower-level path: `RuntimeCall::map_result(...)`.
  - Escape hatch: raw `RuntimeCall::new(...)` plus `CallOutput` extractors.
- Do not ship multiple overlapping micro-DSLs.
  - Public call combinator for 021 is `reply(...)`.
  - `then(...).or(...)` and similar variants stay out.
- Public docs and examples must not introduce unexplained placeholder nouns out
  of nowhere.
  - No floating `FooMsg::Bar` fragments with no nearby enum definition.
  - No README snippets that require the reader to guess what the message
    variants are.
- In docs and first-contact examples, prefer local enum names like `Message` or
  `Event` unless multiple isolate vocabularies are on screen and a more
  specific name genuinely helps.
- If a snippet needs more than one message vocabulary, define each vocabulary
  immediately above the code that uses it.
- Prefer smaller, more humane helper names when they preserve the same truth.
  - `ctx.me()` is the preferred self-address surface.
  - `ctx.send_self(...)` is the preferred self-rearm / self-loop surface.
  - Avoid teaching `ctx.current_address()` in the preferred path.
- If one tiny macro can delete repeated associated-type sludge without hiding
  any meaning, that is acceptable in 021.
  - `tina::isolate_types! { ... }` is the intended ceiling here, not the
    start of a second macro-heavy framework language.
- README should show tiny complete examples first and link to larger runnable
  examples second.
- README should explain what the reader is about to see before showing the
  snippet. First-contact examples should read like onboarding, not like puzzle
  fragments.
- Keep the preferred path fully backed by compile-tested and e2e-tested proof,
  not just prose.

## Preferred Vocabulary

The canonical single-shard Tina vocabulary after 021 is:

- `Runtime`
- `RuntimeCall`
- `CallInput`
- `CallOutput`
- `CallError`
- `Outbound`
- `ChildDefinition`
- `RestartableChildDefinition`
- `with_initial_message(...)`
- `ctx.me()`
- `ctx.send_self(...)`
- `tina::isolate_types! { ... }`

020 multi-shard work should adopt this vocabulary verbatim unless intent
changes first.

## Build Steps

1. Add the 021 intent artifacts to this worktree so the plan exists next to the
   implementation it is shaping.
2. Pin the self-address surface to `ctx.me()` and remove older wording from the
   preferred path.
3. Pin the self-loop surface to `ctx.send_self(...)` where it reads better than
   spelling `send(ctx.me(), ...)`.
4. Sweep high-signal public examples/docs to the new readability bar:
   - README
   - crate-level docs in `tina`
   - preferred-surface tests
   - downstream consumer tests that intentionally prove the user path
5. Rewrite README's "What Tina code looks like" section so it uses complete
   tiny examples:
   - one local-state/send example
   - one timer/backoff example
   - a link to the larger TCP example rather than a floating half-state-machine
     fragment
   - explain what the reader is seeing before showing the code
6. Make the preferred user-shaped proofs read like the preferred user surface.
7. Add the tiny isolate declaration helper if the associated-type slab is still
   the loudest remaining source of user-visible ceremony.
8. Update `.intent/SYSTEM.md` if the preferred surface description or example
   wording still teaches the older, rougher path.
9. Run focused tests, downstream consumer tests, runnable examples, and
   workspace verification.
10. Report back with:
   - exact public-surface simplifications shipped
   - exact user-facing proofs updated
   - remaining taste violations that are still real and worth a later phase

## How We Will Prove It

Direct proof for the changed behavior:

- compile-tested preferred-surface proofs in `tina/tests/`
- downstream live-runtime consumer proof using:
  - `Runtime`
  - `register_with_capacity(...)`
  - `ctx.me()`
  - `ctx.send_self(...)`
  - typed call helpers
- downstream simulator consumer proof using the same preferred names
- README-aligned examples that are mirrored by compile-tested code, not just
  prose snippets

Required closeout scorecard:

- README no longer shows floating `FooMsg::Bar` fragments
- README no longer teaches `ctx.current_address()`
- README explains the message story before showing the code
- at least one downstream runtime proof uses `ctx.me()`
- at least one downstream runtime proof uses `ctx.send_self(...)`
- at least one downstream simulator proof uses `ctx.send_self(...)`
- crate-level docs no longer use tutorial-style `*Msg` naming for the first
  contact example
- public runnable examples use the preferred vocabulary and no longer store
  self-address state just to re-arm themselves
- `make verify` passes

Proof modes expected in this package:

- unit proof:
  helper constructors and tiny surface helpers build the right effects
- integration proof:
  runtime call helper path and self-send/self-address path compile and execute
- e2e proof:
  downstream consumer tests and runnable examples
- blast-radius proof:
  workspace verification still passes after the surface cleanup

Surrogate proof that helps but does not close the claim:

- a README rewrite with no compile-backed proof nearby
- examples that are shorter only because they hide Tina semantics
- aesthetic renames without runnable downstream usage

## How We Will Prove We Did Not Break Earlier Intent

- the preferred public path still keeps Tina message-driven and synchronous
- no helper hides runtime-owned I/O behind fake `await`
- runtime and simulator consumer tests still prove the real end-to-end path
- workspace `make verify` still passes

## Pause Gates

Stop and ask for human input only if one of these happens:

- a readability improvement appears to require hiding a real semantic boundary
- a naming cleanup would make multi-shard vocabulary materially worse
- helper simplification starts implying a second, conflicting public style

## Traps

- Do not confuse "shorter" with "less honest."
- Do not show code fragments that require the reader to imagine missing enum
  definitions.
- Do not let the README become the only place where the preferred path exists.
- Do not leave old rough wording in the public docs after changing the code.

## Files Likely To Change

- `.intent/phases/021-single-shard-devex-and-call-ergonomics/plan.md`
- `.intent/phases/021-single-shard-devex-and-call-ergonomics/review.md`
- `README.md`
- `.intent/SYSTEM.md`
- `tina/src/lib.rs`
- `tina/tests/preferred_surface.rs`
- `tina-runtime-current/tests/consumer_api.rs`
- `tina-sim/tests/consumer_api.rs`
- public runnable examples if they still teach the rougher path

## Areas That Should Not Be Touched

- multi-shard semantics
- mailbox crate internals
- Tokio bridge design
- simulator-only semantics that diverge from the runtime

## Report Back

- exact plan updates made here
- exact public surface changes implemented from the plan
- exact verification results
- remaining taste violations still worth a follow-on slice
