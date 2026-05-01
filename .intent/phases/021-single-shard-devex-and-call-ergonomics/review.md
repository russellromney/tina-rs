# 021 Review

Session:

- B (review)

## Plan Review 1

Artifact reviewed:

- `.intent/phases/021-single-shard-devex-and-call-ergonomics/plan.md`

Reviewed against `.intent/SYSTEM.md`, the closeout state of 016-020, the
current `tina` / `tina-runtime-current` / `tina-sim` public surface, and
the named in-repo examples that 021 plans to convert (README, `tcp_echo`,
`task_dispatcher`, plus 016-020 tests). Confirmed against
`tina/src/lib.rs` that `SendMessage`, `SpawnSpec`, and
`RestartableSpawnSpec` all live in `tina` — i.e., the proposed renames
are real `tina` public-boundary changes, not just internal rename work.

### What looks strong

- The phase goal is named once and named loudly: "Tokio-level
  readability, Tina-level semantic honesty." The plan repeats it as the
  standard against which every change is judged. That is unusual and
  helpful — devex phases usually drift because nobody pinned the bar.
- Concrete before/after code shapes are in the plan, not just
  directional language. A reader can immediately see what the rewrite
  is supposed to make possible (`sleep(delay).reply(...)`,
  `tcp_read(stream, n).reply(Msg::ReadCompleted)`) and what it is
  supposed to make go away (`Effect::Call(CurrentCall::new(...))`).
- "What Will Not Change" fences the right things: bounded mailboxes,
  visible `Full`/`Closed`, synchronous handlers, runtime-owned I/O,
  explicit delivery semantics, replayability, and a hard refusal of
  `Router`/`Locator`/`Registry` for multi-shard ergonomics. The phase
  refuses macro magic until proven necessary.
- The "Phase Standard" section names good and bad change shapes
  (remove repetitive ceremony / hide failure paths). That is the right
  guardrail for a devex phase.
- Naming direction is not a blanket rename: it lists which user-facing
  names should change, names that should not (`Isolate`, `Effect`,
  `Address`, `Spawn`, `Shard`, `Simulator`, `CallRequest`, `CallResult`),
  and pins a specific replacement vocabulary.
- The plan separates the **preferred** path (typed helpers) from the
  **escape-hatch** path (raw `RuntimeCall::new`) deliberately, and
  refuses to ship "multiple overlapping micro-DSLs." That refusal is
  the right phase discipline.
- "Done Means" is sharp: the phase goal must be plainly true in the
  resulting code, not "somewhat shorter than before."

### What is weak or missing

1. **"Pre-020" framing on a phase numbered 021.**
   The plan opens "Build a small but concrete pre-020 ergonomics
   phase." But 020 (Galileo multi-shard) is already planned and
   pinned. Numbering says 021 follows 020; framing says it precedes
   it. The likely intended meaning is "021 deliberately stays in
   single-shard scope and does not touch 020's new multi-shard
   surface." That is fine, but the wording reads as a phase-ordering
   claim. Pin the meaning: 021 scopes itself to single-shard
   ergonomics; both single-shard runtime and 020's multi-shard
   sibling runtime/simulator inherit the renames where they overlap
   (e.g., `register_with_capacity`); no new multi-shard convenience
   APIs land here.

2. **The `reply(...)` vs `then(...).or(...)` choice is left to
   implementation-time.**
   The plan says "should be closed explicitly at the start of
   implementation" and `then(...).or(...)` is "either in or out of
   021." For a phase whose own surrogate-proof list says "Do not ship
   multiple overlapping micro-DSLs here," the choice should be made
   in the plan, not deferred. Pick one.
   - My read: `reply(...)` only. `then(...).or(...)` only
     meaningfully covers the `Result<(), CallError>` shape (sleep,
     listener-close, stream-close), so its incremental compactness
     is small relative to its API-surface cost.
   - `sleep(delay).reply(|r| match r { Ok(()) => ..., Err(e) => ... })`
     is already short and uniform with `tcp_read` / `tcp_write` /
     `tcp_accept` shapes.

3. **Naming pass blast radius crosses the `tina` public boundary.**
   - `SendMessage` → `Envelope` — `tina` boundary change
   - `SpawnSpec` → `ChildDefinition` — `tina` boundary change
   - `RestartableSpawnSpec` → `RestartableChildDefinition` — `tina`
     boundary change
   - `CurrentRuntime` → `Runtime` — `tina-runtime-current` change
   - `CurrentCall` → `RuntimeCall` — `tina-runtime-current` change
   - `CallFailureReason` → `CallError` — `tina-runtime-current` change
   - `register_with_mailbox_capacity` → `register_with_capacity`
   The plan also says "This phase does not redesign Isolate, Effect,
   or the core runtime-owned call model from scratch." That is
   accurate, but the renames *are* `tina` public-vocabulary
   changes. Acknowledge plainly: 021 widens the public `tina`
   vocabulary from history-flavored names to user-flavored names.
   SYSTEM.md update is not "if needed"; it is mandatory.

4. **`Envelope` invites richer-than-tina expectations.**
   In other messaging frameworks (Akka, JMS, AMQP) an "Envelope"
   carries headers, metadata, sender, correlation id, properties — a
   *richer* concept than tina's `SendMessage<M>`, which is just
   `(Address<M>, M)`. A reader who knows those frameworks will look
   for a metadata API on `Envelope` and not find one. Worth flagging
   the prior-art overload and considering alternatives:
   - keep `SendMessage` (already precise — "a message to send")
   - `MessageTo<M>` (also precise — "a message addressed to ...")
   - `Send<M>` (collides with `std::marker::Send`; not honest)
   This is a question, not a block. But the plan should at least
   acknowledge the overload and explain why `Envelope` is the right
   call for tina specifically.

5. **`ChildDefinition` asymmetry between one-shot and restartable
   shapes.**
   `RestartableSpawnSpec` carries a factory closure — it really is a
   *definition* of how to make a child instance.
   `SpawnSpec` carries an isolate **value** (single-use) plus
   mailbox capacity plus bootstrap. Calling that a "definition" is
   slightly misleading because the value gets consumed on use; it is
   more of a "spawn payload" than a definition. `ChildDefinition`
   fits the restartable case better than the one-shot case. Either
   accept the cosmetic imprecision deliberately, or pick a uniform
   pair (e.g., `Child` / `RestartableChild`, or keep `SpawnSpec` /
   `RestartableSpawnSpec`).

6. **Compat-alias discipline is unstated.**
   "Compatibility aliases are allowed, but the preferred path should
   be the clear path." Pin the rule:
   - During the migration: silent `pub use New as Old;` aliases so
     existing 016-020 tests keep compiling unchanged.
   - At closeout: aliases either gain
     `#[deprecated(note = "use <New> instead")]` or are removed.
   - Closeout artifact: every renamed name is in one of three states
     — preferred (with a SYSTEM.md mention), aliased
     `#[deprecated]`, or removed. No silent aliases survive past
     closeout.

7. **016-020 test migration vs alias coverage is unspoken.**
   The blast-radius bar is "all prior Mariner and Voyager suites
   stay green." Two paths:
   - Silent compat aliases keep every existing test compiling
     unchanged; the new vocabulary is proved on the named examples
     plus additive new tests.
   - Existing tests are migrated to the new vocabulary as part of
     the phase, accepting larger churn.
   The plan should pick path 1 for closeout discipline (existing
   tests prove existing semantics; new tests prove the new
   vocabulary), and explicitly mark build step 14's representative
   test rewrites as additive new test surface, not rewrite of the
   old.

8. **Symmetry with 020's multi-shard surface is unspecified.**
   020 just landed sibling multi-shard coordinator surfaces.
   `register_with_capacity` is named for `Runtime` and `Simulator`
   (the single-shard ones). What about
   `MultiShardRuntime::register_with_capacity` /
   `MultiShardSimulator::register_with_capacity`? Two honest options:
   - 021 mirrors the rename across the multi-shard sibling surface
     (small cost, large symmetry payoff).
   - 021 explicitly leaves the multi-shard sibling surface alone and
     accepts asymmetry until a later devex pass.
   Pick one. My read: mirror; the multi-shard side is sibling
   surface area in the same crate.

9. **Downstream consumer proof is conditional ("if implementation
   proves...").**
   This is the same plan-shape weasel that 019's `and/or` had. Pick
   now:
   - Yes: name the consumer-style crate (e.g., `examples/devex/`),
     have it depend only on `tina::prelude` exports, and require it
     to compile and run as part of `make verify`.
   - No: the named in-repo examples (README, `tcp_echo`,
     `task_dispatcher`) are sufficient on their own; the phase does
     not ship a separate consumer crate.
   My read: yes, but lightweight — one small example showing the
   typical handler shape using only the prelude. It both proves
   re-export correctness and gives users one place to copy from.

10. **Canonical import path for typed helpers — three options listed.**
    The plan says helpers live in `tina_runtime_current::effect`, are
    re-exported from `tina_runtime_current`, and are also in
    `tina::prelude`. That is three import paths for `sleep` and
    friends. Pin the user-facing canonical:
    - `tina::prelude::*` for ordinary user code (preferred)
    - `tina_runtime_current::effect::sleep` for users who want
      module-qualified imports
    - The bare `tina_runtime_current::sleep` middle re-export adds
      noise without buying clarity. Drop it, or justify it
      explicitly.

11. **The helper carrier type has a real name.**
    "The concrete helper type name is not itself a primary devex
    goal." Sympathetic, but every public method on this type appears
    in `cargo doc` and in compiler error messages. If the type is
    `pub struct CallBuilder<I, T>` (or whatever), "users rarely need
    to name it" is true for happy-path call sites but false for
    type-inference hints. Pin a name. Suggestion: `RuntimeCallBuilder`
    or `EffectCallBuilder` — boring, but self-describing.

12. **`CallResult` extractor methods that panic on wrong success
    shape.**
    The plan lists 7 extractors (`into_timer_fired`, `into_tcp_bound`,
    etc.), each `Result<T, CallError>`, panic on wrong variant. Two
    related issues:
    - Panic in *user* code is not a tina-honest happy path. The
      trap is acceptable for translator code that the typed-helper
      surface writes, because the helper picks the matching extractor
      by construction. But the plan also says these extractors are
      "an escape hatch for advanced low-level call construction."
      That second framing puts panic in user-reachable code.
    - Pin: extractors are internal-support for typed helpers and a
      narrow escape hatch for low-level
      `RuntimeCall::map_result` writers; they are not the main user
      surface. Document the panic discipline clearly on each method.

13. **SYSTEM.md update is mandatory, not "if needed".**
    Build step 18 says "`.intent/SYSTEM.md` if boundary wording needs
    adjustment." Given finding 3, the renames *are* `tina` boundary
    changes. SYSTEM.md will need updates at least to the "Current
    shipped surface" section (which currently names `SendMessage`,
    `SpawnSpec`, `RestartableSpawnSpec`, `CurrentRuntime` paths) and
    the public vocabulary block. Drop the "if".

14. **Phase size honesty.**
    "Build a small but concrete pre-020 ergonomics phase." 18 build
    steps and 8 concrete deliverables across 4 crates is not small.
    Comparable to 019, which had a similar scope and was named
    correctly as "intentionally a large slice." Same honesty applies
    here.

15. **Helpers work for both runtime and simulator code — say so.**
    `sleep(delay)` returns `Effect<I>` where `I::Call =
    RuntimeCall<I::Message>`. That works in both `tina-runtime-current`
    and `tina-sim` workloads (since the simulator already uses the
    same `CurrentCall<Msg>` shape). The plan implies it but does not
    state it. State plainly: typed helpers are usable unchanged from
    any isolate code that the live runtime accepts, including code
    under `tina-sim`.

16. **`Runtime` rename collision with future runtimes.**
    "Current" was named to leave room for `tina-runtime-monoio` and
    `tina-runtime-tokio`. If both `tina-runtime-current` and
    `tina-runtime-monoio` ship a type named `Runtime`, importers
    will need to qualify (`tina_runtime_current::Runtime` vs
    `tina_runtime_monoio::Runtime`). That is fine as long as we
    accept that the unqualified `Runtime` token is shared. Worth one
    line acknowledging that the rename is per-crate, not global, and
    that `tina::prelude` re-exports only one of them as the
    unqualified default.

17. **Before/after evidence format is unspecified.**
    Build step 16 mandates "explicit before/after evidence for
    README, `tcp_echo`, `task_dispatcher`." Pin the format:
    - section in `commits.txt` with before/after snippets, or
    - separate `evidence.md` with diff references, or
    - PR descriptions only (weakest, not recommended).
    The "devex scorecard" in How We Will Prove It is excellent and
    concrete. Promote it from a proof-list bullet to a required
    closeout artifact and pin where it lives.

18. **`Isolate::Spawn` associated type stays — say so explicitly.**
    `Isolate` has `type Spawn = SpawnSpec<I>;` (or `Infallible`, or
    `RestartableSpawnSpec<I>`) across every existing test. Renaming
    `SpawnSpec` → `ChildDefinition` flips this to `type Spawn =
    ChildDefinition<I>;`. The associated type *name* `Spawn` itself
    is good and stays. Worth one line confirming this so a reader
    does not imagine `Isolate::Child` or similar.

### How this could still be broken while the listed tests pass

- 016-020 tests stay green because silent compat aliases preserve
  every old name. The blast-radius claim is satisfied. But the
  preferred path is *not* preferred in practice, because the new
  vocabulary is exercised only on three named examples and a small
  number of new tests; the bulk of the test surface still uses old
  names. Closeout review reads "all green" but the new vocabulary is
  undertested. Mitigation: require at least one renamed test in each
  affected crate uses the new vocabulary explicitly.
- Typed helpers compile and pass the named-example rewrites but
  produce a different trace shape than `RuntimeCall::new`. Behavior
  passes (same `CallResult` reaches the same `Message`), but the
  trace drifts. Pin: typed-helper call sites must produce trace
  events indistinguishable from the same call constructed via
  `RuntimeCall::new` (same `CallDispatchAttempted` /
  `CallCompleted`/`CallFailed` cause linkage and ordering).
- `then(...).or(...)` lands as a second combinator alongside
  `reply(...)` (because the choice was deferred), multiplying the
  API surface despite the plan's own warning. Tests pass; the
  invariant "one preferred path" silently broadens.
  Mitigation: finding 2.
- Compat aliases land but never get `#[deprecated]`. The codebase
  carries dual vocabularies indefinitely. Mitigation: finding 6.
- `Envelope` ships, users start putting metadata-shaped concepts
  next to it (correlation ids, headers), and a future phase has to
  either grow `Envelope` to match the name or rename it again.
  Mitigation: finding 4.
- The before/after evidence in closeout consists of three small
  snippet pairs from the named examples and a vague claim that "the
  preferred path is preferred." A real reader cannot judge whether
  Tokio readability has actually been approached. Mitigation:
  finding 17 — promote the devex scorecard to a closeout artifact.
- The downstream consumer proof was deferred under "if needed" and
  was not added; the only proof of re-export correctness is the
  in-repo examples. A user who imports only `tina::prelude` finds a
  hole at integration time. Mitigation: finding 9.

### What old behavior is still at risk

- Semantic invariants that 016-020 already prove. The phase
  explicitly preserves them, but the renaming churn touches every
  test. Implementation review should treat any test rewrite that
  drifts behavior as a regression, not a refactor.
- 020's sibling multi-shard surface is silently affected by
  transitive renames. Implementation should preserve symmetry
  between single-shard and multi-shard surfaces (finding 8).
- The 017 structural checker over event-id monotonicity assumes
  `RuntimeEvent` shape stability. Typed helpers that produce the
  same `RuntimeEvent` sequence as `RuntimeCall::new` keep the
  checker meaningful. A typed-helper implementation that takes a
  shortcut around the canonical `Effect::Call(...)` path could
  invalidate it.

### What needs a human decision

- **`reply(...)` only, or `then().or()` blessed alongside?**
  My recommendation: `reply(...)` only.
- **`SendMessage` → `Envelope` vs alternative.**
  My recommendation: re-evaluate; either keep `SendMessage` (already
  precise) or pick a name that does not invite metadata expectations
  (`MessageTo<M>` is honest). Plan should at minimum acknowledge the
  prior-art overload before committing.
- **`ChildDefinition` vs `Child`-pair vs keep `SpawnSpec`.**
  My recommendation: keep `SpawnSpec` and `RestartableSpawnSpec`
  (accurate; asymmetry between value-carry and factory-carry is
  real), or pick `Child` / `RestartableChild` (uniform and short).
  `ChildDefinition` fits one half but not the other.
- **Symmetric rename across 020's multi-shard sibling surface.**
  My recommendation: yes; mirror.
- **Downstream consumer-style example.**
  My recommendation: yes, lightweight.

### Recommendation

Plan is structurally on-shape: clear named goal, concrete target
shapes, explicit standard, tight surrogate-proof list, sharp Done
Means, refusal of macro magic and multi-shard ergonomics. Not yet
ready to hand off to implementation — three open decisions
(`reply` vs `then().or()`, `SendMessage`/`Envelope`, `SpawnSpec`/
`ChildDefinition`) are surface-shaping decisions that should not be
deferred to implementation taste, and several smaller refinements
should be pinned now so the phase does not drift.

Amend the plan before implementation begins to:

1. Replace "pre-020" framing with explicit single-shard scope and
   say plainly which renames mirror onto 020's multi-shard sibling
   surface.
2. Close the `reply(...)` vs `then(...).or(...)` choice. Recommended:
   `reply(...)` only.
3. Acknowledge plainly that the rename pass widens the `tina` public
   boundary; SYSTEM.md update is mandatory.
4. Re-evaluate `Envelope` against the prior-art overload risk; pick
   a name and justify it explicitly.
5. Resolve the `ChildDefinition` asymmetry (pick uniform new pair or
   keep current names).
6. Pin compat-alias discipline: silent during migration, then
   `#[deprecated]` or removed at closeout. No silent aliases survive.
7. State the existing-tests posture: silent aliases keep them green;
   new vocabulary is exercised on additive new tests + named-example
   rewrites.
8. State whether the rename mirrors onto 020's multi-shard sibling
   surface (recommended: yes).
9. Decide downstream consumer proof: in or out (recommended: in,
   lightweight).
10. Pin canonical import paths: `tina::prelude` for ordinary user
    code; `tina_runtime_current::effect::sleep` for module-qualified;
    drop or justify the bare `tina_runtime_current::sleep` middle
    re-export.
11. Pin the helper carrier type's public name, even if rarely typed.
12. State that `CallResult` extractor methods are internal-support /
    narrow escape hatch only, with documented panic discipline.
13. Drop "if needed" on SYSTEM.md update.
14. Be honest about phase size: "intentionally a large devex phase,"
    not "small."
15. State that typed helpers work unchanged in `tina-sim` workloads.
16. Acknowledge that `Runtime` will become a per-crate token; the
    prelude re-exports only one as the unqualified default.
17. Promote the devex scorecard from a proof-list bullet to a
    required closeout artifact and pin where it lives.
18. State that `Isolate::Spawn` associated type name stays.

Findings 3, 4, 5 involve `tina` boundary changes that are real
public-surface decisions. Treating those as plan-time decisions
instead of implementation-time decisions is the main bar of this
review.

## Plan Review 2

Reviewed the amended plan against the 18 Plan Review 1 findings, and
checked for new decisions introduced in the amendment pass.

### Plan Review 1 findings: status against the amended plan

Closed (15/18):

1. **"Pre-020" framing** — closed. Opening line is now "Build a
   concrete medium-sized ergonomics phase"; "pre-020" is gone.
2. **`reply(...)` vs `then().or()`** — closed. "The public combinator
   surface is pinned now: `reply(...)` is the only preferred public
   mapping combinator in 021. Do not ship overlapping micro-DSLs like
   `then(...).or(...)` in this phase." Target Code Shapes also
   dropped the `then().or()` example.
3. **`tina` boundary acknowledgment** — closed. Build step 18 now
   reads "`.intent/SYSTEM.md` with mandatory updates for the `tina`
   public-boundary rename set and preferred-path rules." The "if"
   was dropped.
4. **`Envelope` overload** — closed, and the right call.
   "`SendMessage` stays `SendMessage` for 021. `Envelope` sounds
   nice, but in messaging-framework prior art it often implies
   headers, correlation ids, or metadata that Tina's current send
   payload does not carry." `SendMessage` is back in the prelude and
   the Preferred Common Vocabulary list.
5. **`ChildDefinition` asymmetry** — closed by deliberate framing,
   not by name change. "`ChildDefinition` is the right noun for both
   child creation forms. In the one-shot case it defines one child
   incarnation from concrete isolate state; in the restartable case
   it defines one child incarnation via a repeatable factory." Plus
   the related `with_bootstrap` → `with_initial_message` rename
   sharpens the user-facing payload terminology. Acceptable framing.
6. **Compat-alias discipline** — closed. New "compatibility
   discipline" sub-bullet pins the rule: silent during migration; at
   closeout each alias is classified retained / `#[deprecated]` /
   removed; "none survive as silent equal peers" reaches Done Means.
7. **016-020 test posture** — closed. Proof list: "unchanged 016-020
   tests keep compiling through compatibility aliases where needed,
   so we prove migration safety."
8. **020 multi-shard sibling symmetry** — partially closed. New
   sub-bullet under Remaining Design Questions: "021 should establish
   vocabulary that 020 can inherit later rather than creating a
   single-shard-only naming dialect." Build step 1's design-pinning
   list includes "020 sibling-surface naming symmetry." Proof list:
   "the chosen vocabulary is consistent with later 020 naming rather
   than creating a single-shard-only dialect." This works because 020
   currently has only `plan.md` + `review.md` (no implementation
   landed yet) — so 020's eventual implementation will inherit the
   021 vocabulary. Closed.
9. **Downstream consumer proof** — closed. No longer conditional:
   "021 should include one public consumer-style example or test that
   uses only the preferred exported surface." Build step 15
   unconditional.
10. **Canonical import path** — closed. "preferred import path:
    app-facing docs and examples should prefer `tina::prelude::*` /
    direct re-exports from `tina_runtime_current` may exist for
    lower-level use and tests, but they are not the primary teaching
    path."
11. **Helper carrier type name** — closed by posture. "It should
    stay private or doc-hidden if possible." Acceptable: a
    private/hidden type doesn't appear in `cargo doc`, and inference
    sites avoid naming it through `Effect<I>` return type.
12. **Extractor panic discipline** — closed. "panic is acceptable for
    impossible 'asked for the wrong result kind' misuse in translator
    code, **and only for that misuse**" — the constraint is now
    explicit. Proof line mirrors it.
13. **SYSTEM.md mandatory** — closed (see finding 3 above).
14. **Phase size honesty** — closed. "small but concrete pre-020"
    became "concrete medium-sized."
15. **Helpers usable in `tina-sim`** — partially closed. Build step
    14 explicitly converts "representative `tina-sim` proof
    workloads" plus "representative `tina-runtime-current` call/timer
    tests"; proof list says "all existing Mariner and Voyager tests
    stay green." Implicit but reasonable. Could be sharper as a
    one-liner.
16. **`Runtime` rename per-crate** — not explicitly addressed.
    Minor; the implication is fine in practice (per-crate `Runtime`
    type, `tina::prelude` re-exports the single-shard one as the
    default). Could be flagged with one line.
17. **Devex scorecard as required closeout artifact** — partially
    closed. The scorecard remains a proof-list bullet, not promoted
    to a standalone required artifact section. In combination with
    Done Means it is effectively required, but a future closeout
    review will have to reconstruct that combination.
18. **`Isolate::Spawn` associated type stays** — not explicitly
    addressed. Minor; the rename pass does not list it, and `type
    Spawn = ChildDefinition<I>` still works.

Findings 15, 16, 17, 18 are tightening-grade gaps, not structural.

### What changed that warrants a fresh look

The amendment introduced one new rename pair that was not in Plan
Review 1's scope:

- **`CallRequest` → `CallInput`**
- **`CallResult` → `CallOutput`**

The plan justifies them: "This reads less like web or RPC request
language" / "This gives the low-level pair a clean `Input` / `Output`
symmetry while still staying distinct from Rust's plain `Result`."
That is a defensible rationale.

Considerations:

- These types live in `tina-runtime-current` (confirmed:
  `tina-runtime-current/src/call.rs:103,194` define
  `CallRequest::TcpAccept`, `CallResult::TcpAccepted` etc.). So this
  is a `tina-runtime-current` boundary rename, not a `tina` boundary
  rename. SYSTEM.md will need an update either way (already pinned
  in build step 18).
- Blast radius: every test and example that types `CallRequest::...`
  or `CallResult::...` (significant fraction of the test surface
  across `tina-runtime-current`, `tina-sim`, examples). Compat
  aliases keep them green per the discipline pinned in finding 6.
- `Request`/`Result` had a slight RPC flavor; `Input`/`Output` is
  more neutral. Both are reasonable. Reader cost is small either way
  once the rename lands.
- One edge: `CallOutput` shadows the colloquial usage of "output"
  (e.g., for byte buffers in `Wrote { count }`). Mildly confusing
  for new readers in TCP-write contexts. Not a blocker.

This is a defensible plan-time decision. Worth confirming the
intended cost/benefit before the rename pass starts, but not worth
delaying handoff over.

### New small concerns introduced by the amendment

1. **Markdown artifact in the "Intended user shape" example block.**
   Lines 388-401 contain a malformed code-fence sequence:
   ```
   ```rust
   sleep(delay).reply(|result| match result {
       Ok(()) => Msg::RetryNow,
       Err(reason) => Msg::SleepFailed(reason),
   })
   ```

   tcp_read(stream, 1024).reply(Msg::ReadCompleted)
   ```
   ```
   The first code fence closes after the `sleep` example, then
   `tcp_read(...)` appears as bare prose before another stray fence
   close. This was likely a mid-edit artifact when removing the
   `then().or()` example. Not a semantic issue; fix before handoff
   so the rendered plan is clean.

2. **`with_bootstrap` → `with_initial_message` is a `tina` boundary
   change too.**
   Confirmed: `with_bootstrap` lives in `tina/src/lib.rs:643,712`
   on `SpawnSpec` and `RestartableSpawnSpec`. So this rename also
   crosses the `tina` public boundary. The plan lists it under
   naming decisions and the new vocabulary adopts it, but the
   build-step rename list (step 3) does not include it explicitly:
   step 3 lists `CurrentRuntime`/`CurrentCall`/`CallFailureReason`/
   `SpawnSpec`/`RestartableSpawnSpec`. Add `with_bootstrap` →
   `with_initial_message` to step 3 so the rename pass is
   exhaustive.

3. **Multi-shard sibling-surface symmetry is in design-pinning, not
   answered.**
   Build step 1 sub-bullet "020 sibling-surface naming symmetry" is
   listed for the design-pinning pass to close. That is a "we'll
   decide first" not a "we've decided." Given that 020 has not
   landed in code (only `plan.md` + `review.md`), the practical
   effect of "deciding first" is small — there is no 020 surface to
   rename. The decision reduces to: 021's vocabulary is the canonical
   tina vocabulary; 020's eventual implementation adopts it
   verbatim. State that plainly so the design-pinning step does not
   re-litigate it.

### Recommendation

Plan is now ready to hand off to implementation after three small
fixes:

1. Fix the markdown artifact in the "Intended user shape" code-fence
   block (Plan Review 2 finding 1).
2. Add `with_bootstrap` → `with_initial_message` to build step 3's
   explicit rename list (Plan Review 2 finding 2).
3. State plainly under "020 sibling-surface naming symmetry" that
   021's vocabulary is the canonical tina vocabulary and 020's
   eventual implementation adopts it (Plan Review 2 finding 3).

Optional one-liners that would tighten the plan further but do not
block handoff:

- State that typed helpers work unchanged in `tina-sim` workloads
  (Plan Review 1 finding 15).
- Acknowledge the per-crate `Runtime` token (Plan Review 1
  finding 16).
- Promote the devex scorecard from a proof bullet to a required
  closeout artifact heading (Plan Review 1 finding 17).
- Note that `Isolate::Spawn` associated type name itself stays (Plan
  Review 1 finding 18).

The structural decisions — preferred combinator (`reply` only),
public-boundary rename acknowledgment, compat-alias discipline,
canonical import path, downstream consumer proof, `SendMessage`
preserved over `Envelope`, `ChildDefinition` justified — are all
closed. The amendment to add `CallRequest`/`CallResult` →
`CallInput`/`CallOutput` is a defensible plan-time decision that
widens the rename pass slightly without changing the phase shape.

Reasonable to hand off after the three fixes above.
