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
- No North Sea / `io_uring` work.
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

1. **Typed Isolate Result Waiters**

   Build a typed host-visible result path for isolates.

   Problem:

   - `observe_isolate_complete(addr)` says an isolate stopped;
   - examples still use `Arc<Outcome>`, atomics, mpsc channels, or driver
     isolates to retrieve final app data.

   Desired shape:

   ```rust
   let done = runtime.observe_result(addr);
   let value: T = done.wait(timeout)?;
   ```

   Requirements:

   - works for explicit-step and threaded runtimes;
   - waiter registration is bounded;
   - timeout is caller-visible;
   - dropped waiter cleanup is bounded;
   - stopped-without-result, closed, runtime-shutdown, and timeout outcomes are
     typed;
   - trace still records isolate stop/completion truth;
   - no unbounded result registry.

   Proof:

   - focused runtime tests for success, timeout, dropped waiter, isolate stops
     without result, runtime shutdown;
   - update at least two Eiffel specimens that currently carry side-channel
     final data.

2. **Continuation And Pipeline Sugar**

   Reduce ceremony for linear protocols without pretending handlers are async.

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

3. **TCP Loop Helpers**

   Ship first-class helpers for boring TCP loops.

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

4. **Capacity Diagnostics And Reply-Slot Budgets**

   Make mailbox sizing less mystical.

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

5. **Tiny Native HTTP Router**

   Close the obvious HTTP ergonomics gap.

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

6. **Bridge Specimen Rewrite**

   Bring deferred bridge examples up to the specimens rule.

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

7. **RPC Service Topology**

   Make the 058 topology sketch real enough for hot services.

   Requirements:

   - implement `PooledService` first form;
   - implement or stub-with-tests `ShardedService` depending on 053 readiness;
   - registry API remains name to address;
   - capacity/pressure semantics explicit per topology;
   - `Full`, `Closed`, `Timeout`, partial failure remain visible.

   Proof:

   - unit tests for pool admission and shutdown;
   - Eiffel RPC follow-up if the example teaches something new.

8. **Pressure Report Convention**

   Give pressure runners one small vocabulary without reviving the harness.

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
3. TCP loop helpers.
4. Continuation/pipeline sugar.
5. Tiny HTTP router.
6. Bridge specimen rewrite.
7. RPC pooled service.
8. Pressure report convention.

Reasoning:

- result waiters and capacity diagnostics delete the most side-channel code;
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
- Native HTTP has a small routing story.
- Bridge examples show the current blessed bridge shape.
- The next Eiffel pressure round can focus on behavior under load, not old
  setup ceremony.
