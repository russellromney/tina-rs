# Specimen Findings History

This is the longer field journal from prior Specimen rounds. The current action
list lives in [`FINDINGS.md`](FINDINGS.md).

Cross-cutting observations from the Tokio-vs-Tina comparisons in this
directory. Per-comparison ergonomic notes live in each comparison's own
`README.md` (e.g. `specimen_real_io_chat/README.md`,
`specimen_mini_keyspace/README.md`); this file collects the patterns that show
up across more than one comparison and the runtime/API suggestions they
imply.

Findings here are dated and signed with the comparison that surfaced them so
we can track when something keeps reappearing vs. when it was a one-off.

## Round 1 closed items (Phase 059 + Phase 053)

These nine findings were the first Specimen round's product action list. All
nine landed in Phase 059 ("Specimen actionable ergonomics") or Phase 053
("Sharded service primitives"). Kept here for archaeology; new code should
not copy the patterns these replaced.

### 1. Typed isolate result waiters — landed in 059 Rock 1

Use:

```rust
// isolate
stop_with(self.outcome.clone())

// host
let result = runtime.observe_result::<T, _, _>(addr)?;
let value = result.wait(timeout)?;
```

Bounded one-slot, single-claim per `(isolate, generation)`, no replay cache.
Eager `AlreadyStopped` / `AlreadyClaimed` / `ObservationFull` at register time;
`Timeout` / `RuntimeStopped` / `StoppedWithoutResult` / `TypeMismatch` at `wait`.
Trace still emits `IsolateStopped`; the new `EffectKind::StopWith`
distinguishes the with-result path. The single-shard `ThreadedRuntime` has it;
the multi-shard variant does not yet (see Round 2 finding 1).

### 2. Continuation and pipeline sugar — landed (first form) in 059 Rock 2

Closed as "documented canonical pattern + reply aliases" rather than a macro.
`tina_runtime` ships per-call-kind reply aliases (`TcpConnectReply`,
`SignalWaitReply`, `FileReadReply`, …) so isolate enums spell the call kind
by name instead of `Result<X, CallError>`. Chapter 16 ("Continuation And
Pipeline Patterns") in the user guide is the blessed shape and names the
four anti-patterns (hidden retry, multi-call effects on one resource, async
wrapper, shared accumulator).

Deliberately not shipped: a `pipeline!` macro, a `for_each` helper, or
anything that would hide per-step trace truth.

### 3. First-class TCP loop helpers — landed (client-side first form) in 059 Rock 3

`tina_runtime::tcp_loops` ships `TcpWriteAll`, `TcpReadExact`,
`TcpReadToEof`. Each helper is a small client-side state machine; each
`next_effect`/`advance` step expands to exactly one `tcp_write` / `tcp_read`,
so partial progress is one trace event per call. Driver-level
`CallInput::TcpWriteAll`/`TcpReadExact`/`TcpReadToEof` deferred — those are a
substrate change.

### 4. Capacity diagnostics and reply-slot budgets — landed in 059 Rock 4

`tina_runtime` ships `PressureSummary::from_events(events)`,
`Runtime::pressure_summary()` / `ThreadedRuntime::pressure_summary()`, plus
`MailboxBudget { incoming, replies }` with `listener` / `session` /
`service` / `fanout` presets. Chapter 6 ("Boundedness And Overload")
rewritten to walk the `total = incoming + replies` math.

### 5. Bounded host send helpers — plan-only as of 2026-05-07

`ThreadedRuntime::send_blocking` / `send_retrying` were planned in commit
4a9df12 but not yet implemented. The closest current shape is
`send_and_observe` (sync, distinguishes `MailboxFull` from `IngressFull` /
`Closed`). `try_send_and_observe_with` is the non-blocking observer-callback
form. See Round 2 finding 4 for the actionable follow-up.

### 6. Tiny native HTTP router — landed in 059 Rock 6

`tina_http` ships `Router` (stateless handlers) and `StatefulRouter<S>`
(handlers with `&mut S`), both with `.get`/`.post`/`.put`/`.delete`/`.patch`
sugar over the generic `.route(method, path, handler)`, plus opt-in
`.method_not_allowed()` to distinguish 405 from 404. `specimen_native_http` and
`specimen_outbound_http` use `StatefulRouter<Counter>`.

### 7. Bridge specimen cleanup — landed in 059 Rock 7

`specimen_axum_counter` and `specimen_ws_room` rewritten to the specimens-rule
shape. Both use the `BridgeHost::new` / `register_bridge` /
`drain_and_shutdown` lifecycle. Follow-up bridge polish rebased the
HTTP-shaped bridge specimens onto `tina_tower_bridge::TinaTowerService`,
added the `TinaService<M, R>` alias, and re-exported Tower's `Service` trait.

### 8. RPC service topology beyond single — partially unblocked by 053

A real concurrent `PooledService` requires an isolate to hold *multiple*
pending `IsolateCall` continuations simultaneously. Today the runtime stores
`MessageCallContext` as a single `Option<...>` per isolate, so a pool
frontend would serialize through one-at-a-time. The unblocking work is at
the runtime level. `ShardedService` is now feasible on top of phase 053
sharded primitives but is not in `tina_rpc` itself yet.

### 9. Uniform overload reports for pressure runners — landed in 059 Rock 9

`tina_runtime` ships `PressureReport { side, accepted, full, closed,
timeouts, other, rss_peak_kb, exit }` plus `format_pressure_line(...)` for
the `pressure side=...` line. Chapter 17 ("Pressure Report Convention")
is the blessed shape.

## What feels good (keep these)

### Owned state through isolates is the right model
*Surfaced by:* `specimen_mini_keyspace`.

Declaring `Store` as an isolate that owns a `BTreeMap` removed the
`Arc<Mutex<_>>` temptation entirely — there is no syntactic path to shared
mutable state. This is a real property the type system enforces, not a
convention. Every comparison so far has reinforced that this is the model's
core strength.

### `call(addr, msg, timeout).then(map_outcome)` is honest
*Surfaced by:* `specimen_mini_keyspace`.

Request/reply at an isolate boundary reads like what it is: send a message,
the answer comes back as another message. Verbose vs. async/await, but no
hidden state machine and no implicit cancellation point. The right shape for
a system we want to model formally later.

### `BridgeHandle` composes cleanly with `axum::State`
*Surfaced by:* `specimen_axum_counter`.

`BridgeHandle::new(...)` produces a `Clone` value that drops straight into
`Router::with_state(...)`, and `bridge.call(req).await` is the whole call
site. The fact that this composes with axum's extractor model with zero
adapter glue is the single strongest thing about the bridge story so far.

### Visible HTTP backpressure
*Surfaced by:* `specimen_axum_counter`.

`BridgeError::{Full,Closed,Timeout}` reach an axum handler as a real error
variant, so HTTP-shaped pushback (503 etc.) is visible at the call site
instead of silently buffered. The Tokio side's `Arc<Mutex<_>>` pattern
cannot offer this property at all.

### Subscriber pruning falls out of `retain` + `try_send`
*Surfaced by:* `specimen_ws_room`.

The Room isolate's publish path is one expression:
`subscribers.retain(|tx| tx.send(text.clone()).is_ok())`. Dead subscribers
are removed in the same pass as the broadcast. The Tokio
`broadcast::channel` recipe quietly converts the same condition into
`RecvError::Lagged` that callers usually swallow.

### Out-of-order multiplexing without a shared map
*Surfaced by:* `specimen_mux_client`.

The Tina client never builds an `Arc<Mutex<HashMap<u32, oneshot::Sender>>>`
because the parser, the buffer, and the pending counter all live behind
the same mailbox. Out-of-order arrival just works — the runtime delivers
`tcp_read` replies as bytes land, and the handler walks complete lines.
The Tokio recipe needs a reader task, a submit task, a shared map, and a
oneshot per request.

### State machines as `enum` + `match` are legible
*Surfaced by:* `specimen_mini_keyspace`, `specimen_outbound_fetch`,
`specimen_persistent_counter`.

Once written, an isolate's `handle` is one of the easier-to-trace pieces of
code in the example. Each transition is one arm. Each effect is one
expression. No "where does this resume" mystery. The shape transfers across
roles (server connection, durable state machine, outbound TCP client) — the
same `Begin → IO → IO → ... → Done` skeleton fits all three.

### Append-before-apply is enforced by message shape
*Surfaced by:* `specimen_persistent_counter`.

The Tina counter cannot update `self.value` until `AppendDurable(Ok(()))`
returns, because that is the only message variant where the new value is
known. The Tokio side could trivially be written in the wrong order and
only break under crash. Durability ordering becomes a typestate property
rather than a discipline.

### Supervision policy is named, finite, and observable
*Surfaced by:* `specimen_supervised_worker`.

`runtime.supervise(parent, SupervisorConfig::new(OneForOne, RestartBudget::new(N)))`
is the entire restart story. The policy has a name, the budget is finite,
and the runtime emits `RuntimeEventKind::SupervisorRestartTriggered` so the
restart count is asserted from the trace, not from a counter the user
maintained. Tokio shops re-write the supervise loop every time, slightly
differently, with no shared vocabulary.

### Deterministic replay is a real, asserted property
*Surfaced by:* `specimen_replay_dst`.

`Simulator::new(seed)` plus `run_until_quiescent` plus
`stable_trace_hash(...)` produces a fingerprint that is byte-identical
across two runs of the same seed and *different* across two seeds. Tokio
has no analogue — `start_paused: true` is a paused clock, not a seeded
scheduler. This is the property the rest of Specimen silently relies on:
every other comparison can in principle be replayed under seeded faults.

### Tina-as-client and Tina-as-server are the same Tina
*Surfaced by:* `specimen_outbound_fetch`.

`tcp_connect(addr).then(...)` reads the same as `tcp_bind` and
`tcp_accept` from server comparisons. The `Connected` reply also returns
both endpoints (`local`, `peer`), where `tokio::net::TcpStream::connect`
returns just the stream and forces a separate `.local_addr()` call.

### `signal_wait` is the whole signal story at the user-code surface
*Surfaced by:* `specimen_graceful_shutdown`.

`signal_wait("sigint", timeout).then(SignalMsg::Received)` is one
runtime call. The reply carries the signal name, so a single watcher
can distinguish "sigint" (graceful) from "sigterm" (forced). The
shutdown *trigger* is decoupled from the shutdown *effect* — the
watcher's only job is `send(producer, ProducerMsg::Stop)`, and the
producer absorbs shutdown via a normal match arm. State machines
treat shutdown the way they treat every other event.

### Cancellation as a message arm beats `select!` for inspectability
*Surfaced by:* `specimen_graceful_shutdown`.

Tokio's idiom for "do work or shutdown, whichever arrives first" is
`select! { _ = sleep(...) => ..., _ = ctrl_c => ... }`. It's correct
and short — and *invisible*: there is no trace event for "we
cancelled the timer because shutdown won the race." Tina's idiom is
that the producer's `Stop` arm flips `self.stopped = true`, the next
`TimerFired` sees the flag, and the handler emits `noop()`. No
cancellation, no race — the timer just completes into a state that
no longer cares. The same property that makes Tina more verbose
(every transition is a message) makes its cancellation decisions
inspectable in the trace.

### `runtime.shutdown_report()` is a primitive Tokio doesn't have
*Surfaced by:* `specimen_graceful_shutdown`.

When a Tokio runtime shuts down, what was in flight, when, in what
order, against which task — gone. Whatever the application was
tracking via shared atomics is what you have. Tina exposes
`runtime.shutdown_report()` so the operator can ask the runtime
itself what work was outstanding at shutdown time. We don't even
exercise it in the example — we already track produced/processed
via telemetry — but it's worth flagging as the kind of primitive
that only exists when the runtime knows about the work it owns.

### Tina can be shorter than Tokio when the work is genuinely stateful
*Surfaced by:* `specimen_native_http`.

`specimen_native_http` is the first specimen where the Tina side has
*fewer* lines than the Tokio side: 73 vs 87 for an HTTP/1.1 counter.
The Tina side is `Counter` (one `#[isolate]` impl with a
`(method, path)` match) plus an
`HttpListener::with_config(addr, counter, HttpServerConfig::dev())`.
The Tokio side is `axum::Router` plus an `Arc<CounterState>` plus
two extractor handlers plus a `with_graceful_shutdown` hook plus a
side-thread runtime to host the server while the test client runs.

The runtime model carries its own ergonomics weight when the work
is *really* stateful. Owning a `u32` directly (`self.value += 1`)
beats `Arc<AtomicU32>` + extractor State + handler function. This
isn't going to be true for every workload — most of the existing
comparisons are still longer on the Tina side because the runtime
adds setup cost (Listener, Registry, Connection isolates) that
pays back at scale, not in a 50-line example. But it shows the
trend line: as the runtime model retires accidental complexity
elsewhere (mailbox factory, side channels, manual shard types via
047), the per-example Tina cost keeps shrinking.

## What feels bad (papercuts)

### Mailbox boilerplate per example — resolved in Phase 047
*Surfaced by:* `specimen_mini_keyspace`, `specimen_real_io_chat`,
`specimen_axum_counter`, `specimen_ws_room`, `specimen_mux_client`,
`specimen_supervised_worker`, `specimen_persistent_counter`,
`specimen_outbound_fetch`, `specimen_graceful_shutdown`.

Before Phase 047, every Tina example rolled its own `Mailbox<T>` +
`MailboxFactory` implementation backed by `Rc<RefCell<VecDeque<_>>>`.
Forty lines of mostly identical boilerplate to do the most obvious
in-process thing.

**Phase 047 replacement:** use `tina_runtime::DefaultMailboxFactory` for
explicit-step runtimes and `DefaultThreadedMailboxFactory` for threaded
runtimes. Capacity is still explicit at registration and spawn.

### The runtime knows; the user has to scrape — partly resolved in Phase 047
*Surfaced by:* `specimen_mini_keyspace`, `specimen_real_io_chat`,
`specimen_mux_client`, `specimen_supervised_worker`, `specimen_persistent_counter`,
`specimen_outbound_fetch`, `specimen_outbound_http`, `specimen_graceful_shutdown`,
`specimen_replay_dst`.

The most-recurring papercut in the suite. Before Phase 047, the runtime *had* the
information the driver thread needs — every comparison's "wait for X
to happen" is something the runtime emits as a trace event or knows
internally. But the only public way to read it is `complete_trace()`
polling or hand-rolled side channels. Every comparison invents its
own variant:

- `specimen_mini_keyspace`, `specimen_real_io_chat`:
  `Arc<Mutex<Option<SocketAddr>>>` because `tcp_bind` won't tell the
  spawning thread what port it got. **Resolved:** `observe_next_bound()`.
- `specimen_mux_client`: `Arc<Mutex<Vec<u32>>>` to harvest arrival
  order from the client isolate.
- `specimen_supervised_worker`: `Arc<Mutex<Option<Address<...>>>>` plus
  an `AtomicU64` generation counter so the driver can wait for the
  *next* worker incarnation after a restart. **Partly resolved:**
  `observe_child_restarted()` replaces the generation counter; initial
  child address publish still needs a small slot until Tina grows an
  observe-child-spawned shape.
- `specimen_persistent_counter`: a `u64` correlation id (`op`) threaded
  through every continuation message so the driver can know when a
  *specific* increment has finished.
- `specimen_outbound_fetch`: `Arc<AtomicBool>` `done` flag the driver
  spins on while the fetcher isolate completes. **Resolved:**
  `observe_isolate_complete()` for the *completion* signal; the
  per-fetch `Arc<Outcome>` (successful / failed / bytes counters)
  is still app data.
- `specimen_outbound_http`: per-request `std::sync::mpsc` channel +
  short-lived `Driver` isolate that does `call(client, ...,
  timeout).then(Returned)` and forwards the result. The pattern is
  the documented bridge between sync host code and isolate-driven
  `call(...)`; small but recurs at every sync-host call site.
- `specimen_graceful_shutdown`: `Arc<Telemetry>` with four atomics
  (produced, processed, signal_received, producer_stopped) plus a
  three-condition spin-loop on the driver thread.
- `specimen_mini_keyspace`, `specimen_supervised_worker`: `complete_trace()`
  polled in a loop for `CallKind::TcpStreamClose` /
  `SupervisorRestartTriggered` events the runtime already emits.
  **Partly resolved:** operation and restart waiters cover the common
  cases; terminal/shutdown observation is still future work.
- `specimen_replay_dst`: `format!("{event:?}").hash(...)` to fingerprint
  the trace because there was no stable event hash. **Resolved:**
  `RuntimeEvent::stable_hash()` and `stable_trace_hash()`.

Nine comparisons, all reaching for the same missing primitive from
slightly different angles. The runtime already has the information;
example code shouldn't be the one polling for it.

**Phase 047 replacement:** typed bounded waiters for bound address,
isolate complete, operation done, and child restarted; plus stable trace
hashing. Remaining pain: richer child-spawn observation, terminal/shutdown
waiters, sync-host bridging for in-process `call(...)`, and app-specific
facts (mux arrival order, per-fetch outcomes, per-op telemetry) still
need either ordinary state or a future observability shape. After the
specimens-rewrite pass, the cleanest unifying primitive would be a
typed observation handle that resolves to the isolate's *final state*
(a typed `Result<AppData, ObservationError>`), retiring the
`Arc<Outcome>` / `Arc<Telemetry>` / `Driver`-isolate-+-mpsc patterns
across `specimen_mux_client`, `specimen_persistent_counter`,
`specimen_outbound_fetch`, `specimen_outbound_http`, and
`specimen_graceful_shutdown` at once.

### Tokio + Tina signal handlers do not coexist cleanly in one process
*Surfaced by:* `specimen_graceful_shutdown`.

Both `tokio::signal::ctrl_c()` and `tina_runtime::signal_wait("sigint", _)`
register process-global handlers via `signal-hook`. `signal-hook` chains
handlers, so multiple registrations *technically* coexist, but when the
Tokio runtime drops, its registration stays in the chain. Subsequent
SIGINTs fire the now-orphaned Tokio handler too. This works in practice
but is the kind of cross-runtime sharp edge that is hard to debug when
something does break, and there is no public API to query "which
handlers are registered" or "tear down my registrations."

`specimen_graceful_shutdown` works around it by spawning each side as a
subprocess in `compare` mode. Worth a public note in any "embedding
Tina inside a Tokio app" guide.

**Improvement:** document the coexistence pattern explicitly, and
ideally expose a `runtime.unregister_signal_handlers()` for tests
that want to swap signal ownership cleanly.


### "Process a list of things" has no native shape — partly resolved in Phase 047
*Surfaced by:* `specimen_mini_keyspace`, `specimen_mux_client`.

A `VecDeque<Command>` plus "do them one at a time" required a hand-rolled
recursive `next_effect()` helper that pops + dispatches + tail-calls into
itself via response messages. There is no built-in iteration combinator and
nothing that resembles `for cmd in commands { ... .await }`. This shape is
going to recur in every connection handler.

`specimen_mux_client` ran into the same gap from a slightly different angle:
issuing three independent `tcp_write` effects on a single stream as a
`batch(...)` wedged the runtime; the example had to concatenate the three
requests into one payload. Multiplexing in Tina currently has to either
collapse independent ops into one buffer or chain them sequentially via
continuation messages — Tokio's "spawn N tasks that each `.await` on the
same connection" has no clean analogue.

**Phase 047 replacement:** `tina::sequence(...)` is documented sugar for
ordered effect lists; `Effect::Batch` now names the same-stream caveat
explicitly in its docstring; `docs/tcp-loops.md` ships canonical
write-all and read-to-eof patterns plus the "do these writes one after
another" continuation pattern. The recursive `next_effect()` shape is
still the right pattern for "process a list" — what changed is that the
caveat is documented and the runtime gives it a name.

### Mailbox capacities are load-bearing magic numbers — partly resolved in Phase 047
*Surfaced by:* `specimen_mini_keyspace`, `specimen_real_io_chat`.

**Phase 047 replacement (docs):** `docs/mailbox-capacity.md` names the
"reply slots count against the requester's mailbox" rule plainly, ships a
small sizing table for listener / connection / store / worker / fanout
roles, and points at the trace events
(`CallCompletionRejected { MailboxFull }`,
`SendRejected { Full }`) that diagnose under-sizing deterministically.
Tests in `tina-runtime/tests/capacity_truth.rs` pin the rule. A separate
"reply capacity" budget on registration remains future work.



We pick 16 because other tests pick 16. Pick 4 and the run silently breaks
— no compile-time hint, no warning, just dropped messages or deadlock.

`specimen_real_io_chat` exposes the specific *cause* that makes capacities
load-bearing: every `send_observed(...).then(...)` outcome comes back
through the *requester's* mailbox. A connection isolate that fans out a
burst of 64 admissions has to absorb 64 reply messages before it can
finish writing its response. The first draft of that example sized the
connection mailbox at the obvious "one per concurrent operation" value
and could not collect enough observed outcomes to make progress; the
fix was sizing the connection mailbox separately to account for replies.
This is not unique to `send_observed` — every `call(...).then(...)`
and `tcp_*(...).then(...)` consumes one slot in the requester's
mailbox when it lands. Mailbox capacity is therefore a function of
both incoming traffic *and* outstanding outbound continuations, and
that relationship is implicit.

**Improvement:**
- Better diagnostics when a mailbox is undersized for its observed
  traffic; ideally clearer guidance per isolate role
  (connection-handler, store, listener).
- Explicit documentation of the "reply slots count against the
  caller's mailbox" rule, plus a sizing rule of thumb in the user
  guide. Optionally a separate "reply capacity" budget on isolate
  registration so the two concerns aren't tangled.

### Result-shaped continuations carry dead `Err` arms
*Surfaced by:* `specimen_mini_keyspace`, `specimen_graceful_shutdown`.

Two flavors of the same shape — runtime calls return outcomes that
are wider than the common case, forcing every call site to write
match arms for failure modes that effectively never fire.

- **`CallOutcome<Reply>` for in-process calls.** For a call to a
  store isolate that always replies, the `Timeout` / `Closed` arms
  are unreachable but still have to be matched on every call site
  (`specimen_mini_keyspace`).
- **`Result<(), CallError>` on `sleep(...).then(...)`.** Every Tina
  handler that uses `sleep(...).then(...)` ends up with a
  `TimerFired(_, Result<(), CallError>)` continuation whose `Err`
  arm is dead code on healthy systems (`specimen_graceful_shutdown`).
  The same will apply to other "rarely fail" runtime calls.

**Improvement:**
- A "call that can't time out" form for in-process callees, or a
  more focused outcome type when the timeout is the only possible
  failure.
- A `sleep(...).reply_ignore_error(...)` shorthand or a
  `Result<(), Infallible>`-shaped narrower outcome for
  effectively-infallible runtime calls.

### `Result<T, CallError>` payloads dead-code-flagged when both arms `stop()`
*Surfaced by:* `specimen_mux_client`, `specimen_outbound_fetch`.

A close cousin of the entry above. When a continuation variant
carries `Result<T, CallError>` but the handler treats both `Ok` and
`Err` as terminal — collapse-into-`stop()`-or-cleanup — `clippy`
flags the inner field as dead code. Pattern:

```rust
ConnectionMsg::Closed(_) => {
    self.state.stream = None;
    self.next_iteration()
}
```

`clippy` complains that `Result<(), CallError>`'s contents are
"intentionally ignored." Fix: match `Ok(())` and `Err(_)` arms
explicitly, even if both bodies are identical:

```rust
ConnectionMsg::Closed(Ok(())) | ConnectionMsg::Closed(Err(_)) => {
    self.state.stream = None;
    self.next_iteration()
}
```

This is the spelled-out form that makes the typed payload "active"
in dead-code analysis. Same pattern works for the original
`IoFailed`-style collapse seen in the Listener arms.

**Improvement:** mostly stylistic — the new ergonomics-checklist
entry "Continuation messages for runtime calls" pins the explicit
`Ok` / `Err` arm pattern. A "call that always succeeds" outcome
type (above) would retire the issue entirely for runtime calls
that genuinely can't fail.

### Hand-zeroed counter fields at every isolate spawn site
*Surfaced by:* `specimen_real_io_chat`, `specimen_mini_keyspace`,
`specimen_mux_client`, `specimen_persistent_counter`,
`specimen_outbound_fetch`.

Pre-rewrite, each example's isolate spawn site listed its
zero-initialized counters by hand:

```rust
Connection {
    stream,
    slow_client: self.slow_client,
    requested_burst: 0,
    observed: 0,
    accepted: 0,
    full: 0,
    closed: 0,
}
```

Five default-init lines per spawn, on top of the required-state
fields. Two-fold smell: the spawn site is louder than it needs to
be, and the redundant counter `observed = accepted + full + closed`
gets re-introduced at every site.

**Resolved (style):** group the default-zero fields into a
`Default`-derived sub-struct:

```rust
#[derive(Debug, Default)]
struct Counts { burst: usize, accepted: usize, full: usize, closed: usize }

struct Connection {
    stream: StreamId,
    slow_client: Address<DeliverMsg>,
    counts: Counts,
}

// Spawn site:
Connection { stream, slow_client: self.slow_client, counts: Counts::default() }
```

The new ergonomics-checklist entry "Default-zero state on isolates"
codifies it. Every specimen rewrite uses it where it applies.

### Bridge-hosted services: two runtimes that don't compose cleanly
*Surfaced by:* `specimen_axum_counter`, `specimen_ws_room`, `specimen_mux_client`.

A bridge service is one Tina runtime (its own thread) plus a Tokio
runtime that hosts axum and calls into the bridge. That is exactly
what the bridge *is*, and `BridgeHandle` composes cleanly with axum at
the call site (see "What feels good"). The friction is at the seams.

Two failure modes hit during the comparisons:

- **Sync recv inside a Tokio current_thread `block_on(...)` deadlocks
  the executor.** `specimen_mux_client` originally used
  `std::sync::mpsc::Receiver::recv()` to wait for a server-shutdown
  signal inside the Tokio runtime hosting the responder. The
  current_thread runtime cannot drive futures while the OS thread is
  blocked on a sync recv, so the responder task never advanced and
  the test wedged. Fix: `tokio::sync::oneshot`. This is a real
  cross-runtime footgun — the failure looks like "my server didn't
  start" but the cause is "my driver thread blocked the executor."
- **Resolved in Phase 047: `Arc<ThreadedRuntime>` no longer has to be
  unwrapped in example code.** `BridgeHost::drain_and_shutdown()` owns
  the runtime, waits for outstanding `BridgeHandle` clones to drop, and
  leaves the host retryable if a drain timeout fires before shutdown.

The comprehension cost the first time you see the two-runtime
arrangement is also real — first-time readers do not expect the Tina
side of an axum app to spin up *both* a `ThreadedRuntime` and a
`tokio::runtime::Builder::new_current_thread()`.

**Phase 047 replacement:** a documented `BridgeHost` composition pattern,
plus `drain_and_shutdown()`. The bridge is still a two-runtime compromise,
not native Tina HTTP.

### Continuation enum growth
*Surfaced by:* `specimen_persistent_counter`, `specimen_outbound_fetch`,
`specimen_mini_keyspace`, `specimen_mux_client`, `specimen_graceful_shutdown`,
`specimen_outbound_http`.

The user-guide ergonomics page already lists this; the new comparisons
confirm it lands harder when the protocol has more than three steps.
`CounterMsg` had to thread an `op: u64` through `Increment →
AppendDurable → publish`, plus the recovery chain `Recover →
SnapshotLoaded → JournalLoaded`. `FetchMsg` ballooned to `Begin →
Connected → Wrote → Read → Closed` plus their `Ok`/`Err` arms.
`MuxMsg`, `ProducerMsg`/`ConsumerMsg`/`SignalMsg`, and
`DriverMsg`/`HttpClientMsg` add another four sets of "one variant per
runtime call" enums.

**Resolved (style):** the new
[`docs/tina-user-guide/11-ergonomics-checklist.md`](../docs/tina-user-guide/11-ergonomics-checklist.md)
entry "Continuation messages for runtime calls" pins the canonical
shape — `Read(Result<Vec<u8>, CallError>)` carrying the runtime-call
result directly, `.then(Variant::Read)` passing the variant
constructor, error arms collapsed into one `stop()`. Every specimen
rewrite uses it.

**Still open (primitive):** typed continuation aliases or a
generated-name helper, as noted in
[`docs/tina-user-guide/10-ergonomics-notes.md`](../docs/tina-user-guide/10-ergonomics-notes.md).
The spelled-out shape is consistent now; the real win would be sugar
that hides the per-step variant for linear pipelines.

### No `read_to_end` / `write_all` at the runtime-call layer — partly resolved in Phase 047
*Surfaced by:* `specimen_outbound_fetch`.

**Phase 047 replacement (docs):** `docs/tcp-loops.md` ships canonical
write-all and read-to-eof patterns at the user level. Driver-level
`tcp_write_all` / `tcp_read_to_eof` are deliberately deferred: a runtime
helper that hides the loop also hides the per-step trace event, which is
a Tina trace-truth regression. The user-side patterns keep every
partial-write progress observable in the trace.



`tcp_read` returns one `Vec<u8>` chunk; EOF is "zero bytes" and has to
be hand-detected. `tcp_write` may write less than the buffer; partial
writes need a `pending_write.drain(..count)` self-loop. Tokio's
`read_to_end` / `write_all` papers over both. Probably correct that
Tina exposes the truthful one-shot form — but every TCP client will
re-implement the same loop until a helper lands.

**Improvement:** companion helpers (`tcp_read_to_eof`, `tcp_write_all`)
or a documented snippet in the TCP guide.

### Threaded and explicit-step API surfaces have drifted apart — partly resolved in Phase 047
*Surfaced by:* `specimen_supervised_worker`.

**Phase 047 replacement:** `Runtime::try_supervise` and
`ThreadedRuntime::try_supervise` ship as non-panicking variants that both
return `Result<(), SuperviseError>` so the explicit and threaded surfaces
have matching signatures. The panicking `supervise` is kept for setup-time
assertions on both. `ThreadedRuntime::try_send` keeps fire-and-forget
semantics; the docstring now names the asymmetry vs. `Runtime::try_send`
explicitly and points callers who need message-recoverable strict mode to
[`send_and_observe`](`tina_runtime::ThreadedRuntime::send_and_observe`),
which already exists.



Two near-twin APIs (`ThreadedRuntime::*` vs. `Runtime::*`) with
divergent failure surfaces and return shapes. Users porting between
the two — or reading code that mixes them — will get tripped up.

- **`try_send` failure surface:** `ThreadedTrySendError` carries
  `IngressFull` and `WorkerStopped` only — sending to a dead address
  returns `Ok(())` and the runtime drops the message silently on the
  worker side. The explicit-step `Runtime::try_send` returns
  `Err(Closed(message))` and gives the message back. The threaded
  form is "fire-and-forget under panic," the explicit form is
  "explicit closed signal." Same method name, different contract.
  This shows up first when something panics — exactly when a silent
  drop is the worst possible default.
- **`try_send` message ownership on full:** `ThreadedTrySendError`
  consumes the message even on `IngressFull`, so retrying requires
  `Copy` or rebuilding the message. The explicit-step form returns
  the message in the error variant.
- **`supervise` return type:** the threaded version returns
  `Result<(), ThreadedRuntimeError>`; the explicit-step version
  returns `()`. Every threaded call site ends in
  `.expect("supervise parent")`. Small but asymmetric.

**Improvement:** unify the two surfaces (or have a single public
method that internally handles the threaded-vs-explicit distinction),
and at minimum document the divergence in the porting guide and
call it out at the type level.

### `tina::isolate` vs `tina_runtime::isolate` divergence is invisible until simulator — resolved in Phase 047
*Surfaced by:* `specimen_replay_dst`.

**Phase 047 replacement:** `tina_runtime::RuntimeCallable` is a sealed
marker trait implemented only for `RuntimeCall<M>`, decorated with
`#[diagnostic::on_unimplemented]` that names the fix
("switch the attribute to `#[tina_runtime::isolate(...)]`"). Simulator
registration surfaces (`Simulator::register*`,
`MultiShardSimulator::register*_on`) carry the bound, so a `Call =
Infallible` isolate now produces a clear "the trait `RuntimeCallable` is
not implemented for `Infallible`" diagnostic.



`#[tina::isolate(...)]` wires `Call = Infallible`. `#[tina_runtime::isolate(...)]`
wires `Call = RuntimeCall<Msg>`. The simulator requires the latter,
and the failure mode is a generic-bound mismatch in the type checker,
not a comprehensible diagnostic.

**Improvement:** either lift the simulator's `Call` requirement, or
emit a targeted diagnostic when an isolate using `#[tina::isolate(...)]`
is registered with `Simulator::register_with_mailbox_capacity`.

### Simulated process restart needs a fresh runtime
*Surfaced by:* `specimen_persistent_counter`.

There is no public "warm-restart" or "re-recover" path on a live
`ThreadedRuntime`. The persistence example splits into two
`run_phase()` calls each with its own runtime against the same data
dir. Probably correct (you really do want a fresh runtime on a real
restart), but the example reads more like "two embedded services"
than "one service across a restart."

**Improvement:** either bless the "two-runtime" pattern with a
documented helper, or expose a `runtime.simulate_restart()` for tests
that re-recovers without tearing down the host process.

### `shard = SomeShard` is mandatory even with one shard — resolved in Phase 047
*Surfaced by:* `specimen_mini_keyspace`, `specimen_axum_counter`,
`specimen_ws_room`, `specimen_supervised_worker`,
`specimen_persistent_counter`, `specimen_outbound_fetch`,
`specimen_replay_dst`, `specimen_graceful_shutdown`.

**Phase 047 replacement:** `tina::SingleShard` is a built-in `Shard`
type re-exported from `tina::prelude`. The `#[tina::isolate]` and
`#[tina_runtime::isolate]` macros default `shard = ::tina::SingleShard`
when the argument is omitted, so single-shard programs no longer need a
one-off `KeyspaceShard` / `RoomShard` / etc. struct just to satisfy the
macro. Multi-shard programs continue to declare their own shard types.

### Comparisons don't yet expose load-shedding metrics
*Surfaced by:* `specimen_cpu_run`, `specimen_mem_run`.

The CPU contention runner can answer "did the comparison still pass
under N spinners?" but it cannot answer "did Tina shed load visibly while
Tokio buffered silently?" — because the existing comparisons assert a
fixed scripted output and do not yet expose accepted/full/closed counts
under load. The closest existing metric is
`specimen_real_io_chat::SideReport::saw_visible_full`.

**Improvement:** when load drivers land in individual comparisons,
surface a uniform overload-counter shape (accepted/full/closed/timeouts)
that the contention/memory runners can diff between baseline and
constrained runs.

### Constraint runners are platform-asymmetric
*Surfaced by:* `specimen_mem_run`.

`RLIMIT_AS` is a real cap on Linux but is unhelpful on macOS — sub-GB
caps reject child spawn with `EINVAL` because of address-space reserved
at process startup. The honest fix is to gate the cap to Linux and
clearly document that on other platforms the runner is a no-op. The
broader lesson: any runner that depends on kernel-level resource
limits must declare its platform truth, not pretend otherwise.

### `#[tina_runtime::isolate(shard = S)]` does not accept a generic shard
*Surfaced by:* `tina-http` (phase 048a).

`tina-http` defines `HttpConnection<S: Shard>` and `HttpListener<S: Shard>`
so a single implementation works for any user-chosen shard. The
`#[tina_runtime::isolate(...)]` attribute parses `shard = S` as a
literal type and emits an `impl Isolate for HttpConnection` whose
generic header collides with the surrounding `impl<S>` block, producing
a parse error in the user's source. The workaround is to hand-roll the
`Isolate` impl using `tina::isolate_types!` — `tina-http` does this
twice, and any future generic-over-shard isolate will hit the same
friction.

**Improvement:** the macro should propagate the generic parameters of
the impl block it is attached to. Until then, the workaround pattern
needs documenting in the user guide.

### `tcp_close_stream` rejects while a `tcp_read` is pending on the same lane
*Surfaced by:* `tina-http` (phase 048a). **Fixed** in the
`runtime-tcp-close-cancels-pending` slice.

Originally close failed with `CallError::ResourceBusy` if any lane
had pending work, which left HTTP error paths (slow-loris, parse-fail
mid-read) unable to close cleanly.

Fix: `tcp_close_stream`, `tcp_close_listener`, and `udp_close_socket`
now cancel any pending op on the resource and close. The pending
caller's continuation never fires; the cancellation is recorded as
`CallCompletionRejectedReason::ResourceClosed`. Simulator and live
driver behave the same. `run_until_quiescent` no longer hangs after a
close-while-pending. The slow-loris path in `tina-http` can now write
`408 Request Timeout` then close cleanly.

### Wire-level `CallOutcome::Full` is not deterministically constructible on a single shard
*Surfaced by:* `tina-http` (phase 048a).

The connection isolate maps `CallOutcome::Full` to `503 Service
Unavailable` and the unit tests prove the mapping. A wire-level
integration test of "service mailbox full -> 503 over TCP" was
attempted and removed: with single-shard execution, even a
capacity-1 service drains too quickly between effect-processing rounds
for concurrent calls to find the mailbox occupied. 048a substitutes a
deterministic 504 (`CallError::Timeout`) test via a service that never
replies; rock 5b in 048b is expected to ship the wire-level Full test
naturally once the connection pool primitive lands and admission
limits introduce real Full conditions.

**Improvement:** noted as 048b scope. Two complementary primitives
would also unlock it cleanly: a delayed-reply primitive on the service
side, or multi-shard service placement so the dispatcher and the
service run on different threads with their own scheduling rates.

### Outbound HTTP: visible call boundary vs. hidden await
*Surfaced by:* `specimen_outbound_http`.

The Tokio side's outbound is `client.get(url).send().await?.text().await?`.
One line. Cancellation, backpressure, the state machine, and the
connection lifecycle all live behind `await`.

The Tina side is `call(client, HttpClientMsg::call(target, request),
timeout).then(DriverMsg::Returned)` plus the matching arm. The reply
arrives as a typed `CallOutcome::{Replied, Full, Closed, Timeout}`,
so connection-busy and timeout are not silent — they are arms in the
caller's `match`. Verbose vs. async/await, and that is the trade. The
shape is the same one Tina services already use everywhere else.

## Suggested follow-ups, ranked by frequency of trip

Counted by how many comparisons surfaced the issue. Several of these
collapse to a smaller number of underlying primitives, called out in
parentheses. **Updated after the specimens-rewrite pass:** counts
reflect the current state, with style-only resolutions noted where
047 / the ergonomics checklist closed the gap.

1. **Typed observation handle that resolves to the isolate's final
   state.** *Five comparisons.*
   (`specimen_mux_client`, `specimen_persistent_counter`,
   `specimen_outbound_fetch`, `specimen_outbound_http`,
   `specimen_graceful_shutdown`.)
   The single biggest unifying primitive. Every isolate that has
   "the host wants to read app data after I stop" reaches for the
   same shape: `Arc<AtomicU32>` / `Arc<Mutex<Vec<_>>>` / a per-op
   correlator + atomic-publish slot, or — for sync host bridging
   — a `Driver` isolate + `std::sync::mpsc`. A typed
   `IsolateResultWaiter<T>` whose `wait(timeout)` resolves to the
   isolate's final state would retire all of them.

2. **Continuation enum sugar / narrower outcome types.** *Six
   comparisons.* `CounterMsg`, `FetchMsg`, `MuxMsg`, `ProducerMsg`,
   `DriverMsg`, plus `specimen_mini_keyspace`'s `next_effect()`
   recursion. **Style resolved:** the
   [ergonomics checklist](../docs/tina-user-guide/11-ergonomics-checklist.md)
   pins `Result<T, CallError>` variants + variant-constructor
   `.then(...)`. **Primitive open:** typed continuation aliases
   or a "linear pipeline" combinator that hides the per-step
   variant.

3. **Reply-slot accounting in mailbox sizing.** *Two comparisons.*
   Every `call(...).then(...)` and observed send consumes one
   slot in the *requester's* mailbox; the chat example wedged on
   this in its first draft. **Partly resolved:** documented in
   `docs/mailbox-capacity.md` with role-based sizing guidance.
   Diagnostic improvements + an optional separate "reply capacity"
   budget on registration would close it further.

4. **Sugar / docs for "sequence of calls then continue," and an
   explicit contract for `batch(...)` on same-stream effects.**
   *Two comparisons.* `next_effect()` recursive helper in
   keyspace; the wedge that hit `specimen_mux_client` when batching
   same-stream writes alongside a read. **Partly resolved:**
   `tina::sequence(...)` documented; same-stream caveat called out
   on `Effect::Batch`. Still no combinator for "for each command,
   call store, accumulate."

5. **`tcp_read_to_eof` / `tcp_write_all` companions.** *Two
   comparisons.* `specimen_outbound_fetch` hand-rolls the partial-
   write loop and the read-to-EOF loop. **Partly resolved:** user
   patterns in `docs/tcp-loops.md`. Driver-level helpers
   deliberately deferred to keep per-step trace events visible.

6. **Bless a "first-child-spawned" observation handle.** *One
   comparison.* `specimen_supervised_worker`'s `WorkerSlot` is the
   one remaining `Arc<Mutex<Option<Address>>>` side channel after
   `observe_child_restarted()` retired the generation counter.

7. **Unify `ThreadedTrySendError` / `Runtime::try_send` failure
   surfaces and `runtime.supervise(...)` / `Runtime::supervise(...)`
   return types.** *One comparison* but a type-level papercut that
   will trip every porter. **Partly resolved:** `try_supervise(...)`
   ships on both surfaces; `try_send` semantic difference is now
   documented in the doc strings.

8. **Bless the "fresh runtime per phase" pattern (or expose a
   `simulate_restart()`) for persistence tests.** *One comparison.*

9. **Document the Tokio + Tina signal-handler coexistence pattern,
   optionally expose `runtime.unregister_signal_handlers()`.**
   *One comparison.* `specimen_graceful_shutdown` deliberately drops
   the `both` mode for this reason.

10. **Tiny routing shape for `tina_http`.** *One comparison.*
    `specimen_native_http`'s server handler matches on
    `(method, path)` arms; an axum-style declarative router would
    close the last per-line gap to `axum::Router`.

11. **Uniform overload-counter shape on per-comparison `Report`s,
    so `specimen_cpu_run` and `specimen_mem_run` can diff baseline vs.
    constrained.** *Two runners.*

## How to add to this file

When a comparison surfaces something:

- Add it under "what feels good" or "what feels bad" with a one-line
  *Surfaced by:* tag listing the comparison(s).
- If multiple comparisons hit the same thing, append the new one to the tag
  rather than duplicating the entry — that's how we see what's recurring.
- Per-comparison flavor (specific code shapes, surprising error messages,
  domain-specific quirks) belongs in the comparison's own README, not here.
