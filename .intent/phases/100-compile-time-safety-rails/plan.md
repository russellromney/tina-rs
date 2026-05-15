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

If the type trick makes code clever and scary, stop and use a loud runtime
rejection plus good diagnostics.

## Goal

Move Tina's common silent failure modes left:

```text
runtime timeout/rejection -> compile error
trait soup -> useful diagnostic
copyable footgun -> impossible or hard to write
```

This phase should leave Tina nicer for humans and much safer for LLMs.

## Non-Goals

- no broad rewrite just to prove type theory;
- no typestate maze in ordinary user code;
- no breaking every specimen unless the migration is small and automatic;
- no hiding capacity/liveness as compile-time claims;
- no custom lint crate unless the macro/trait path cannot work;
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

Required proof:

- compile-fail: `call(send_only_addr, ...)`;
- compile-fail or macro error: callable isolate missing `handle_call`;
- compile-fail: internal continuation sent through call address;
- existing runtime call tests still pass;
- one specimen/system migrates to split public call message from internal
  continuation message.

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

Do not turn every call site into conversion soup. Migrate only one or two
high-value surfaces first.

Proof:

- compile-fail calling through send-only capability;
- compile-fail sending through call-only capability if such a type exists;
- one service/specimen uses narrow capabilities in state fields.

## Rock 7: Config Builder Typestate Where Worth It

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

## Rock 8: Protocol Typestate Internals

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

Run at least:

```text
cargo fmt --all --check
cargo test -p tina
cargo test -p tina-runtime
cargo test -p tina-http --tests
cargo clippy -p tina -p tina-runtime -p tina-http --tests -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

If compile-fail harness needs a new dev dependency, keep it small and justified.

## Success

At least one major runtime silent failure becomes a compile error.

At least one ugly trait-soup error becomes a useful diagnostic.

Callable public messages and internal continuation messages have a clear copied
shape.

Cancelable deferred work is harder to dispatch before bounded admission.

The docs say which failures stay runtime because they are real runtime facts.
