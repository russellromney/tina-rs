# Phase 114: Framework Ergonomics After 110-113

## Status

- Ready.
- One PR.

## Goal

Make common Tina service code read like the user's job.

User words first:

- shared work: many callers wait for one result;
- request work: start work for this caller and answer later;
- bridge: install, close, drain, metrics, pressure;
- service stop: stop ingress, drain, close, report;
- pressure: show caps, current use, high water, full count.

Mechanism words stay, but should not be the first copied path:

- `DeferredReply`;
- `PendingReplies`;
- `PendingCancelableCallSet`;
- `WaitList`;
- `PoolLease`;
- `RuntimeEventKind`.

No hidden queues. No hidden callbacks. No fake async. Ordinary Tina
messages, bounded storage, typed outcomes, visible pressure.

## Why Now

Systems/specimens proved the same pain more than once:

- `system_cache_with_fill` and `ergonomics_playground`: "many callers
  wait on one key" is now real. `WaitList` works, but `SharedWork` is
  the better user-facing name.
- `system_job_queue`: cancelable request work is safe, but docs should
  say "one active request" vs "many active requests for the same key"
  before naming the tables.
- `system_metrics_shipper`: drain/batch helpers are good; docs should
  name the service workflow first.
- `system_webhook_relay`: bridge classifier and bridge author terms need
  one copied "write a bridge" path.
- `examples/FINDINGS.md` finding 26: runtime-call completions from
  `handle_call` must be pinned by a regression and then moved out of
  Active.
- Phase 112 review: `TraceProjection::http2_streams()`,
  `websocket_sessions()`, and `grpc_status()` read like fact-family
  filters. They must actually filter by family, not just alias "all
  protocol facts."

Do not do these here:

- scatter/gather builder;
- paired registration;
- shared scope registry;
- DST adapters for scopes/sinks;
- AWS bridge state-machine factoring;
- new protocol work;
- new request/event typestate.

## Rock 1: `SharedWork`

Add `SharedWork<K, R>` in `tina-runtime`.

It is a thin user-facing wrapper over `WaitList<K, R>`. Do not build a
new storage engine.

Exact first-form API:

```rust
pub struct SharedWork<K, R>;
pub struct SharedWorkTicket<K>;
pub enum SharedWorkError<'a, K, I: Isolate> {
    Full { key: K, call: RequestCall<'a, I> },
    KeyFull { key: K, call: RequestCall<'a, I> },
}
pub enum SharedWorkCallError<'a, K, I: Isolate> {
    NoCaller { key: K, call: CallContext<'a, I> },
    CrossShardUnsupported { key: K, call: CallContext<'a, I> },
    Full { key: K, call: CallContext<'a, I> },
    KeyFull { key: K, call: CallContext<'a, I> },
}
```

Required methods:

- `with_capacity(cap)`;
- `with_key_limit(cap, per_key)`;
- `named(name)`;
- `with_capacity_mode(mode)`;
- `wait(key, RequestCall) -> Result<SharedWorkTicket<K>, SharedWorkError>`;
- `wait_call(key, CallContext) -> Result<SharedWorkTicket<K>, SharedWorkCallError>`;
- `reply_one(ticket, reply)`;
- `reply_all_clone(key, reply)`;
- `reply_all_with(key, factory)`;
- `close_all_clone(key, reply)`;
- `close_all_with(key, factory)`;
- `drain_all_with(factory)`;
- `sweep()`;
- `snapshot()`;
- `capacity_report()`.

Add:

```rust
request_effect_after_shared_wait(ticket, effect)
```

Same role as `request_effect_after_wait_park`: the ticket proves caller
authority was consumed before a `RequestEffect` is returned.

Keep `WaitList` public. Rustdoc should say:

- use `SharedWork` for "many callers wait for one result";
- use `PendingReplies` when you own unrelated reply slots by id;
- use `WaitList` only when you need the lower-level name.

Do not make `SharedWork` start the upstream fill. The service still owns:

- fill-in-flight flag/set;
- stale fill generation;
- upstream call/timer;
- reply value;
- retry policy.

## Rock 2: Request Work Names

Document request work by user intent.

Two shapes:

- one active request per key: `PendingCancelableCallSet<K, Q, R>`;
- many active requests grouped by key: `CancelableWork<K, Q, R>`.

Do not rename or remove either type.

Add docs/examples that show:

- start cancelable work;
- admit before dispatch;
- `Full` / `KeyFull` replies immediately;
- completion removes by ticket;
- stale ticket does not remove newer work;
- stop drains/cancels and replies to every caller.

If `system_job_queue` correctly uses `PendingCancelableCallSet`, leave the
code. Its README must say why: it wants one active job per id. It must also
say retry/new-attempt semantics should use `CancelableWork`.

## Rock 3: Service Pattern Docs

Update:

- `docs/tina-user-guide/10-service-patterns.md`;
- `docs/tina-user-guide/11-ergonomics-checklist.md`;
- `docs/tina-user-guide/12-outcome-glossary.md`;
- any nearby page that still teaches a common workflow by leading with raw
  `PendingReplies`.

Add short copied boxes:

- many callers wait for one result -> `SharedWork`;
- one active cancelable request -> `PendingCancelableCallSet`;
- many cancelable requests per natural key -> `CancelableWork`;
- reply later to current caller -> `call.defer(...).reply(...)`;
- close/drain on stop -> existing drain/lifecycle helpers;
- write a bridge -> install, close, drain, metrics, pressure.

Each box must say:

- what the user is doing;
- helper to use;
- what stays explicit;
- what not to use.

Keep snippets small enough to copy.

## Rock 4: Bridge Author Copy Path

Digest phase 113 into one "write a bridge" path.

Show the job first:

1. config validates caps;
2. install starts worker and returns handles;
3. closer stops admission;
4. drain waits or reports timeout;
5. metrics count worker-terminal outcomes;
6. pressure reports capacity/full/high-water;
7. classifier names retry/fatal/success;
8. late result truth is documented.

Then map to:

- `BridgeInstall`;
- `BridgeCloser`;
- bridge-specific `close_and_drain`;
- metrics handle;
- pressure report;
- classifier.

No bridge framework blob. Bridge crates still own real messages and worker
state machines.

Docs to touch:

- bridge user guide page;
- one non-AWS bridge README or crate doc;
- one AWS bridge README or crate doc.

## Rock 5: Runtime Completion Regression

Close `examples/FINDINGS.md` finding 26 with a real test.

Test user truth:

```text
runtime_call_returned_from_handle_call_completes_as_event
```

Shape:

- service receives a call in `handle_call`;
- handler returns a runtime call effect;
- runtime call completion produces an internal event message;
- internal event is delivered to `handle`, not `handle_call`;
- original caller receives final reply;
- trace has no `UnsupportedMessage` rejection for the internal event.

Use a hermetic timer or observed-send call. Do not use network.

Run this on the live runtime path that would have caught
`system_realtime_rooms`. Add simulator coverage only if the same path is
cheap and clear.

After the test lands, move finding 26 to Closed and cite the test.

## Rock 6: System Rewrites

Rewrite exactly:

- `examples/systems/system_cache_with_fill`: use `SharedWork`;
- `examples/systems/ergonomics_playground`: use `SharedWork` for the
  cache-fill probe;
- `examples/systems/system_webhook_relay` README: point to the bridge
  author path.

Do not redesign those systems.

Each touched README must say:

- what got shorter or safer;
- what still stays explicit;
- which helper is now the copied path;
- remaining rough bits.

## Rock 7: Findings Cleanup

Update `examples/FINDINGS.md`.

Required:

- finding 21: close or rewrite around `SharedWork`;
- finding 26: move to Closed with the regression test;
- findings 32/33: point to bridge-author docs if the docs now answer the
  immediate copy-path issue;
- do not leave solved pain in Active.

If something is still real product work, keep it Active and make the
remaining build specific.

## Rock 8: Protocol Fact Projection Names

Make the projection helpers do what their names say.

Added:

```rust
TraceProjection::protocol_family(ProtocolFamily)
```

Rules (now enforced):

- `protocol_facts()` keeps every `FactObserved` event;
- `http2_streams()` keeps only `ProtocolFamily::Http2`;
- `websocket_sessions()` keeps only `ProtocolFamily::WebSocket`;
- `grpc_status()` keeps only `ProtocolFamily::Grpc`;
- non-matching `FactObserved` events are dropped silently the way
  `ignored` event kinds are;
- unknown runtime event kinds still fail closed.

The family check reads `RuntimeFact::Protocol(fact).family()`. No
debug-string parsing.

`TraceProjection::Projected` gains a `family_filter: Option<ProtocolFamily>`
field. Three existing call sites updated (two in
`tina-sim/tests/saved_replay_cases.rs`, one in
`tina-sim/tests/protocol_fact.rs`).

Docs updated:

- `docs/tina-user-guide/08-simulation-and-dst.md`;
- `docs/tina-user-guide/22-http-http2-grpc.md`.

Mixed-protocol projection test
`tina-sim/tests/protocol_fact.rs::http2_streams_keeps_only_http2_facts`
(plus four siblings) proves a single trace with HTTP/2 + WebSocket +
gRPC facts produces three distinct trace hashes under the three named
helpers and one fact-count per family.

## Tests

Unit tests for `SharedWork`:

- global full returns caller authority;
- per-key full returns caller authority;
- FIFO per key;
- stale ticket cannot reply newer waiter;
- closed caller sweep frees capacity;
- `snapshot()` omits closed waiters;
- `capacity_report()` reports live count, high-water, full count;
- `drain_all_with` replies every open waiter and clears capacity;
- zero capacity and zero per-key cap panic like `WaitList`.

Compile-fail tests:

- cannot forge `SharedWorkTicket`;
- cannot build `RequestEffect` from `noop()` with
  `request_effect_after_shared_wait` without a real ticket;
- docs compile for copied snippets.

End-to-end tests:

- `system_cache_with_fill`: burst N callers for one missing key; exactly one
  fill starts; every caller gets the same value; second fill generation
  ignores stale old completion;
- `system_cache_with_fill`: shared-work global full returns `Busy`;
- `system_cache_with_fill`: per-key full returns `Busy` if configured;
- `ergonomics_playground`: cache-fill probe still passes and uses no
  `WaitList` in copied service code;
- bridge docs/rustdoc pass with broken intra-doc links denied;
- runtime completion regression from Rock 5.
- protocol projection mixed trace: HTTP/2 + WebSocket + gRPC facts in one
  trace, and each named helper keeps only its family.

Run:

```sh
cargo fmt --all --check
cargo test -p tina-runtime shared_work --lib -- --nocapture
cargo test -p tina-runtime --test workflow_pending_ergonomics -- --nocapture
cargo test -p tina-runtime --test compile_fail -- shared_work
cargo test -p tina-runtime runtime_call_returned_from_handle_call_completes_as_event --lib -- --nocapture
cargo test -p tina-sim --test protocol_fact -- projection --nocapture
cargo test --manifest-path examples/systems/system_cache_with_fill/Cargo.toml
cargo test --manifest-path examples/systems/ergonomics_playground/Cargo.toml
cargo test --manifest-path examples/systems/system_webhook_relay/Cargo.toml
cargo clippy -p tina-runtime --lib -- -D warnings
cargo clippy --manifest-path examples/systems/system_cache_with_fill/Cargo.toml --all-targets -- -D warnings
cargo clippy --manifest-path examples/systems/ergonomics_playground/Cargo.toml --all-targets -- -D warnings
cargo clippy --manifest-path examples/systems/system_webhook_relay/Cargo.toml --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc -p tina-runtime --no-deps
```

## Done Means

- New code says `SharedWork` before `WaitList`.
- Docs say "start request work" before table names.
- Bridge authors see the normal job path before trait names.
- Protocol fact projection names match what they keep.
- Systems prove the names in real code.
- Findings match reality.
- No helper hides overload, cancellation, caller authority, timeout,
  pressure, or trace truth.
