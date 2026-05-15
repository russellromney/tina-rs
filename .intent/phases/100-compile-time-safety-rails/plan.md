# 100 Compile-Time Safety Rails

## Status

- IDD phase.
- One PR if the first slice stays mechanical. Split if the send/call address
  split becomes a broad migration.
- Runs after Phase 095. Coordinate with Phase 097 if it already owns
  cancelable deferred admission.
- Must include user-style compile-pass and compile-fail proof. Unit tests alone
  are not enough.
- Strong bias: change the heart of the service model if it makes wrong code
  impossible for LLMs to write. Do not preserve the old shape just to avoid
  migration.

## Grug Truth

Runtime handles runtime facts:

- `Full`;
- `Closed`;
- `Timeout`;
- stale generation;
- peer/backend failure.

Compile time should catch impossible-program facts:

- calling a send-only service;
- calling an internal continuation message;
- forgetting `handle_call` on a callable service;
- returning an effect the isolate did not declare;
- dispatching cancelable deferred work before bounded admission, if the API can
  make that impossible;
- private protocol transitions like DATA after trailers, if the private type
  shape stays simple.

If the type trick gets clever, stop. A loud runtime rejection is better than a
clever type maze.

## Goal

Make service-shaped Tina code split public requests from internal events:

```rust
type Message = InternalMsg;
type Call = PublicCall;
type Reply = PublicReply;
```

Then make the wrong path not compile:

```rust
call(api.call, PublicCall::Create(item), timeout); // good
send(api.send, InternalMsg::DbReturned(row));      // good

call(api.call, InternalMsg::DbReturned(row), timeout); // compile error
send(api.send, PublicCall::Create(item));              // compile error
```

Move real silent failures left:

```text
runtime timeout/rejection -> compile error
trait soup -> useful diagnostic
copyable footgun -> harder/impossible to write
```

The proof must look like code a user or cheap model would write.

## Non-Goals

- no broad rewrite for type theory;
- no custom lint crate in first form;
- no typestate maze in public service code;
- no hiding `Full`/`Closed`/`Timeout` as type claims;
- no mass specimen churn unless migration is small and mechanical.

## Rock 0: Audit And Pick The Slice

Read:

- `tina/src/lib.rs`;
- `tina-runtime/src/lib.rs`;
- `tina-runtime/src/call.rs`;
- `tina/src/pending_call_set.rs`;
- `tina-http/src/http2.rs`;
- `tina-http/src/grpc.rs`;
- docs request/reply and ergonomics checklist;
- systems/specimens that return `UnsupportedMessage`.

Add a short status table:

| Failure | Current behavior | Compile-time fix? | Decision |
|---|---|---|---|

Include these failures:

- call to send-only/internal message;
- missing `handle_call`;
- wildcard arm hiding new public call variant;
- wrong `Send` / `Call` associated type;
- non-`Send` / non-`'static` user payload;
- wrong shard/reply shape;
- cancelable deferred dispatch before admission;
- protocol transition after EOF/trailers/close;
- missing/zero config where zero is never valid.

Pick the smallest slice that gives the strong service shape:

- public call messages separate from internal messages;
- narrow send/call capabilities or service handle;
- one real diagnostic win;
- one passing user-shaped service.

Record the bigger deferrals. Do not leave them fuzzy.

## Rock 1: Better Diagnostics

Improve error messages near the public boundary.

Targets:

- `Isolate`;
- `DeferThrough`;
- `DeferCancelableThrough`;
- runtime call/send conversion traits;
- macro-generated isolate impls;
- non-`Send` / non-`'static` payload paths.

Use stable Rust where possible. If `#[diagnostic::on_unimplemented]` needs the
current nightly story, name that in the status.

Required proof:

- one compile-fail fixture that used to be trait soup;
- one pinned Tina-facing phrase, such as:
  - "message is not Send";
  - "add `send = Outbound<...>`";
  - "callable message requires `handle_call`";
  - "use `call_ctx.defer(...)` for multi-turn replies".

If exact stderr is brittle, assert compile failure plus one stable phrase.

## Rock 2: Strong Service Shape

This is the main semantic rail.

Current footgun:

```rust
type Message = ApiMsg; // public call requests and internal continuations mixed
```

Blessed service shape:

```rust
type Message = InternalMsg; // sends / continuations
type Call = PublicCall; // callable requests
type Reply = PublicReply;
```

Blessed address shape:

```rust
SendAddress<InternalMsg>
CallAddress<PublicCall, PublicReply>
```

Preferred registration result:

```rust
let api = runtime.register_service(Api::new(), cap);

api.send // SendAddress<InternalMsg>
api.call // CallAddress<PublicCall, PublicReply>
```

Preferred macro shape:

```rust
#[tina::isolate(message = InternalMsg, call = PublicCall, reply = PublicReply)]
impl Api {
    fn handle(&mut self, msg: InternalMsg, ctx: &mut Context<'_, AppShard>) -> Effect<Self> {
        // internal events and continuations
    }

    fn handle_call(&mut self, msg: PublicCall, call: CallContext<'_, Self>) -> Effect<Self> {
        // public request/reply API
    }
}

#[tina::isolate(message = WorkerMsg, send_only)]
impl Worker {
    fn handle(&mut self, msg: WorkerMsg, ctx: &mut Context<'_, AppShard>) -> Effect<Self> {
        // no public call API
    }
}
```

Rules:

- send-only services cannot be called;
- internal continuation messages cannot be called;
- callable services must implement the call handler;
- `handle_call` should match public call variants explicitly;
- old `Address<M, R>` may stay as compatibility/low-level escape hatch;
- new docs and new service specimens use the split handle by default.

Cut line: if full runtime migration gets big, ship the split service handle as
the new copied path and leave old `Address<M, R>` compatible. Do not half-migrate
the runtime.

## Rock 3: Cancelable Deferred Admission Rail

Coordinate with 097.

If 097 ships `PendingCancelableCallSet`, use it. If not, either implement the
type rail here or update 097 to own it.

Target copied shape:

```rust
let admitted = pending.try_admit(key, token, effect)?;
return admitted.effect();
```

Rules:

- child effect should be hard/impossible to obtain before admission;
- `Full`/duplicate failure returns enough authority to answer caller;
- no hidden dispatch;
- no unbounded table;
- no ABA key bug.

Proof:

- rejected admission answers caller now;
- rejected admission does not dispatch child work;
- trace proves no child call was started.

## Rock 4: Optional Type Rails

Do these only if Rock 0 finds a copied, common, cheap-to-prevent mistake.

Config builder typestate candidates:

- pool capacity;
- HTTP body caps;
- bridge max in-flight;
- listener startup config.

Default: runtime validation remains the truth for env/file config.

Private protocol typestate candidates:

- HTTP/2 stream state;
- response writer state;
- WebSocket close state.

Default: private only. Do not leak protocol typestate into user service code.

This rock must not block the main callable/send-only win.

## Rock 5: User Proof Matrix

Build one tiny proof crate/test/specimen that looks like user code.

It should have:

- `PublicCall`;
- `InternalMsg`;
- one callable service;
- one internal continuation;
- one `register_service(...)` or equivalent split handle;
- one host call that succeeds;
- one send-only/internal path that succeeds;
- one old mistake that now fails to compile.

Negative fixtures:

| Fixture | Must fail because |
|---|---|
| `call_send_only.rs` | send-only address/capability cannot be called |
| `call_internal_message.rs` | internal continuation is not callable |
| `missing_handle_call.rs` | callable isolate forgot `handle_call` |
| `wrong_send_effect.rs` | isolate did not declare outbound send capability |
| `non_send_message.rs` | user payload is not `Send`, if cleanly pinnable |
| `cancelable_dispatch_before_admit.rs` | if 097/type rail supports this |

Positive fixtures:

| Fixture | Must pass because |
|---|---|
| `call_public_message.rs` | public call path works |
| `send_internal_message.rs` | internal continuation can be sent |
| `send_only_isolate.rs` | no-call isolate needs no `handle_call` |
| `defer_multiturn_good.rs` | `call_ctx.defer(...).reply(...)` is copied path |
| `register_service_handle.rs` | split service handle exposes `.send` and `.call` |

Prefer `trybuild` if acceptable. Otherwise use `compile_fail` doctests. Each
negative fixture needs a nearby passing fixture.

## Rock 6: Docs And Review Rule

Update docs with one short section:

- use separate public call and internal message types;
- use split service handles as the default copied path;
- avoid wildcard arms in public `handle_call`;
- accept narrow capabilities in structs when possible;
- use `call_ctx.defer(...)` for ordinary multi-turn work;
- use the admitted-effect shape for cancelable work;
- runtime still owns `Full`, `Closed`, `Timeout`, stale generation, and peer
  failure.

Add this review question:

```text
Could this runtime rejection be a type error?
```

Add one good/bad box:

- bad: public and internal variants in one callable enum plus wildcard reject;
- good: `PublicCall`, `InternalMsg`, split service handle, explicit public
  match.

## Required Checks

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

If a compile-fail harness adds a dev dependency, keep it small and justify it
in the status.

## Success

- One major runtime silent failure is now a compile error in user-shaped code.
- One ugly trait-soup error has a useful pinned diagnostic phrase.
- Public calls and internal continuations have a split service-handle copied
  shape.
- Cancelable deferred work is harder to dispatch before admission, or 097 owns
  that exact follow-up.
- Docs name what remains runtime truth.
