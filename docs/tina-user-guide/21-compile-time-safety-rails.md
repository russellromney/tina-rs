# Compile-Time Safety Rails

Tina prefers compile errors over runtime rejections for facts that are not
runtime facts. A target full mailbox, a closed peer, a stale generation, a
timeout — these are runtime truth. But "this isolate has no callable surface,"
"the wrong message was routed to `handle_call`," and "the caller forgot a
`handle_call` impl" can move left into the type system.

## Use the split service handle

`tina_runtime::Runtime::register_service` returns a `ServiceHandle<M, R>` with
two lanes:

```rust
let api = runtime.register_service::<Api, Infallible>(Api::new(), 64);

api.send // tina::SendAddress<ApiMsg>
api.call // tina::CallAddress<ApiMsg, ApiReply>
```

The two are different types. A function that takes `CallAddress<ApiMsg, R>`
cannot accept `api.send`; a function that takes `SendAddress<ApiMsg>` cannot
accept `api.call`. The wrong path is a compile error.

Use the lanes deliberately:

- Internal continuations (deferred work returning, sleep completing,
  fanout to self) go through `api.send`.
- Public requests (anything the caller cares about a reply for) go through
  `api.call`.

## Use `call_typed` and `send_to`

For the new capability-typed helpers:

```rust
tina_runtime::call_typed(api.call, ApiMsg::Get(key), timeout) // CallAddress
tina::send_to::<I, _>(api.send, ApiMsg::FillDone(value))      // SendAddress
```

`tina_runtime::ThreadedRuntime::call_blocking_typed` is the host-thread
companion. The older `call` and `send` helpers still accept raw `Address` for
low-level interop, but every accidental mismatch they let through is
something the typed helpers would have caught.

## Send-only services declare the intent

```rust
#[tina_runtime::isolate(message = WorkerMsg, send_only)]
impl Worker {
    fn handle(&mut self, msg: WorkerMsg, ctx: &mut Context<'_, MyShard, Self::Reply>)
        -> Effect<Self>
    { ... }
}

let worker = runtime.register_service_send_only::<Worker, _>(Worker, 16);
worker.send // SendAddress<WorkerMsg>
// `worker.call` does not exist; nobody can call this isolate by accident.
```

`send_only` forces `Reply = ()`. The `register_service_send_only` registration
helper requires `Reply = ()`, so a worker that wants to keep some reply lane
cannot accidentally end up on the send-only registration shape.

## Missing `handle_call` is a compile error

`register_service` requires `tina::CallableIsolate`. The
`#[tina::isolate]` and `#[tina_runtime::isolate]` macros emit this impl
automatically when the impl block defines `fn handle_call(...)`. An isolate
without `handle_call` cannot be registered as callable:

```text
error[E0277]: `MyIsolate` is not a callable service
help: missing `fn handle_call`
note: callable services must define `handle_call(&mut self, msg, call)`
      on the isolate impl
note: send-only services must register through
      `register_service_send_only` instead
```

A hand-rolled `impl Isolate for MyIsolate` that uses `tina::isolate_types!`
must stamp `impl tina::CallableIsolate for MyIsolate {}` itself when it
defines `handle_call`. The trait is empty; the marker is the contract.

## Review rule

Ask:

> Could this runtime rejection be a type error?

For the cases the runtime catches today:

| Failure | Runtime truth or type rail? |
|---|---|
| target mailbox full | runtime |
| target isolate closed | runtime |
| call timed out | runtime |
| stale address generation | runtime |
| peer/backend failure | runtime |
| called a send-only handle | **type rail** (`SendAddress` is not `CallAddress`) |
| missing `handle_call` on a callable service | **type rail** (`CallableIsolate`) |
| `send_only` service tried to define `handle_call` | **type rail** (macro rejects) |
| internal continuation message reached `handle_call` | runtime — see follow-up below |

The last row is the deferred message-split work. The plan ships the capability
address split first; a later phase generates a wire enum so calling an
internal continuation message becomes a compile error too. See the comment in
`examples/systems/system_realtime_rooms/src/lib.rs` for the kind of
field-level discipline that closes the gap today.

## Good and bad shapes

Bad:

```rust
enum ApiMsg { Get(K), FillDone(V) }

impl Isolate for Api {
    type Message = ApiMsg;
    type Reply = ApiReply;
    fn handle_call(&mut self, m: ApiMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match m {
            ApiMsg::Get(k) => self.get(k, call),
            ApiMsg::FillDone(_) => call.reject(UnsupportedMessage), // wildcard at runtime
        }
    }
}
let api = runtime.register_with_capacity::<Api, _>(Api::new(), 64);
// any function that takes `Address<ApiMsg, ApiReply>` can call FillDone
```

Good:

```rust
// (same message enum; the split will become enforced when the wire enum
// follow-up lands)

let api = runtime.register_service::<Api, _>(Api::new(), 64);
// api.send: SendAddress<ApiMsg>  — for FillDone and other continuations
// api.call: CallAddress<ApiMsg, ApiReply>  — for Get and other public calls
```

The narrow lanes flow into worker structs:

```rust
struct CacheWorker {
    fill_targets: tina::SendAddress<ApiMsg>, // narrow capability passed down
}
```

The worker physically cannot call the API; only push internal continuations.
