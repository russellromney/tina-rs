# Ergonomics Checklist

Phases 047, 048, 052, and 058 retired most of the hand-rolled
boilerplate that early Eiffel examples carried. This page lists the
primitives you should reach for when writing a new Tina program,
example, or test.

If you're reading older code that does not use these, treat that as
debt rather than precedent.

This page is the "use this, not that" checklist for new code. Live paper cuts
belong in `examples/FINDINGS.md` or the next phase plan, not in the user guide.

## Use this, not that

### Mailbox factory

Use `tina_runtime::DefaultThreadedMailboxFactory` (threaded runtime)
or `tina_runtime::DefaultMailboxFactory` (explicit-step runtime).

Do not hand-roll `Rc<RefCell<VecDeque<_>>>` mailboxes. (~50 lines per
example.)

### Single-shard programs

If your program runs on one shard, use `tina::SingleShard` and omit
the `shard = ...` argument from `#[tina::isolate]` /
`#[tina_runtime::isolate]` — both default to `SingleShard`. Multi-shard
programs still declare their own shard type explicitly.

Do not declare a per-program `EiffelShard` / `MyShard` just to satisfy
the macro.

### Bound address

Use `runtime.observe_next_bound().wait(timeout)` to learn the address
the listener bound. Register the waiter *before* you `try_send` the
listener's `Start` so the registration lands ahead of the bind
completion.

Do not pass an `Arc<Mutex<Option<SocketAddr>>>` into the listener
isolate and poll for it.

### Isolate completion

Use `runtime.observe_isolate_complete(addr).wait(timeout)` to learn
when an isolate has stopped.

Do not pass an `Arc<AtomicBool>` "done" flag through the isolate.

### Isolate final result

Use `stop_with(value)` from the isolate plus
`runtime.observe_result::<T, _, _>(addr)?.wait(timeout)` from the host
when the host needs the isolate's typed final value (counters, parsed
output, accumulated state). Single-claim per `(isolate, generation)`;
no replay cache.

Do not pass an `Arc<Outcome>`, `Arc<Mutex<Vec<_>>>`, or atomics into
the isolate just so the host can read the final value after stop.

### Child restart

Use `runtime.observe_child_restarted(parent).wait(timeout)` to learn
when a supervised child has been restarted.

Do not hand-roll an `Arc<AtomicU64>` generation counter.

### Operation completion

Use `runtime.observe_operation_done(addr, CallKind::...).wait(timeout)`
when host code needs to know a runtime call completed.

Do not poll the trace for "did my sleep/read/write finish yet?"

### Trace fingerprint (replay)

Use `RuntimeEvent::stable_hash()` or `stable_trace_hash(&events)` for
deterministic-replay fingerprinting.

Do not `format!("{event:?}").hash(...)`. The `Debug` representation
is not stable across releases.

### Native HTTP server

Use `tina_http::HttpListener::with_config(...)` with
`HttpServerConfig::dev()` or `HttpServerConfig::pressure()`.

Do not hand-thread `HttpLimits`, service-call timeout, and connection
mailbox capacity through every example unless you are testing those
knobs directly.

### HTTP routing

For routes that read isolate state, use `StatefulRouter<S>`:

```rust
use tina_http::StatefulRouter;

let router = StatefulRouter::<Counter>::new()
    .get("/counter", get_counter)
    .post("/counter", post_counter)
    .method_not_allowed();
reply(router.dispatch(self, &request))
```

For routes that don't need the isolate's state, use the plain
`Router` with stateless `fn(&HttpRequest) -> HttpResponse` handlers.
`method_not_allowed()` distinguishes 405 (path known, method
mismatch) from 404 (path unknown).

Do not hand-write `match (request.method.clone(), request.path.as_str())`
in service isolates with more than one or two routes.

### HTTP requests and responses

Use the builders and status helpers:

```rust
HttpRequest::get("/x").header("Host", "example").build()
HttpRequest::post("/x").body(bytes).build()
HttpResponse::text("ok")
HttpResponse::not_found()
HttpResponse::service_unavailable()
HttpResponse::gateway_timeout()
```

Do not manually assemble method/path/header/body structs for boring
cases.

### HTTP bodies

Use `HttpRequestBody::Buffered` / `HttpResponseBody::Buffered` for small
bodies. Use `HttpRequestBody::Stream` / `HttpResponseBody::Stream` when
the body should move in bounded chunks.

Streaming request bodies are pulled with
`HttpConnectionMsg::body_next()`. Streaming responses are pulled with
`ResponseChunkMsg::Next`.

Do not call buffered bodies "streaming" just because you iterate over
the bytes later.

### Native HTTP client and pool

Use `HttpClientConfig::dev()` / `pressure()` for direct outbound calls.
Use `PoolConfig::dev()` / `pressure()` plus `HttpConnectionPool` when
you want visible pool admission (`PoolFull`) instead of hidden outbound
fanout.

Do not build an unbounded outbound request queue beside the client.

### `tina-rpc` typed client encoding

Use the macro-generated request and decode helpers when you are
talking through `tina_rpc::Client` or `tina-rpc-tokio`:

```rust
EchoClient::ping_request(payload, deadline, correlator, reply_to, max_payload)
EchoClient::ping_decode_reply(bytes, max_payload)
```

Use `tina-rpc-tokio::BridgeClient::call(...)` when a Tokio caller wants
async/await plus correlator demux for many in-flight calls.

For raw-frame specimen clients, `Json::encode(&args_tuple, max_payload)`
plus `Frame::request(...)` is enough. That is what
`examples/eiffel_rpc` does so the client code stays local and visible.

Do not reach for `serde_json::to_vec` directly unless you have a
specific reason. The `Encoding` trait is the public seam.

### `tina-rpc` service shape

Use `#[tina_rpc::service]` for typed services, then wrap the generated
dispatch in the first-form topology:

```rust
#[tina_rpc::service]
trait Echo {
    fn ping(&mut self, payload: Vec<u8>) -> Vec<u8>;
}

let dispatch = EchoService::dispatch::<EchoState, SingleShard>(
    EchoState,
    PayloadLimits::default(),
);
let service = SingleService::new(dispatch);
```

Do not string-match service method names or hand-decode `ServiceCall`
payload bytes in new code. `PooledService` and `ShardedService` are
reserved shapes, not ready user tools yet.

### `tina-rpc` connection pressure

Use `ConnectionConfig::dev()` for roomy examples,
`ConnectionConfig::bounded(n)` when the cap matters, and
`ConnectionConfig::tiny_pressure()` only for demos that want
`max_in_flight = 1`.

Do not hide overload behind a side queue when the connection can report
`Full` on the wire.

### RPC outcomes

Remember the split: server-reported wire errors are `Full`,
`UnknownService`, `UnknownMethod`, `Decode`, `Protocol`, and
`Internal`. Local client outcomes are `Timeout`, `ConnectionClosed`,
`Idle`, and `IoError`.

Do not wait for timeout or closed as wire frames. They are local client
truth.

### RPC retry

Use `tina_rpc_tokio::call_with_retry(&RetryPolicy, ...)` only when you
want explicit bridge-edge retry. Keep attempts bounded and say which
outcomes retry.

Do not bake hidden retry into service handlers or `ClientRequest`.

### RPC tracing

Put request id / correlator in spans, events, or logs as correlation.

Do not put high-cardinality request ids in Prometheus-style metric
labels.

### `#[isolate]` attribute: which path?

Use `#[tina_runtime::isolate(message = M)]` if your `handle` calls
`call(...)` against another isolate. The runtime path infers
`Call = RuntimeCall<M>` from the body.

Use `#[tina::isolate(message = M, ...)]` for pure
message/reply/spawn isolates. The `tina` path defaults `Call =
Infallible`, so `call(...).reply(...)` will not type-check there.

Do not fall back to a hand-written `impl Isolate` with
`tina::isolate_types! { call: RuntimeCall<M>, ... }` just to use
`call(...)` — the runtime macro already does that for you.

### Registering isolates

Use `runtime.register_with_capacity::<_, Infallible>(isolate, cap)`
and let the compiler infer the concrete isolate type.

Do not spell out
`register_with_capacity::<SingleService<Dispatch<MyState, Json,
SingleShard>, SingleShard>, Infallible>(...)`. Only the `Outbound`
parameter (here `Infallible`) needs an explicit hint.

### Runtime config

Use `ThreadedRuntime::new(shard, factory)` for examples and small
programs. The defaults work.

Do not hand-tune `command_capacity`, `idle_wait`, or other
`ThreadedRuntimeConfig` fields unless you have a measured reason.

### Supervision

Use `runtime.try_supervise(parent, config)` so unknown / stale
parents surface as a typed `SuperviseError::UnknownParent` instead
of panicking.

Do not use the panicking `runtime.supervise(...)` unless you genuinely
want a setup-time assertion.

### Bridge lifecycle

Use the bridge host/handle close-drain-shutdown helper when embedding
Tina inside a Tokio edge.

Do not do `Arc::try_unwrap` shutdown dances in examples.

### Bridge state aliases

Use `tina_tower_bridge::TinaService<M, R>`. Do not spell out the
six-generic `TinaTowerService<M, R, SingleShard,
DefaultThreadedMailboxFactory, ()>`.

Use `tina_reqwest_bridge::ReqwestAddress` for the worker address
field. Do not spell out
`Address<ReqwestMsg, Result<ReqwestResponse, ReqwestError>>`.

Use `tina_reqwest_bridge::ReqwestCallOutcome` for the AppMsg variant
payload that carries the reply.

### Bridge call helpers

Use `tina_reqwest_bridge::send_request(addr, req, timeout)`. Do not
hand-wrap `call(addr, ReqwestMsg::Send(req), timeout)`.

Use the re-exported `tina_tower_bridge::Service`. Do not add a direct
`tower-service` dep in your `Cargo.toml`.

### Bridge error layering

Match on the layered `CallOutcome<Result<...>>` shape by default. The
outer arm is *bridge delivery* truth, the inner is *worker outcome*
truth. Do not collapse them silently.

For app-edge code that does not need to distinguish, opt in to
`flatten_outcome(...)`. The flat `ReqwestCallError::Bridge(...)` /
`Worker(...)` variants still name which layer failed.

See [Bridge Crates](18-bridge-crates.md) for the contract.

### Ordered effects

Use `tina::sequence(...)` for "do these effects one after another."
Use `Effect::Batch` only for genuinely-independent effects (and read
its docstring — same-stream batches have a caveat).

Do not concatenate three writes into one buffer to avoid a batch.

### Deferred reply slots

Use `ctx.take_reply_slot::<R>()` and `tina::reply_to(slot, value)` to
answer a caller from a later turn (pool frontend, sharded frontend,
fanout, bridge worker).

```rust
let slot: DeferredReply<MyReply> = ctx.take_reply_slot()?;
self.pending.try_insert(req_id, slot)?;
// later:
return tina::reply_to(self.pending.take(&req_id).unwrap(), MyReply::Ok(v));
```

Don't hand-roll `Arc<Mutex<HashMap<RequestId, oneshot>>>`. No cap, no
caller signal, no terminal trace.

### Bounded pending replies

Use `tina_runtime::PendingReplies::<K, R>::with_capacity(n)` as the
named pending-promise box. Sweeps closed/replied slots before each
admit; returns `Full` when no slot can be reclaimed.

```rust
let mut pending: PendingReplies<RequestId, MyReply> = PendingReplies::with_capacity(64);
match pending.try_insert(id, slot) {
    Ok(()) => /* dispatch */,
    Err(InsertError::Full(_, _)) => /* reply Full to caller */,
    Err(InsertError::DuplicateKey(_, _)) => /* bug or stale id */,
}
```

Don't store slots in a plain `HashMap`. No cap, no sweep — abandoned
slots eat capacity forever.

### Continuation messages for runtime calls

Use `Result<T, CallError>` directly in the message variant, then pass
the variant constructor to `.reply(...)`:

```rust
enum ConnMsg {
    Read(Result<Vec<u8>, CallError>),
    Wrote(Result<usize, CallError>),
    Closed(Result<(), CallError>),
}

tcp_read(stream, max).reply(ConnMsg::Read);
```

Match `Ok` / `Err` per arm and collapse error arms into one
`stop()` (or whichever cleanup) at the bottom:

```rust
ConnMsg::Read(Ok(bytes)) => { ... }
ConnMsg::Wrote(Ok(_))    => { ... }
ConnMsg::Closed(Ok(()))  => stop(),
ConnMsg::Read(Err(_)) | ConnMsg::Wrote(Err(_)) | ConnMsg::Closed(Err(_)) => stop(),
```

Do not introduce a parallel `IoFailed` variant plus a per-call-site
closure that re-maps `Err(_)` into it. That doubles the variants and
adds the same closure to every `.reply(...)` call.

### Default-zero state on isolates

When an isolate carries several counters that all start at zero on
construction, group them into a `Default`-derived sub-struct so the
spawn site reads `counts: Counts::default()` instead of zeroing each
field by hand:

```rust
#[derive(Debug, Default, Clone, Copy)]
struct Counts { burst: usize, accepted: usize, full: usize, closed: usize }

struct Connection {
    stream: StreamId,
    slow_client: Address<DeliverMsg>,
    counts: Counts,
}
```

Do not redundantly track a sum of other counters. If `observed ==
accepted + full + closed`, drop `observed` and compute it inline (or
as a small method on the sub-struct).

### TCP loops (read-to-eof, write-all)

Follow the canonical patterns in
[`docs/tcp-loops.md`](../tcp-loops.md). Driver-level
`tcp_write_all` / `tcp_read_to_eof` are deliberately deferred — the
documented user patterns keep partial-write progress observable in
the trace.

### Mailbox capacity sizing

Read [`docs/mailbox-capacity.md`](../mailbox-capacity.md). The rule
to memorize:

> Runtime-call replies, isolate-call replies, and observed-send
> replies land in the **requester's** mailbox.

So an isolate's capacity is "incoming traffic + outstanding
continuations," not just incoming. Common diagnoses live in
`CallCompletionRejected { MailboxFull }` and `SendRejected { Full }`
trace events.

### Examples

Use examples as specimens: readable `tokio_impl.rs` and
`tina_impl.rs`, smoke tests only, README discussion. Exact invariants
live in crate tests.

## When in doubt

- Read [`examples/eiffel_rpc/src/tina_impl.rs`](../../examples/eiffel_rpc/src/tina_impl.rs)
  as a current-shape reference.
- Read [`examples/FINDINGS.md`](../../examples/FINDINGS.md) for the
  history of why these primitives exist.
- Read the per-comparison `README.md` for any specimen-specific
  notes.

## Adding to this checklist

If a new ergonomics primitive lands, add a "Use this, not that"
entry here, link the deep-dive doc if there is one, and remove or
mark the matching paper cut in `examples/FINDINGS.md` or the next phase plan.

Keep entries one paragraph. Detail goes in the deep-dive doc.
