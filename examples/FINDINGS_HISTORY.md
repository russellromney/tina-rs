# Specimen Findings History

This is the longer field journal from prior Specimen rounds. The current action
list lives in [`FINDINGS.md`](FINDINGS.md).

*Historical journal: entries below are pre-closure claims about the corpus;
many missing-primitive claims are resolved in the closure ledger at
[`FINDINGS.md`](FINDINGS.md).*

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

**Closed in the public-corpus closure (2026-07):** multi-shard observation
shipped — `ThreadedMultiShardRuntime::observe_result`, and both `LocalSystem`
facades expose `observe_result`; Round 2 finding 1 is closed.

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
shape. Both use the `LocalSystem::try_build` / `BridgeHost::from_app` /
`register_bridge` /
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

**Updated in the public-corpus closure (2026-07):** `tina_rpc` now re-exports
`PooledService`/`ShardedService` as deliberate type-only reservations; still
unimplemented.

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

**Updated in the public-corpus closure (2026-07):** the Tina side now
returns its terminal report through `run_to_shutdown_reported`; the
four-atomic telemetry shape survives only on the Tokio control.

### Tina can be shorter than Tokio when the work is genuinely stateful
*Surfaced by:* `specimen_native_http`.

`specimen_native_http` is the first specimen where the Tina side has
*fewer* lines than the Tokio side: 73 vs 87 for an HTTP/1.1 counter.

*(Counts as of the original entry; both sides have changed since — the
comparison claim is historical, the `HttpListener::with_config` shape is
current.)*
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

**Closed in the public-corpus closure (2026-07):** the unifying primitive
landed — actors publish terminal reports via `stop_with`, hosts claim typed
result observation before start, and `run_to_shutdown_reported` pairs every
workload with a bounded observed shutdown. The named comparisons
(`mux_client`, `persistent_counter`, `outbound_fetch`, `outbound_http`,
`graceful_shutdown`) all migrated off the sidecar patterns listed here.

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

**Updated in the public-corpus closure (2026-07):** there is no `compare`
mode today; the runner exposes `tokio`/`tina` modes and drops `both`
deliberately in-process. The coexistence caveat above (process-global
handlers, no unregister API) still stands.

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
ordered effect lists;

**Corrected in the public-corpus closure (2026-07):** no `tina::sequence`
exists today; the canonical shape is the continuation chain above (and the
`tina::flow!` state-machine macro for multi-step flows). `Effect::Batch` now names the same-stream caveat
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
[`docs/tina-user-guide/11-ergonomics-checklist.md`](../docs/tina-user-guide/11-ergonomics-checklist.md).
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

**Updated in the public-corpus closure (2026-07):** `SideReport` no longer
exists; the chat report now carries exact counters
(`accepted, full, closed, delivered, buffered`). The follow-up below —
uniform overload counters on the benchmark runners — remains genuinely open.

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
`Isolate` impl using `tina::isolate_types!` — `tina-http` does this at
four sites today, and any future generic-over-shard isolate will hit
the same friction.

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

**Corrected in the public-corpus closure (2026-07):** the settled typed HTTP
delivery maps `Full` to **429**, `Closed` to 503, `Timeout` to 504, and
accepted event-only input to 202 — never 503 for `Full`. The wire-level test
exists and pins the mapping in `tina-http/tests/typed_service_delivery.rs`.

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

**Closed in the public-corpus closure (2026-07):** shipped as typed
result observation (`observe_result` + `stop_with`) on both facades; the
five named comparisons all use it. See the closure ledger.

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

**Closed in the public-corpus closure (2026-07):** typed initial/replacement
child lifecycle retired `WorkerSlot`; no `Arc<Mutex<Option<Address>>>` side
channel remains in `specimen_supervised_worker`.
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

---

# Archive: the pre-closure dated rounds and numbered findings

*Appended from `FINDINGS.md` at the closure ledger reconciliation
(2026-07). Everything below this banner is historical: it records the
dated rounds and numbered findings that the closure ledger supersedes.
Resolution markers above each entry name what closed it where the entry
predates the marker; entries without markers are closed by the ledger
itself unless explicitly marked still-open.*

## Active

### 2026-07-14 Actor-backed typed gRPC routes

`specimen_grpc_counter` exposed three state sidecars in the synchronous route
closure surface: an `Arc<Mutex<u64>>` counter, mutex-protected request-stream
handoff slots backed by a one-use source pool, and a preallocated pool of Watch
responses. The route closures were correct on the wire but taught users to move
actor state outside actors.

`GrpcRouter::try_unary_actor` and `try_streaming_actor` now register typed
split-service request addresses. Unary protobuf requests and bidirectional
`GrpcStreamingCall` authority move into those services atomically. The router
parks only the explicitly configured `with_actor_route_capacity` total,
preserves `Full`, `Closed`, `Timeout`, and `Rejected(reason)` as distinct typed
route failures and wire statuses, rejects duplicate paths, cancels actor calls
whose HTTP caller disappears, and cancels response sources returned by stale or
abandoned completions.

The specimen now keeps counter state in a request-only service. Its streaming
factory receives the owned request stream, observes one child source spawn, and
returns the typed child address. Watch uses the finite buffered-stream helper.
No router state mutex, stream slot, source pool, or response pool remains.

| Example | Current friction | Desired user-facing form | Current API sufficient | Framework prerequisite | Example PR | Tests | Status |
|---|---|---|---|---|---|---|---|
| `specimen_grpc_counter` | none | typed service addresses for stateful unary/stream routes; child-owned stream authority | yes | actor-backed gRPC route registration | this cohort | route failure taxonomy, capacity, duplicate/stale/caller-gone cleanup, native and tonic h2c smoke | migrated |

### 2026-07-14 Bounded producers and exhaustive specimen terminals

The direct example cohort replaced request-sized raw batches with
`BoundedItems` plus `bounded_batch`, retained exact `Full`, `Closed`,
`Timeout`, `Rejected(reason)`, cancellation, timer, protocol, and shutdown
results, and made completion wait for the resources each report claims have
settled. Adversarial review exposed and fixed premature stop in graceful pool
shutdown and cancel/reclaim, silent partial TCP writes, panic-based wire
parsing, a roughly 4 GiB RPC reservation derived from the maximum frame size,
public reports that still collapsed exact aggregate and worker failures,
time-triggered cancellation completion, single-read TCP request parsing, and
a TCP child that discarded its typed terminal when the parent's mailbox was
temporarily full. RPC now observes the connection's exact `CloseReason` as
well as the listener terminal; graceful shutdown scheduling is actor-owned;
and the live RPC and TCP echo paths use reported `LocalSystem` shutdown so an
unexpected typed terminal cannot bypass cleanup.

The remaining blocked chat child still preserves exact broadcast construction,
duplicate/unknown target, report assertion, protocol, and I/O terminal payloads
internally; only routing that typed result across its second outbound type is
missing.

The remaining framework finding is concrete: `specimen_real_io_chat`'s
spawned connection already requires `Outbound<DeliverMsg>` for observed
broadcast. Returning its typed connection terminal to the parent listener
requires a second outbound message type, while root registration/spawn
erasure currently requires the child's `Send` capability to be one
`Outbound<T>`. The example therefore has typed connection stop results and a
typed observed listener result, but cannot yet route the child result to the
host without application glue. Build multi-outbound spawned-child
registration/result-routing parity in a separate framework prerequisite, then
finish that final observation path.

| Example | Current friction | Desired user-facing form | Current API sufficient | Framework prerequisite | Example PR | Tests | Status |
|---|---|---|---|---|---|---|---|
| `specimen_graceful_pool_shutdown` | none | fixed-cap job state, exact pool/worker/release/close terminals | yes | none | this cohort | exact shutdown, settlement, rejection, bounded-state smoke | migrated |
| `specimen_request_scope_fanout` | none | bounded fanout/cancel with actor-owned sequencing and exact outcomes | yes | none | this cohort | cancel authority, late-reply trace, exact timers/acks | migrated |
| `specimen_pool_cancel_reclaim` | none | bounded actor-owned cancel/retry sequence, report only after full settlement | yes | none | this cohort | cancel/refill/release/pressure/timer invariants | migrated |
| `specimen_scatter_gather` | none | bounded client producer and exact invalid aggregate rows | yes | none | this cohort | success, capacity/refill, reordered/misrouted/terminal rows | migrated |
| `specimen_two_stage_pipeline` | none | bounded driver and exact per-stage/outer terminals | yes | none | this cohort | Tokio/Tina behavior and empty clean terminal set | migrated |
| `specimen_webhook_fanout` | none | bounded endpoints and exact classifier reasons | yes | none | this cohort | success/503/timeout reasons and over-cap rejection | migrated |
| `specimen_tracing_demo` | none | bounded fanout, exact timer failures, observed pressure | yes | none | this cohort | accounting, typed stop, zero/over-cap rejection | migrated |
| `specimen_rpc` | none | validated count/byte bounds and exhaustive client/listener/connection/wire terminals | yes | none | this cohort | exact clean buckets, connection close reason, and zero/over-cap rejection | migrated |
| `specimen_tcp_echo` | none | partial-write retry plus child terminal reported to listener under reported LocalSystem shutdown | yes | none | this cohort | live, simulator parity/replay/golden, README sync | migrated |
| `specimen_real_io_chat` | child result cannot traverse its second outbound type | typed connection result observed by listener and host | no | multi-outbound spawned-child registration/result parity | this cohort plus prerequisite | loopback, pre-shed bound, protocol/config negatives | blocked on prerequisite |
| `specimen_worker_pool` | none | exact worker/frontend terminals in reported LocalSystem runner | yes | none | this cohort | live terminal branches, authority settlement, bounded driver, smoke | migrated |

### 2026-07-13 Local I/O terminal observation and framed output closure

`specimen_local_io_codec_ipc` now uses the same typed terminal-result contract
on live and simulated owners. Every seeder, ingest, copy, admin, keyspace, and
live probe actor owns its report, closes every acquired resource, then publishes
with `stop_with`; hosts claim `observe_result` before start. The result mutexes,
poll loops, custom live mailbox, and simulator teardown as implicit cleanup are
gone.

Normal line and length-delimited traffic now uses bounded `UnixFramedWriter`
batches. Only the two deliberately malformed protocol injectors retain raw
`UnixWriteAll`, and their early peer-close result remains typed. The migration
also fixed three latent behavioral defects: coalesced admin shutdown discarded
already-built replies, the keyspace client treated one arbitrary read as the
whole response, and file copy stopped after the first of two close callbacks.
The adversarial pass found three more: codec EOF was not finalized, the U16
keyspace body cap did not reserve room for the `ack:` response prefix, and the
live probe used an unbounded join that did not require clean terminal
accounting. The fixes use `finish`, validate the wire-representable response
bound, and consume the live owner through bounded `run_to_shutdown`.
Focused tests force two-byte Unix writes and one-byte file writes, prove exact
decoded responses and two-file settlement, and cover empty/zero-cap/config,
bounded refusal, clean-boundary premature EOF, and truncated EOF paths.

The panic-only zero body-cap constructors remain visible at the framework
boundary. Public specimen runners validate those values before construction;
one local validation is smaller than another framework abstraction. Likewise,
the one two-file owner keeps two named close continuations; there is not yet a
second motivating consumer for a speculative multi-resource close helper.

### 2026-07-13 Classified select-race continuation routing

`ergonomics_playground` showed that `CallSelectSet` already owned branch
reservation and cancel handles, but application code still repeated the
mechanical continuation protocol. Every race declared separate reply and
cancel variants carrying `(key, token, outcome)`, unpacked each loser cancel
request, rebuilt its continuation, and assembled the cancellation batch.

`CallSelectEvent<K, R>` is now that protocol vocabulary.
`CallSelectSet::start_service` installs one cancelable split-service branch,
and `advance_service` validates the returned token, applies the caller's
business-success classifier, records reply/cancel truth, and returns the
bounded loser-cancellation effect plus exact completion state. The helper does
not choose business success, own the parent caller, or report completion before
every required cancel acknowledgement settles. The dependent playground
migration keeps that honest lifecycle visible while removing the adapter
variants and cancel mapper. Its public report now carries the exact
`CancelOutcome` instead of a count, removes the synthetic `completed: true`
field, and returns no known rough edges for winner, no-winner, batch, or cache
probes. The whole-crate pass also moved every simulator send boundary to
exhaustive `IngressSendError::{Full, Closed, ForeignSystem}` handling.

### 2026-07-13 Typed multi-shard host routing

`ThreadedRuntime`, `ThreadedMultiShardRuntime`, `LocalSystem`, and
`LocalMultiShardSystem` reject host calls and result observations before
admission when an address has foreign provenance or targets a shard outside the
local topology. Foreign provenance remains `ForeignSystem`; a same-incarnation
address for an unowned shard remains `UnknownShard`. Neither path panics,
claims result-observation capacity, nor creates a host-call driver on a routing
failure.

`LocalSystem` and `LocalMultiShardSystem` also expose the lower owner's
`call_blocking_with_host_timeout` form, preserving separate target and host
budgets without leaving the preferred application facade.

### 2026-07-12 Rate-limit decision ergonomics (closed)

`system_tenant_rate_limiter` configured immediate shedding for key-table
pressure but had to match the full seven-variant `AdmissionDecision`, then
collapse `Wait`, `Degrade`, `TimedOut`, and pressure-triggered close into an
unsupported-message rejection. Those outcomes were impossible for the chosen
policy, so the exhaustive match made the example less truthful rather than
more defensive.

`RateLimit::try_admit_at` now returns
`RateLimitDecision::{Admitted, RateLimited, KeyCapacityFull, Closed}` directly.
The `_at` suffix makes the explicit logical-time boundary visible;
`KeyCapacityFull` names the user-facing failure instead of exposing a generic
table implementation detail. `Admitted` is payload-free because the token is
already consumed and there is no permit authority to settle. Generic
`ServicePolicy::decide` deliberately widens success to
`AdmissionDecision::Admitted(())`.

`RateLimitConfig { max_keys, rate_per_sec, burst }` replaces the positional
numeric constructor arguments. Both direct examples use the narrow decision,
and `system_tenant_rate_limiter` now removes caller-supplied `Instant` from its
request and charges against the gateway owner's `call.now()`. Runtime and
simulator tests cover admission, exact retry timing, refill, tracked-key
capacity and eviction, closed state, named configuration, and deterministic
virtual-time replay across runs, seeds, and stable trace hashes.

`specimen_rate_limited_worker` also preserves `KeyCapacityFull` and `Closed`
as distinct application report terminals with their policy rejection reports. Its policy tests use explicit
logical time for exact retry/refill assertions, while its live threaded smoke
tests assert structural accounting rather than a wall-clock-sensitive exact
admit/full split. A closed-policy path proves the typed worker terminal
survives observed host-control settlement and reported shutdown. The returned
report retains the exact host-burst snapshot, pacing-call failure, and
`Delivered`/`Closed`/`WorkerStopped` control settlement alongside its compact
cross-runtime totals, so the comparison projection does not erase source
truth. Host-admitted and worker-received counts also remain distinct, so an
early worker terminal cannot hide queued messages that it never handled.

### 2026-07-12 Report-preserving LocalSystem terminal runner prerequisite

Five application-shaped specimens use `anyhow::Result` for their host
workloads. `anyhow::Error` intentionally does not implement
`std::error::Error`, so the otherwise canonical `run_to_shutdown(...)?` form
cannot participate in an outer standard-error conversion. Stringifying the
report or restoring a local four-way result merge would erase either source
truth or the terminal-runner ergonomics.

`LocalSystem::run_to_shutdown_reported` and its multi-shard parity method wrap
only the workload report in `ReportedWorkloadError<E>`. The adapter retains the
owned report, exposes `get_ref` and `into_inner`, and delegates the standard
error source to the report's `AsRef<dyn Error + Send + Sync>` contract. The
existing typed-error runner remains unchanged. Framework tests cover actual
downstream `anyhow` use with outer `?`, clean, workload-only, shutdown-only,
and dual results, exact report settlement, source-chain preservation, and a
non-`anyhow` report container. The five motivating specimens now use the
reported runner directly, preserving their `anyhow` source chains without
local terminal-result combiners.

### 2026-07-12 LocalSystem atomic root bootstrap parity

`system_job_queue` exposed a facade gap during its `LocalSystem` migration:
the lower explicit, threaded, and multi-shard owners could atomically register
and bootstrap the queue, but the preferred application owner could not. A
manual `register_root` followed by `try_send(Bootstrap)` would weaken the
required first-message ordering and turn startup into a cleanup protocol.

`LocalSystem::register_root_with_bootstrap` and
`LocalMultiShardSystem::register_root_with_bootstrap_on` now forward the
existing atomic threaded contract. Their typed
`ThreadedRegisterBootstrapError` preserves the bootstrap message on mailbox
`Full`/`Closed`, command `Full`/`Closed`, and unknown-shard failures; an
accepted command followed by worker failure does not counterfeit authority,
and `WorkerUnresponsive` remains distinct from `WorkerStopped` because a
timed-out accepted command may still register later.
The simulator now has matching single- and multi-shard
`register_with_capacity_and_bootstrap[_on]` vocabulary, with prefill before
entry/address publication. Focused tests prove first delivery, typed host-call
visibility, exact authority settlement, rollback, bounded refusal, closed
startup lanes, and owner parity. `system_job_queue` now consumes this facade
through the dependent migration described below.

### 2026-07-13 Job-queue LocalSystem migration (closed)

`system_job_queue` now uses `LocalSystem` as its only live application owner.
Each scenario atomically registers and bootstraps the queue, retains a typed
split-service handle, uses `call_blocking_request`, and delegates unconditional
terminal observation to `run_to_shutdown_reported`. The four former
`Arc<ThreadedRuntime>` shells and local shutdown combiner are gone.

The old cancellation-ordering note was stale: `RequestCall::reply_and`
explicitly puts the current caller's reply before its follow-up effects. The
migration's refill probe did find a separate application bug. The queue marked
a worker idle before the worker released its deferred process reply, and a
stale wake could then take and drop a newer job. Cancellation now uses a typed
worker request and acknowledgement; the queue retains the worker's in-flight
charge until acknowledgement, and stale wakes preserve newer worker state.
Repeated tests prove exact cancel settlement followed by immediate refill,
while a caller-timeout probe proves both the worker charge and parked token
drain after the caller disappears. Cancel reconciliation is exhaustive:
bounded Tina-time retries handle `Full` and `Timeout`, `Closed` retires and
respawns the worker, and
`Rejected`, malformed acknowledgements, timer failure, or retry exhaustion
stop the queue instead of silently leaking admission capacity.

`RunConfig::validate` rejects zero or excessive worker, mailbox, sleep, and
timeout values before startup. Overflow burst size, barrier participants, and
worker dispatch deadlines use checked arithmetic before any corresponding
thread, barrier, vector, or batch allocation. The host report preserves
`Full`, `Closed`, `Timeout`, and `Rejected` terminals instead of folding them
into `Busy`.

### 2026-07-12 Guaranteed LocalSystem terminal runner prerequisite

Application-shaped `LocalSystem` hosts repeatedly capture a workload result,
request and observe bounded shutdown unconditionally, require a clean terminal
report, then manually merge four cases. Returning early with `?` before that
helper falls back to drop teardown and loses the workload's explicit terminal
contract. String-formatted dual failures also erase the independent shutdown
error and its source chain.

The framework now provides the same consuming form on single- and multi-shard
owners:

```rust
app.run_to_shutdown(Duration::from_secs(5), |app| {
    let service = app.register_request_service(Service::new(), 64)?;
    app.call_blocking_request(service, Request::Report, timeout)
})
```

`RunToShutdownError<E>` represents workload-only, shutdown-only, and dual
failure. `TerminalShutdownError` keeps bounded admission/observation failure
distinct from an observed `UncleanShutdownError`. No failure is flattened and
the dual variant owns both values. The closure's panic continues unwinding;
the owner's existing drop contract remains its panic teardown path. The
explicit timeout is one budget for shutdown admission and terminal
observation, not for workload execution. The consumed owner does not start a
second blocking shutdown attempt after that deadline; an escaped handle can
retry partial admission or observe terminal truth that arrives later. Such a
handle retains shutdown control and must retry or be dropped; without one,
owner consumption disconnects the remaining control senders and does not claim
terminal truth.

Framework tests cover clean authority settlement, workload-only,
shutdown-only, dual failure, bounded terminal timeout, real registration and
host-call early returns, panic propagation, admission timeout without a second
blocking owner drop, escaped-handle retry, partial multi-shard progress, and
single-/multi-shard parity.

The motivating `specimen_bounded_batcher`, `system_lock_manager`,
`system_soak_http_db`, and `system_copied_service_path` hosts now use this
runner directly. Their concurrent workloads borrow the runner-owned facade
through scoped threads, so application code no longer needs an exclusive
`Arc<LocalSystem>` plus a separately captured shutdown handle. This sweep also
found that `tina-proof-harness::run_with_observation` joined every worker before
returning but still required its operation closure to be `'static`. The harness
now uses scoped workers and accepts borrowed operations; a focused regression
test proves caller-owned host state can be borrowed. The copied-service
specimen applies that contract to its real `LocalSystem` host.

The cohort's adversarial review removed the lock-manager FIFO probe's final
host-scheduling delay in favor of observed server-side waiter admission, and
made the overflow-holder release assert its exact terminal rather than
discarding it. Scoped synchronization state is borrowed directly; only the
lower-level shutdown probes retain shared owners.

Two adversarial tests retain lower-level shutdown handles intentionally: they
initiate shutdown while a caller is still parked to prove closed-caller and
exact lease-settlement behavior. That is a distinct control-flow probe, not an
application lifecycle workaround.

### 2026-07-12 Address-aware LocalSystem root construction prerequisite

**Surfaced by:** `specimen_dynamic_worker_pool` during the LocalSystem host
migration.

The specimen had already removed `Begin { self_addr }` through the lower-level
`ThreadedRuntime::register_with_capacity_using`, but that forced an otherwise
application-shaped host to retain the raw threaded owner. The preferred facade
now exposes the honest form directly:

```rust
let coordinator = app.register_root_using(capacity, |self_addr| Coordinator {
    self_addr,
    // application state
})?;
```

`LocalMultiShardSystem::register_root_using_on` and the explicit-step,
threaded-multi-shard, single-simulator, and multi-shard-simulator mirrors use
the same constructor-address contract. The entry is not published until the
constructor returns. Panic consumes the monotonic id without registering an
isolate; bounded threaded pre-admission and unknown-shard rejection do not
execute the closure. An accepted threaded constructor may still publish after
`WorkerUnresponsive`, so the returned error does not counterfeit address
authority. A later adversarial pass found an even smaller shape for the
motivating specimen: `spawn_observed` followed by a typed child request removes
the parent's need to know its own address. The constructor API remains useful
for applications whose children genuinely need a parent capability, but the
example no longer retains that dependency merely because it works.

### 2026-07-12 Pure bounded-workload LocalSystem migration

Five bounded-workload specimens were reviewed as one host-authoring cohort.
All five now use the fallible `LocalSystem` application facade and
`run_to_shutdown_reported`. The dynamic worker pool's final migration uses
`spawn_observed` plus a typed request to each request-only child, so every spawn
and call outcome settles the parent even if a child panics before replying.
Cancellation now bounds both effect
producers, reports only after `CallGroup::report_ready()`, and consumes the
exact `CallGroupReport`. Backpressure preserves Full, Closed, Rejected,
per-hop Timeout, domain failure, and runtime continuation failure through
the complete chain. Hot-key and rate-limit reports account for every
`HostBurstSnapshot` terminal bucket. No example-local shutdown combiner, raw
threaded owner, fake settlement delay, raw request-sized batch, or outcome
collapse remains.

| Example | Current friction | Desired user-facing form | Current API sufficient | Framework prerequisite | Example PR | Tests | Status |
|---|---|---|---|---|---|---|---|
| `specimen_backpressure_chain` | none | exhaustive typed chain outcomes inside `run_to_shutdown_reported` | yes | reported runner merged | this cohort | Tokio/Tina smoke with real domain failure; direct mapping tests for every terminal bucket; Tina 20x | migrated |
| `specimen_cancellation_chain` | none | bounded cancel fan-out and consumed exact `CallGroupReport` inside `run_to_shutdown_reported` | yes | reported runner merged | this cohort | Tokio/Tina smoke; direct exhaustive branch/cancel mapping tests; exact cancellation invariants; Tina 20x | migrated |
| `specimen_dynamic_worker_pool` | none | observed spawn plus typed request outcome as the bounded join | yes | none | this cohort | happy-path fanout/sum smoke; injected child panic proves exact `HandlerPanicked` settlement without hang | migrated |
| `specimen_hot_key_fairness` | none | exhaustive bounded observed bursts inside `run_to_shutdown_reported` | yes | reported runner merged | this cohort | Tokio/Tina invariant smoke, terminal-bucket negative tests; Tina 20x | migrated |
| `specimen_rate_limited_worker` | none | exhaustive observed burst/control/result inside `run_to_shutdown_reported` | yes | reported runner merged | this cohort | Tokio/Tina boundedness/refill and terminal-bucket invariants; Tina 20x | migrated |

### 2026-07-12 Copied service path flow migration

The canonical `system_copied_service_path` now uses `LocalSystem` and a
request-aware raw flow for its held work. The flow carries the original
`RequestContext`, durable record id, `SharedLease`, and exhaustive `SleepReply`
without qids, `GuardedPendingReplies`, a redundant pending-capacity knob,
take/reinsert logic, or manual service envelopes. The shared scope remains the
honest bound on parked requests. Callers already closed when their queued turn
runs never cross the durable admission boundary; later caller loss, timer
failure, and owner stop all settle the move-only lease.

Load callers and the Stats host call distinguish every `CallOutcome`; typed
work failures retain their `CallError` class, including `TimerFull`, and outer
host errors remain separate. The application facade is fallible. Registration,
reporting, and proof failures cannot bypass terminal shutdown, and the final
scope snapshot proves admitted equals released with zero current authority.
This applies the request-aware flow to the first service new users are told to
copy.

Post-merge stress exposed a lower driver defect in the new owner-stop proof:
empty storage, TLS, Unix, and TCP lanes still invoked whole-loop backend
cancellation. A timer-only shutdown could therefore fail spuriously with
`DriverShutdownFailed` even though no I/O completion slot existed. Empty lanes
now skip backend cancellation while non-empty lanes retain the bounded
drain/quarantine contract. A tracked-backend regression proves zero cancel
calls for empty shared I/O; retained-completion quarantine tests and 200
consecutive copied-path owner-stop runs prove both sides of the boundary.

This driver hotfix is independent of address provenance; the provenance work
only stamps and validates routing identity and does not alter backend
cancellation or completion draining.

### 2026-07-12 Split-service outbound facade prerequisite

**Surfaced by:** `ergonomics_playground`'s debounced batch client.

A mixed event/request client could use the typed `send_event` and
`call_request` helpers, but its isolate declaration still had to expose the
private routing shape as
`Outbound<ServiceMessage<BatcherEvent, BatcherRequest>>`. Tina now exports
`ServiceOutbound<Event, Request>` as the canonical associated type for that
capability. It is a transparent alias, so it adds no conversion layer and does
not weaken the separate event/request address rails. Runtime and compile-fail
proofs use the public spelling, and the motivating batching migration can now
contain no direct service-envelope vocabulary.

### 2026-07-12 Debounced batch shared-work migration

The `ergonomics_playground` batch probe now models the actual operation: many
callers join one bounded batch. `SharedWork<BatchId, BatchReply>` replaces the
monotonic qid, `PendingReplies`, `(qid, value)` sidecar rows, and manual drain
correlation. One raw typed `flow!` step carries the batch id and exhaustive
`SleepReply`; `TimerFull` is a distinct `TimerFailed(CallError)` reply rather
than being collapsed into application `Full`.

Adversarial review caught a second bound hidden by that simplification:
`SharedWork` bounds live parked callers, while the batch values are accepted
operations. A timed-out caller can be reclaimed before the window closes, so
operation admission now checks the batch-value cap before parking authority.
The regression proves timeout settlement, in-window overload, next-window
refill, and exact terminal accounting; a live one-timer test proves real
`TimerFull` classification rather than only testing the report classifier.

The simulator client records and classifies every `CallOutcome` instead of
discarding non-reply terminals. Drain closes every waiter, clears the active
batch, and makes the physically armed late timer harmless. No batch-specific
framework helper was added: `SharedWork::reply_all_clone` and
`drain_all_with` already produce the smaller, honest application form. Its
split-service declaration now uses `ServiceOutbound`, so the motivating
example contains no direct service-envelope vocabulary.

### 2026-07-12 LocalSystem default-host application cohort

`system_api_gateway_limits`, `system_bounded_object_lane`,
`system_cache_with_fill`, `system_tenant_rate_limiter`, and
`system_webhook_relay` now construct through fallible `LocalSystem` builders
and use typed registration, host calls, and `run_to_shutdown_reported` instead
of shared threaded owners or application-local terminal combiners. Scoped host
threads borrow the facade. Configurations bound every caller/thread producer,
mailbox, parked-call table, duration, and result allocation before startup.

The gateway and object lane use `ConcurrencyPendingReplies`; owner stop,
caller departure, completion, rollback, and refill settle the guarded
authority exactly. The cache uses `SharedWork` for bounded single-flight
callers and generation-stamps invalidation. The request-only tenant limiter
stamps `RateLimit` admission with `RequestCall::now()` and preserves the narrow
four-way decision vocabulary on live and simulator owners. `Admitted` consumes
the token in place, so the example carries no permit-shaped cleanup. The
request-only webhook fake removes its dummy event lane and preserves every
outer call, bridge, rejection, and worker-domain outcome. A final audit also
stopped the gateway from treating a failed runtime timer as successful held
work.

The real AWS paths exposed one final framework prerequisite: an installed
S3/SQS address could belong to a different threaded owner than the new
`LocalSystem` created by the runner. AWS bridge installation now has
`LocalSystem` parity across S3, SQS, SNS, DynamoDB, and Secrets Manager.
`run_against_s3` and `run_against_sqs` accept bridge config, install into the
same facade as the application service, retain typed install and application
failures, close and drain while the facade remains live, preserve a combined
workload-plus-drain failure, and only then consume facade shutdown. Hermetic
real bridge tests prove the complete lifecycle without an AWS account.

### 2026-07-12 Request-aware raw flow prerequisite

`flow!` now accepts `-> raw request T` for typed timer and runtime-I/O
continuations that must keep the original `RequestContext`. The generated
variant owns caller authority, move-only captures, and the raw typed outcome;
it does not coerce `Result<T, CallError>` into the broader isolate-call
`CallOutcome<T>`. `then_service_event_with_request` supplies the private split
service envelope for typed calls and sleeps.

The soak-shaped compile proof threads an HTTP lease and then a DB lease through
two timer steps without a qid, `GuardedPendingReplies`, or take/reinsert cycle.
Live and simulator tests use the same service authoring and prove exhaustive
timer results, exact lease release, caller timeout, and owner-stop cancellation
while caller authority is captured. Migrating `system_soak_http_db` remains a
separate example cohort.

Adversarial review also closed two boundary defects. The contextual `request`
qualifier no longer steals an existing plain raw type path such as
`request::Outcome`. More importantly, a local request that times out after it
is queued but before its handler turn now supplies a closed `RequestContext`
if the handler captures it; the runtime preserves the established typed late
reply trace without minting fresh caller authority. Live/simulator proofs now
also preserve an exact raw `CallError::InvalidResource` from typed file I/O and
cover caller-gone and owner-stop capture settlement on both backends.

### 2026-07-12 Unix write-all split-service continuation prerequisite

`UnixWriteAll` now has `next_service_event` and `advance_service_event`, the
domain-event siblings of `next_effect` and `advance`. They delegate to the same
partial-progress state machine, hide only `ServiceMessage::Event`, and preserve
the complete `UnixWriteOwnedReply` plus original `Vec` allocation on success or
failure. Adjacent one-shot Unix operations already inherit
`TypedCall::then_service_event`; no broader loop abstraction was added because
the motivating custom-codec migration only needs write-all.

One event-only writer service runs unchanged on explicit `Runtime` and
`Simulator`. Simulator coverage forces two-byte partial writes through bounded
peer pressure, proves exact completion count and allocation identity, preserves
peer close as `CallError::Io`, and proves owner stop cancels a genuinely parked
write with no report or in-flight authority left. Unix peer-buffer Full is a
parking condition rather than a user-facing `Full` terminal outcome; the test
therefore proves bounded park-and-resume instead of inventing a false variant.
The refill proof also saturates a one-slot service-event mailbox when the
parked write resumes and proves the continuation is retained through overflow.

Adversarial review found that both Unix and adjacent TCP write-all helpers
accepted a plausible fabricated or stale owned reply without proving that a
write was armed or that the reply carried the original allocation. Both now
track one in-flight write, validate allocation identity, reject unarmed/stale
advance calls with `InvariantViolation`, and leave genuine in-flight work armed
when a stale reply is rejected.

This closes the framework prerequisite found by
`tina-extension-custom-codec`; the extension migration is recorded below.

### 2026-07-12 Typed sharded request-service table prerequisite

`ShardRequestServiceTable<Request, Reply, Event = Infallible>` preserves
canonical request-only service capabilities, and split-service request lanes
when explicitly selected, through `new`, `from_placement`,
`try_from_placement`, `address_for`, and key-owner lookup. It shares the
existing placement-order, typed missing-shard, and fallible-registration
contracts without exposing the internal `ServiceMessage` envelope. This is the
narrow prerequisite surfaced and now applied by
`specimen_sharded_fanout_read`.

Adversarial review factored the raw and typed tables onto one invariant
implementation, rejected mislabeled capabilities whose actual address shard
does not match the entry shard, and made both fallible and infallible placement
builders return all already-registered capabilities on failure. Direct tests
exercise registration and typed lookup on explicit, threaded, LocalSystem, and
simulated multi-shard owners; generation tests prove tables remain snapshots
until rebuilt after restart.

### 2026-07-12 Soak HTTP/DB request-aware flow migration

`system_soak_http_db` now applies the request-aware raw flow directly. The
service carries the original `RequestContext`, HTTP lease, and DB lease through
two exhaustive `SleepReply` stages without qids, `GuardedPendingReplies`,
take/reinsert cycles, or manual `ServiceMessage` construction. The former
`pending_capacity`, `PendingFull`, and `PendingDuplicate` surface was
implementation state rather than business pressure; the HTTP shared scope now
provides the honest bound on parked work.

The host uses `call_blocking_request` and exhaustively counts Replied, Full,
Closed, Timeout, and Rejected outcomes. Timer failure remains a distinct reply.
Every run verifies both shared scopes return to zero after terminal shutdown;
a focused caller-timeout smoke proves a parked HTTP lease is cancelled and
released. This closes the motivating example for the request-aware flow
prerequisite.

Adversarial review moved the live host to fallible `LocalSystem`, releases an
HTTP lease immediately when the caller was already gone before its handler
turn, and guarantees registration, worker, classification, and capacity-report
failures still pass through bounded terminal shutdown. Live proofs cover caller
timeout in both the HTTP and DB stages, timer-lane Full as an exact
`TimerFailed(TimerFull)`, gateway mailbox Full, completion-only slow events,
concurrent workers, and zero HTTP/DB authority after shutdown. Unit accounting
keeps every call terminal and every outer threaded-host error distinct.

### 2026-07-12 Extension corpus canonicalization ledger

Every crate under `examples/extensions` was read by hand and run with
`--all-targets`. Four extension proofs needed no isolate-authoring migration;
adversarial review still corrected pressure and validation defects rather than
declaring their existing shapes canonical by inspection:

| Example | Current friction | Desired form | API sufficient | Framework prerequisite | Example branch | Tests | Status |
|---|---|---|---|---|---|---|---|
| `tina-extension-capacity-surface` | None; owned report data joins `CapacitySummary` directly. | Current `CapacitySurfaceReport` constructors and typed assertions. | yes | none | `agent/extensions-canonical` | 1 unit test | canonical |
| `tina-extension-compile-fail` | None; public/private ownership boundaries are compile-fail doctests. | Current public constructors with unforgeable private state. | yes | none | `agent/extensions-canonical` | 4 doctests + count guard | canonical |
| `tina-extension-fake-bridge` | Closed: in-flight accounting happened after enqueue, so a fast worker could underflow the counter and the queue admitted one more job than the reported installed cap. | Reserve total queued-plus-active capacity before dispatch; roll back failed dispatch exactly. | yes | none | `agent/extensions-canonical` | 3 unit tests, including 100 fast-worker iterations | canonical; docs migrated to event handle vocabulary |
| `tina-extension-service-policy` | Closed: a zero limit still admitted the first request for a new key; a zero window had no stable retry contract. | Fallible configuration before policy use, then exhaustive decisions from caller-supplied time. | yes | none | `agent/extensions-canonical` | 2 unit tests | canonical |
| `tina-extension-custom-codec` | Closed: both actors were event-only but used generic message authoring and collapsed Unix errors. | Event-only isolates/registration/sends, envelope-free typed continuations, and exact staged Unix failures. | yes | `UnixWriteAll::next_service_event` and `advance_service_event` (PR #331). | `agent/extensions-canonical` | 8 unit/simulator tests | canonical |

The custom codec README and extension user guide now show the correct public
`SyncCodec::feed` signature (`-> usize`). Fake-bridge documentation now teaches
typed event-only registration and `try_send_event` rather than a generic
message address. No example-local envelope adapter or duplicate write loop was
added. After PR #331, the custom codec resumed and now uses event-only service
handles throughout. `CodecIoFailure` preserves endpoint, bind/accept/connect/
read/write/close stage, and exact `CallError`; codec Full/Malformed policy
outcomes remain separate from transport failure. Adversarial failure probes
exercise every staged rail outcome on both endpoints where applicable, and the
one-shot server now closes both its stream and listener instead of relying on
simulator teardown to hide the listener. The fake bridge now reserves its
installed in-flight cap before dispatch, and the custom policy rejects
self-contradictory zero configurations. The extension sweep is now complete.

### 2026-07-12 Runtime address provenance prerequisite

Address identity now includes an opaque `SystemIncarnation` ahead of shard,
isolate, and generation. Every live or simulated owner stamps one incarnation
across all of its shards, while independently constructed owners receive
distinct nonzero incarnations. Deterministic owners can configure the value
explicitly, including matching live/simulator fixtures without relying on
process-global construction order. Address capability wrappers, contexts,
typed continuations, erased sends, remote envelopes, observation keys, and
host call routing all preserve and validate the stamp before shard or isolate
routing.

Typed threaded and call surfaces report `ForeignSystem` without claiming call
or observation authority. Explicit-step and simulated ingress use a routing-
level `IngressSendError::ForeignSystem` that returns message ownership without
misreporting a mailbox closure. Focused tests cover coincident
foreign address tuples, exact message drop settlement, same-owner cross-shard
identity, preferred `LocalSystem` routing, deterministic replay, configured
live/simulator parity, stale post-restart addresses, and replacement delivery
within the original system incarnation. This prerequisite prevents an address
from one example-owned runtime from accidentally targeting a coincident tuple
in another as the corpus moves onto `LocalSystem`.

Post-rebase review found two restart examples reconstructing replacement
addresses with the unscoped marker; both now inherit the known owner
incarnation and prove replacement delivery. The first PR matrix then caught
example workspaces outside the root workspace that had exhaustive terminal
classifiers predating `ForeignSystem`. Copied-path, bounded-batcher, bridge,
worker-pool, and soak probes now classify or directly assert that terminal
instead of adding wildcard arms or collapsing it into an unrelated outcome.
The final lifecycle-parity review also made a stopped or stale parent distinct
from a stopped worker. Copied-path, bounded-batcher, and soak now preserve and
prove the `ParentStopped` host terminal explicitly.

### 2026-07-12 Lock-manager keyed FIFO canonicalization

Migrated `system_lock_manager` from the historical
`PendingReplies<u64, LockReply>` + monotonic waiter ids + per-lock
`VecDeque<u64>` sidecar onto `SharedWork<String, LockReply>`. The specimen now
uses `with_key_limit`, `wait`, and `take_next`; the helper owns FIFO order,
global and per-key admission, caller-gone reclamation, and exact occupancy.
`SharedWorkError::Full` and `KeyFull` remain distinct as
`Busy(GlobalFull)` and `Busy(KeyFull)` rather than collapsing terminal
pressure. The host is now a fallible `LocalSystem`, and lease continuations
carry and exhaustively handle `SleepReply`.

Direct live coverage proves FIFO hand-off, capacity-one caller-timeout
reclamation and refill, distinct global/per-key Full rails, keyspace Full,
release and expiry hand-off, renew plus stale timer suppression, stale
release/renew rejection, zero final waiter/key occupancy, and clean bounded
shutdown. Focused unit probes prove a current-generation timer failure retires
an unenforceable lease, a stale failure cannot revoke the current holder, and a
caller that closes after FIFO selection leaves at most a lease-bounded ghost
holder before expiry rollback. This fully applies closed finding 21 to its
remaining motivating specimen; no new framework gap surfaced.

### 2026-07-12 Bounded scatter/gather operation prerequisite

`ScatterGather<K, R, Q>` now owns the original `RequestContext`, ordered target
rows, and cancelable child authority through `CallJoinSet`. `start_service`
accepts a fully `BoundedItems`-validated target list and a typed call factory;
the factory receives the configured per-target timeout, so the documented
deadline cannot drift from the executed call. Missing targets, replies, Full,
Closed, Timeout, Rejected, and aggregate timeout remain distinct and preserve
caller order.

Every reply, aggregate timeout, and cancel acknowledgement carries the public
operation token, so a bounded collection can route concurrent aggregates
without private qids or colliding per-operation branch generations. Aggregate
expiry marks only still-pending rows, emits a bounded cancellation
batch, and withholds caller authority until every cancel acknowledgement is
recorded. Generation tokens reject duplicate and late overwrites. The aggregate
timer also carries an operation token, so a physically non-cancelable timer from
a completed request cannot expire a newer request in the same coordinator.
Start failures return the untouched `RequestContext`, and over-cap or duplicate
input is rejected before the call factory or effect batch exists. One
coordinator implementation is exercised unchanged on `Runtime`,
`ThreadedRuntime`, `Simulator`, `MultiShardRuntime`, and
`ThreadedMultiShardRuntime`; owner stop with child authority pending closes the
original caller.

`ScatterGatherOperations<K, R, Q>` closes the concurrent coordinator gap. It
owns a fixed-capacity operation collection, rejects `Full` before building a
call, and routes the unified `ScatterGatherEvent<K, R>` vocabulary. Application
coordinators now need one event variant, one bounded field, one inferred
`start_service` call, and one inferred `advance_service` call; they no longer
spell reply/cancel/timer variants, qids, token lookup, or find/remove logic.

`specimen_scatter_gather` now applies this prerequisite directly. Its
coordinator contains only the worker list plus `ScatterGatherOperations`; one
`Scatter` event replaces qids, `PendingReplies`, partial rows, manual batches,
and terminal folding. The completed typed report reaches the driver without
collapsing target outcomes. A capacity-one live probe proves one admitted
operation, typed `Full` for every excess caller, exact caller settlement,
same-runtime refill, target-order and reply-identity validation, and clean
shutdown.

`specimen_sharded_fanout_read` now applies both prerequisites. Shard counters
are request-only services stored in `ShardRequestServiceTable`; the coordinator
has one request, one scatter event, and one capacity-one operations owner.
`ReplyAdapter`, `Bind`, `Start`, `pending_targets`, manual sorting, raw outbound
sends, and service-envelope types are gone. The host uses
`call_blocking_request`, matches every terminal outcome, and rejects partial,
reordered, misrouted, or wrong-value reports before producing the public sum.
Together the two specimens
close the motivating scatter/gather example cohort.

### 2026-07-11 Bounded shutdown truth across the example corpus

Migrated production examples away from exclusive-`Arc` teardown and
transport-only shutdown success. Shared runtimes capture a
`ThreadedShutdownHandle` at construction, use
`request_and_wait_report(total_timeout)`, drop the remaining owner only after
terminal observation, and require `LocalSystemTerminalReport::ensure_clean()`.
Owned runtimes use `shutdown_report().ensure_clean()`. Explicit auxiliary
server, worker, stop-signal, and join failures are propagated instead of
discarded; WebSocket room shutdown returns a typed timeout with the last
snapshot when close settlement misses its bound.

The perf corpus now records leak cleanliness only after Tina terminal truth or
Tokio stop-and-join truth succeeds, preserving any earlier failed surface
observation. `scripts/examples_shutdown_truth_guard.sh`, wired into
`verify-guards`, rejects exclusive-owner and transport-only runtime shutdown,
discarded synchronous or Tokio task joins, ignored service stop sends, and
ignored bridge drain reports. The guard strips literals and comments before
matching so documentation cannot masquerade as lifecycle code.

**Still open:** broader `LocalSystem` host-facade migration remains a separate
ergonomics cohort; this slice establishes truthful shutdown behavior for the
current hosts.

**Closed in the public-corpus closure (2026-07):** the facade cohort landed —
every production-shaped host in the corpus runs on `LocalSystem` /
`LocalMultiShardSystem`, and the structural guard rejects new raw-runtime
hosts outside the reviewed allowlist.

### 2026-07-11 Fallible production startup propagation

Migrated every production-shaped example host from the panic convenience
constructors to `ThreadedRuntime::try_*` or `LocalSystem::try_build`, preserving
`StartupError` and its source chain through the host's existing error return.
Public server/demo helpers that previously returned an initialized host or
panicked now return `Result`; test-only fixtures unwrap explicitly at the test
boundary. `scripts/examples_startup_api_guard.sh`, wired into `verify-guards`,
prevents the infallible constructors from returning to production example
sources.

**Still open:** this closes panic-on-startup behavior, not the broader
`LocalSystem` host-facade migration. Raw `ThreadedRuntime::try_*` hosts remain
the next applied-ergonomics probe; bridge-heavy examples should expose whether
`LocalSystem::into_threaded_runtime` is sufficient or a facade API is missing.

**Closed in the public-corpus closure (2026-07):** every production-shaped
host is on the facade; `into_threaded_runtime` covers the bridge escapes that
remained (see the closure ledger's residual-host rows).

### 2026-07-12 Remaining raw Isolate → macro (rooms / fanout / grpc / mini_saas)

Converted the last hand-rolled `impl Isolate` / `isolate_types!` blocks:

- `specimen_sharded_fanout_read` ShardCounter (`send` + `AppShard`) + ScatterCoord (`tina::isolate`, `Io=Infallible`)
- `specimen_grpc_counter` StreamingEchoSource
- `specimen_websocket_room` Gateway + Room
- `system_realtime_rooms` Room + Gateway (dropped manual `CallableIsolate` stamps; macro now owns them)
- `mini_saas_api` NotifySink + Controller

**Still open (not raw Isolate):**
- Bind/Start paired-registration ceremony in scatter fanout (finding 3)
- LocalSystem / fallible startup migration for production-shaped hosts
- event-only / request-only form sweep where placeholders remain

**Closed in the public-corpus closure (2026-07):** all three — the facade
migration and fallible startup swept every production-shaped host, and typed
event-only / request-only / split-service HTTP delivery shipped (see the
closure ledger's framework prerequisites).


### 2026-07-11 Raw `impl Isolate` → macro cohort (local I/O + sqlite + cross-shard)

- `specimen_local_io_codec_ipc` — Ingest/Seeder/CopyPump, AdminServer/Client,
  KeyspaceServer/Client, live Unix Probe all on `#[tina_runtime::isolate]`
- `specimen_sqlite_counter` Caller/QueryCaller
- `specimen_cross_shard_child_ownership` Worker

Still raw: websocket/realtime rooms, mini_saas Controller/NotifySink,
sharded_fanout_read, grpc StreamingEchoSource (specialized Io/Send/protocol
shapes).


Finding numbers are stable across phases — when a finding closes it
moves to the [Closed](#closed) section below with the same number.

### 2026-07-11 Raw `impl Isolate` → macro cohort (partial)

Converted remaining mechanical raw/`isolate_types!` blocks onto
`#[tina::isolate]` / `#[tina_runtime::isolate]`:

- `tina-extension-custom-codec` CodecServer + CodecClient (`shard = CodecShard`)
- `specimen_http_body_streaming` StreamingService
- `specimen_webhook_publisher` Driver
- `ergonomics_playground` QuoteClient / BatchClient / CacheClient

**Still raw (next slice):** `specimen_local_io_codec_ipc/*`,
`specimen_sharded_fanout_read`, `specimen_sqlite_counter`,
`specimen_grpc_counter`, `specimen_cross_shard_child_ownership`,
`specimen_websocket_room`, `system_realtime_rooms`, `mini_saas_api`
Controller/NotifySink. Some of these own non-default `Io`/`Send`/shard
shapes and need careful macro attributes rather than a rename.

### 2026-07-11 Envelope-free continuation cohort

Closed the remaining application-level `ServiceMessage::Event` /
`ServiceMessage::Request` construction in the examples corpus by
migrating onto the landed helpers (`then_service_event`,
`reply_service_event`, `call_request`, `call_cancelable_request`,
`send_event`, `register_split_service`) and two small missing set/scope
helpers:

- `CallGroup` / `CallJoinSet` / `CallSelectSet::start_cancelable_service_event`
- `RequestScope::cancel_into_service_event_effect`

The later classified select-race phase replaces the playground's remaining
select adapters with `CallSelectEvent`, `start_service`, and `advance_service`.

**Migrated (no remaining envelope construction in effect/call sites):**

- specimens: `backpressure_chain`, `cancellation_chain`,
  `multi_turn_request_context`, `request_scope_fanout`, `scatter_gather`,
  `two_stage_pipeline` (comment only), `worker_pool`
- systems: `api_gateway_limits`, `bounded_object_lane`, `cache_with_fill`,
  `copied_service_path`, `job_queue`, `lock_manager`, `metrics_shipper`,
  `scoped_request_tree`, `soak_http_db`, `webhook_relay`,
  `ergonomics_playground` (also switched races/batch/cache probes onto
  typed `ServiceRequestAddress` + `register_split_service`)

**Still open after this cohort (not envelope construction):**

- Raw `impl Isolate` blocks (extensions custom-codec, local I/O specimens,
  websocket/room gateways, driver clients in ergonomics_playground) —
  next cohort: macro/`#[isolate]` form where lanes allow.
- Production-shaped hosts still on bare `ThreadedRuntime::new` rather
  than `LocalSystem` + fallible startup — next cohort.
- `specimen_sharded_fanout_read` Bind/Start paired-registration ceremony
  (finding 3) — still a framework gap.
- Type aliases like `SoakMsg = ServiceMessage<…>` remain only where an
  `HttpListener` (or similar rail) needs the envelope type parameter.

**Closed in the public-corpus closure (2026-07):** the first three bullets
all landed — no raw `impl Isolate` remains in the corpus, every
production-shaped host runs on the facade with fallible startup, and the
Bind/Start ceremony no longer exists (the specimen addresses shards through
the typed placement table). The last bullet is historical: `SoakMsg` is
gone; the only remaining private envelope alias that
requires an allowlist row is the benchmark-control `ChainMsg` row
(`QueueMsg`/`WorkerMsg` in `system_job_queue` are attribute-only and never
construct envelopes).

### 2026-07-09 Examples Canonicalization Pass

Swept the example crates to the current canonical Tina shapes. Every
touched crate still builds `--tests --offline` and its existing
tests/goldens pass unchanged (canonicalization must not change observed
behavior). What moved, and what was deliberately left:

**Canonicalized:**

- **`tina::flow!`** — `specimen_two_stage_pipeline` (closes finding 11;
  also deleted the qid/`PendingReplies` correlation table).
- **Split-service `#[isolate(event=.., request=.., reply=..)]`** —
  `system_job_queue` (Worker; closes finding 25),
  `system_metrics_shipper`, `system_webhook_relay`,
  `system_api_gateway_limits`, `system_bounded_object_lane`,
  `ergonomics_playground` (5 isolates), and specimens
  `specimen_cancellation_chain`, `specimen_scatter_gather`,
  `specimen_bounded_batcher`, `specimen_worker_pool`, and `ServiceC` in
  `specimen_backpressure_chain`.
- **`register_with_capacity_and_bootstrap[_on]`** — `system_job_queue`,
  `system_session_auth` (closes finding 24), `perf_native` (3 h2 client
  sites), `system_realtime_rooms`.

**Deliberately left (reason):**

- `system_soak_http_db` — `flow!` cannot type its `sleep().then()`
  continuations (finding 29, negative result recorded there).
- `system_scoped_request_tree` — split-service breaks its generic
  `HttpListener<S, TreeMsg>` ingress: the `From<HttpRequest>` impl the
  listener needs would land on the foreign `ServiceMessage<..>` alias
  (orphan rule, E0117). Migrating re-architects the inbound path.
- `system_tenant_rate_limiter`, `specimen_rate_limited_worker`,
  `specimen_idempotent_retry`, `system_live_replay_bugbox` — all-request
  or single-variant message sets; no event/request split to make, and
  the reject arm (where present) documents a real policy invariant.
- `ServiceB`/`ServiceA` in `specimen_backpressure_chain` — now split-service
  after finding 36 added `RequestCall::now()`.
- `QuoteGateway` race in `ergonomics_playground` — later migrated to
  `CallSelectSet`; `record_classified_reply` added its business-success
  classifier, and the classified select-race phase removed the remaining
  reply/cancel adapters.
- `specimen_sharded_fanout_read` (Bind/Start is open runtime gap,
  finding 3), `specimen_dynamic_worker_pool` / `specimen_supervised_worker`
  (now canonical: observed spawn plus typed request / `spawn_observed`),
  `specimen_pool_cancel_reclaim` / `specimen_cancellation_chain` (now on
  their canonical pending structure / consumed `CallGroupReport`),
  `specimen_graceful_pool_shutdown` /
  `specimen_graceful_drain_server` / `specimen_webhook_publisher`
  (README frames the manual shape as the lesson, or register→observe
  ordering is load-bearing).

**New rough edges:** finding 36 — `RequestCall::now()` is missing (now
fixed). Finding 38 — the HTTP/2 rail's `Http2ServiceMessage` lacks the twin
`FromHttpRequest for ServiceMessage` impl, so a split-service isolate cannot
yet serve over HTTP/2 (surfaced migrating `system_scoped_request_tree` over
HTTP/1; PR #277 fixed the HTTP/1 `HttpListener` path only). CLOSED (PR #279):
the twin `Http2ServiceMessage for ServiceMessage` impl landed with an
e2e split-service-over-h2 test.

**API-gap fixes landed (2026-07-09):** the four crates left above were all
unblocked and migrated to canonical form:
- `system_soak_http_db` → `flow!` now has `-> raw T` steps for non-call
  continuations (PR #276, closes finding 29).
- `system_scoped_request_tree` → `tina-http`'s new `FromHttpRequest` trait
  routes around the orphan rule (PR #277).
- `ServiceB`/`ServiceA` in `specimen_backpressure_chain` → `RequestCall::now()`
  added (PR #275, closes finding 36).
- `QuoteGateway` in `ergonomics_playground` → `record_classified_reply` on
  `CallSelectSet` carries a business-success predicate (PR #278).

**Not swept this pass (follow-up):** the ~30 remaining `specimen_*`
crates and `examples/extensions/*` were triaged (no split/bootstrap
anti-patterns found via grep) but not each individually migrated;
`examples/extensions/tina-extension-custom-codec` has two raw
`impl Isolate` blocks a future pass could look at.

### 2026-07-09 Examples Canonicalization Pass (by-hand follow-up)

Hand-read the remaining reject-arm / mixed-lane isolates and migrated
the ones that still carried finding-25 anti-patterns. Every touched crate
builds `--tests --offline` and existing smoke tests pass unchanged.

**Canonicalized (split-service + drop hand-written reject arms):**

- `specimen_multi_turn_request_context` — Probe / Db / Service from raw
  `impl Isolate` + reject arms → `#[tina_runtime::isolate(event=..,
  request=..)]`; Client → `#[isolate(message=..)]`; call sites use
  `call_request` / `SplitServiceHandle::from_address` (sim has no
  `register_split_service`).
- `specimen_two_stage_pipeline` — Pipeline finishes the earlier `flow!`
  migration with split form; stage continuations wrap as
  `ServiceMessage::Event(PipelineEvent::Stage(...))`.
- `specimen_request_scope_fanout` — Worker `handle`/`handle_call` mix on
  one `Wake` message → `WorkerRequest::Run` / `WorkerEvent::Wake`.
- `system_soak_http_db` — Soak `Request`/`Flow` → split; parks via
  `call.capture` + `insert_deferred_guarded` (same shape as
  `system_api_gateway_limits`); host uses `register_split_service`.
- `system_session_auth` — SessionBucket Bootstrap/Sweep events vs
  Login/Touch/Logout/Stats requests; bootstrap prefill becomes
  `ServiceMessage::Event(Bootstrap)`.
- `system_metrics_shipper` — Shipper Tick/FlushDone events vs
  Submit/Stats/Stop requests; `reply_and` for size-flush + arm-tick.
- `system_job_queue` — Queue finishes the earlier Worker-only split;
  Bootstrap/spawn/call-return events vs Submit/Cancel/Stats requests;
  `register_with_capacity_and_bootstrap` keeps working with the Event
  envelope.
- `perf_native` — ChainService Run request / PingReturned event.

**Still deliberately left (reason):**

- `mini_saas_api` NotifySink / Controller — large HTTP ingress surface;
  Controller carries multi-flow `NotifyFlow` + body/capacity ceremony;
  split needs a careful `FromHttpRequest` path, not a drive-by rename.
- `system_scoped_request_tree` — already unblocked by `FromHttpRequest`
  (PR #277) but the TreeMsg split is a separate re-architecture of the
  generic `HttpListener<S, TreeMsg>` parameter; not pure example polish.
- `system_tenant_rate_limiter` — reject arm is for unreachable policy
  decisions under Shed, not an event/request lane mix.
- Pure request/reply isolates (`HttpRequest` counters, keyspace stores)
  and pure fire-and-forget drivers — nothing to split; reject arm is
  absent by construction.
- `examples/extensions/tina-extension-custom-codec` — two raw
  `impl Isolate` blocks that are pure event loops; macro conversion is
  mechanical, not a lane-correctness fix. Left for a formatting pass.
- Driver `register` + `try_send(Begin)` sites — host-owned kick messages
  are not the register-and-bootstrap footgun (finding 24); the service
  does not always need Bootstrap before other work.

### 2026-05-23 Status Pass

The recent Wave A / post-122 / Phase 120 work closed a lot of old pain:
native HTTP/2/gRPC client parity, local I/O/codec/Unix IPC, admission and
rate policy, resource lifetime, durable outbox, ecosystem hooks, and
supervision/fairness reports are now landed and recorded in `CHANGELOG.md`.
Phase 120 also made the copied service path explicit:
`system_copied_service_path`, its companion proof, and a smoke-copy crate now
show the blessed service shape without asking readers to stitch ten specimens
together.

What is still active after reading the specimens and systems:

- **Admission across parked work.** Closed for local concurrency by
  `ConcurrencyPendingReplies`: one bounded owner holds the local
  `ConcurrencyLimit`, parked caller, and optional auxiliary RAII guard.
  `system_api_gateway_limits` uses it with `SharedCapacityReservation`, so
  owner-stop and caller-gone cleanup no longer depend on dropping an explicit
  local permit. Multi-stage guard replacement in `system_soak_http_db` remains
  intentionally explicit because it changes which external budget is held.
- **Race / cancel / retry ceremony.** `ergonomics_playground` and
  `system_job_queue` show the model is correct. `CallGroup::start_cancelable`
  removes branch-start token/handle ceremony; `CallJoinSet` / `CallSelectSet`
  cover common join-all and select-next cases; and the classified select event
  start/advance path removes manual loser-cancel adapter plumbing. Re-binding
  a cancelable caller after worker crash remains intentionally unsolved.
- **Cross-isolate setup.** Scatter/gather and paired registration still make
  users write bind/start adapter plumbing for the happy path.
- **Runtime observation while running.** Closed for the motivating IPC corpus:
  accumulated protocol facts are terminal actor results, so simulator/live
  `stop_with` + `observe_result` removes shared mutation without inventing a
  second mid-run state-inspection model. Trace projection remains appropriate
  for genuinely intermediate runtime facts.
- **Local I/O companions.** Closed for the motivating corpus. `UnixWriteAll`,
  `UnixReadToEof`, `FileCopyBounded`, and bounded `UnixFramedWriter::{lines,
  length_delimited}` cover the repeated loops while preserving each rail
  continuation.
- **Session/control-message lifecycle.** Phase 127 added the native WebSocket
  client session and tightened session protocol facts. Phase 120 added typed
  `WebSocketSessionMsg::AppControl` for app-injected `Start` / `Tick` /
  `Drain` messages so systems do not smuggle control through peer text.
  Remaining rough edges are pooled/reconnecting client managers and broader
  protocol hardening.
- **Live trace to sim.** Phase 128 made projection/capture/shrink the copied
  path, and Phase 120 added `RunCapture` plus `capture_run` / `save_bug` /
  `replay_bug` / `shrink_bug` workflow wrappers. Remaining rough edges are
  adding more supported live facts and using the workflow in more
  production-shaped systems. Phase 143 adds the overload-shaped names
  (`capture_overload_run`, `save_overload_bug`, `replay_overload_bug`) and
  bounded capacity assertions; protocol-specific overload facts remain the
  next expansion point.

Some older entries below are partly historical and say "shipped" inside the
section. Keep their numbers stable until the next cleanup pass moves those
paragraphs to `FINDINGS_HISTORY.md`.

### Admission and rate policy ergonomics

**Surfaced by:** `system_tenant_rate_limiter`, `system_api_gateway_limits`,
`specimen_rate_limited_worker`, `specimen_idempotent_retry`.

What felt good:

- Concurrency policies share `AdmissionDecision`; rate limiting now uses the
  smaller `RateLimitDecision::{Admitted, RateLimited, KeyCapacityFull, Closed}`.
  Generic `ServicePolicy` code can still widen rate outcomes explicitly.
- Passing `now` explicitly (`try_admit_at(&key, ctx.now())`) feels like
  boilerplate until replay; the `_at` suffix makes that authority boundary
  intentional. The sim test runs the exact same line under
  virtual time and gets byte-identical decisions across runs *and across
  seeds*. The boilerplate buys determinism nothing else can.
- `retry_after` is exact, not approximate — time-based tests assert
  `== 100ms` and `== ["k=ok", "k=rate(100ms)"]` with no jitter tolerance.
- Move-only permits + RAII release: parking a
  `SharedCapacityReservation` as a `GuardedPendingReplies` guard makes
  owner-stop release fall out for free (`current == 0` after shutdown
  is the proof).
- `FullHandling` composition keeps retry visibly caller-owned, and
  "idempotency key named on the message" is the right home for the safety
  claim.

What felt rough:

- **Closed: local concurrency across parked work.**
  `ConcurrencyPendingReplies` owns both the `ConcurrencyLimit` and guarded
  pending slots, rather than changing `ConcurrencyPermit`'s deliberately loud
  drop semantics. Reply releases as completed; caller-gone sweep, drain,
  rollback, and owner drop retire without completion. Because permits never
  leave the owner, wrong-gate release is unrepresentable and no `Arc`/atomic
  back-reference is required. Its report exposes policy current, parked
  current, completion/retirement, duplicate, reclaim, and both Full counters;
  `counts_agree()` makes ownership drift directly testable.
- **Charging two shared budgets per request used to be manual two-phase with
  rollback.** Closed by `SharedCapacityReservation::try_reserve([...])`, which
  admits every charge or drops earlier leases before returning the full scope.
  `ConcurrencyLimit::with_shared_scope` still takes only one shared scope.
- **Closed: the exhaustive 7-variant rate match was not honest.** The
  canonical inherent `RateLimit::try_admit_at` now returns the four outcomes
  its state machine can produce. Speculative table-pressure
  wait/degrade/close builders were removed before 0.1; explicit `close()`
  remains typed.
- `evict_key_for_capacity` is a footgun the type system can't guard —
  convention + the `evicted_count()` counter only. And the
  `KeyedLimit`-has-no-eviction / `RateLimit`-does asymmetry (live permits
  would dangle) takes a beat to internalize.

### Protocol facts to replay (Phase 112)

What felt good:

- Adding `Fact = ProtocolFact` to a protocol isolate is one line on the
  macro form. The `IntoRuntimeFact` bound at registration catches a
  typoed fact type as a compile error instead of a runtime mystery.
- `TraceProjection::protocol_facts()` and the named siblings let test
  code compare only protocol behaviour without touching the broader
  trace shape.
- The compile-fail fixtures pin the diagnostic shape: an ordinary
  isolate emitting a `ProtocolFact` shows "expected `Infallible`,
  found `ProtocolFact`" right at the call site, which is the shape a
  future reader will recognise.

What felt rough:

- Threading a mutable `effects: &mut Vec<Effect<Self>>` through five
  layers of response helpers (`enqueue_response`,
  `queue_or_send_response`, `send_pending_response`,
  `flush_pending_responses`, `handle_window_update`) is the price of
  emitting facts at the point each truth happens. The alternative
  shape — buffering facts on the isolate and draining at handler
  return — was tried and reverted: it added a hidden `pending_facts`
  field, separated emission from truth, and was a worse spelling. The
  thread-through version is verbose but makes the call sites honest.

### 2. ScatterCoord setup is heavy for the happy path

**Surfaced by:** `specimen_sharded_fanout_read`.

A bounded scatter/gather over three shards needs:

- coord isolate registration with `ScatterCoordMsg::{Bind, Start, Reply}`;
- a `ReplyAdapter<ShardReply, ScatterCoordMsg, S>` registration and
  `From<ShardReply> for ScatterCoordMsg` impl;
- a `Bind { bridge }` send before the `Start`;
- caller-owned `pending_targets` / `outcomes` bookkeeping until every
  target is in.

That is the right *shape* for the rich pressure form (per-target timer,
aggregate timer, partial outcomes), but the ceremony is the same for the
"three shards, all reply, sum the results" case. The per-call-site setup is
roughly the size of the actual scatter/gather logic.

**Build:** a small `scatter_gather!` builder or a
`ScatterCoord::register(table, config, on_complete)` helper that wires the
adapter, the bind/start handshake, and the `pending_targets` /
`outcomes` accumulator at the same shard the coord lives on. Must keep the
typed partial-outcome surface — convenience may not collapse `Full` /
`Closed` / `Timeout` into one bucket.

### 3. Self-address at registration time

**Surfaced by:** `specimen_sharded_fanout_read`,
`specimen_dynamic_worker_pool`.

The `ReplyAdapter` pattern needs the coord's own address to wire the
adapter, and the coord needs the adapter's address before it can fan out.
Today the answer is a `Bind { bridge }` (or `Begin { self_addr }`) message
before `Start`. That works but adds a variant whose only job is to land
"you, isolate, look here for your replies" into the isolate's state.

**Build:** a way for an isolate to learn its own typed address at register
time — for example, a constructor closure parameter `|self_addr| {
ScatterCoord { ..., self_addr } }`. Avoids the bind-before-start handshake
and removes the `Option<Address<...>>` field that's only `None` for one
turn.

Self-address construction now ships across all root owners:
`Runtime::register_with_capacity_using(cap, |self_addr| ...)` and
the threaded mirror; `MultiShardRuntime` and
`ThreadedMultiShardRuntime` expose `register_with_capacity_using_on`;
`LocalSystem` and `LocalMultiShardSystem` expose
`register_root_using[_on]`; and both simulator owners mirror the explicit-step
vocabulary. `specimen_dynamic_worker_pool` first removed its chicken-and-egg
`Begin { self_addr }` variant with the lower-level method, then its final
LocalSystem migration removed the parent-address dependency entirely through
`spawn_observed` plus a typed child request.

Still open: the cross-isolate handshake half — `Bind { bridge }` in
`specimen_sharded_fanout_read` is *not* about self-address, it's about
two isolates needing each other's addresses at registration. That
needs a paired-registration primitive or a different shape.

**Closed in the public-corpus closure (2026-07):** the named shape no longer
exists — the specimen addresses its shards through the typed
`ShardRequestServiceTable` placement table, so no `Bind { bridge }`
handshake remains to generalize.

### 7. Reqwest-bridge flatten edge: useful but per-call-site

**Surfaced by:** `specimen_webhook_publisher`.

The `tina-reqwest-bridge` ergonomics polish shipped
`flatten_outcome(outcome) -> Result<R, ReqwestCallError>` as an
opt-in flat-error helper. Building a specimen that uses all three
call shapes (`send_request`, raw `call(addr, ReqwestMsg::Send(...))`,
and `send_request` + `flatten_outcome` at the reply translator) made
it clear that flattening is *useful* — the consumer-side match drops
from five arms to three without losing the bridge-vs-worker layer
naming — but the call-site syntax for shape 3 is denser than for
shapes 1 and 2:

```rust
.then(DriverMsg::PostedViaSendRequest)                // shape 1: bare ctor
.then(DriverMsg::PostedViaRawCall)                     // shape 2: bare ctor
.then(|outcome| DriverMsg::PostedFlattened(flatten_outcome(outcome))) // shape 3: closure
```

A first-time reader has to look at shape 3 twice. Mixing layered
and flat call sites in the same isolate without a comment explaining
why some are layered is confusing.

**Build:**

- Keep `flatten_outcome` opt-in. Do not default it.
- Document explicitly: "pick layered or flat per call-site cluster,
  not per-isolate-mixed-mode."
- Consider a derive-style helper that produces a continuation enum
  variant + a bare-function translator from one declaration, so
  shape-3 call sites read the same as shapes 1/2. Not urgent —
  punt until a non-pedagogical user actually mixes the two and
  flinches.

### 8. External cancellation API — first form shipped

**Surfaced by:** `specimen_cancellation_chain`.

**Resolved (Tina cancellation phase):** Tina now ships
`call_cancelable(addr, msg, t).then(...)` returning a caller-owned
`CallHandle`, plus `cancel_call(handle).then(...)` that closes one
pending isolate call's wait. The handle is move-only and not `Clone`,
and is stamped with `(call_id, shard_id)` on dispatch so a cancel
issued from a different shard is rejected with a typed
`CancelOutcome::WrongShard` instead of silently no-op'ing.
Cancellation is visible truth: `CancelOutcome` (`Cancelled` /
`AlreadyCompleted` / `AlreadyCancelled` / `WrongShard`) is
`#[must_use]`. Late callee replies surface with a cause-specific
rejection reason from a bounded recently-cancelled ring:
`CallReplyRejected { CallerCancelled / CallerTimedOut / OwnerStopped
/ RuntimeStopped }` or the deferred-path equivalent; ring-evicted
fall-through is the generic `NoPendingCall` / `CallerClosed`.

**Resolved (Tina pending-call helper phase):** the bounded
[`PendingCallSet<K, R>`](../tina/src/pending_call_set.rs) helper now
ships in `tina`. Specimens that previously hand-rolled
`Vec<CallHandle<R>>` use it: `specimen_cancellation_chain` keys the
table by worker index, `specimen_pool_cancel_reclaim` keys by waiter
index. Insert returns `Full` / `DuplicateKey` as typed errors —
duplicate-key is rejected even when the prior handle has settled,
because an auto-sweep would create a silent ABA bug if a `Returned`
continuation for the prior call were already queued in the user's
mailbox. Forgetting `remove(&key)` therefore *does* leak slots until
the set is dropped, drained, or `sweep_terminal()`-pruned — that
leak is loud (eventual `Full`); silent ABA would not be. No `Drop`
magic, no background timer; the drain-and-cancel pattern stays in
user code — the helper does not own the workflow. End-to-end fill
-> cancel -> refill and fill -> timeout -> refill proofs in
`tina-runtime/tests/pending_call_set.rs`.

**Still open:** runtime-level `runtime.cancel_isolate(addr)` (third
form — closes every call an isolate owns) is a small wrapper around
`cancel_call` and `PendingCallSet::drain`; will land when a real
service consumer asks for it.

### 9. Drain helper for `PendingReplies` at service stop

**Surfaced by:** `specimen_graceful_pool_shutdown`,
`specimen_graceful_drain_server`.

`PendingReplies::drain()` returns `Vec<(K, DeferredReply<R>)>`,
which the user has to map into `Effect::Batch(reply_to(slot,
value))` calls plus a final `stop()`. The service-stop pattern
is identical at every call site:

```rust
let mut effects: Vec<_> =
    self.pending.drain().into_iter().map(|(_, slot)| reply_to(slot, R::Closed)).collect();
effects.push(stop());
Effect::Batch(effects)
```

The same area also wants a *deadline* — a drain that says "finish
in-flight work, but give up after T". Today that's a hand-rolled
`DrainDeadlineFired` continuation message scheduled via `sleep`
plus a check in the isolate's "is it done" predicate that returns
true on deadline-fired even when `pending > 0`. The
`tina-tokio-bridge::BridgeShutdownReport::drained_within_timeout`
flag is the bridge-side version of the same idea.

**Build:**

- ~~`pending.drain_into_effect(R::Closed) -> Effect<I>` (or
  similarly named) that returns the matching `Effect::Batch` in
  one call, with the trailing `stop()` opt-in via a sibling
  `drain_into_stop_effect(R::Closed)`.~~ Shipped:
  `PendingReplies::drain_replies` / `drain_replies_with` /
  `drain_replies_into_effect` / `drain_replies_into_stop` /
  `drain_replies_with_into_effect` /
  `drain_replies_with_into_stop`, all typed so a
  `PendingReplies<K, R>` only produces `Effect<I>` when
  `I::Reply = R`. `specimen_graceful_pool_shutdown` used
  `pending.drain_replies_into_stop::<Self>(R::Closed)` before
  its 067 migration; it now relies on
  `WorkerPoolMsg::Close(CloseMode::Drain)` for the same
  parked-callers-get-`Closed` outcome. The helper is still
  load-bearing for `PendingReplies`-shaped frontends. The
  deadline half of this finding (DrainGate) folds into finding
  15 (Deadline as first-class context).
- An isolate-state `DrainGate` helper that holds the deadline +
  the pending-count predicate, with an `is_done` /
  `drained_within_timeout` accessor that the handler reuses.

### 11. Multi-stage pipeline ergonomics

**Surfaced by:** `specimen_two_stage_pipeline`.

A 3-stage pipeline reads as 4 enum variants in `PipelineMsg`
(Submit + Parsed + Validated + Executed), each with its own match
arm. The Tokio side reads as `parse(i).await?; validate(p).await?;
execute(v).await?` — three lines. The Tina version is correct and
trace-visible at every stage, but the variant count grows
linearly with stage count.

**Decision:** do not build a pipeline helper yet. The long form is
not merely noise: it names each suspension point and each
per-stage `Full` / `Closed` / `Timeout` edge. A helper that makes
Tina look like fake `async` would be worse for humans and LLMs.

**Revisit only if:** a non-pedagogical pipeline repeats enough
boilerplate that a helper can delete plumbing while keeping every
stage, timeout, and partial-progress fact visible. The raw
match-state-machine form remains semantic truth.

**Update (verified on this audit):** `tina::flow!` (`tina-macros/src/lib.rs`)
now generates exactly this shape — a named continuation enum + dispatcher
per linear step, with no runtime behavior added (each step is still an
ordinary ` .then_with_request` continuation) — and ships in `mini_saas_api`
and `specimen_multi_turn_request_context`. It plausibly satisfies the
revisit condition above, but `specimen_two_stage_pipeline` — the specimen
that surfaced this finding — still hand-writes `PipelineMsg` by hand
(`examples/specimen_two_stage_pipeline/src/tina_impl.rs`). Not closing
until that specimen (or an equivalent) is migrated and proves the fit.

**Closed (2026-07 examples canonicalization pass):**
`specimen_two_stage_pipeline` now declares `PipelineFlow` with
`tina::flow!` for its `Parsed` / `Validated` / `Executed` steps. The
migration proves more than the boilerplate deletion this finding asked
for: threading `req: RequestContext<PipelineReply>` directly through each
step also removed the `qid`-keyed `PendingReplies<u64, PipelineReply>`
table the hand-written version needed purely to correlate a continuation
back to its caller — `flow!`'s req-threading makes that correlation table
unnecessary, not just its dispatch boilerplate. Both existing smoke tests
(`tina_smoke`, `tokio_smoke`) pass unchanged, including the exact
completed/parse-failed/validate-failed counts `assert_report_invariants`
checks. `flow!` is still linear-only by design (see finding 29's negative
result below for the sleep-driven shape it does not cover); an N-stage
fan-out pipeline remains hand-written by design.

### 12. Rust footgun replication: shared receiver in worker pool

**Surfaced by:** `specimen_graceful_pool_shutdown` (Tokio side).

Not a Tina finding per se — but worth recording as the *kind of
footgun* Tina structurally avoids. The Tokio shutdown path needs
both `JoinSet::abort_all` AND `drop(rx_arc)`. Forgetting the
second leaves buffered jobs (and their reply oneshots) alive,
blocking queued callers forever. The test passes under low burst
because all jobs were in flight.

Tina's `pending.drain()` + `Effect::Batch(reply_to)` makes this
class of bug structurally impossible: every captured slot has one
container, and shutdown is one effect away.

This is a positive observation about Tina's model. The build is
documentation, not new product work — call it out in the user
guide's lifecycle chapter as a contrast with the Tokio shape.

### 14. Spawn API surfaces the child's address

**Surfaced by:** `specimen_dynamic_worker_pool`,
`specimen_supervised_worker`.

`spawn(ChildDefinition::new(...))` still returns nothing and stays
the fire-and-forget primitive. Phase 084 adds the explicit observed
form:

```rust
spawn_observed(ChildDefinition::new(worker, cap))
    .then(ParentMsg::ChildStarted)
```

The continuation receives
`Result<ChildRef<ChildMsg, ChildReply>, SpawnObservedError>` as an
ordinary later parent message. The parent can store the typed child
address, send follow-up messages, and treat restart-created
incarnations as new/stale generation truth.

The error half covers spawn construction rejection, for example a zero
mailbox capacity. If delivering the continuation to the parent is itself
rejected because the parent's bounded mailbox is full or closed, the runtime
records the normal send rejection in the trace; there is no hidden queue to
force a second message through the failed path.

Before this, the parent did not learn the child's `Address`. That
meant the parent could not:

- ask the runtime "is this specific child still alive?" via
  `observe_isolate_complete(child_addr)`;
- send the child a follow-up message;
- aggregate "missing partials" as a typed timeout (the parent
  doesn't know which child is missing).

The old supervised-worker workaround had the child send a
`Boot(self_addr)` message back to a shared `Arc<Mutex<...>>` slot. The first
observed-spawn pass removed that boot message for the initial address but still
made the host rebuild every replacement address from untyped isolate and
generation fields.

**Closed (2026-07 typed restart continuation):**
`spawn_observed(restartable).then_with_restarts(initial, restarted)` now maps
the initial `Result<ChildRef<...>, SpawnObservedError>` and every successful
replacement `ChildRef` into ordinary parent messages. Both live runtime and
simulator preserve system/shard/generation provenance, stale-address truth,
and bounded parent delivery. A full parent mailbox records the normal
`SendRejected::Full` and does not retain a hidden retry queue.

**Closed (2026-07 split-service restart continuation):**
`then_service_event_with_restarts(initial, restarted)` preserves that same
initial-result, replacement-ref, stale-address, and bounded-delivery contract
for split services while keeping the framework-owned `ServiceMessage::Event`
envelope out of application code. `specimen_supervised_worker` is the motivating
migration and no longer constructs a service envelope in either lifecycle
continuation.

The adversarial pass also closed an initial/replacement asymmetry: a panic in a
restartable child's first isolate or bootstrap factory now produces the typed
initial `SpawnObservedError::FactoryPanicked`, publishes no child, and leaves
the live runtime or simulator able to continue. Replacement factory panics
remain `RestartSkippedReason::FactoryPanicked` lifecycle facts and do not invoke
the replacement continuation.

`specimen_supervised_worker` now stores the current child ref only in its
parent. Startup carries a typed host request through the initial continuation;
subsequent work routes through the parent as a typed worker request. Work is
counted only after the worker replies, poison only after
`Rejected(HandlerPanicked)`, and every other call terminal remains distinct.
The parent also names its in-progress startup state so concurrent starts cannot
create duplicate children. The `WorkerSlot`, `Arc<Mutex>`, polling loop, and
manual `Address::new_with_generation_in` reconstruction are gone. The host
restart waiter remains only as a synchronization/reporting fact, not as address
authority.

**Still open:** join/stop child convenience. Typed restart refresh is closed.
Existing `observe_child_restarted` remains the appropriate host waiter when
host code needs to know that a restart completed but does not own the child's
message type.

A *host-side* alternative —
`runtime.observe_child_started::<M>(parent).wait(timeout)?` —
was considered and rejected for now: the existing
`RuntimeEventKind::Spawned { child_isolate }` event has no
`TypeId` for the child's `Message`, so a typed waiter would
either need a new field on `Spawned` (a runtime-event change)
or a caller-asserted `M` (not honest under the LLM rule). Pick
the typed-event vs. continuation form when the supervisor/spawn
API gets revisited.

### 19. Pool consumer ergonomics — host-side acquire and scenario runner

**Surfaced by:** `specimen_pool_cancel_reclaim` (and to a lesser extent
`specimen_graceful_pool_shutdown`).

The cancel-reclaim specimen is ~245 lines tina vs ~113 lines tokio.
Roughly 115 of the gap is a `Driver` isolate that exists *only*
because `cancel_call(handle)` requires being inside an isolate's
`handle()`. Roughly 34 lines are a host-side
`try_send` + `std::thread::sleep` dance to step the driver through
seven scripted stages. Both costs go away in real services (the
service's own handler is the isolate, and there are no scripted
stages), but they hurt readability of test-shaped specimens.

Two helpers would cut the gap further when a real consumer pulls them
into existence:

- A host-side `runtime.acquire_owned(pool_addr, timeout)` analogous
  to `observe_result`. Lets test code acquire from outside an isolate
  context, eliminating the coordination Driver. Real risk: creates a
  second pool-interaction model from the host side. Defer until a
  real consumer (HTTP keepalive, DB pool) pulls on it.
- A host-side scenario runner —
  `runtime.scenario(addr).send(M).then_wait(D).send(N).run()` — that
  collapses the `try_send` + `sleep` dance. Real risk: becomes fake
  async choreography that hides ordering bugs. Test sugar only;
  defer until a second test specimen wants the same shape.

**Decision:** both are watch-list, not next-up. The
result-flavored `acquire_result_effect` / `release_result_effect`
helpers (shipped with 067) gave the same payoff for the no-loss case
and remain the right place to push first.

**Revisit when:** an HTTP keepalive consumer or a third pool
specimen wants either shape.

### 20. HTTP body-streaming ergonomics — first round shipped

**Surfaced by:** `specimen_http_body_streaming`.

Two ergonomic gaps showed up in the first specimen and got fixed
in this slice:

- **Hand-rolling an `Isolate` just to yield bytes.** A single-route
  streamed response needed two custom isolates: a chunk source
  with `tina::isolate_types!` + `ResponseChunkMsg`/`ResponseChunkReply`
  arms, plus the request handler. Wrapping any
  `Iterator<Item = Vec<u8>>` is now `IterBodySource::new(iter)`;
  no `Isolate` impl. The handler still names the framing
  (`stream_known_length` / `stream_chunked`), so the choice is
  visible without macro magic.
- **Framing was a struct literal, not a typed choice.** Callers
  built `ResponseStream { content_length: ..., source }` with no
  hint that an "unknown length" shape existed. Loud constructors
  (`HttpResponse::stream_known_length` and
  `HttpResponse::stream_chunked`) make the call site name the
  framing; a chunked response is `Transfer-Encoding: chunked` on
  the wire with the connection writing the terminator on `Eof`.
- **Cancel signal from connection back to source — closed.** Verified
  on this audit: `ResponseChunkMsg::Cancel` ships
  (`tina-http/src/streaming.rs`), the connection sends it on abandon
  (`tina-http/src/connection.rs`, `tina-http/src/http2/server.rs`,
  `tina-http/src/http2/client.rs`), and `cancel_response_source`
  (`tina-http/src/scope.rs`) is the scoped host-side helper.

What still needs work but is deferred:

- **Live metrics ticks.** `BodyMetrics::snapshot()` is callable
  from any thread at any time (the counter is `Arc`-backed), but
  there is no built-in periodic emit. A `runtime.metrics_tick(D)`-
  shaped helper or a generic capacity-tick channel belongs in the
  observability slice, not here.
- **Chunked decoding on the HTTP/1 client.** Server can emit
  chunked; the client still rejects real chunked bodies. Verified on
  this audit: `tina-http/src/parse.rs`'s response parser still treats
  `transfer_encoding_chunked` as `content_length = 0` (a body-forbidden
  shape), it does not decode a chunked body. Symmetric support is a
  separate slice with its own decoder + tests.

**Status:** shipped (server-side chunked emit, `IterBodySource`,
loud-API constructors, `body_io_error_count` proves mid-stream
client close, cancel signal to source).

### 28. Service-level scope registry mirroring `register_with_capacity`

**Surfaced by:** `system_api_gateway_limits`.

`SharedCapacityScope` is shard-local. A service today builds one with
`SharedCapacityScope::new("gateway.in_flight", "weight", 4)` and clones
the handle into every isolate that needs to admit. That works, but a
service builder may want `register_scope("name", unit, max)` next to
`register_with_capacity` so the discovery report and the lifecycle are
owned by the runtime, not by user code.

**Build:** a runtime-side `SharedScopeRegistry` keyed by name with
register/get/snapshot. Reuse the existing `CapacitySummary` shape so
the runtime can produce one merged discovery line per shard.

### 29. Effect chaining over multiple runtime calls inside one logical request

**CLOSED (2026-07-09, PR #276):** `flow!` gained `-> raw T` steps that carry a
non-call continuation (e.g. a `sleep().then()` timer wake-up yielding `Result`);
`system_soak_http_db` is migrated onto it. Kept here (not yet moved to
FINDINGS_HISTORY) per the ledger's stable-number convention.

**Surfaced by:** `system_soak_http_db`.

A request rail that admits HTTP, sleeps, releases, admits DB, sleeps,
replies needs two custom message variants (`HttpReleased`, `DbReleased`)
so the post-sleep state mutation can land in `handle`. The pattern is
the same across systems: "after this timer wakes, do the next stage".
Today every system rebuilds the variants by hand.

**Build:** an effect combinator like `sleep(d).then_in_isolate(|this:
&mut Self| this.start_db(...))` that wires the message envelope and
the post-wake state mutation in one place.

**Update (verified on this audit):** `tina::flow!` (`tina-macros/src/lib.rs`)
now generates a continuation enum + dispatcher for a named linear step
sequence and ships in `mini_saas_api` (`tina_impl/controller.rs`) and
`specimen_multi_turn_request_context`. It looks like the answer to this
finding's shape, but `system_soak_http_db` — the specimen that actually
surfaced this pain — still hand-rolls `HttpReleased` / `DbReleased`
(`examples/systems/system_soak_http_db/src/lib.rs`). Not closing until a
migrated `system_soak_http_db` (or an equivalent multi-hop-with-timers
case) proves `flow!` covers this exact shape.

**Checked (2026-07 examples canonicalization pass): `flow!` does not cover
this shape, and forcing it would be dishonest.** Every `flow!` step is
generated as `(RequestContext<Reply>, ..captures.., CallOutcome<T>)` —
the outcome slot is hard-coded to `tina_runtime::CallOutcome<T>`, the type
an isolate-to-isolate `call(...)` returns. `system_soak_http_db`'s
`HttpReleased` / `DbReleased` continuations are not isolate calls; they are
runtime-owned `sleep(d).then_with_request(req, ...)` wake-ups, which yield
`Result<(), CallError>` — a different, narrower outcome type with no `Full`
/ `Closed`/`Rejected` variants a real dependency call would have. The two
shapes are not interchangeable: writing a `flow!` step for a sleep wake-up
would need a hand-written `Result<(), CallError> -> CallOutcome<()>` shim at
every step, which reintroduces the exact boilerplate `flow!` exists to
remove. Replacing the sleeps with fake isolate calls to make the macro fit
was considered and rejected — it would invent architecture (two dummy
worker isolates) that does not exist in the real "admit, wait, release"
shape this specimen demonstrates, just to satisfy a macro's type signature.
`system_soak_http_db` is left on its hand-rolled `SoakMsg::{HttpReleased,
DbReleased}` form. **Revised build:** `flow!` (or a sibling macro) would
need a second outcome shape — a timer-wake step whose outcome slot is
`Result<(), CallError>` instead of `CallOutcome<T>` — before this finding
can close through the macro path. No such macro exists today.

### 30. DST adapter for `SharedCapacityScope` / `BoundedEventSink`

**Surfaced by:** `system_api_gateway_limits`, `system_soak_http_db`,
phase 107 findings.

The new observability primitives live outside the `RuntimeEvent`
trace, so DST replay does not currently carry their facts forward.
Existing trace-based pressure (`PressureSummary::from_events`) still
works; the new shared-scope full counts and event-sink drops do not.
The `ServicePressureReport` shape already encodes
`Unavailable { reason }` so a sim that does not have these primitives
yet stays honest.

**Build:** a small adapter in `tina-sim` that snapshots scope/sink
counters into the trace at well-defined points (admit, release,
drop, push, drain) so a replay can reconstruct `assert_no_full`
semantics. Or expose the snapshots as `LiveReplayFact` entries so
they ride alongside the existing fact stream.

### 32. AWS bridge surface duplication across services

**Surfaced by:** adding DynamoDB / SNS / Secrets Manager workers to
`tina-aws-bridge`.

Each AWS service worker repeats the same scaffolding: `OwnedRuntime`
wrapper, `*MetricsInner` struct with the same eight counters plus
service-specific ones, `note_admit_kind` / `note_terminal_kind` /
`in_flight_kinds`, `*Closer::close_and_drain` polling loop, and the
admit/poll/timeout state machine. Five services share roughly 80% of
their lifecycle code. The phase plan explicitly forbade a shared bridge
base crate to keep the per-service stories independent, so all five live
side-by-side with copy-pasted plumbing.

**Build:** when the bridge surface stops growing in shape, factor out
the common state machine into an internal `bridge_core` module within
`tina-aws-bridge` (still not a separate crate). The factoring needs to
preserve each service's per-error tally semantics — counters like
`DynamoMetrics::conditional_check_failed` or
`SecretsMetrics::decryption_failed` are service-specific. A trait that
the per-service module implements (validate, run_request, classify_sdk
error, tally_terminal) is probably the right shape.

Reference: the canonical bridge shape now lives in
[`docs/tina-user-guide/30-bridge-author-kit.md`](../docs/tina-user-guide/30-bridge-author-kit.md).
Any internal AWS refactor must keep those eight steps user-visible —
no hidden queues, no hidden classifier collapse, no late-result
silent rollup.

### 35. Local I/O / codec / IPC rails feel low-level next to the file loops (Phase 117)

**Surfaced by:** `specimen_local_io_codec_ipc` (file-ingest, file-copy,
admin-socket, framed-keyspace, live-unix), plus the live Unix echo and
`unix_simulation` tests.

What felt good:

- The `.then(Msg::Variant)` continuation model reads top-to-bottom as a
  pure state machine; every resume point is a named enum variant. The
  whole admin server fits in your head.
- The simulator carries the IPC story end-to-end: the admin and keyspace
  protocols were written and run deterministically with no real socket,
  then the *same* `unix_*` calls run live. That live/sim parity is the
  payoff.
- Typed outcomes (`Ok(bytes)` / empty-is-EOF / `Err(CallError)`) are
  uniform across TCP/Unix/file rails — one `CallError`, no per-rail
  relearning.
- The codec *decode* side is the right shape: `feed(bytes)` then loop
  `next_frame()`, with `FrameDecision::{Full, Malformed}` forcing the bad
  cases. `FileReadChunks`' `Eof` vs `CapReached` report is honest and
  genuinely useful.

What felt rough — each is a `Build`:

- **Unix loop helpers were missing.** Closed by `UnixWriteAll` and
  `UnixReadToEof`, mirroring the TCP helpers and surfacing `Ok(0)` stuck writes
  as `CallError::Io` instead of hot-spinning.

- **The codec owns decode and bounded encode+write.** Closed by
  `UnixFramedWriter::{lines, length_delimited}`. Both body and encoded-batch
  caps refuse before mutation, and the helper delegates partial progress to
  `UnixWriteAll`. The specimens use raw output only to inject deliberately
  malformed bytes.

- **`FileCopyBounded`'s two-method API was clunky.** Closed by the unified
  `next_effect(...)` / `advance(FileCopyProgress, ...)` path. The old
  `next_leg` / `record_*` gears remain when a caller wants the mechanism.

- **Terminal protocol facts no longer need mid-run observation.** Closed by
  simulator result-observation parity. The actors accumulate facts privately,
  close owned resources, and `stop_with` one typed report. Registering the
  waiter before start proves authority and avoids the historical
  `Arc::try_unwrap`/default-data failure entirely.

## Closed

Findings shipped by recent phases. Numbers are kept stable so
existing README references stay valid.

### 2026-07-12 Worker-pool caller authority canonicalization

`specimen_worker_pool` no longer invents a `qid` or parks each caller in a
`PendingReplies` sidecar for a workflow with exactly one child call per
request. The frontend now uses
`RequestCall::defer(call_request(...)).reply_service_event(...)`, which moves
the typed `RequestContext` directly into the worker completion event. This
deletes the synthetic pending-full and duplicate-key outcomes while preserving
distinct worker `Full`, `Closed`, `Timeout`, and `Rejected` outcomes. The live
host also preserves the timer continuation's typed `CallError` instead of
collapsing every timer failure into one flag, and uses `LocalSystem` for
startup, registration, result observation, typed ingress, and terminal
shutdown. No framework prerequisite was needed.

### 2026-07-12 Bounded batcher synthetic reply correlation

`specimen_bounded_batcher` now uses `SharedWork<u64, BatcherReply>` keyed by
the honest batch generation. `SharedWork::wait` owns each `RequestCall`, and
`reply_all_clone` settles every live waiter for a flushed generation. This
removes the monotonic `qid`, `PendingReplies<qid, _>`, and parallel `qids`
vector without replacing them with example-local glue. The item vector is the
only batch payload state; caller timeout reclaims reply capacity without
silently retracting accepted work.

The specimen also moved from a driver isolate over raw `ThreadedRuntime` to
typed host requests through fallibly built `LocalSystem`, exhaustive
`CallOutcome` and outer host-control accounting, and bounded terminal-report shutdown. Direct tests
cover size/timer flushes, global `Full`, caller-gone reclamation and refill,
post-`Full` refill, stale success/error invalidation, typed timer-failure settlement and refill, exact
capacity counters, and clean shutdown. This also closes the old timer-error
`noop` path that abandoned accepted callers until their individual deadlines.

**Framework result:** the current `SharedWork` FIFO/all-waiter API is
sufficient. No new batch-specific abstraction or example-local adapter is
needed.

### 13. Tina-owned database client (`tina-sqlx-bridge`) — closed

**Surfaced by:** `specimen_sqlite_counter`.

There was no native or bridged path for "Tina service talks to a
database." The honest first-form shape used in the specimen was one
isolate that owns a `rusqlite::Connection` and runs each query inline
in `handle`, which blocks the shard thread for the query's duration —
fine for SQLite, dishonest for a remote DB with millisecond latency.

**Closed. Verified on this audit:** both requested shapes ship as full
crates. `tina-sqlx-bridge` (`tina-sqlx-bridge/src/{lib,worker,helpers,
metrics,types}.rs`) covers the async/remote-DB path with a
Tokio-owned worker, bounded ingress, and a `PgMetricsHandle`.
`tina-sqlite-bridge` (`tina-sqlite-bridge/src/{lib,worker,helpers,
metrics,types,budget}.rs`) covers the sync path with `SqliteError::*`
variants and `SqliteMetricsHandle`. `specimen_postgres_counter` and
`specimen_sqlite_counter` use them directly.

### 15. Deadline as first-class context — closed

**Surfaced by:** `specimen_backpressure_chain`.

A multi-hop chain had to thread a deadline (or remaining-budget
duration) through every call by hand, with the outer hop's call
timeout kept slightly longer than the inner's so slack didn't
accumulate silently.

**Closed. Verified on this audit:** [`Deadline`](../tina/src/context.rs)
ships with the explicit-`now` constructor `Deadline::from_instant(now,
after)` plus `Context::now()` / `Context::deadline_after(after)` as the
runtime/sim-aware sugar (`tina/src/context.rs`). The runtime stamps
`Context::now()` from its monotonic `Clock` before each handler turn;
the simulator stamps it from a stable virtual-clock anchor. There is no
`Deadline::after(Duration)` shortcut, since it would call
`Instant::now()` internally and silently break DST/replay.

`Deadline` is a budget value: it does not retry, extend, or cancel
work. `remaining(now)` returns `Option<Duration>`, `remaining_or_zero
(now)` returns the duration for use as a call timeout. Proved live in
`tina-runtime/tests/deadline.rs` and deterministically in
`tina-sim/tests/deadline.rs`. `specimen_backpressure_chain` propagates
a `Deadline` through A -> B so each hop sees the remaining budget
against its own `now`.

### 16. Multi-worker TLS lane (or split accept/stream lanes) — closed

**Surfaced by:** `specimen_native_https`, `tina-http/tests/client_tls_smoke.rs`.

**Closed.** TLS no longer runs on worker threads at all. The runtime owns a
rustls connection (sans-I/O) per `TlsStreamId` and drives the
handshake/read/write/close state machine on the shard thread as Betelgeuse
harvests TCP completions — TLS is a layer over the runtime's own TCP rail, not a
second socket stack. The single TLS worker that head-of-line-blocked accepts and
deadlocked a same-runtime client+server is gone. `local_system_tls_quiet_stream_does_not_block_second_connection`
still pins the quiet-stream story, and `local_system_tls_client_and_server_share_one_runtime`
runs a Tina TLS client and server on one shard in one runtime — the exact case
this finding called impossible. The substrate guard
(`tina-runtime/tests/tls_substrate_guard.rs`) pins the absence of any
`tina-tls-*` worker thread or private socket stack.

**Still true:** `tls_lane_capacity` is a hard cap — now the shard-total count of
in-flight TLS ops, not magic unbounded concurrency. Handshake asymmetric crypto
runs on the shard thread, an accepted tradeoff: visible and boundable by accept
rate rather than hidden on a serial worker that deadlocks.

**Verified on this audit:** `TlsStreamId` and the driver/call TLS state
machine live in `tina-runtime/src/driver/tls.rs` and
`tina-runtime/src/call/tls.rs`; `tina-runtime/tests/tls_substrate_guard.rs`
exists and is exactly the guard test named above.

### 17. Private Unix-domain socket worker thread — closed

**Note:** this shares finding number 17 with "Host-thread `call_blocking`"
below — a pre-existing duplicate in the ledger's numbering, not introduced
by this pass. Flagged for a human to renumber; left as-is here since
inventing a new number was out of scope for this audit.

**Surfaced by:** `specimen_local_io_codec_ipc` (`live_unix_smoke`, `admin_socket`),
`tina-runtime/tests/local_system.rs` (`unix_live_echo`).

**Closed.** Unix-domain sockets no longer run on a private worker thread over
`std::os::unix::net`. The runtime drives bind/accept/connect/read/write/close on
the shard thread as completions on the same per-shard Betelgeuse loop TCP and TLS
already ride — Unix sockets are sockets, so they follow the same substrate rule.
The narrow Unix addressing the substrate lacked (`bind_unix` / `connect_unix` and
the socket-file unlink lifecycle) was added to vendored Betelgeuse rather than
left in a hidden worker. The lane keeps TCP's discipline: one accept/read/write
lane each, `ResourceBusy` on duplicates, close-wins cancellation, tombstoned
shutdown. The capability report now classifies it completion-backed, and the
rail-inventory guard (`scripts/rail_inventory_guard.sh`) fails the build if a
worker thread or blocking std socket reappears in a runtime rail off-inventory.

**Still true:** DNS (platform resolver) and process spawn/wait stay bounded
blocking lanes on purpose — they are OS lifecycle / library calls with no
portable completion opcode, and the capability report carries their written
reason. A narrow rename/remove/readdir/metadata storage fallback is the only
remaining off-shard storage worker.

**Verified on this audit:** `scripts/rail_inventory_guard.sh` exists and
greps `tina-runtime/src/driver` for `thread::spawn` / `os::unix::net` /
blocking `std::fs` calls against a written inventory
(`.intent/runtime-rail-inventory.txt`); the only live
`std::os::unix::net` hit left in `tina-runtime/src/driver` is the
documented process-spawn exception in `driver/process.rs`.
`UnixWriteAll` / `UnixReadToEof` ship in `tina-runtime/src/unix_loops.rs`.

### 21. Per-bucket FIFO wait list next to a global `PendingReplies` — closed

`tina_runtime::SharedWork<K, R>` is now the user-facing copy path:
"many callers wait for one result", one global cap, optional per-key
cap (`with_key_limit`), FIFO per key, ticketed `reply_one`, and
`reply_all_clone` / `reply_all_with` / `close_all_clone` /
`close_all_with` / `drain_all_with` for multi-waiter replies. Stale
tickets are rejected; tickets are move-only with crate-private fields.
`request_effect_after_shared_wait(&ticket, effect)` is the only path
that produces a `RequestEffect` after admission.

`SharedWork` is a thin wrapper over `WaitList`; the lower-level
`WaitList` name remains public for call sites that read better under
the mechanism name. `system_cache_with_fill` and the
`ergonomics_playground` single-flight cache probe both copy from
`SharedWork` now.
*(Update: `WaitList` has since been made private; `SharedWork` is the
only public name.)*

**Verified on this audit:** `SharedWork<K, R>` is defined in
`tina-runtime/src/shared_work.rs`; no remaining ask from the
historical finding below is open.

*(Historical finding kept below for context.)*

### 21-historical. Per-bucket FIFO wait list next to a global `PendingReplies`

**Surfaced by:** `system_cache_with_fill`, `system_lock_manager`.

Both specimens want "one bounded global pending box, plus a FIFO
wait list per cache key / lock key, plus a hand-off loop that skips
slots whose caller went away." Each writes the same shape by hand:

- `pending: PendingReplies<u64, Reply>` keyed by a monotonic waiter id;
- per-bucket `VecDeque<u64>` of waiter ids inside the bucket's state;
- on hand-off / fill-done, pop a waiter id from the queue, `take` from
  pending, and if the slot is gone (caller cancelled / timed out) loop
  to the next id.

The cap accounting splits awkwardly: the global cap lives on
`PendingReplies`; the per-bucket cap lives in handler code; the
"skip reclaimed" loop is repeated.

**Build:** a small `WaitList<K, R>` (or `KeyedPendingReplies<K, R>`)
helper that owns both caps, takes the inbound `CallContext`, and
exposes a single `pop_next(&K) -> Option<DeferredReply<R>>` that walks
past reclaimed slots. Must keep typed admission errors
(`Full` / `BucketFull`) so callers can reply `Busy` distinctly.
Revisit only after a third specimen needs the same shape so the helper
shape is informed by three call sites, not two.

### 22. Internal-event variants need a `handle_call` rejection arm — closed

**Surfaced by:** `system_cache_with_fill`, `system_lock_manager`.

Specimens that mix caller-authority messages (`Acquire`, `Get`) with
runtime-owned continuations (`LeaseExpired`, `FillDone`) used to write
a `handle_call` arm whose only job was
`call.reject(CallRejectedReason::UnsupportedMessage)` for every
internal variant, repeated per isolate.

**Closed. Verified on this audit:** the `#[tina_runtime::isolate(event =
Event, request = Request, reply = Reply)]` split-service form
(`tina-macros/src/lib.rs`, `build_isolate`) generates `ServiceMessage
<Event, Request>` (`tina/src/address.rs`) and auto-generates the
rejection arm on both sides — an `Event` delivered to the generated
`handle_call` is rejected with `UnsupportedMessage` and a `Request`
delivered to `handle` is rejected the same way, with no user-written
match arm. Compile-fail fixtures pin the type-level half
(`tina-runtime/tests/safety_rails_compile_fail/split_event_on_request_lane.rs`);
the live test `split_service_routes_events_and_requests_on_separate_capabilities`
in `tina-runtime/tests/safety_rails.rs` passes (verified: `cargo test -p
tina-runtime --test safety_rails`, 10/10 ok). `system_cache_with_fill`
and `system_lock_manager` — the two specimens that surfaced this
finding — both use the split form today with no hand-written rejection
arm left in either file.

### 23. Mailbox-first service ergonomics — Phase 101 shipped — closed

**Note:** this shares finding number 23 with "Host-side `call_blocking`"
below — a pre-existing duplicate in the ledger's numbering, not
introduced by this pass. Flagged for a human to renumber.

**Surfaced by:** `system_metrics_shipper`, `system_bounded_object_lane`,
the recurring-tick / single-flight / drain / Full-handling repetition
across system specimens.

Shipped helpers:

- `tina::time::RecurringTick` — fixed-period service ticks with
  `Skip` / `Bounded(n)` / `Delay` catch-up policies; explicit
  `RecurringTickToken` for stale-tick detection. `system_metrics_shipper`
  now uses it for time-window flushes.
- `tina_runtime::LocalPermitGate` — fixed-capacity, move-only `Permit`,
  explicit release/retire; reports
  capacity/current/full_count/high_water/retired_count/completed_count/
  invalid_release_count. `system_bounded_object_lane` and the metrics
  shipper's single-flight flush slot both run on it.
- `tina_runtime::DrainState` — small admit/complete/cancel/drop
  counter state plus `begin/finish/can_stop`. Late completions counted
  separately. Resource close still belongs to the service.
- `runtime.register_with_capacity_and_bootstrap[_on]` — prefills the
  mailbox with the bootstrap message before inserting the isolate entry.
  No cleanup-after-registration path; typed `RegisterBootstrapError` on
  prefill refusal. Available on `Runtime`, `ThreadedRuntime`,
  `MultiShardRuntime`, `ThreadedMultiShardRuntime`.
- `tina_runtime::FullHandling` — decision-only state for the
  "on Full, shed or retry-with-backoff" shape; the service still
  schedules the visible Tina sleep.

Out of scope here: lifecycle `on_start` callbacks (not shipped,
register-and-bootstrap covers the common footgun without breaking
mailbox truth), broad retry frameworks (FullHandling is the only one).

**Verified on this audit:** `RecurringTick` in `tina/src/time.rs`;
`LocalPermitGate` in `tina-runtime/src/local_permit.rs`; `DrainState`
in `tina-runtime/src/drain_state.rs`; `FullHandling` in
`tina-runtime/src/full_handling.rs`. `system_bounded_object_lane` and
`system_metrics_shipper` both import and use `LocalPermitGate` /
`DrainState` directly. `register_with_capacity_and_bootstrap` exists in
`tina-runtime/src/{registration,threaded,multi_shard,
threaded_multi_shard}.rs`, though see finding 24 below for the caveat
that neither surfacing example has migrated onto it yet.

### 23. Host-side `call_blocking` on `ThreadedMultiShardRuntime` *(closed by phase 102)*

`ThreadedMultiShardRuntime::call_blocking(addr, msg, timeout)` now
ships and routes by `addr.shard()` — same convention as `try_send` and
`observe_result`. Bounded admission: a full worker command queue
surfaces as `ThreadedRuntimeError::CommandFull` instead of a host
hang. Single-shard `ThreadedRuntime::call_blocking` got the same
bounded-admission treatment. `system_session_auth` was migrated to
real multi-shard placement (one bucket isolate per shard, host routes
by `ShardPlacement`); the in-isolate fallback note is gone.

No `call_blocking_on(shard, addr, ...)` ships — passing the shard
twice is a place to introduce a mismatch bug. A future host-to-shard
variant only earns its place when a real caller needs "call as if
from shard A into target shard B" and has a remote-path proof.

**Verified on this audit:** `call_blocking` exists on both
`tina-runtime/src/threaded.rs` (single-shard) and
`tina-runtime/src/threaded_multi_shard.rs` (multi-shard), each routed
by `addr.shard()`; no `call_blocking_on` exists anywhere in
`tina-runtime/src`, matching the "no host-to-shard variant" claim.

The preferred `LocalSystem` and `LocalMultiShardSystem` facades now forward
only the two host-call shapes justified by their public registrations:
`call_blocking` for `register_root[_on]` and `call_blocking_request` for
request/split service handles. The multi-shard facade keeps address-owned
routing and the same unknown-shard panic convention. Separate host-wait
budgeting remains a lower-level threaded-runtime control rather than widening
the app facade.

### 24. Register-and-bootstrap helper for start-up effects — closed

**Surfaced by:** `system_job_queue`, `system_session_auth`.

Both specimens have a startup effect (job_queue spawns N worker children;
session_auth schedules the first sweep timer). The ceremony this finding
complained about: define a public `Msg::Bootstrap` variant, handle it in
`handle`, and after `register_with_capacity` remember a separate
`try_send(addr, Msg::Bootstrap)` — forgettable, and the failure mode is
silent.

**Closed at the library level. Verified on this audit:**
`register_with_capacity_and_bootstrap` (and `_on` / `_using` siblings)
ship in `tina-runtime/src/registration.rs`,
`tina-runtime/src/threaded.rs`, `tina-runtime/src/multi_shard.rs`, and
`tina-runtime/src/threaded_multi_shard.rs`. The preferred application owners
expose the same contract as `LocalSystem::register_root_with_bootstrap` and
`LocalMultiShardSystem::register_root_with_bootstrap_on`; `tina-sim` mirrors
the explicit single- and multi-shard vocabulary. All forms prefill the mailbox
before the address is published, with a typed `RegisterBootstrapError` or
`ThreadedRegisterBootstrapError` preserving honest authority on failure
(`tina-runtime/src/errors.rs`).

**Closed at the example level (2026-07 examples canonicalization pass):**
both surfacing specimens now use the bootstrap-prefill form.
`system_job_queue` registers its `Queue` isolate with
`register_with_capacity_and_bootstrap::<Queue, WorkerMsg>(Queue::new(...),
mailbox, QueueMsg::Bootstrap)` — this also let the `Queue::self_addr` field
(dead code; it fed the old `register_with_capacity_using` closure and was
never read) drop out entirely. `system_session_auth` registers each
per-shard `SessionBucket` with
`register_with_capacity_and_bootstrap_on::<SessionBucket,
Infallible>(shard_id, ..., SessionAuthMsg::Bootstrap)`. Both crates'
existing smoke tests pass unchanged (`system_job_queue`: 4/4;
`system_session_auth`: 1/1), proving the prefill-then-register ordering
does not change observable behavior.

### 25. Request/reply variants in `handle` compile but reject at runtime — closed

**Surfaced by:** `system_cache_with_fill`, `system_job_queue` (Worker
isolate).

A request/reply isolate used to route incoming messages through
`handle` for fire-and-forget variants and `handle_call` for
caller-authority variants, with both handlers sharing the same
`Message` type — a variant belonged on one side by convention only,
and putting it on the wrong side compiled cleanly but rejected at
runtime.

**Closed. Verified on this audit:** the isolate macro accepts
`event = Event, request = Request` in place of `message = Message`
(`tina-macros/src/lib.rs`, keys parsed at line ~93-94, `split_service`
branch in `build_isolate`) and expands to `ServiceMessage<Event,
Request>` (`tina/src/address.rs`). This makes the split
unrepresentable at the type level exactly as the finding's `Build`
section asked: an `Event` can never reach the generated `handle_call`
match, a `Request` can never reach `handle`, and the compile-fail
fixture `split_event_on_request_lane.rs` pins a real `E0308`
diagnostic (`expected Request, found Event`) at the call site, not a
runtime rejection. Live coverage: `cargo test -p tina-runtime --test
safety_rails` passes 10/10, including
`split_service_routes_events_and_requests_on_separate_capabilities`.
`system_cache_with_fill` and `system_lock_manager` both use the split
form today. **Migrated (2026-07 examples canonicalization pass):**
`system_job_queue`'s `Worker` isolate
(`examples/systems/system_job_queue/src/lib.rs`) now uses `event =
WorkerEvent, request = WorkerRequest, reply = WorkerReply` — `WorkerEvent`
carries `Cancel`/`Wake` (fire-and-forget), `WorkerRequest` carries the one
caller-authority `Process` message — with no hand-written rejection arm on
either side. The queue-side call sites wrap messages explicitly
(`tina::ServiceMessage::Request(WorkerRequest::Process { .. })` for
`call_cancelable`, `tina::ServiceMessage::Event(WorkerEvent::Cancel(id))`
for the opportunistic wake send), since `send`/`call_cancelable` take a
plain `Address<M, R>` and the split form's `M` is
`ServiceMessage<Event, Request>` — there is no split-service-typed
`call_cancelable` helper today, only `send_event` for the send side. All
4 existing smoke tests still pass unchanged.

### 27. Lease handoff into a `PendingReplies` slot — Phase 110 shipped — closed

`tina_runtime::GuardedPendingReplies<K, R, G>` pairs the parked caller
with one RAII `G` guard, drops it exactly once on reply / drain /
caller-gone sweep, and returns it back to the caller on failed
admission. `system_api_gateway_limits` now parks a
`SharedCapacityReservation` directly in the slot, so there is no
sidecar charge table.

**Verified on this audit:** `GuardedPendingReplies` is defined in
`tina-runtime/src/guarded_pending.rs`;
`examples/systems/system_api_gateway_limits/src/lib.rs` declares
`pending: GuardedPendingReplies<u64, GatewayReply,
SharedCapacityReservation>` directly — no sidecar lease map.

*(Historical finding kept below for context.)*

### 27-historical. Lease handoff into a `PendingReplies` slot

**Surfaced by:** `system_api_gateway_limits`, `system_soak_http_db`.

A request that admits against a `SharedCapacityScope` and then parks
its reply in `PendingReplies` has to carry the `SharedLease` in a
sidecar `HashMap<qid, SharedLease>` so the lease outlives the
post-sleep handler. Both new specimens do this manually. The mapping
between "this qid" and "this lease" is invariant under the slot
lifecycle and would compose cleanly into the slot itself.

**Build:** a slot variant — `PendingReplies::try_insert_with_lease(qid,
slot, lease)` — or a generic `SharedLease`-carrying wrapper that
`reply_to` consumes. Either form removes the parallel map.

### 31. `SleepReply` leaks into user-defined message variants — Phase 110 shipped — closed

`tina_runtime::sleep(d).then_event(move || Msg::Wake { id })` is the
sleep-only sugar: the user enum has no `SleepReply` field, and the
helper does not exist on non-timer `TypedCall<()>` so file/process/TCP
close errors stay visible. The phase still ships `sleep_then(d, m)` and
`sleep(d).then(...)` for the cases that *do* want the timer reply.

**Verified on this audit:** `then_event` is defined in
`tina-runtime/src/call/time.rs`.

*(Historical finding kept below for context.)*

### 31-historical. `SleepReply` leaks into user-defined message variants

**Surfaced by:** `system_api_gateway_limits`, `system_soak_http_db`.

Every variant a specimen builds for a post-sleep wake-up carries
`result: SleepReply` even when the handler never inspects it. The
gateway's `HoldDone { qid, route, result: SleepReply }` and the
soak's `HttpReleased { qid, ..., result: SleepReply }` are both
shaped this way. The field is dead weight in the user's message
enum, but the `sleep(d).then(move |r| Msg { result: r, ... })`
signature requires it.

**Build:** either (a) accept `then(move |_| Msg { ... })` without
the placeholder field as the blessed shape and add a `then_no_result`
variant, or (b) drop the `Result` from `SleepReply` for the
infallible-sleep case so the carrying variant is a unit. The wider
form is right for cancellation-aware sleeps; for the typical "wake
me up later, I don't care if you were nudged" the unit form would
keep the user's enum clean.

### 33. Bridge classifier vocabulary lives in `tina-aws-bridge` — shipped — closed

`tina_runtime::bridge::BridgeOutcomeClass` (with
`BridgeRetryable` / `BridgeUnavailable` / `BridgeFatal`) is the shared
shape every bridge classifier projects onto. Each per-bridge
classifier (reqwest, AWS workers) is still free to expose richer
per-bridge reasons, but the shared `bridge_class()` projection makes
mixed-bridge classification a typed fold instead of caller-private
re-mapping. The bridge-author copy path in
[`docs/tina-user-guide/30-bridge-author-kit.md`](../docs/tina-user-guide/30-bridge-author-kit.md)
step 7 names this contract.

**Verified on this audit:** `BridgeOutcomeClass` is defined in
`tina-runtime/src/bridge.rs`; `bridge_class()` projections exist in
`tina-aws-bridge/src/classifier.rs` and `tina-reqwest-bridge`.

*(Historical finding kept below for context.)*

### 33-historical. Bridge classifier vocabulary lives in `tina-aws-bridge`

**Surfaced by:** `system_webhook_relay`, classifier extension traits.

`BridgeOutcomeClass` / `TransientReason` / `FatalReason` were useful
outside AWS too — the reqwest bridge already has `ReqwestOutcomeClass`
with its own per-bridge vocabulary. A relay or retry-driver needs
*both* shapes to classify mixed outcomes (one outbound HTTP, one SQS)
the same way, so callers re-classify into a private enum.

**Build:** decide whether the bridge classifier should be in
`tina-runtime` (shared by all bridges) or whether each bridge keeps
its own private vocabulary and callers map at the boundary. The plan
forbids a shared bridge crate, but the *classifier vocabulary* is
plain data and could live alongside `CallOutcome` without coupling
the bridges themselves.

### 36. Whole-service copied path — Phase 120 shipped

**Surfaced by:** `mini_saas_api`, `system_metrics_shipper`,
`system_job_queue`, `system_realtime_rooms`, and the post-Wave-A system
specimens.

Closed by `system_copied_service_path`,
`system_copied_service_path_companion`, and
`system_copied_service_path_smoke`. A reader can now copy one ordinary service
skeleton and see the normal Tina path for request entry, bounded replies,
session app-control messages, service limits, reports, owner-stop shutdown,
live capture/replay/shrink workflow, fairness/load assertions, and join/select
helpers.

The important product choice: the copied path did not hide request/reply
authority in callbacks and did not build a fake async/select framework.
`CallJoinSet` and `CallSelectSet` keep named branch identity, bounded
pending/results, explicit loser cancellation, partial reports, and late-reply
truth visible. The companion proof and smoke-copy crate exist so a cheap model
or tired human can tell whether the path is actually copyable.

**Correction (external review, P0):** the claim above did not hold. The
skeleton built `CopiedServiceReport` from constants — no isolate, runtime,
listener, or shutdown ever ran — and its own smoke test failed
(`assert_no_leaked_capacity_at_shutdown` panicked with `leak=unchecked`
because the run never supplied a real leak check). `system_copied_service_path`
is rebuilt around one real `#[tina_runtime::isolate]` on a real
`ThreadedRuntime`: bounded admission via `SharedCapacityScope`, a durable-state
ledger step, real concurrent callers through `tina_proof_harness::load`, and a
leak check that reads the scope's real post-shutdown state. Skipping the
release (`Gateway::hold_done`'s `drop(lease)`) now makes the smoke test fail
for a real reason. `system_copied_service_path_companion` and
`system_copied_service_path_smoke` were deleted — they only re-verified the
same fake fields (`session_control`, `replay_roles`, `join`/`select`
capacities) and added no coverage beyond the rebuilt crate's own smoke test.
Systems examples are now gated in CI (`.github/workflows/`) and in
`Makefile`'s example-verification target, so this class of bug fails a PR
instead of shipping silently.

The config/budget half of the copied path is now closed too. Services used to
scatter caps through handlers and `register_*` literals, so a reader could not
see all knobs before the service ran. `tina_runtime::budget::ServiceBudgetManifest`
makes boundedness copyable: one object declares every cap with kind/unit/replay
impact, validates before startup with typed errors, builds rows from existing
configs through adapters, joins configured caps with observed pressure, and
exports the replay-affecting caps a saved DST case depends on. `mini_saas_api`
declares all its caps in one `src/budget.rs` manifest and reads them back from
there; `tests/budget.rs` proves the documented caps are exactly the manifest
rows and that every live surface has a row. Still deliberately manual: time
deadlines and retry-budget *durations* (the unit vocabulary is count and weight,
not time) and per-isolate mailbox depth the runtime does not sample.

### 36. `RequestCall` has no `now()`, blocking split-service migration for time-reading request handlers — closed

**Note:** shares finding number 36 with "Whole-service copied path" above —
a pre-existing duplicate in the ledger's numbering, not a re-use by this
entry.

**Closed.** `RequestCall::now()` (`tina/src/context.rs`) now delegates to
the inner `CallContext::now()`, borrow-only (`&self`), so a handler can
read the clock for deadline math ahead of `.defer(...)` without losing
caller authority. `specimen_backpressure_chain`'s `ServiceB` and `ServiceA`
are now on the split `#[tina_runtime::isolate(event = .., request = ..,
reply = ..)]` form (`examples/specimen_backpressure_chain/src/tina_impl.rs`),
each reading `call.now()` before `call.defer(...)`, dropping the manual
`handle`/`handle_call` pair and its hand-written `UnsupportedMessage`
reject arm. `cargo test -p tina --test request_call_now` proves the
accessor's value and borrow behavior; `cargo build --tests` +
`cargo test` on the specimen pass unchanged (2/2).

### 37. Accept-loop bad-peer survivability — Phase 120 hostile review

**Surfaced by:** `system_realtime_rooms/tests/bad_peer.rs`,
`tina-http/tests/server_bad_input.rs`.

The realtime-room bad-peer suite exposed a real listener survivability bug.
The plain HTTP listener treated any `tcp_accept` error as fatal and closed the
listener. A peer reset or half-close can surface as accept-side
`CallError::Io`, so one bad peer could shut the front door and make the next
peer observe `ConnectionRefused`.

Closed by re-arming the HTTP/1 and h2c accept loops on accept-side
`CallError::Io` while preserving fatal handling for non-`Io` internal contract
errors. The proof is user-facing, not pretty-wire-output-facing: reset,
half-close, and malformed peers may observe reply bytes, EOF, or reset
depending on OS close timing, but later fresh connections must still be
accepted and served.

### 26. Call-shaped sends from `handle_call` deliver completions back as calls — closed by phase 114

Closed by the live-runtime regression
`tina-runtime/tests/runtime_call_completion_from_handle_call.rs::runtime_call_returned_from_handle_call_completes_as_event`.

The test pins the user-truth resolution chosen in option (a) of the
original finding: when an isolate's `handle_call` returns a
runtime-owned call effect (`sleep(...).then_with_request(req, ...)` in
the regression, but applies to any `.then` continuation), the
completion arrives as an ordinary internal-event message at `handle`,
not back at `handle_call`. The original caller receives the deferred
reply through `reply_to_request`, and the trace records no
`CallRejected { UnsupportedMessage }` event for the continuation.

This is the path `system_realtime_rooms` would have wanted: send-shaped
effects emitted from `handle_call` no longer carry hidden routing back
into `handle_call`. If a future change reintroduces the hidden routing,
this regression test catches it on the live threaded runtime path
(non-split isolate, no fixtures, hermetic timer).

### 34. `call.defer(async_bridge).reply(...)` from `handle_call` — Phase 104 proof

The suspected runtime gap was re-tested before Phase 104 merged. The
general runtime path already works (`handle_call` defers through a
multi-turn callee and preserves the original caller). Phase 104 now
pins the AWS-shaped version directly with hermetic S3 and SQS bridge
tests:

- `tina-aws-bridge/tests/bridge.rs::handle_call_defer_through_s3_bridge_replies_to_original_caller`
- `tina-aws-bridge/tests/sqs_bridge.rs::handle_call_defer_through_sqs_bridge_replies_to_original_caller`

Both tests put a relay/lane isolate in front of the AWS bridge, issue
`call.defer(send_s3/send_sqs(...)).reply(...)` from `handle_call`, let
the AWS bridge complete through its async SDK task + `sleep().then(Poll)`
loop, and assert the original caller receives the final reply. The public
`run_against_s3` / `run_against_sqs` paths remain available for larger
system specimens, but the panic report is closed.

### 17. Host-thread `call_blocking` — Phase 068 follow-up

Surfaced by `specimen_native_https` and native HTTP/TLS tests.
`ThreadedRuntime::call_blocking(addr, msg, timeout)` now performs
the ordinary typed Tina call through a temporary driver isolate and
returns `CallOutcome<R>` to the host thread. The HTTPS specimen and
the direct TLS client/server tests use it; tests that intentionally
need a concurrent in-flight call still keep an explicit driver.

### 18. Trace query helpers — Phase 068 follow-up

Surfaced by TLS regression tests that repeatedly scanned for
`RuntimeEventKind::CallCompleted` / `CallFailed` by hand.
`RuntimeTraceExt` now adds `count_completed`, `any_completed`,
`count_failed`, `any_failed`, `count_failed_with`, and
`count_completion_rejected` on trace slices. The helpers summarize
existing trace facts only; they do not infer hidden causality.

### 1. `observe_result` on `ThreadedMultiShardRuntime` — Phase 062 Rock 1

Surfaced by `specimen_sharded_fanout_read`, `specimen_sharded_keyspace`.
`runtime.observe_result::<Report, _, _>(addr)` now exists on the
multi-shard threaded shell with the same single-claim semantics as
the single-shard form. Both 053 specimens use it directly; the
`Arc<Mutex<Option<Report>>>` polling is gone.

### 4. Synchronous `try_send_outcome` — Phase 062 Rocks 3 & 4

Surfaced by `specimen_rate_limited_worker`,
`specimen_hot_key_fairness`. `runtime.try_send_outcome(addr, msg,
&outcomes)` plus a shared `HostBurstOutcomes` accumulator removes
the per-send observer closure, the Arc-cloned counters, and the
manual observed barrier. `runtime.send_observed_until(addr,
deadline, backoff, || msg)` covers the "control message through a
saturated mailbox" pattern with a typed
`SendObservedUntilError::{Timeout, Closed, WorkerStopped}`.

Per-send precision still rides on the worker-thread observer: true
synchronous-in-the-host mailbox inspection would violate SPSC and
expose the worker's address->mailbox registry to the host thread,
so the helper removes bookkeeping, not the worker roundtrip.

### 5. Single-in-flight gate for timer-driven workers — Phase 062 Rock 5

Surfaced by `specimen_rate_limited_worker`,
`specimen_hot_key_fairness`, and reinforced by
`specimen_periodic_batcher` / `specimen_graceful_drain_server`.
`tina_runtime::SingleCallGate` names the "at most one timer/call in
flight, plus N queued" invariant. `submit()` returns `true` when
the caller should schedule; `complete()` returns `true` when more
work is queued and the next timer should be scheduled. The gate is
plain data — it does not own the timer or the trace; the caller
still writes `sleep(...).then(...)` so every event is visible.

### 6. Bridge call retry classifier — Phase 062 Rock 6

Surfaced by `specimen_retrying_outbound_http`,
`specimen_webhook_fanout`. `ReqwestOutcomeExt::classify` returns
`ReqwestOutcomeClass::{Succeeded, Transient(reason),
Fatal(reason)}` with typed reason payloads. The raw layered
`ReqwestCallOutcome` and `flatten_outcome` are unchanged; the
classifier is opt-in sugar. `specimen_retrying_outbound_http` and
`specimen_webhook_fanout` now match three arms instead of six.

### 10. Retry helper at the service edge — Phase 062 Rock 4

Closed by the same Rock as finding 4. `send_observed_until` covers
both shapes — burst-message ingress and one-shot control-message
delivery through a saturated mailbox.

## How To Add A Finding

Only add to this file when the finding implies Tina product work.

```md
### N. Short product-shaped title

**Surfaced by:** `example_name`, `other_example`.

What repeated pain we saw.

**Build:** concrete primitive, API, doc, or test work.
```

Per-example flavor belongs in the example README. Resolved
archaeology belongs in `FINDINGS_HISTORY.md`.

Numbers are stable: when a finding closes, move it down to
[Closed](#closed) and keep its number so external references
(README links, commit messages, prior PRs) stay valid.

## Resolved Or Retired Round 1 (Phase 053 + 059)

Round 1 closed in Phase 059 + Phase 053. Those nine items are
archived verbatim in [`FINDINGS_HISTORY.md`](FINDINGS_HISTORY.md).
Short summary of patterns no new code should copy:

- hand-rolled mailbox factories: use `DefaultMailboxFactory` /
  `DefaultThreadedMailboxFactory`;
- `Arc<Mutex<Option<SocketAddr>>>` for listener bind address: use
  `observe_next_bound()`;
- trace fingerprinting via `Debug`: use `RuntimeEvent::stable_hash()` /
  `stable_trace_hash(...)`;
- one-off shard types for single-shard programs: use `SingleShard` or omit
  `shard = ...`;
- `Arc::try_unwrap` bridge shutdown dances: use the bridge host lifecycle;
- `Arc::try_unwrap(runtime)` host shutdown dances on threaded runtimes:
  use `runtime.shutdown_handle()` and the cloneable
  `ThreadedShutdownHandle` (`request_shutdown` is nonblocking and
  idempotent; `wait_report(timeout)` returns the cached terminal
  report; see [docs/tina-user-guide/14-lifecycle-and-shutdown.md](../docs/tina-user-guide/14-lifecycle-and-shutdown.md));
- old shared comparison harnesses: examples are specimens, tests are proof;
- `Arc<Outcome>` / `Arc<Mutex<Vec<_>>>` for an isolate's *final* app
  value: use `stop_with(value)` +
  `runtime.observe_result::<T>(addr)?` (works on single-shard and
  multi-shard threaded runtimes; see active finding 1's closure
  above);
- per-comparison shard types: use `SingleShard` for one-shard programs and
  `tina_runtime::sharded::ShardPlacement` / `ShardServiceTable` for
  multi-shard placement.
