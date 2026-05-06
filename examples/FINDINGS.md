# Eiffel Findings

Cross-cutting observations from the Tokio-vs-Tina comparisons in this
directory. Per-comparison ergonomic notes live in each comparison's own
`README.md` (e.g. `eiffel_real_io_chat/README.md`,
`eiffel_mini_keyspace/README.md`); this file collects the patterns that show
up across more than one comparison and the runtime/API suggestions they
imply.

Findings here are dated and signed with the comparison that surfaced them so
we can track when something keeps reappearing vs. when it was a one-off.

## What feels good (keep these)

### Owned state through isolates is the right model
*Surfaced by:* `eiffel_mini_keyspace`.

Declaring `Store` as an isolate that owns a `BTreeMap` removed the
`Arc<Mutex<_>>` temptation entirely — there is no syntactic path to shared
mutable state. This is a real property the type system enforces, not a
convention. Every comparison so far has reinforced that this is the model's
core strength.

### `call(addr, msg, timeout).reply(map_outcome)` is honest
*Surfaced by:* `eiffel_mini_keyspace`.

Request/reply at an isolate boundary reads like what it is: send a message,
the answer comes back as another message. Verbose vs. async/await, but no
hidden state machine and no implicit cancellation point. The right shape for
a system we want to model formally later.

### `BridgeHandle` composes cleanly with `axum::State`
*Surfaced by:* `eiffel_axum_counter`.

`BridgeHandle::new(...)` produces a `Clone` value that drops straight into
`Router::with_state(...)`, and `bridge.call(req).await` is the whole call
site. The fact that this composes with axum's extractor model with zero
adapter glue is the single strongest thing about the bridge story so far.

### Visible HTTP backpressure
*Surfaced by:* `eiffel_axum_counter`.

`BridgeError::{Full,Closed,Timeout}` reach an axum handler as a real error
variant, so HTTP-shaped pushback (503 etc.) is visible at the call site
instead of silently buffered. The Tokio side's `Arc<Mutex<_>>` pattern
cannot offer this property at all.

### Subscriber pruning falls out of `retain` + `try_send`
*Surfaced by:* `eiffel_ws_room`.

The Room isolate's publish path is one expression:
`subscribers.retain(|tx| tx.send(text.clone()).is_ok())`. Dead subscribers
are removed in the same pass as the broadcast. The Tokio
`broadcast::channel` recipe quietly converts the same condition into
`RecvError::Lagged` that callers usually swallow.

### Out-of-order multiplexing without a shared map
*Surfaced by:* `eiffel_mux_client`.

The Tina client never builds an `Arc<Mutex<HashMap<u32, oneshot::Sender>>>`
because the parser, the buffer, and the pending counter all live behind
the same mailbox. Out-of-order arrival just works — the runtime delivers
`tcp_read` replies as bytes land, and the handler walks complete lines.
The Tokio recipe needs a reader task, a submit task, a shared map, and a
oneshot per request.

### State machines as `enum` + `match` are legible
*Surfaced by:* `eiffel_mini_keyspace`, `eiffel_outbound_fetch`,
`eiffel_persistent_counter`.

Once written, an isolate's `handle` is one of the easier-to-trace pieces of
code in the example. Each transition is one arm. Each effect is one
expression. No "where does this resume" mystery. The shape transfers across
roles (server connection, durable state machine, outbound TCP client) — the
same `Begin → IO → IO → ... → Done` skeleton fits all three.

### Append-before-apply is enforced by message shape
*Surfaced by:* `eiffel_persistent_counter`.

The Tina counter cannot update `self.value` until `AppendDurable(Ok(()))`
returns, because that is the only message variant where the new value is
known. The Tokio side could trivially be written in the wrong order and
only break under crash. Durability ordering becomes a typestate property
rather than a discipline.

### Supervision policy is named, finite, and observable
*Surfaced by:* `eiffel_supervised_worker`.

`runtime.supervise(parent, SupervisorConfig::new(OneForOne, RestartBudget::new(N)))`
is the entire restart story. The policy has a name, the budget is finite,
and the runtime emits `RuntimeEventKind::SupervisorRestartTriggered` so the
restart count is asserted from the trace, not from a counter the user
maintained. Tokio shops re-write the supervise loop every time, slightly
differently, with no shared vocabulary.

### Deterministic replay is a real, asserted property
*Surfaced by:* `eiffel_replay_dst`.

`Simulator::new(seed)` plus `run_until_quiescent` plus a
`DefaultHasher`-of-debug-trace produces a fingerprint that is
byte-identical across two runs of the same seed and *different* across
two seeds. Tokio has no analogue — `start_paused: true` is a paused
clock, not a seeded scheduler. This is the property the rest of Eiffel
silently relies on: every other comparison can in principle be replayed
under seeded faults.

### Tina-as-client and Tina-as-server are the same Tina
*Surfaced by:* `eiffel_outbound_fetch`.

`tcp_connect(addr).reply(...)` reads the same as `tcp_bind` and
`tcp_accept` from server comparisons. The `Connected` reply also returns
both endpoints (`local`, `peer`), where `tokio::net::TcpStream::connect`
returns just the stream and forces a separate `.local_addr()` call.

### `signal_wait` is the whole signal story at the user-code surface
*Surfaced by:* `eiffel_graceful_shutdown`.

`signal_wait("sigint", timeout).reply(SignalMsg::Received)` is one
runtime call. The reply carries the signal name, so a single watcher
can distinguish "sigint" (graceful) from "sigterm" (forced). The
shutdown *trigger* is decoupled from the shutdown *effect* — the
watcher's only job is `send(producer, ProducerMsg::Stop)`, and the
producer absorbs shutdown via a normal match arm. State machines
treat shutdown the way they treat every other event.

### Cancellation as a message arm beats `select!` for inspectability
*Surfaced by:* `eiffel_graceful_shutdown`.

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
*Surfaced by:* `eiffel_graceful_shutdown`.

When a Tokio runtime shuts down, what was in flight, when, in what
order, against which task — gone. Whatever the application was
tracking via shared atomics is what you have. Tina exposes
`runtime.shutdown_report()` so the operator can ask the runtime
itself what work was outstanding at shutdown time. We don't even
exercise it in the example — we already track produced/processed
via telemetry — but it's worth flagging as the kind of primitive
that only exists when the runtime knows about the work it owns.

## What feels bad (papercuts)

### Mailbox boilerplate per example
*Surfaced by:* `eiffel_mini_keyspace`, `eiffel_real_io_chat`,
`eiffel_axum_counter`, `eiffel_ws_room`, `eiffel_mux_client`,
`eiffel_supervised_worker`, `eiffel_persistent_counter`,
`eiffel_outbound_fetch`, `eiffel_graceful_shutdown`.

Every example ends up rolling its own `Mailbox<T>` + `MailboxFactory`
implementation backed by `Rc<RefCell<VecDeque<_>>>`. Forty lines of mostly
identical boilerplate to do the most obvious in-process thing.

**Improvement:** ship a default in-process `MailboxFactory` so examples and
small services don't have to reinvent it.

### The runtime knows; the user has to scrape
*Surfaced by:* `eiffel_mini_keyspace`, `eiffel_real_io_chat`,
`eiffel_mux_client`, `eiffel_supervised_worker`, `eiffel_persistent_counter`,
`eiffel_outbound_fetch`, `eiffel_graceful_shutdown`, `eiffel_replay_dst`.

The most-recurring papercut in the suite. The runtime *has* the
information the driver thread needs — every comparison's "wait for X
to happen" is something the runtime emits as a trace event or knows
internally. But the only public way to read it is `complete_trace()`
polling or hand-rolled side channels. Every comparison invents its
own variant:

- `eiffel_mini_keyspace`, `eiffel_real_io_chat`:
  `Arc<Mutex<Option<SocketAddr>>>` because `tcp_bind` won't tell the
  spawning thread what port it got.
- `eiffel_mux_client`: `Arc<Mutex<Vec<u32>>>` to harvest arrival
  order from the client isolate.
- `eiffel_supervised_worker`: `Arc<Mutex<Option<Address<...>>>>` plus
  an `AtomicU64` generation counter so the driver can wait for the
  *next* worker incarnation after a restart.
- `eiffel_persistent_counter`: a `u64` correlation id (`op`) threaded
  through every continuation message so the driver can know when a
  *specific* increment has finished.
- `eiffel_outbound_fetch`: `Arc<AtomicBool>` `done` flag the driver
  spins on while the fetcher isolate completes.
- `eiffel_graceful_shutdown`: `Arc<Telemetry>` with four atomics
  (produced, processed, signal_received, producer_stopped) plus a
  three-condition spin-loop on the driver thread.
- `eiffel_mini_keyspace`, `eiffel_supervised_worker`: `complete_trace()`
  polled in a loop for `CallKind::TcpStreamClose` /
  `SupervisorRestartTriggered` events the runtime already emits.
- `eiffel_replay_dst`: `format!("{event:?}").hash(...)` to fingerprint
  the trace because there is no `RuntimeEvent::stable_hash()` —
  works, but trusts `Debug` to be stable.

Eight comparisons, all reaching for the same missing primitive from
slightly different angles. The runtime already has the information;
example code shouldn't be the one polling for it.

**Improvement:** the highest-leverage change in this whole document.
1. A typed "isolate completion result" handle (a Tina
   `JoinHandle`-equivalent) so callers `.await` an isolate's outcome
   without side channels and without scraping the trace.
2. `tcp_bind` reply path that exposes the bound `SocketAddr` to the
   spawning code without `Arc<Mutex<Option<_>>>`.
3. `RuntimeEvent::stable_hash()` (or `serialize`) so trace-equality
   proofs don't depend on `Debug` formatting.

Together these three would shorten every comparison in the suite,
remove three different hand-rolls of the same shape, and let the
runtime's existing observability surface actually be observable.

### Tokio + Tina signal handlers do not coexist cleanly in one process
*Surfaced by:* `eiffel_graceful_shutdown`.

Both `tokio::signal::ctrl_c()` and `tina_runtime::signal_wait("sigint", _)`
register process-global handlers via `signal-hook`. `signal-hook` chains
handlers, so multiple registrations *technically* coexist, but when the
Tokio runtime drops, its registration stays in the chain. Subsequent
SIGINTs fire the now-orphaned Tokio handler too. This works in practice
but is the kind of cross-runtime sharp edge that is hard to debug when
something does break, and there is no public API to query "which
handlers are registered" or "tear down my registrations."

`eiffel_graceful_shutdown` works around it by spawning each side as a
subprocess in `compare` mode. Worth a public note in any "embedding
Tina inside a Tokio app" guide.

**Improvement:** document the coexistence pattern explicitly, and
ideally expose a `runtime.unregister_signal_handlers()` for tests
that want to swap signal ownership cleanly.


### "Process a list of things" has no native shape
*Surfaced by:* `eiffel_mini_keyspace`, `eiffel_mux_client`.

A `VecDeque<Command>` plus "do them one at a time" required a hand-rolled
recursive `next_effect()` helper that pops + dispatches + tail-calls into
itself via response messages. There is no built-in iteration combinator and
nothing that resembles `for cmd in commands { ... .await }`. This shape is
going to recur in every connection handler.

`eiffel_mux_client` ran into the same gap from a slightly different angle:
issuing three independent `tcp_write` effects on a single stream as a
`batch(...)` wedged the runtime; the example had to concatenate the three
requests into one payload. Multiplexing in Tina currently has to either
collapse independent ops into one buffer or chain them sequentially via
continuation messages — Tokio's "spawn N tasks that each `.await` on the
same connection" has no clean analogue.

**Improvement:** a sugar/combinator for sequenced calls and explicit docs
on what `batch(...)` does and does not guarantee for same-stream effects.
This is the gap that will most consistently make Tina feel verbose vs.
Tokio.

### Mailbox capacities are load-bearing magic numbers
*Surfaced by:* `eiffel_mini_keyspace`, `eiffel_real_io_chat`.

We pick 16 because other tests pick 16. Pick 4 and the run silently breaks
— no compile-time hint, no warning, just dropped messages or deadlock.

`eiffel_real_io_chat` exposes the specific *cause* that makes capacities
load-bearing: every `send_observed(...).reply(...)` outcome comes back
through the *requester's* mailbox. A connection isolate that fans out a
burst of 64 admissions has to absorb 64 reply messages before it can
finish writing its response. The first draft of that example sized the
connection mailbox at the obvious "one per concurrent operation" value
and could not collect enough observed outcomes to make progress; the
fix was sizing the connection mailbox separately to account for replies.
This is not unique to `send_observed` — every `call(...).reply(...)`
and `tcp_*(...).reply(...)` consumes one slot in the requester's
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
*Surfaced by:* `eiffel_mini_keyspace`, `eiffel_graceful_shutdown`.

Two flavors of the same shape — runtime calls return outcomes that
are wider than the common case, forcing every call site to write
match arms for failure modes that effectively never fire.

- **`CallOutcome<Reply>` for in-process calls.** For a call to a
  store isolate that always replies, the `Timeout` / `Closed` arms
  are unreachable but still have to be matched on every call site
  (`eiffel_mini_keyspace`).
- **`Result<(), CallError>` on `sleep(...).reply(...)`.** Every Tina
  handler that uses `sleep(...).reply(...)` ends up with a
  `TimerFired(_, Result<(), CallError>)` continuation whose `Err`
  arm is dead code on healthy systems (`eiffel_graceful_shutdown`).
  The same will apply to other "rarely fail" runtime calls.

**Improvement:**
- A "call that can't time out" form for in-process callees, or a
  more focused outcome type when the timeout is the only possible
  failure.
- A `sleep(...).reply_ignore_error(...)` shorthand or a
  `Result<(), Infallible>`-shaped narrower outcome for
  effectively-infallible runtime calls.

### Bridge-hosted services: two runtimes that don't compose cleanly
*Surfaced by:* `eiffel_axum_counter`, `eiffel_ws_room`, `eiffel_mux_client`.

A bridge service is one Tina runtime (its own thread) plus a Tokio
runtime that hosts axum and calls into the bridge. That is exactly
what the bridge *is*, and `BridgeHandle` composes cleanly with axum at
the call site (see "What feels good"). The friction is at the seams.

Two failure modes hit during the comparisons:

- **Sync recv inside a Tokio current_thread `block_on(...)` deadlocks
  the executor.** `eiffel_mux_client` originally used
  `std::sync::mpsc::Receiver::recv()` to wait for a server-shutdown
  signal inside the Tokio runtime hosting the responder. The
  current_thread runtime cannot drive futures while the OS thread is
  blocked on a sync recv, so the responder task never advanced and
  the test wedged. Fix: `tokio::sync::oneshot`. This is a real
  cross-runtime footgun — the failure looks like "my server didn't
  start" but the cause is "my driver thread blocked the executor."
- **`Arc<ThreadedRuntime>` has to be unwrapped with `Arc::try_unwrap`
  before `shutdown()` can run.** `eiffel_axum_counter` and
  `eiffel_ws_room` both end with the same dance because `BridgeHandle`
  clones still hold references at typical scope-exit time. There is
  no "drain and stop" affordance on `BridgeHandle` itself; the user
  has to remember to drop every clone before the runtime can shut
  down cleanly.

The comprehension cost the first time you see the two-runtime
arrangement is also real — first-time readers do not expect the Tina
side of an axum app to spin up *both* a `ThreadedRuntime` and a
`tokio::runtime::Builder::new_current_thread()`.

**Improvement:**
- A one-call shutdown on `BridgeHost` / `BridgeHandle` that closes the
  handle, drains pending requests, and returns the runtime trace
  without forcing example code to do `Arc::try_unwrap` dances.
- A documented "runtime composition" pattern (or thin helpers) for
  the canonical bridge service shape, naming the sync-recv-inside-
  block_on footgun explicitly.

### Continuation enum growth
*Surfaced by:* `eiffel_persistent_counter`, `eiffel_outbound_fetch`,
`eiffel_mini_keyspace`.

The user-guide ergonomics page already lists this; the new comparisons
confirm it lands harder when the protocol has more than three steps.
`CounterMsg` had to thread an `op: u64` through `Increment →
AppendDurable → publish`, plus the recovery chain `Recover →
SnapshotLoaded → JournalLoaded`. `FetchMsg` ballooned to `Begin →
Connected → Wrote → Read → Closed` plus their `Ok`/`Err` arms.

**Improvement:** typed continuation aliases or a generated-name
helper, as already noted in `docs/tina-user-guide/10-ergonomics-notes.md`.

### No `read_to_end` / `write_all` at the runtime-call layer
*Surfaced by:* `eiffel_outbound_fetch`.

`tcp_read` returns one `Vec<u8>` chunk; EOF is "zero bytes" and has to
be hand-detected. `tcp_write` may write less than the buffer; partial
writes need a `pending_write.drain(..count)` self-loop. Tokio's
`read_to_end` / `write_all` papers over both. Probably correct that
Tina exposes the truthful one-shot form — but every TCP client will
re-implement the same loop until a helper lands.

**Improvement:** companion helpers (`tcp_read_to_eof`, `tcp_write_all`)
or a documented snippet in the TCP guide.

### Threaded and explicit-step API surfaces have drifted apart
*Surfaced by:* `eiffel_supervised_worker`.

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

### `tina::isolate` vs `tina_runtime::isolate` divergence is invisible until simulator
*Surfaced by:* `eiffel_replay_dst`.

`#[tina::isolate(...)]` wires `Call = Infallible`. `#[tina_runtime::isolate(...)]`
wires `Call = RuntimeCall<Msg>`. The simulator requires the latter,
and the failure mode is a generic-bound mismatch in the type checker,
not a comprehensible diagnostic.

**Improvement:** either lift the simulator's `Call` requirement, or
emit a targeted diagnostic when an isolate using `#[tina::isolate(...)]`
is registered with `Simulator::register_with_mailbox_capacity`.

### Simulated process restart needs a fresh runtime
*Surfaced by:* `eiffel_persistent_counter`.

There is no public "warm-restart" or "re-recover" path on a live
`ThreadedRuntime`. The persistence example splits into two
`run_phase()` calls each with its own runtime against the same data
dir. Probably correct (you really do want a fresh runtime on a real
restart), but the example reads more like "two embedded services"
than "one service across a restart."

**Improvement:** either bless the "two-runtime" pattern with a
documented helper, or expose a `runtime.simulate_restart()` for tests
that re-recovers without tearing down the host process.

### `shard = SomeShard` is mandatory even with one shard
*Surfaced by:* `eiffel_mini_keyspace`, `eiffel_axum_counter`,
`eiffel_ws_room`, `eiffel_supervised_worker`,
`eiffel_persistent_counter`, `eiffel_outbound_fetch`,
`eiffel_replay_dst`, `eiffel_graceful_shutdown`.

Every isolate declares a `shard` even when the program only has one. The
`KeyspaceShard` type exists solely to satisfy the macro.

**Improvement:** allow shard to be omitted (or default to a built-in
single-shard) for single-shard programs.

### Comparisons don't yet expose load-shedding metrics
*Surfaced by:* `eiffel_cpu_run`, `eiffel_mem_run`.

The CPU contention runner can answer "did the comparison still pass
under N spinners?" but it cannot answer "did Tina shed load visibly while
Tokio buffered silently?" — because the existing comparisons assert a
fixed scripted output and do not yet expose accepted/full/closed counts
under load. The closest existing metric is
`eiffel_real_io_chat::SideReport::saw_visible_full`.

**Improvement:** when load drivers land in individual comparisons,
surface a uniform overload-counter shape (accepted/full/closed/timeouts)
that the contention/memory runners can diff between baseline and
constrained runs.

### Constraint runners are platform-asymmetric
*Surfaced by:* `eiffel_mem_run`.

`RLIMIT_AS` is a real cap on Linux but is unhelpful on macOS — sub-GB
caps reject child spawn with `EINVAL` because of address-space reserved
at process startup. The honest fix is to gate the cap to Linux and
clearly document that on other platforms the runner is a no-op. The
broader lesson: any runner that depends on kernel-level resource
limits must declare its platform truth, not pretend otherwise.

## Suggested follow-ups, ranked by frequency of trip

Counted by how many comparisons surfaced the issue. Several of these
collapse to a smaller number of underlying primitives, called out in
parentheses.

1. **Typed isolate-completion handle (a Tina `JoinHandle`-equivalent),
   plus `tcp_bind` reply available without `Arc<Mutex<Option<_>>>`,
   plus `RuntimeEvent::stable_hash()` for replay fingerprints.**
   *Eight comparisons.* The single highest-leverage change in the
   document — see "The runtime knows; the user has to scrape" above.
   Eight different hand-rolls of the same shape collapse to one
   missing primitive (plus two adjacent ones the same handle would
   compose with).
2. **Default in-process `MailboxFactory`.** *Nine comparisons.* Kills
   the most boilerplate per file; quietly the biggest line-count win.
3. **Optional shard for single-shard programs.** *Eight comparisons.*
   Trivial macro change.
4. **Continuation enum aliases / sugar, plus a narrower outcome type
   when failure modes are constrained.** *Four comparisons.*
   `CounterMsg` and `FetchMsg` ballooned with `Result<_, CallError>`-
   shaped continuations whose `Err` arms are dead code; same shape
   shows up on `sleep(...).reply(...)` continuations. Already filed
   in `docs/tina-user-guide/10-ergonomics-notes.md`; the new
   comparisons confirm it lands harder with more protocol steps.
5. **Bridge service ergonomics: one-call shutdown plus a documented
   composition pattern.** *Three comparisons.* `Arc::try_unwrap`
   dance to call `shutdown()`, plus the sync-recv-inside-block_on
   footgun, plus the comprehension cost of two runtimes for a
   first-time reader.
6. **Sugar / docs for "sequence of calls then continue," and an
   explicit contract for `batch(...)` on same-stream effects.** *Two
   comparisons.* `next_effect()` recursive helper in keyspace; the
   wedge that hit `eiffel_mux_client` when batching same-stream
   writes alongside a read.
7. **Unify `ThreadedTrySendError` / `Runtime::try_send` failure
   surfaces and `runtime.supervise(...)` / `Runtime::supervise(...)`
   return types.** *One comparison* but a type-level papercut that
   will trip every porter, and the type system will not catch the
   semantic difference between "fire-and-forget" and "explicit
   closed signal."
8. **Reply-slot accounting in mailbox sizing.** *Two comparisons.*
   The hidden rule that every `call(...).reply(...)` and observed
   send consumes one slot in the *requester's* mailbox; the chat
   example wedged on this in its first draft. Either better
   diagnostics, a separate "reply capacity" budget, or explicit
   sizing guidance in the user guide.
9. **`tcp_read_to_eof` / `tcp_write_all` companions.** *One
   comparison*, but every TCP client will re-implement these.
10. **Better diagnostic when `#[tina::isolate(...)]` is registered
    with `Simulator`.** *One comparison*; the current generic-bound
    mismatch is hard to read.
11. **Bless the "fresh runtime per phase" pattern (or expose a
    `simulate_restart()`) for persistence tests.** *One comparison.*
12. **Document the Tokio + Tina signal-handler coexistence pattern,
    optionally expose `runtime.unregister_signal_handlers()`.** *One
    comparison.*
13. **Uniform overload-counter shape (accepted/full/closed/timeouts)
    on per-comparison `SideReport`s, so `eiffel_cpu_run` and
    `eiffel_mem_run` can diff baseline vs. constrained.** *Two
    runners.*

## How to add to this file

When a comparison surfaces something:

- Add it under "what feels good" or "what feels bad" with a one-line
  *Surfaced by:* tag listing the comparison(s).
- If multiple comparisons hit the same thing, append the new one to the tag
  rather than duplicating the entry — that's how we see what's recurring.
- Per-comparison flavor (specific code shapes, surprising error messages,
  domain-specific quirks) belongs in the comparison's own README, not here.
