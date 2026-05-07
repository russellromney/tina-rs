# Ergonomics Checklist

Phases 047, 048, 052, and 058 retired most of the hand-rolled
boilerplate that early Eiffel examples carried. This page lists the
primitives you should reach for when writing a new Tina program,
example, or test.

If you're reading older code that does not use these, treat that as
debt rather than precedent.

> Sister doc: [`10-ergonomics-notes.md`](10-ergonomics-notes.md) is
> a scratchpad of paper cuts found during porting. This page is the
> "use this, not that" checklist for new code.

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

### Child restart

Use `runtime.observe_child_restarted(parent).wait(timeout)` to learn
when a supervised child has been restarted.

Do not hand-roll an `Arc<AtomicU64>` generation counter.

### Trace fingerprint (replay)

Use `RuntimeEvent::stable_hash()` or `stable_trace_hash(&events)` for
deterministic-replay fingerprinting.

Do not `format!("{event:?}").hash(...)`. The `Debug` representation
is not stable across releases.

### `tina-rpc` typed client encoding

Use `Json::encode(&args_tuple, max_payload)` for the same wire bytes
the `#[tina_rpc::service]` macro decoder expects on the server side.

Do not reach for `serde_json::to_vec` directly unless you have a
specific reason — the `Encoding` trait is the public seam.

You only need `tina-rpc-tokio::BridgeClient` when you want
async/await + correlator demux for many in-flight calls. For a
single-connection raw-TCP client, `Json::encode` plus `Frame::request`
is enough.

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

### Ordered effects

Use `tina::sequence(...)` for "do these effects one after another."
Use `Effect::Batch` only for genuinely-independent effects (and read
its docstring — same-stream batches have a caveat).

Do not concatenate three writes into one buffer to avoid a batch.

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
mark the matching paper cut in `10-ergonomics-notes.md` and
`examples/FINDINGS.md`.

Keep entries one paragraph. Detail goes in the deep-dive doc.
