# 021 Tokio-Readable, Tina-Honest Single-Shard Ergonomics Plan

Session:

- A

## What We Are Building

Build a concrete medium-sized ergonomics phase whose explicit goal is:

> **Tokio-level readability, Tina-level semantic honesty**

That phrase is not just inspiration. It is the named goal of the phase and the
standard by which every change in 021 should be judged.

This phase exists because the single-shard Tina model is already real and worth
keeping, but the common authoring path is still more ceremonious than it needs
to be. Right now Tina often makes users say true things in too many words.

021 should make common single-shard Tina code read more like:

- `sleep(delay).reply(...)`
- `tcp_read(stream, n).reply(...)`
- `tcp_write(stream, bytes).reply(...)`

and less like:

- `Effect::Call(CurrentCall::new(... |result| match result { ... }))`

In plain terms: 021 is trying to get much closer to the way a Tokio user reads
and writes ordinary application code, while refusing the hidden control flow
and hidden scheduler semantics that make async Rust hard to explain under
pressure.

without losing the parts that make Tina worth having:

- explicit state machines
- explicit message delivery
- explicit failure handling
- runtime-owned I/O and timers
- replayable behavior
- no hidden scheduler magic

This is not a semantics redesign. It is not a routing phase. It is not a macro
explosion phase. It is a direct pass over the parts of the single-shard API
that currently make users type too much to say something straightforward.

## Target Code Shapes

The plan should be read against explicit example shapes, not just directional
 statements.

### Tokio shape we are trying to approach

This is the readability bar, not the semantic model:

```rust
match client.try_work().await {
    Ok(()) => { /* continue */ }
    Err(err) => { /* handle error */ }
}
```

```rust
let n = stream.read(&mut buf).await?;
stream.write_all(&buf[..n]).await?;
```

Tokio gets a lot of readability from compact call sites and obvious control
flow. 021 is trying to approach that compactness on the page without copying
Tokio's hidden scheduler behavior.

### Tina today shape we are trying to compress

```rust
Effect::Call(CurrentCall::new(
    CallInput::Sleep { after: delay },
    |result| match result {
        CallOutput::TimerFired => Msg::RetryNow,
        CallOutput::Failed(reason) => Msg::SleepFailed(reason),
        _ => panic!("wrong result"),
    },
))
```

```rust
Effect::Call(CurrentCall::new(
    CallInput::TcpRead {
        stream: self.stream,
        max_len: 1024,
    },
    |result| match result {
        CallOutput::TcpRead { bytes } => Msg::ReadCompleted(Ok(bytes)),
        CallOutput::Failed(reason) => Msg::ReadCompleted(Err(reason)),
        _ => panic!("wrong result"),
    },
))
```

This code is semantically honest, but too noisy for the common path.

### Tina after 021 shape we are explicitly trying to enable

```rust
sleep(delay).reply(|result| match result {
    Ok(()) => Msg::RetryNow,
    Err(reason) => Msg::SleepFailed(reason),
})
```

```rust
tcp_read(self.stream, 1024).reply(Msg::ReadCompleted)
```

```rust
tcp_write(self.stream, bytes).reply(Msg::WriteCompleted)
```

with message shapes like:

```rust
enum Msg {
    ReadCompleted(Result<Vec<u8>, CallError>),
    WriteCompleted(Result<usize, CallError>),
}
```

These are not merely illustrative. They are the intended user-facing shapes
that implementation should target unless a review-backed refinement produces a
clearer Tina/Odin-aligned version.

## What Will Not Change

- Boundedness remains real:
  - bounded mailboxes
  - visible `Full` / `Closed`
  - no hidden queues
- Handlers remain synchronous and effect-returning.
- Runtime-owned I/O, timers, and completions remain runtime-owned.
- Explicit delivery semantics remain visible in the API.
- Replayability remains part of the model.
- This phase does **not** introduce routing or placement ergonomics for
  multi-shard work. That belongs after 020's semantics are real.
- This phase does **not** introduce a `Router`, `Locator`, or `Registry`
  abstraction for address discovery.
- This phase does **not** relax address/shard/generation honesty.
- This phase does **not** redesign `Isolate`, `Effect`, or the core
  runtime-owned call model from scratch.
- This phase does **not** add broad proc-macro convenience or derive-heavy
  framework magic.
- Existing Mariner and Voyager behavior must stay green.

## Phase Standard

Every ergonomic change in 021 should be judged against one question:

> Does this make Tina code read closer to Tokio while preserving Tina's truth?

The point is not to make Tina secretly behave like Tokio. The point is to make
Tina code feel almost that readable while staying message-driven, replayable,
and explicit about what the runtime is doing.

Good 021 changes:

- remove repetitive ceremony
- make the happy path easier to read
- compress common runtime-owned call patterns
- keep all meaningful success/failure states explicit

Bad 021 changes:

- hide failure paths
- hide delivery semantics
- make handlers secretly async
- turn runtime-owned calls into fake direct-style I/O
- guess at multi-shard ergonomics early

## Remaining Design Questions To Close Early

021 is already pointed in the right direction, but a few smaller edges still
need to be pinned deliberately before the bulk rewrite starts.

Questions that still need explicit answers:

- helper import / qualification shape:
  - helper constructors may live in a module internally, but ordinary call
    sites should not require repetitive `effect::...` qualification if the
    imported context already makes it obvious that these are effect helpers
  - default preference: after normal imports, user code reads `sleep(...)`,
    `tcp_read(...)`, `tcp_write(...)`
- preferred import path:
  - app-facing docs and examples should prefer `tina::prelude::*`
  - direct re-exports from `tina_runtime_current` may exist for lower-level
    use and tests, but they are not the primary teaching path
- replacement posture:
  - 021 should start by layering the clearer surface on top of the existing
    one, not by ripping old entrypoints out immediately
  - once the new surface is proven on real examples and tests, decide which old
    names or helpers can be demoted, deprecated, or removed safely
  - docs, examples, tests, and closeout evidence should prefer the new surface
    even if some older entrypoints remain during migration
- compatibility discipline:
  - legacy names may survive temporarily as compatibility aliases so 016-020
    tests and examples do not have to churn all at once
  - they should not remain silently as equal long-term teaching paths
  - closeout must say whether each compatibility alias is retained, marked
    `#[deprecated]`, or removed
- prelude contents:
  - confirm the exact `tina::prelude` export set after the naming pass lands,
    so the prelude carries the real preferred vocabulary rather than stale
    names
- macro threshold:
  - macros are not part of the first answer
  - first prove the helper surface and naming pass
  - only introduce a macro if repeated boilerplate still survives after the
    helper rewrite and the repeated shape is truly mechanical
- downstream consumer proof:
  - 021 should include one public consumer-style example or test that uses
    only the preferred exported surface
- multi-shard naming symmetry:
  - 021 should establish the canonical Tina vocabulary that 020 adopts
    verbatim rather than re-litigating sibling names during implementation

These are not reasons to delay forever. They are the small number of decisions
that should be pinned deliberately so the rest of the phase can move fast.

## Concrete Deliverables

021 should ship the following concrete surface improvements.

### 0. Purposeful naming cleanup for user-facing surface

Review the names users see most often and rename any that violate the standard:

- clear
- purposeful
- easier to read in real service code
- aligned with Tina/Odin's mental model where possible
- not leaking implementation-history names into the main user path

This is not a blanket rename spree. It is a deliberate pass over the names
users touch constantly.

Names that are already clear and model-carrying should stay. Names that feel
historical, accidental, or awkward should change.

021 should use the following replacement vocabulary:

- `CurrentRuntime` -> `Runtime`
- `CurrentCall` -> `RuntimeCall`
- `CallFailureReason` -> `CallError`
- `register_with_mailbox_capacity` -> `register_with_capacity`

More concrete naming decisions:

- `Runtime` is the right common-path noun for the current single-shard runtime.
  `CurrentRuntime` reads like implementation history, not user vocabulary.
- `RuntimeCall` stays the low-level escape-hatch noun.
  Plain `Call` is too overloaded next to `Effect::Call`, `CallInput`, and
  helper calls like `sleep(...)`.
- `CallRequest` should become `CallInput`.
  This reads less like web or RPC request language while still naming the
  low-level input side of a runtime-owned call.
- `CallResult` should become `CallOutput`.
  This gives the low-level pair a clean `Input` / `Output` symmetry while still
  staying distinct from Rust's plain `Result`.
- `CallError` is the preferred failure noun.
  `CallFailureReason` is unnecessarily long, and `CallError` matches what most
  users already expect to see in application code.
- `SpawnSpec` should become `ChildDefinition`.
  The payload is not the live child; it defines how to create one child
  instance, including mailbox capacity and optional initial message.
- `RestartableSpawnSpec` should become `RestartableChildDefinition`.
  This is the same idea, but with a repeatable factory so runtime can create a
  fresh child instance again later.
- `with_bootstrap` should become `with_initial_message`.
  The argument is an initial message (or initial-message factory in the
  restartable form), and the name should say that directly.
- `SendMessage` stays `SendMessage` for 021.
  `Envelope` sounds nice, but in messaging-framework prior art it often implies
  headers, correlation ids, or metadata that Tina's current send payload does
  not carry.
- `ChildDefinition` is the right noun for both child creation forms.
  In the one-shot case it defines one child incarnation from concrete isolate
  state; in the restartable case it defines one child incarnation via a
  repeatable factory.
- the helper-carrier type used behind `sleep(...)` / `tcp_read(...)` /
  `tcp_write(...)` is not a primary user-facing noun and should stay as hidden
  as possible.
  It should stay private or doc-hidden if possible. Do not optimize the phase
  around inventing a cute public type name like `CallBuilder` if users rarely
  need to name it.
- keep low-level support names more explicit than the happy-path helper names;
  the compact path should be short, while the escape-hatch path can afford more
  literal naming

Naming direction:

- prefer names centered on runtime / simulator / call / spawn / address /
  isolate / shard, rather than names centered on implementation phase history
- keep terms like `Isolate`, `Effect`, `Address`, `Spawn`, `Shard`, and
  `Simulator` unless a stronger Tina/Odin-aligned reason appears
- if a name already matches the Tina/Odin mental model, keep it
- compatibility aliases are allowed, but the preferred path should be the clear
  path, not the historical one

The phase should leave behind one obvious common vocabulary for single-shard
Tina service code.

Preferred common vocabulary after 021:

- `Runtime`
- `Simulator`
- `Isolate`
- `Effect`
- `Address`
- `Context`
- `ChildDefinition`
- `RestartableChildDefinition`
- `RuntimeCall`
- `CallInput`
- `CallOutput`
- `CallError`
- `SendMessage`
- `with_initial_message(...)`
- `sleep(...)`
- `tcp_read(...)`
- `tcp_write(...)`
- `register_with_capacity(...)`

### 1. Preferred typed call helper surface

021 should pin one preferred user-facing helper family defined in:

- `tina_runtime_current::effect`

but ordinary user code should not have to keep spelling `effect::...` at every
call site. The preferred path should be:

- helper constructors re-exported from `tina_runtime_current`
- helper constructors also available through `tina::prelude`

so that ordinary handler code reads:

- `sleep(...)`
- `tcp_read(...)`
- `tcp_write(...)`

The preferred helpers should already know their success payload type:

- `sleep(after: Duration) -> <typed call helper>`
- `tcp_read(stream: StreamId, max_len: usize) -> <typed call helper>`
- `tcp_write(stream: StreamId, bytes: Vec<u8>) -> <typed call helper>`
- `tcp_accept(listener: ListenerId) -> <typed call helper>`
- `tcp_bind(addr: SocketAddr) -> <typed call helper>`
- `tcp_close_listener(listener: ListenerId) -> <typed call helper>`
- `tcp_close_stream(stream: StreamId) -> <typed call helper>`

The concrete helper type name is not itself a primary devex goal. The call-site
shape is. If the helper type remains mostly invisible to users, that is good.

The builder should return `Effect<I>` directly so users do **not** need to
write:

- `Effect::Call(...)`

around the helper.

The builder must expose one main mapping method:

- `reply(self, f) -> Effect<I>`

where `f` receives:

- `Result<T, CallError>`

and returns one ordinary isolate `Message`.

The public combinator surface is pinned now:

- `reply(...)` is the only preferred public mapping combinator in 021

Do not ship overlapping micro-DSLs like `then(...).or(...)` in this phase.

This is the preferred public path for single-shard runtime-owned calls.

Intended user shape:

```rust
sleep(delay).reply(|result| match result {
    Ok(()) => Msg::RetryNow,
    Err(reason) => Msg::SleepFailed(reason),
})
```

```rust
tcp_read(stream, 1024).reply(Msg::ReadCompleted)
```

```rust
tcp_write(stream, bytes).reply(Msg::WriteCompleted)
```

with message shapes like:

```rust
enum Msg {
    ReadCompleted(Result<Vec<u8>, CallError>),
    WriteCompleted(Result<usize, CallError>),
}
```

This is the core ergonomic move of 021.

The bar here is not merely "less verbose than today." The bar is that ordinary
runtime-owned call code should start reading much closer to the kind of compact
flow a Tokio user expects, while still routing every completion through an
explicit Tina message.

More concrete example:

Today:

```rust
Effect::Call(CurrentCall::new(
    CallInput::TcpWrite { stream, bytes },
    |result| match result {
        CallOutput::TcpWrote { count } => Msg::WriteCompleted(Ok(count)),
        CallOutput::Failed(reason) => Msg::WriteCompleted(Err(reason)),
        _ => panic!("wrong result"),
    },
))
```

Target:

```rust
tcp_write(stream, bytes).reply(Msg::WriteCompleted)
```

### 2. `CallOutput` extractor methods for shipped runtime-owned calls

Add extractor methods on `tina-runtime-current::CallOutput` for the currently
shipped single-shard result vocabulary:

- `into_timer_fired(self) -> Result<(), CallError>`
- `into_tcp_bound(self) -> Result<(ListenerId, SocketAddr), CallError>`
- `into_tcp_accepted(self) -> Result<(StreamId, SocketAddr), CallError>`
- `into_tcp_read(self) -> Result<Vec<u8>, CallError>`
- `into_tcp_wrote(self) -> Result<usize, CallError>`
- `into_tcp_listener_closed(self) -> Result<(), CallError>`
- `into_tcp_stream_closed(self) -> Result<(), CallError>`

These helpers must:

- preserve failure visibility
- reject wrong success-shape use loudly (panic is acceptable for impossible
  "asked for the wrong result kind" misuse in translator code, and only for
  that misuse)
- exist primarily as support machinery for the typed helper surface and as an
  escape hatch for advanced low-level call construction
- not become the main preferred authoring path if the typed helper facade is
  working as intended

### 3. One explicit `RuntimeCall` helper for failure splitting

Add one helper constructor alongside `RuntimeCall::new`:

- `RuntimeCall::map_result(request, translator)`

Where `translator` receives:

- `Result<CallOutput, CallError>`

instead of raw `CallOutput`.

This attacks a repeated universal pain point directly:

- today users repeatedly split `CallOutput::Failed(reason)` from the success
  variants by hand
- `map_result` makes failure-vs-success explicit once, then lets extractor
  methods handle the success shape

`RuntimeCall::new` stays.

This is not the preferred happy-path surface once the typed helpers exist.
It is the lower-level escape hatch and implementation-support layer directly
under the typed helper facade.

Concrete intended shape for low-level users:

Today:

```rust
RuntimeCall::new(request, |result| match result {
    CallOutput::Failed(reason) => Msg::Finished(Err(reason)),
    ok => Msg::Finished(ok.into_tcp_read()),
})
```

Target:

```rust
RuntimeCall::map_result(request, |result| {
    Msg::Finished(result.and_then(CallOutput::into_tcp_read))
})
```

### 4. Symmetric root registration-by-capacity helpers

The current runtime already owns a mailbox factory for spawned children, and
the simulator already exposes `register_with_mailbox_capacity`.

021 should make root registration less awkward by adding:

- `Runtime::register_with_capacity(isolate, capacity)`
- `Simulator::register_with_capacity(isolate, capacity)`

and treating the old long simulator spelling as, at most, a temporary
migration surface rather than a real user-facing default.

The goal is one obvious registration pattern:

- `register(...)` when the caller already has a mailbox
- `register_with_capacity(...)` when the runtime/simulator should create one

### 5. A `tina::prelude`

Add a real `tina::prelude` that re-exports the common single-shard authoring
surface:

- `Address`
- `Context`
- `Effect`
- `Isolate`
- `IsolateId`
- `RestartableChildDefinition`
- `SendMessage`
- `Shard`
- `ShardId`
- `ChildDefinition`

The goal is not to hide the crate model completely. The goal is to make the
common authoring path feel like one thing instead of a scavenger hunt.

### 6. Small address/context convenience cleanup

Add only tiny convenience where it reduces obvious noise without changing
semantics. In scope:

- any missing obvious `Address` / `Context` helpers that mirror already-exposed
  shard/isolate/generation data or common current/local address construction
- naming cleanup where the common path is awkward but the meaning is already
  settled

Out of scope:

- remote-routing sugar
- location lookup APIs
- automatic placement helpers

### 7. Real examples converted to the new surface

This phase is not done if the helpers only exist in isolation. Convert real
single-shard surfaces to prove the new ergonomics are worth having:

- README example
- `tcp_echo`
- `task_dispatcher`
- representative `tina-sim` proof workloads
- representative `tina-runtime-current` call/timer tests

## How We Will Build It

- Start with the call/completion pain first. That is the best stable target
  before 020.
- Treat the phase goal as product direction, not slogan:
  - Tokio readability on the page
  - Tina honesty in the semantics
- Use helper APIs to make the common truth compact, not invisible.
- Pin one preferred path, not three competing ones:
  - preferred public path after ordinary imports: bare helper names like
    `sleep(...)` and `tcp_read(...)`
  - canonical definition path: `tina_runtime_current::effect`
  - support layer: `RuntimeCall::map_result(...)`
  - low-level/escape-hatch layer: raw `RuntimeCall::new(...)` plus extractor
    methods
- Treat naming as real ergonomics work:
  - user-facing names should match the mental model
  - implementation-history names should not dominate the happy path
  - where Tina/Odin already has the right noun, prefer to line up with it
- Keep all setup ergonomics additive first:
  - layer the clearer path on top
  - prove it on real code
  - then decide what older entrypoints can be demoted, deprecated, or removed
    without harming reliability
  - once the new path is proven, the docs/examples/test surface should stop
    advertising the older one as equally preferred
- Prefer surface unification through re-exports and symmetry, not through a
  new umbrella crate.
- Defer all remote-placement/routing ergonomics until after 020.

## Build Steps

1. Perform a short design-pinning pass if needed to close the remaining open
   ergonomic questions:
   - helper import / re-export shape
   - replacement vs temporary migration posture
   - macro threshold
   - 020 sibling-surface naming symmetry
2. Perform the naming pass on the most user-visible single-shard surface.
   - rename awkward historical names
   - preserve semantic clarity
   - add migration shims only when they help transition without muddying the
     preferred path
3. Rename the core runtime/call/failure terms to the preferred user-facing
   vocabulary:
   - `CurrentRuntime` -> `Runtime`
   - `CurrentCall` -> `RuntimeCall`
   - `CallFailureReason` -> `CallError`
   - `SpawnSpec` -> `ChildDefinition`
   - `RestartableSpawnSpec` -> `RestartableChildDefinition`
   - `with_bootstrap` -> `with_initial_message`
4. Add `CallOutput` extractor methods for all currently shipped single-shard
   runtime-owned call results.
5. Add the typed-call helper surface in `tina_runtime_current::effect` for the
   shipped runtime-owned calls.
6. Ensure the typed-call helper builder returns `Effect<I>` directly so users
   do not wrap it in `Effect::Call(...)`.
7. Add `RuntimeCall::map_result(request, translator)` with
   `Result<CallOutput, CallError>` translator input.
8. Update representative single-shard call sites to use:
   - typed call helpers
   - `map_result` where low-level control is still useful
   - extractor methods where still useful
9. Add `Runtime::register_with_capacity(isolate, capacity)`.
10. Add `Simulator::register_with_capacity(isolate, capacity)`.
11. After the new surface is proven, decide which older setup entrypoints can
    be reduced to migration-only status and which should remain longer for
    compatibility.
12. Add `tina::prelude`.
13. Add any tiny `Address` / `Context` convenience that obviously reduces
    current-path noise without changing semantics.
14. Convert real user-shaped code to the new preferred surface:
    - README
    - `tcp_echo`
    - `task_dispatcher`
    - representative runtime/simulator tests
15. Add one downstream consumer-style proof that uses only the preferred
    exported surface.
16. Produce explicit before/after evidence for:
    - README example
    - `tcp_echo`
    - `task_dispatcher`
    showing the preferred-path rewrite and the concrete ceremony removed
17. Only if a small amount of purely mechanical repetition remains after all
    of the above, add one tiny declarative macro surface for that repetition.
    This step is optional and should be skipped unless the repetition is truly
    boring, user-visible, and still present after the helper rewrite.
18. Close out docs/evidence:
    - `.intent/SYSTEM.md` with mandatory updates for the `tina` public-boundary
      rename set and preferred-path rules
    - `README.md`
    - `ROADMAP.md`
    - `CHANGELOG.md`
    - phase `commits.txt`
    - review receipt

## How We Will Prove It

Direct proof for the changed behavior:

- all existing Mariner and Voyager tests stay green
- unchanged 016-020 tests keep compiling through compatibility aliases where
  needed, so we prove migration safety
- the named phase goal is visible in rewritten user code:
  - common single-shard flows read materially closer to Tokio-style code
  - the same flows still surface explicit Tina messages, explicit failures, and
    runtime-owned completions
- the preferred helper call site is genuinely short:
  - ordinary examples read `sleep(...)`, `tcp_read(...)`, `tcp_write(...)`
  - not repetitive `effect::sleep(...)` qualification at every call site
- renamed user-facing surface is clearer in representative service code and
  does not drift away from Tina/Odin's mental model
- the chosen vocabulary is consistent with later 020 naming rather than
  creating a single-shard-only dialect
- typed call helpers preserve the same runtime-owned semantics as the older
  lower-level call construction path
- the helper builder returns `Effect<I>` directly rather than forcing
  `Effect::Call(...)` ceremony at the call site
- `RuntimeCall::map_result` preserves failure visibility:
  - `CallError` still reaches translator code
  - no failure path is hidden or auto-stopped
- `CallOutput` extractor methods preserve semantics:
  - correct success-shape extraction succeeds
  - failed calls return the same `CallError`
  - wrong success-shape use is caught loudly and only for that shape-mismatch
    misuse
- `register_with_capacity` paths behave the same as the old more verbose setup
  paths
- the new preferred path shortens real workloads in named before/after
  examples:
  - README example uses the prelude and no low-level call plumbing
  - `tcp_echo` listener/connection path uses typed call helpers instead of raw
    `CallOutput` matching in handler code
  - `task_dispatcher` setup uses the preferred registration surface where
    appropriate
  - closeout includes concrete before/after snippets or diff references for
    those named examples
- closeout includes a small devex scorecard for each named example:
  - old call-site shape
  - new call-site shape
  - old translator branches
  - new translator branches
  - old setup steps
  - new setup steps
  - old repeated qualifications/import seams
  - new repeated qualifications/import seams
- before/after closeout evidence should include at least these exact kinds of
  reductions:
  - `Effect::Call(CurrentCall::new(...))` replaced by `sleep(...).reply(...)`
    / `tcp_read(...).reply(...)` / `tcp_write(...).reply(...)`
  - hand-written `CallOutput::Failed(reason)` splitting replaced by
    `RuntimeCall::map_result(...)` or typed helpers
  - root setup that previously needed explicit mailbox-capacity plumbing
    replaced by `register_with_capacity(...)`
  - import surfaces rewritten to prefer `tina::prelude`
- closeout explicitly compares three shapes where useful:
  - Tokio today
  - Tina before 021
  - Tina after 021
  so review can judge whether the new surface is actually approaching the
  intended readability target
- the new surface reduces ceremony in concrete ways:
  - fewer repeated `match result { ... Failed ... }` branches
  - fewer repeated registration/setup steps
  - fewer import seams in authoring code
- representative public-path single-shard examples compile and run through the
  new surface
- one downstream consumer proof uses only the preferred exported API rather
  than private or transitional helpers

Proof modes expected in this package:

- integration proof:
  existing runtime/simulator workloads still behave the same
- e2e proof:
  user-shaped single-shard examples become simpler while preserving behavior
- blast-radius proof:
  all prior Mariner and Voyager suites still pass unchanged in meaning

## Done Means

021 is done when:

- the named phase goal is plainly true in the resulting code:
  single-shard Tina code reads materially closer to Tokio on the happy path
  while staying Tina-honest about delivery, failure, and runtime ownership
- the resulting code actually resembles the target shapes named in this plan,
  rather than merely being "somewhat shorter" than before
- the preferred path is actually preferred:
  docs, examples, and named tests use it rather than straddling old and new
  surfaces equally
- the preferred path is also actually pleasant to read:
  repeated module-roleplay like `effect::...` has been removed from ordinary
  examples unless a review-backed reason proves it necessary
- the most user-visible names are purposeful, clear, and aligned with the
  Tina/Odin mental model
- compatibility aliases, if any remain at closeout, are explicitly classified
  as retained, deprecated, or removed; none survive as silent equal peers
- awkward implementation-history names are no longer the preferred path
- typed runtime-owned call helpers make real workloads much shorter to read
- `RuntimeCall` / `CallOutput` usage is materially simpler on real workloads
- root registration/setup is less noisy in both runtime and simulator
- the common authoring path feels more unified
- existing semantics are unchanged
- no routing/placement ergonomics were guessed early
