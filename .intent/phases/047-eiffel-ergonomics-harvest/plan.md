# Phase 047: Eiffel Ergonomics Harvest

## Goal

Take the repeated pain from `examples/FINDINGS.md` and turn it into small Tina
primitives.

Eiffel found the pain. 047 makes the pain smaller.

This is not "write more examples." It is:

> Make the existing examples less stupid to write while keeping Tina bounded,
> visible, and replayable.

At closeout, Eiffel examples should delete boilerplate, side channels, and
trace scraping. Tina must not buy that comfort with hidden queues, hidden
timeouts, hidden retries, or async-handler cosplay.

## Baseline

Already built:

- root-level `examples/` comparison suite;
- `examples/FINDINGS.md`;
- Tina user guide under `docs/tina-user-guide`;
- real I/O chat comparison;
- mini keyspace comparison;
- Axum bridge counter comparison;
- WebSocket room comparison;
- multiplexed client comparison;
- supervised worker comparison;
- persistent counter comparison;
- replay/DST comparison;
- outbound fetch comparison;
- graceful shutdown comparison;
- CPU and memory runner shells.

Recurring pain from Eiffel:

- every example hand-rolls the same mailbox factory;
- host code uses `Arc<Mutex<_>>`, atomics, and trace polling to learn facts the
  runtime already knows;
- replay fingerprints hash `Debug` strings;
- mailbox capacity hides reply-slot pressure;
- single-shard programs still need ceremonial shard types;
- `#[tina::isolate]` vs. `#[tina_runtime::isolate]` fails late and obscurely
  in simulator paths;
- TCP clients reimplement write-all and read-to-eof loops;
- ordered runtime calls require hand-rolled recursive helpers;
- bridge shutdown requires `Arc::try_unwrap` dances;
- threaded and explicit-step runtime surfaces differ in ways users can trip
  over.

## Non-Goals

- No native HTTP server. That is 048.
- No `io_uring` backend. That is 049/North Sea.
- No broad flow macro.
- No new unbounded observer queue.
- No hidden retry, hidden timeout, hidden load buffer, or hidden task.
- No change to Tina's core model: isolate handlers stay synchronous and return
  effects.
- No performance claim.

## Coordination

047 owns the current public Tina surface.

Parallel lanes may prototype HTTP or substrate work, but they do not stabilize
around missing 047 pieces until 047 lands or the contract is agreed.

If 048 or 049 discovers that 047 needs a different primitive, record it in this
phase review before changing core runtime meaning.

## Rules

- Repeated Eiffel pain earns helpers. One-off weirdness gets documented first.
- Every helper keeps capacity explicit.
- Every host observation path is bounded.
- Every failure remains typed or trace-visible.
- Any runtime fact exposed to host code must still be visible in the trace or
  terminal report where appropriate.
- Simulator and live runtime should agree on public meaning.
- Do not make examples prettier by hiding pressure.
- Rerun Eiffel after changes. The proof is deleted workaround code.

## Rocks

1. **Default Mailbox Factory**

   Ship a blessed bounded in-process `MailboxFactory`.

   It should be the obvious thing examples and small services use when they do
   not need a custom mailbox implementation.

   Requirements:

   - lives in an appropriate public crate/module;
   - capacity remains explicit at registration/spawn;
   - closed/full behavior matches Tina mailbox contracts;
   - examples can import it directly;
   - custom mailbox factories still work;
   - docs stop teaching copy-paste `Rc<RefCell<VecDeque<_>>>` boilerplate.

   Proof:

   - update Eiffel examples to use it;
   - remove duplicated mailbox factory code from comparisons where practical;
   - add focused tests for full, closed, capacity, and FIFO behavior.

2. **Mailbox Capacity Truth**

   Make reply-slot pressure explicit.

   Rule to document and test:

   > Runtime-call replies, isolate-call replies, and observed-send replies land
   > in the requester's mailbox.

   This means requester capacity is not only inbound traffic. It is also
   outstanding continuations.

   Requirements:

   - user guide names the rule plainly;
   - sizing examples for listener, connection, store, worker, and fanout
     isolate roles;
   - trace/diagnostic coverage for reply rejected because requester mailbox is
     full;
   - tests for `send_observed(...).reply(...)`, `call(...).reply(...)`, and at
     least one runtime call reply under requester mailbox pressure.

   Optional if the tests prove it is worth it:

   - separate reply-capacity budget;
   - role-based capacity helper;
   - clearer runtime error text for undersized requesters.

3. **Stable Trace Fingerprint**

   Stop replay examples from hashing `Debug`.

   Requirements:

   - add `RuntimeEvent::stable_hash()` or stable trace serialization;
   - document stability boundary: stable for Tina trace semantics, not a
     forever external wire format unless explicitly promised;
   - simulator replay example uses the stable path;
   - tests prove same seed gives same fingerprint and different seed can differ.

   No hidden nondeterministic fields in the fingerprint.

4. **Host Observation Handles**

   Give host code a typed, bounded way to observe runtime facts.

   Do not build a grand observability framework first. Start with the smallest
   handle shape that deletes Eiffel side channels:

   - a ready/bound-address waiter;
   - a stopped/completed waiter;
   - an operation-done waiter.

   Child-restarted and shutdown/terminal waiters may follow once the base shape
   is boring.

   Target facts:

   - service ready;
   - bound `SocketAddr`;
   - isolate stopped/completed;
   - child restarted;
   - specific operation completed;
   - shutdown/terminal truth available.

   Requirements:

   - handles are typed;
   - each handle has one clear fact it observes;
   - handles do not create secret unbounded queues;
   - host can wait with timeout;
   - closed/runtime-stopped path is visible;
   - trace remains source of audit truth;
   - simulator and live runtime meanings do not diverge silently.

   Proof:

   - remove at least one `Arc<Mutex<Option<SocketAddr>>>`;
   - remove at least one `Arc<AtomicBool>` done flag;
   - remove at least one trace-polling loop from an Eiffel example;
   - update the relevant Eiffel README to say which side channel disappeared;
   - add tests for ready, complete, dropped waiter, and runtime shutdown.

5. **Single-Shard Easy Path**

   Make small programs less ceremonial.

   Requirements:

   - provide a built-in single-shard type or allow isolate macros to omit
     `shard = ...` in single-shard contexts;
   - update docs and a few examples to use the easy path;
   - keep multi-shard explicit and unchanged;
   - simulator diagnostics plainly explain when an isolate using
     `#[tina::isolate]` cannot be registered because runtime calls are needed.

   No global mutable singleton shard.

6. **Sequence And TCP Helpers**

   Smooth common runtime-call loops without hiding semantics.

   Requirements:

   - document what `batch(...)` guarantees and does not guarantee for effects
     targeting the same runtime resource;
   - add small helper(s) for ordered runtime calls or batch iteration where the
     call sequence remains visible;
   - add `tcp_write_all` or equivalent helper;
   - add `tcp_read_to_eof` or equivalent helper;
   - partial write, EOF, close, timeout, and error paths stay typed.

   Proof:

   - outbound fetch gets shorter;
   - mux client avoids same-stream batch ambiguity;
   - tests cover partial writes and EOF.

7. **Bridge Lifecycle**

   Make Tokio/Tina embedding less weird while native HTTP is still future work.

   Requirements:

   - bridge host/handle gets one-call close/drain/shutdown path;
   - pending requests settle visibly;
   - traces and terminal reports remain available;
   - examples stop doing `Arc::try_unwrap` shutdown dances;
   - docs name the two-runtime shape;
   - docs name the sync-blocking-inside-Tokio footgun;
   - signal-handler coexistence caveat is documented.

   No claim that the bridge is native Tina HTTP.

8. **Runtime Surface Alignment**

   Reduce threaded-vs-explicit semantic drift.

   Requirements:

   - audit `ThreadedRuntime::try_send` vs. explicit-step `Runtime::try_send`;
   - align or loudly document full, closed/stale address, and message ownership
     behavior;
   - align or justify `supervise` return shapes;
   - add tests for threaded send to dead/stale address, ingress full, and retry
     after full where message ownership matters;
   - porting guide names any remaining differences.

   Same method name should not hide different user meaning.

## Required Proof

- `cargo fmt --all --check`.
- `cargo check --workspace`.
- `cargo check --manifest-path ...` for each `examples/eiffel_*`.
- Focused tests for new mailbox, observation, trace fingerprint, capacity
  rejection, bridge lifecycle, and runtime surface alignment.
- Eiffel examples updated to use the new primitives where they apply.
- `examples/FINDINGS.md` updated: resolved pain moves out of "what feels bad"
  or is marked resolved with the replacement primitive.

## Done Means

- Eiffel examples have less boilerplate and fewer side channels.
- The top repeated findings are either fixed or deliberately documented as
  model truth.
- Tina is easier to keep testing.
- Tina is not yet a native HTTP framework, not yet `io_uring`, and not yet a
  broad Tokio replacement. It is simply less sharp in the places Eiffel already
  proved were too sharp.
