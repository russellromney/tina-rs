# Phase 059: Eiffel Actionable Ergonomics

## Goal

Turn the current Eiffel findings into Tina product improvements.

047 harvested the first round of obvious boilerplate. 048, 052, and 058 then
added native HTTP and RPC surfaces. The specimen rewrite made the remaining
pain easier to see.

059 answers:

> What should Tina add next so small real services feel boring to write without
> hiding boundedness, timeout, ownership, or trace truth?

Near-grug:

> less ceremony. same truth. no secret queue.

## Baseline

Already exists:

- Eiffel specimens under `examples/`;
- current action list in `examples/FINDINGS.md`;
- longer history in `examples/FINDINGS_HISTORY.md`;
- ergonomics checklist in `docs/tina-user-guide/11-ergonomics-checklist.md`;
- default mailbox factories;
- single-shard defaults;
- stable trace hashing;
- bound-address / isolate-complete / operation-done / child-restart waiters;
- bridge lifecycle helpers;
- native HTTP first form;
- native RPC first form;
- typed RPC service macro and Tokio RPC bridge.

Still recurring:

- host code needs final app data from isolates and still reaches for side
  channels;
- linear I/O protocols grow continuation enums quickly;
- TCP loop patterns repeat;
- mailbox capacity is correct but still feels like magic arithmetic;
- host/test code writes poll loops around `ThreadedRuntime::try_send`
  `IngressFull`;
- native HTTP routing is still hand-written `(method, path)` matching;
- bridge examples are still old-shape because bridge ergonomics landed later;
- RPC topology is single-service only in practice;
- pressure runners lack one small overload report vocabulary.

## Non-Goals

- No fake async handlers.
- No hidden unbounded queue.
- No hidden retry.
- No hidden timeout.
- No hiding `Full`, `Closed`, `Timeout`, or partial failure.
- No new web framework.
- No gRPC or streaming RPC.
- No I/O substrate work.
- No broad performance claim.
- No giant macro that makes trace truth hard to see.

## Rules

- Convenience may hide byte plumbing and repeated state-machine ceremony.
- Convenience may not hide capacity, timeout, topology, retry, or failure.
- Every new helper must keep a testable lower-level shape.
- If a helper starts work, the user must know where capacity lives.
- If a helper waits, the user must choose or see the timeout.
- If a helper retries, the caller must opt in explicitly.
- If a helper collects results, the collection is bounded or visibly partial.
- Example rewrites must preserve the specimens rule: examples show feel;
  library tests prove invariants.

## Rocks

1. **Typed Isolate Result Waiters** — *landed*

   Build a typed host-visible result path for isolates.

   Problem:

   - `observe_isolate_complete(addr)` says an isolate stopped;
   - examples still use `Arc<Outcome>`, atomics, mpsc channels, or driver
     isolates to retrieve final app data.

   Locked shape:

   ```rust
   // isolate side: new effect that piggybacks on existing Stop
   stop_with(value)  // T: Send + 'static

   // host side: turbofish-typed observation, returns a Result eagerly
   let done = runtime.observe_result::<T>(addr)?;
   let value: T = done.wait(timeout)?;
   ```

   Design decisions (locked):

   - `Effect::StopWith(StopResult)` joins `Effect::Stop`. `StopResult` wraps
     `Box<dyn Any + Send>` with a manual `Debug` (no `Debug` bound on `T`).
     Avoids threading a result type onto `Address<M, R>` and avoids an
     `Isolate::Result` associated type with default.
   - `runtime.observe_result::<T>(addr)` is turbofish-typed; the registry
     stores the requested `TypeId` plus a type-erased `ResultSender` trait
     object that owns the typed `SyncSender<ResultDelivery<T>>`.
   - At register time the runtime checks: isolate is alive at this
     `(IsolateId, AddressGeneration)`, no waiter is already claimed, and the
     observation cap is not full. Eager errors return `AlreadyStopped`,
     `AlreadyClaimed`, or `ObservationFull`.
   - Single-claim per `(IsolateId, AddressGeneration)`. Second register call
     returns `AlreadyClaimed`. Result is delivered exactly once.
   - At delivery time: `Effect::Stop` resolves any registered waiter as
     `StoppedWithoutResult`. `Effect::StopWith(any)` downcasts against the
     waiter's stored `TypeId` — match → deliver `T`; mismatch → deliver
     `TypeMismatch` and drop the value. No registered waiter → drop the
     value silently. **No post-hoc result replay cache** keeps memory bounded.
   - `ResultWaitError`: `Timeout`, `RuntimeStopped`, `ObservationFull`,
     `AlreadyClaimed`, `AlreadyStopped`, `StoppedWithoutResult`,
     `TypeMismatch`. Three of these (`ObservationFull`, `AlreadyClaimed`,
     `AlreadyStopped`) only fire at register time; the rest only fire on
     `wait`.
   - Existing `IsolateCompleteWaiter` and the trace's `IsolateStopped` event
     stay unchanged. `observe_result` is additive.

   Requirements:

   - works for explicit-step and threaded runtimes;
   - waiter registration is bounded by the existing observation cap;
   - timeout is caller-visible on `wait`;
   - dropped waiter cleanup happens when the slot's sender is dropped;
   - stopped-without-result, type mismatch, runtime-shutdown, and timeout
     outcomes are typed;
   - trace still records isolate stop/completion truth;
   - no unbounded result registry; no replay cache.

   Proof:

   - focused runtime tests for success, timeout, dropped waiter, isolate
     stops without result, runtime shutdown, `AlreadyStopped` (late
     register), `AlreadyClaimed`, `ObservationFull`, `TypeMismatch`;
   - update at least two Eiffel specimens that currently carry side-channel
     final data (target: `eiffel_outbound_fetch`,
     `eiffel_persistent_counter`).

2. **Continuation And Pipeline Sugar** — *landed (first form)*

   Reduce ceremony for linear protocols without pretending handlers are async.

   Shipped:

   - per-call-kind reply aliases (`TcpConnectReply`, `JournalAppendReply`,
     etc.) re-exported from `tina-runtime`. Isolate enums spell the call
     kind by name instead of `Result<X, CallError>`;
   - `docs/tina-user-guide/16-continuation-and-pipeline-patterns.md` is
     the blessed shape for pipeline + list-processing isolates and names
     the four anti-patterns;
   - `eiffel_outbound_fetch` (TCP client) and `eiffel_persistent_counter`
     (list/journal) converted to the aliases.

   Deliberately not shipped: a `pipeline!` macro, a `for_each` helper, or
   anything that hides per-step trace truth.

   Problem:

   - common protocols spell `Begin -> Connected -> Wrote -> Read -> Closed`;
   - "process this list one item at a time" becomes hand-rolled recursion;
   - every step is truthful but verbose.

   Requirements:

   - preserve one trace-visible runtime call per actual operation;
   - keep every timeout explicit;
   - keep `Full` / `Closed` / error outcomes visible;
   - work with ordinary `enum` message state machines;
   - do not require proc-macro magic for first form.

   Candidate shapes:

   - small aliases/helpers for continuation variants;
   - a bounded "for each item, call, accumulate, continue" helper;
   - documented canonical pattern if code helper is not yet clean.

   Proof:

   - convert one list-processing specimen and one TCP client specimen;
   - keep the before/after diff honest in the README.

3. **TCP Loop Helpers** — *landed (client-side first form)*

   Ship first-class helpers for boring TCP loops.

   Shipped: `tina_runtime::tcp_loops` module with `TcpWriteAll`,
   `TcpReadExact`, `TcpReadToEof` as client-side helper structs.
   Each helper expands to one `tcp_write` / `tcp_read` per
   `next_effect`/`advance` step, so partial progress remains a real
   trace event. `eiffel_outbound_fetch` converted; canonical pattern
   captured in chapter 16.

   Deferred: runtime-owned `CallInput::TcpWriteAll` / `TcpReadExact`
   / `TcpReadToEof`. That's a substrate change (betelgeuse driver +
   tina-sim) and the syntax `tcp_write_all(stream, bytes).reply(...)`
   from the original plan would need new pending-kind machinery.
   Captured as a follow-up; the helper form is the blessed shape for
   now.

   Problem:

   - user code repeats write-all, read-exact, and read-to-EOF loops;
   - hiding the loop in driver magic would lose trace truth.

   Desired shape:

   ```rust
   tcp_write_all(stream, bytes).reply(...)
   tcp_read_exact(stream, len).reply(...)
   tcp_read_to_eof(stream, max).reply(...)
   ```

   Requirements:

   - capacity and max-byte limits explicit;
   - partial progress trace-visible;
   - close/cancel behavior matches existing TCP lane truth;
   - simulator and live runtime agree;
   - no hidden buffer growth.

   Proof:

   - runtime tests for partial write/read, EOF, close while pending, max limit;
   - update `eiffel_outbound_fetch` or another TCP specimen.

4. **Capacity Diagnostics And Reply-Slot Budgets** — *landed*

   Make mailbox sizing less mystical.

   Shipped: `tina_runtime::pressure` module with
   `PressureSummary::from_events(...)` (and matching
   `Runtime::pressure_summary()` / `ThreadedRuntime::pressure_summary()`
   accessors) plus a `MailboxBudget { incoming, replies }` type with
   listener/session/service/fanout presets. Chapter 6
   ("Boundedness And Overload") rewritten to walk the
   `total = incoming + replies` arithmetic and show the diagnostics
   API.

   Problem:

   > mailbox capacity = incoming messages + replies to outstanding work

   This is correct, but users experience it as "pick 16 and pray."

   Requirements:

   - clearer trace/terminal diagnostics when reply delivery fails because the
     requester mailbox is full;
   - role-based docs or presets for listener/session/service/fanout shapes;
   - consider a `MailboxBudget { incoming, replies }` registration shape if it
     makes the model clearer without lying;
   - no automatic overflow queue.

   Proof:

   - focused capacity tests pin diagnostics;
   - at least one specimen README shows the capacity arithmetic in plain words.

5. **Bounded Host Send Helpers**

   Make host/test sends less hand-rolled without hiding pressure.

   Problem:

   - tests and examples that drive a threaded runtime often write:
     `loop { match runtime.try_send(...) { Ok(()) => break, IngressFull => yield_now(), ... } }`;
   - the loop is honest, but repeated and easy to get subtly wrong;
   - a blocking helper would be nice, but only if it keeps the timeout and
     `Full`/`Closed` outcomes visible.

   Desired shape:

   ```rust
   runtime.send_blocking(addr, msg, timeout)?;
   runtime.send_retrying(addr, msg, timeout)?;
   ```

   Names are negotiable. Semantics are not.

   Requirements:

   - bounded wait only; timeout required or default config must be inspectable;
   - no hidden queue beside the runtime ingress;
   - returns typed `Sent`, `Full`/`Timeout`, `Closed`, and `WorkerStopped`
     outcomes as appropriate;
   - preserves message ownership on failure where possible, or documents when
     the threaded ingress cannot return it;
   - works for tests and small host drivers; not a replacement for isolate
     call/reply.

   Proof:

   - focused threaded-runtime tests for success, ingress full then success,
     timeout, closed/stale target, worker stopped;
   - replace at least one manual `IngressFull` retry loop in an Eiffel specimen
     or runtime test.

6. **Tiny Native HTTP Router** — *landed*

   Close the obvious HTTP ergonomics gap.

   Shipped:

   - `tina_http::Router` already had basic `(method, path) → fn`
     dispatch; added sugar (`.get`/`.post`/`.put`/`.delete`/`.patch`)
     and opt-in `method_not_allowed()` to distinguish 405 from 404;
   - new `StatefulRouter<S>` for the in-isolate case where routes
     mutate isolate state, with the same sugar surface;
   - `eiffel_native_http`'s `Counter::handle` converted from a
     `match (method, path)` block to a `StatefulRouter<Counter>`.

   Problem:

   - native HTTP now works;
   - boring services still hand-match `(method, path)`.

   Requirements:

   - small router helper over `HttpRequest` / `HttpResponse`;
   - keeps handlers as Tina isolates or explicit functions that call Tina
     isolates;
   - overload and timeout mapping stays visible;
   - no Tower/Axum clone;
   - no hidden body buffering.

   Candidate shape:

   ```rust
   HttpRouter::new()
       .get("/counter", ...)
       .post("/counter", ...)
   ```

   Proof:

   - update `eiffel_native_http`;
   - tests for not found, method mismatch, service full/timeout mapping.

7. **Bridge Specimen Rewrite** — *landed*

   Bring deferred bridge examples up to the specimens rule.

   Shipped: `eiffel_axum_counter` and `eiffel_ws_room` rewritten to
   the canonical specimen shape — `src/lib.rs` with shared
   types/scripted client, top-level `tokio_impl.rs` /
   `tina_impl.rs`, `main.rs` dispatcher, `tests/smoke.rs`. The
   `src/comparison/` harness directories are gone. Both still use
   the blessed `BridgeHost::new` / `register_bridge` /
   `drain_and_shutdown` lifecycle.

   Scope:

   - `eiffel_axum_counter`;
   - `eiffel_ws_room`;
   - maybe one RPC bridge example if 058 wants a specimen.

   Requirements:

   - no `src/comparison/` harness;
   - local `tokio_impl.rs` and `tina_impl.rs`;
   - `main.rs` dispatcher;
   - smoke tests only;
   - README discusses two-runtime cost, bridge lifecycle, and visible pressure;
   - use blessed bridge lifecycle and RPC bridge APIs.

8. **RPC Service Topology** — *deferred (runtime prerequisite)*

   Make the 058 topology sketch real enough for hot services.

   Investigated and deferred. The frontend-with-N-workers pattern that
   `PooledService` would implement requires an isolate to hold
   *multiple* pending `IsolateCall` continuations simultaneously: one
   per in-flight ServiceCall the pool is dispatching to a worker.
   Today `tina-runtime`'s `MessageCallContext` is held as a single
   `Option<MessageCallContext>` per isolate, so a pool frontend can
   only wait on one worker reply at a time. Building a "first form"
   that serializes through one-at-a-time would advertise as a pool
   while not actually pooling concurrent work.

   Two additional integration questions also still open:

   - Registry's address contract is `Address<ServiceCall, ServiceReply>`,
     i.e. SingleService-shaped. Any pool that uses the Registry-style
     wrapper-enum continuation pattern exposes `Address<PooledMsg,
     ServiceReply>` instead. Bridging that needs either an adapter
     hop or a Registry-level change.
   - `ShardedService` depends on the runtime's sharded primitives
     (053).

   Action: keep the existing sealed-stub `PooledService` /
   `ShardedService` types (they still document the planned shape and
   prevent runtime panics from premature use). The unblocking work
   is "let an isolate hold N pending IsolateCall continuations" —
   that's the right runtime-level rock to plan next.

   Requirements:

   - implement `PooledService` first form;
   - implement or stub-with-tests `ShardedService` depending on 053 readiness;
   - registry API remains name to address;
   - capacity/pressure semantics explicit per topology;
   - `Full`, `Closed`, `Timeout`, partial failure remain visible.

   Proof:

   - unit tests for pool admission and shutdown;
   - Eiffel RPC follow-up if the example teaches something new.

9. **Pressure Report Convention** — *landed*

   Give pressure runners one small vocabulary without reviving the harness.

   Shipped:

   - `tina_runtime::PressureReport` + `format_pressure_line(...)`
     produce the canonical `pressure side=... accepted=N full=N
     closed=N timeouts=N other=N [rss_peak_kb=N] exit=...` line;
   - `eiffel_real_io_chat` opts in and prints the line per side;
   - `eiffel_cpu_run` captures the target's stdout, intercepts
     `pressure ...` lines, and re-emits them tagged with the run
     label; non-pressure lines pass through unchanged;
   - `docs/tina-user-guide/17-pressure-report-convention.md` is the
     blessed shape and explains why this is a convention, not a
     framework.

   Requirements:

   - local reports may expose `accepted`, `full`, `closed`, `timeouts`,
     `other`, `rss_peak`, `exit`;
   - no shared driver that forces implementation shape;
   - runners consume printed key/value lines when present and pass through when
     not.

   Proof:

   - update one pressure-capable specimen plus `eiffel_cpu_run` or
     `eiffel_mem_run`;
   - README says this is a convention, not a framework.

## Suggested Order

1. Typed isolate result waiters.
2. Capacity diagnostics and reply-slot budget docs.
3. Bounded host send helpers.
4. TCP loop helpers.
5. Continuation/pipeline sugar.
6. Tiny HTTP router.
7. Bridge specimen rewrite.
8. RPC pooled service.
9. Pressure report convention.

Reasoning:

- result waiters, capacity diagnostics, and host send helpers delete the most
  side-channel / host-driver code;
- TCP/pipeline helpers reduce repeated protocol ceremony;
- HTTP/router and bridge examples are user-facing polish;
- RPC topology is important but can depend on 053 if needed;
- pressure reports matter most once round-2 pressure starts.

## Required Proof

- `examples/FINDINGS.md` current action list maps to this phase.
- Each rock has at least one focused test in the owning crate.
- At least three Eiffel specimens get shorter or clearer without losing
  explicit capacity/timeout/failure.
- No helper introduces an unbounded queue or hidden retry.
- Docs show the blessed shape and retire the old workaround.

## Done Means

- A normal Tina service needs fewer side channels.
- Linear I/O code is less noisy.
- Capacity failures are easier to diagnose.
- Host drivers do not hand-roll `IngressFull` retry loops.
- Native HTTP has a small routing story.
- Bridge examples show the current blessed bridge shape.
- The next Eiffel pressure round can focus on behavior under load, not old
  setup ceremony.
