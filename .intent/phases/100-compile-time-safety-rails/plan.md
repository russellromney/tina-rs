# 100 Compile-Time Safety Rails

## Status

- IDD phase.
- One PR only if the first slice stays mechanical. Split if the send/call
  address split becomes a broad migration.
- Runs after Phase 095 call-context defer ergonomics and should coordinate with
  Phase 097 cancelable deferred admission.
- Owns compile-time prevention of silent Tina mistakes: callable/send-only
  boundaries, better macro errors, capability-narrow addresses, deferred
  admission ordering, and selected typestate where it catches real protocol
  bugs.
- Must include user-style proof, not only unit tests. A cheap model should be
  able to copy one passing service and several failing fixtures and understand
  the rule.

## Grug Truth

Runtime should handle runtime facts:

- capacity;
- closed peer;
- timeout;
- stale generation;
- backend failure.

Compile time should handle impossible-program facts:

- this message is send-only;
- this message is callable;
- this service forgot `handle_call`;
- this continuation is internal-only;
- this effect cannot be sent by this isolate;
- this cancelable deferred call was not admitted before dispatch;
- this protocol state cannot send DATA after trailers.

If a mistake can be made unrepresentable without making normal code weird,
make it unrepresentable.

If the type trick makes code clever, stop.

Loud runtime rejection is still better than a clever type maze.

## Goal

Move Tina's common silent failure modes left:

```text
runtime timeout/rejection -> compile error
trait soup -> useful diagnostic
copyable footgun -> impossible or hard to write
```

This phase should leave Tina nicer for humans and safer for LLMs.

The proof must look like user code, not only crate-private tests.

## Non-Goals

- no broad rewrite just to prove type theory;
- no typestate maze in ordinary user code;
- no breaking every specimen unless the migration is small and automatic;
- no hiding capacity/liveness as compile-time claims;
- no custom lint crate in first form;
- no flow language;
- no fake exhaustiveness for dynamic runtime facts.

## Rock 0: Audit Current Runtime Rejections

Find the real runtime failures that could be compile-time failures.

Read:

- `tina/src/lib.rs`;
- `tina-runtime/src/lib.rs`;
- `tina-runtime/src/call.rs`;
- `tina/src/pending_call_set.rs`;
- `tina-http/src/http2.rs`;
- `tina-http/src/grpc.rs`;
- `tina-http/src/websocket.rs`;
- docs request/reply and ergonomics checklist;
- specimens/systems that reject `UnsupportedMessage`.

Create a short table in this plan status:

| Failure | Current behavior | Can compile-time catch? | Chosen fix |
|---|---|---|---|

Include at least:

- call to send-only/internal message;
- missing `handle_call`;
- wildcard arm hiding new public call variant;
- wrong `Send`/`Call` associated type;
- non-`Send` / non-`'static` captured message or closure;
- wrong shard/reply shape;
- cancelable deferred dispatch before bounded admission;
- protocol state transition after EOF/trailers/close;
- config missing/zero fields where zero is never valid.

Pick the smallest set that gives one real compile-time win and one real
diagnostic win. Record the bigger deferred items explicitly. Do not leave them
implicit.

## Rock 1: Better Diagnostics First

Add diagnostic polish before changing broad APIs.

Targets:

- public traits: `Isolate`, `DeferThrough`, `DeferCancelableThrough`;
- runtime conversion traits that produce bad bound errors;
- macro-generated isolate impls;
- `Outbound` / `Send` / `Call` associated-type mismatch paths;
- non-`Send` and non-`'static` closure/message paths where the compiler points
  at erased internals instead of the user payload.

Use stable Rust where possible. If `#[diagnostic::on_unimplemented]` requires
nightly/feature gates, document the gate and only use it if it fits the repo's
current compiler story.

Required proof:

- compile-fail/ui tests or doctests for common mistakes;
- error text contains user-facing words like:
  - "message is not Send";
  - "add `send = Outbound<...>`";
  - "this isolate cannot issue this runtime call";
  - "callable message requires `handle_call`";
  - "use `call_ctx.defer(...)` for multi-turn replies".

Do not accept a diagnostics-only phase as complete if larger compile-time
rails are still untouched. This is Rock 1, not the whole phase.

Test shape:

- prefer `trybuild`-style fixtures if the repo already accepts the dependency;
- otherwise use doctest `compile_fail`;
- each negative fixture must be tiny and named after the user mistake;
- each fixture must have a nearby passing version.

Passing and failing pairs matter. A failing test without the copied good shape
does not teach.

## Rock 2: Send-Only vs Callable Surface

This is the highest-value semantic rail.

Current issue:

```rust
type Message = ApiMsg;
```

The same enum is accepted by send and call. Internal continuation variants can
be called at runtime and then rejected.

Target shape, exact API to be decided:

```rust
type Message = ApiMsg;      // ordinary send/internal continuation
type CallMessage = ApiCall; // public callable request
type Reply = ApiReply;
```

or capability split:

```rust
SendAddress<ApiMsg>
CallAddress<ApiCall, ApiReply>
```

Rules:

- ordinary sends cannot call `ApiMsg`;
- calls cannot target internal continuation messages;
- services that are send-only do not expose a call address;
- services that are callable must implement the call handler for the call
  message type;
- existing `Address<M, R>` keeps compatibility if needed, but new copied code
  should use the narrower capability.

Cut line:

- If full `Address` migration is huge, ship a first-form `CallAddress<Q, R>` /
  `SendAddress<M>` adapter and docs. Do not half-migrate the runtime.

Required user proof:

- compile-fail: `call(send_only_addr, ...)`;
- compile-fail or macro error: callable isolate missing `handle_call`;
- compile-fail: internal continuation sent through call address;
- passing: same service sends internal continuation normally;
- passing: same service calls public call message normally;
- existing runtime call tests still pass;
- one specimen/system migrates to split public call messages from internal
  continuation message.

This is the core e2e proof of the phase. It must be user-shaped: one tiny
service with public calls and internal continuations, exercised live through
`ThreadedRuntime::call_blocking` or equivalent, plus compile-fail fixtures for
the bad paths.

## Rock 3: Macro Declarations For Callability

Make the preferred authoring surface say intent.

Candidate attributes:

```rust
#[tina::isolate(message = InternalMsg, call = PublicCall, reply = PublicReply)]
```

or:

```rust
#[tina::isolate(message = Msg, reply = Reply, callable)]
```

and:

```rust
#[tina::isolate(message = Msg, send_only)]
```

Rules:

- `send_only` rejects/does not expose call surface;
- `call = ...` requires `handle_call`;
- `reply = ...` without `handle_call` gets a targeted error unless explicitly
  `send_only`;
- macro defaults stay ergonomic for no-reply internal isolates;
- `isolate_types!` gets an equivalent explicit form or docs say it is the
  low-level escape hatch.

Required proof:

- compile-fail missing `handle_call`;
- compile-fail contradictory `send_only` + `call = ...`;
- passing no-reply send-only isolate with no `handle_call`;
- passing callable isolate with explicit `handle_call`;
- docs examples show the new copied shape.

## Rock 4: Exhaustive Public Call Handling

Rust already catches unhandled enum variants when users avoid `_`.

Tina should teach and nudge that.

Required work:

- docs: public `handle_call` should match explicit public call variants;
- examples/specimens: avoid wildcard `_` for public service requests where
  practical;
- macro/lint research: see if attribute macro can warn/reject wildcard arms in
  `handle_call` for callable services. If not practical, document as a review
  rule.

Do not ban wildcard arms everywhere. Internal continuation handlers sometimes
need a boring rejection fallback. The target is public callable request enums.

Proof:

- at least one migrated specimen demonstrates public call enum with exhaustive
  match and internal message enum separately.
- if macro enforcement is not shipped, add a review-rule doc and an example
  showing the normal Rust way to get exhaustiveness: no wildcard arm for the
  public call enum.

## Rock 5: Cancelable Deferred Admission Type Rail

Coordinate with Phase 097.

If 097 already shipped `PendingCancelableCallSet`, use it. If not, implement
the type rail here or update 097 to own it.

Goal:

```rust
let admitted = pending.try_admit(key, token, effect)?;
return admitted.effect();
```

The child effect should be difficult or impossible to obtain before the
pending token is stored.

Rules:

- admission failure returns token and effect, or consumes neither;
- caller can always be answered/rejected after failure;
- no hidden dispatch;
- no unbounded table;
- no ABA key bug.

Proof:

- compile-time or API-shape proof that the common copied path cannot return the
  child effect before admission;
- runtime trace proof that failed admission does not dispatch child work.
- user-style specimen path: storage full -> caller gets reply/rejection now ->
  child call does not appear in trace.

## Rock 6: Capability-Narrow Addresses

Reduce authority where possible.

Candidate types:

```rust
SendAddress<M>
CallAddress<Q, R>
ObservedAddress<M, R>
ChildAddress<M, R>
```

Rules:

- helpers accept the narrowest capability they need;
- `send(...)`/`try_send(...)` can use send capability;
- `call(...)` requires call capability;
- child refs can expose exactly what the parent should hold;
- compatibility conversions are explicit.

Do not turn every call site into conversion soup. Migrate only one high-value
surface first.

Proof:

- compile-fail calling through send-only capability;
- compile-fail sending through call-only capability if such a type exists;
- one service/specimen uses narrow capabilities in state fields;
- the old broad `Address` path remains documented as compatibility or low-level
  escape hatch.

## Rock 7: Config Builder Typestate Only If Worth It

Some config values should never be zero or missing.

Candidates:

- pool capacity / waiter capacity;
- HTTP body caps;
- bridge max in-flight;
- listener startup config;
- service budget manifest.

Default rule:

- keep runtime validation for user-provided config;
- use typestate builders only for construction paths where missing fields are
  common and compile-time help is worth the type noise.

Proof:

- one targeted builder or deliberate rejection note;
- docs say runtime validation remains the source of truth for config loaded
  from files/env.

This rock is optional. Do not spend the phase here unless Rock 0 finds a config
mistake that is common, copied, and cheap to prevent.

## Rock 8: Protocol Typestate Internals Only If Worth It

Use typestate inside protocol implementation only where it prevents real bugs.

Candidates:

- HTTP/2 stream:
  - `Idle`;
  - `Open`;
  - `HalfClosedRemote`;
  - `HalfClosedLocal`;
  - `Closed`;
- response writer:
  - `HeadersPending`;
  - `DataOpen`;
  - `TrailersSent`;
- WebSocket close:
  - `Open`;
  - `CloseSent`;
  - `CloseReceived`;
  - `Closed`.

Rules:

- typestate can be private/internal;
- public service code should not juggle many state types;
- only ship if it removes runtime "should never happen" branches or prevents
  known bug classes like DATA after trailers.

Proof:

- one protocol internal transition bug becomes unrepresentable, or the plan
  records why this is not worth doing yet;
- existing protocol tests remain green.

This rock is optional. Do not let private protocol typestate block the user
compile-time safety win.

## Rock 9: Docs And Review Rules

Update docs with a short "compile-time rails" section:

- use separate public call and internal message types for services;
- avoid wildcard arms in public `handle_call`;
- accept narrow address/capability types in structs;
- use `call_ctx.defer(...)` for ordinary multi-turn work;
- use `PendingCancelableCallSet` / admitted effect shape for cancelable work;
- runtime still owns `Full`, `Closed`, `Timeout`, and stale generation truth.

Add a review checklist item for LLM-written code:

- "Could this runtime rejection be a type error?"

Also add one small "good / bad" box:

- bad: public and internal variants in one callable enum, wildcard rejection;
- good: public call enum, internal continuation enum, explicit public match.

## Required User Proof Matrix

Build one tiny proof crate/test/specimen that looks like user code. It should
have:

- a `PublicCall` enum;
- an `InternalMsg` enum;
- a callable service;
- one internal continuation;
- one host call that succeeds;
- one send-only/internal path that succeeds;
- one old mistake that now fails to compile.

Minimum negative fixtures:

| Fixture | Must fail because |
|---|---|
| `call_send_only.rs` | send-only address/capability cannot be called |
| `call_internal_message.rs` | internal continuation is not callable |
| `missing_handle_call.rs` | callable isolate forgot `handle_call` |
| `wrong_send_effect.rs` | isolate did not declare outbound send capability |
| `non_send_message.rs` | user message/closure is not `Send`, if pinned cleanly |
| `cancelable_dispatch_before_admit.rs` | if Phase 097/type rail supports this |

Minimum positive fixtures:

| Fixture | Must pass because |
|---|---|
| `call_public_message.rs` | callable service handles public request |
| `send_internal_message.rs` | internal continuation can be sent normally |
| `send_only_isolate.rs` | no-call isolate need not implement `handle_call` |
| `defer_multiturn_good.rs` | `call_ctx.defer(...).reply(...)` is copied shape |

Prefer a checked fixture harness over fragile prose. If exact stderr pinning is
too brittle, assert compile failure plus one stable phrase from Tina's
diagnostic.

## Required Tests

- compile-fail/ui/doctest cases for:
  - call send-only address;
  - missing `handle_call` on callable isolate;
  - wrong associated `Send` or `Call`;
  - non-`Send` message/closure, if diagnostic can be pinned;
  - internal continuation not callable;
  - cancelable deferred admission ordering, if compile-time shape supports it.
- runtime regression tests for compatibility paths;
- one migrated specimen/system;
- docs snippets compile where practical.
- the user proof matrix above.

Run at least:

```text
cargo fmt --all --check
cargo test -p tina
cargo test -p tina-runtime
cargo test -p tina-macros
cargo test -p tina-http --tests
cargo clippy -p tina -p tina-runtime -p tina-http --tests -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

If compile-fail harness needs a new dev dependency, keep it small and justified.

## Success

At least one major runtime silent failure becomes a compile error in a
user-style fixture.

At least one ugly trait-soup error becomes a useful diagnostic with a pinned
phrase.

Callable public messages and internal continuation messages have a clear copied
shape.

Cancelable deferred work is harder to dispatch before bounded admission.

The docs say which failures stay runtime because they are real runtime facts.
