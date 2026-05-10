# Ergonomics Checklist

Phases 047, 048, 052, and 058 retired most of the hand-rolled
boilerplate that early Specimen examples carried. This page lists the
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

Do not declare a per-program `SpecimenShard` / `MyShard` just to satisfy
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

The same surface is on `ThreadedMultiShardRuntime`: registration is
routed to the address's owning shard. Passing an address from a
foreign runtime panics, matching `try_send`'s convention.

Do not pass an `Arc<Outcome>`, `Arc<Mutex<Vec<_>>>`, or atomics into
the isolate just so the host can read the final value after stop.

### Host bursts of typed sends

Use `runtime.try_send_outcome(addr, msg, &outcomes)` plus
`HostBurstOutcomes` when the host issues a tight burst and wants
per-send admission truth (admitted / mailbox_full / mailbox_closed /
ingress_full / worker_stopped) without writing one observer closure
per send:

```rust
let outcomes = HostBurstOutcomes::new();
for n in 0..N {
    let _ = runtime.try_send_outcome(addr, Msg::Submit(n), &outcomes);
}
outcomes.wait_complete(deadline)?;
let snap = outcomes.snapshot();
```

Each per-send outcome stays distinct in the snapshot. The observer
still fires on the worker thread; the helper removes the per-send
closure ceremony, not the worker roundtrip.

Do not hand-roll `Arc<AtomicU32>` accept/full/observed counters with
a `try_send_and_observe_with` closure per send.

### Bounded-wait control message

Use `runtime.send_observed_until(addr, deadline, backoff, || msg)`
when a host-side control message (`BurstClosed(n)`, `Stop`, `Drain`)
travels through the same bounded data mailbox. The helper retries on
`MailboxFull` / `IngressFull` until the deadline and returns typed
`SendObservedUntilError::{Timeout, Closed, WorkerStopped}`. Each
attempt is a worker roundtrip; pick a `backoff` that reflects how
fast the data mailbox actually drains.

Do not hand-roll the same `match send_and_observe { Full | IngressFull
=> sleep, Closed => bail, ... }` retry loop at every host-side
shutdown call site.

### Single-in-flight timer

Use `tina_runtime::SingleCallGate` for isolates whose rate-limit
shape is `sleep(window).reply(Tick)` and must keep at most one timer
in flight:

```rust
WorkerMsg::Submit(_) => {
    if self.gate.submit() { sleep(window).reply(WorkerMsg::Tick) }
    else { noop() }
}
WorkerMsg::Tick(_) => {
    self.processed += 1;
    if self.gate.complete() { sleep(window).reply(WorkerMsg::Tick) }
    else { noop() }
}
```

The gate is plain data; it does not own the timer or the message
type. Every `sleep(...).reply(...)` still appears as one `Sleep`
trace event.

Do not hand-roll `pending: u32` plus `was_idle = pending == 0` in
every timer-driven worker.

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

### Saved-seed regression test

Use `ReplayCase` plus `assert_replay_case` in `tina_sim::dst` for
"saved seed, saved bug" tests. Build the case via
`ReplayCase::new(...).expecting(count, hash)` and the config via
`ReplayConfig::with_faults(...).with_mailbox(role, cap)` so name and
seed are typed once and the case fits on one screen.

```rust
use tina_sim::dst::{assert_replay_case, ReplayCase, ReplayConfig};

const SOURCE: &str = "source";
const SINK: &str = "sink";

fn case() -> ReplayCase<Op> {
    let config = ReplayConfig::with_faults(my_faults())
        .with_mailbox(SOURCE, 8)
        .with_mailbox(SINK, 2);
    ReplayCase::new(/* name */ "...", /* seed */ 7, config,
                    /* scenario */ "...", vec![/* ops */],
                    /* invariant */ "...")
        .expecting(34, 0xe22d_12a5_1cd8_cf10)
}

#[test]
fn saved_seed_replays_bug() {
    let report = assert_replay_case(&case(), run_case);
    assert_eq!(report.output.full_rejections, 5); // pin the pressure shape too
}
```

In the runner, build the simulator via `case.simulator_config()` —
one line, seed already set:

```rust
let mut sim = Simulator::new(MyShard, case.simulator_config());
let sink = sim.register_with_mailbox_capacity(Sink::default(), case.config.mailbox(SINK));
```

Do not roll a per-test `Report` struct, hand-rolled fingerprint
comparison, or "is this the same trace?" pinning logic. Do not pin
only the trace hash and skip the projection counts that name the
invariant.

### Discover the saved constants

Use `observe_replay_case` plus `ReplayReport::pinned_constants` to
get the initial `expected_event_count` and `expected_trace_hash` for
a single new case:

```rust
let report = observe_replay_case(&case(), run_case);
println!("{}", report.pinned_constants());
```

For a batch of cases sharing the same `Op` and runner — typical for
one test file with three or four saved-seed regressions — use
`discover_constants` instead so one `cargo test --ignored` run
prints every block in pasteable form:

```rust
#[test]
#[ignore]
fn discover_constants_for_my_cases() {
    let cases = [
        ("happy_path_case", happy_path_case()),
        ("overflow_case", overflow_case()),
    ];
    for d in discover_constants(cases, run_my_case) {
        eprintln!("{d}\n");
    }
}
```

Either way, chain `.expecting(count, hash)` on the case literal
afterwards. Do not guess; do not run the regression test with
placeholder zeros and copy from the panic; do not write a separate
discovery test per case when one bulk test prints them all.

### Sweep seeds for a bad case

Use `sweep_seeds` for hand-cranked deterministic seed search. Not
QuickCheck. `make_case(seed)` is pure. The first failure returns a
`SweepFailure` whose `failing_case` has refreshed expected count and
hash and is ready for `assert_replay_case`.

```rust
#[test]
#[ignore] // local search, not every PR
fn seed_sweep() {
    let outcome = sweep_seeds("local sweep", 0..1024, make_case, run_case, |r| {
        if r.output.saw_bug { Err("bug appeared".into()) } else { Ok(()) }
    });
    if let Err(failure) = outcome {
        eprintln!("{failure}"); // pasteable case
        panic!("found a bad seed");
    }
}
```

Do not invent your own random generator. Do not use a sweep helper
that hides operations behind a builder.

### Shrink a saved case

Use `shrink_replay_case` to reduce a failing case to the smallest
history that still proves the bug. The shrunk case carries refreshed
`expected_event_count` and `expected_trace_hash` so it can be replayed
by `assert_replay_case` directly.

Do not shrink at the bare `History` level when the surrounding
config/mailboxes/scenario should travel with the smaller case.

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
`examples/specimen_rpc` does so the client code stays local and visible.

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

For caller-owned retry loops, use `outcome.classify()`
(`ReqwestOutcomeExt`) to collapse the layered match into three typed
buckets:

```rust
match outcome.classify() {
    ReqwestOutcomeClass::Succeeded(resp) => finish_ok(resp),
    ReqwestOutcomeClass::Transient(_) => sleep(backoff).reply(Retry),
    ReqwestOutcomeClass::Fatal(_) => fail(),
}
```

The reason payloads keep bridge-vs-worker layering distinct
(`BridgeTimeout` vs `WorkerTimeout`, `BridgeFull` vs `WorkerFull`,
etc.) and `WorkerTransport(String)` / `InvalidRequest(String)`
preserve their underlying message text for logs. The classifier does
not retry — caller still owns idempotency, budget, and backoff.

See [Bridge Crates](18-bridge-crates.md) for the contract.

### Ordered effects

Use `tina::sequence(...)` for "do these effects one after another."
Use `Effect::Batch` only for genuinely-independent effects (and read
its docstring — same-stream batches have a caveat).

Do not concatenate three writes into one buffer to avoid a batch.

### Deferred reply slots

Use `ctx.take_reply_slot()` and `tina::reply_to(slot, value)` to
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

- Read [`examples/specimen_rpc/src/tina_impl.rs`](../../examples/specimen_rpc/src/tina_impl.rs)
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
