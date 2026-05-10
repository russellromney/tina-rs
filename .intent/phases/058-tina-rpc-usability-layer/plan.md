# Phase 058: Tina RPC Usability Layer

## Goal

Make `tina-rpc` feel like something a normal service author can use.

052 proved the bones: framed request/reply, bounded in-flight work, client
timeouts, server-reported wire errors, simulation, replay, and Specimen pressure.

058 adds the skin:

> trait method in. typed request out. bytes stay hidden. pressure still visible.

Big rule for every PR in this phase:

> macro may hide bytes. macro may not hide backpressure.

Near-grug:

> user write service. macro make byte soup. Tina still say full when full.

## Baseline

Already exists in 052:

- length-prefixed frame codec;
- server-side `Connection` isolate;
- `Registry` service router;
- byte-shaped `ServiceCall` / `ServiceReply`;
- client isolate with request ids and local deadlines;
- JSON encoding adapter;
- deterministic simulator coverage;
- Specimen RPC comparison showing wire-visible `Full`.

Pain observed:

- service authors string-match method names;
- service authors manually decode/encode bytes;
- client authors route `ClientResultMsg` through their own response mailbox;
- topology question is still open: one service isolate, a pool, or sharded set;
- retry and tracing are common needs but easy to make dishonest if hidden.

## Non-Goals

- No gRPC.
- No HTTP/2.
- No protobuf codegen phase.
- No streaming request or response bodies.
- No bidirectional streaming.
- No auth, TLS policy, or service identity.
- No remoting or clustering.
- No automatic service discovery.
- No exactly-once or idempotency promise.
- No hidden retry by default.
- No hidden unbounded queue to make the API feel nicer.
- No `async` facade inside Tina-native code. `async` belongs at bridges.

## Convenience Limit

Convenience is good when it removes byte boilerplate.

Convenience is too far when it hides one of Tina's truths:

- capacity;
- timeout;
- full;
- closed;
- local vs wire outcome;
- service topology;
- retry policy;
- serialization size limits;
- trace identity.

Rules:

- Defaults may exist, but every hidden default must be inspectable.
- Generated code must preserve `Full` / `Closed` / `Timeout` outcomes.
- Retrying must be explicit at the call site or wrapper construction.
- Service topology must be chosen explicitly: single, pool, or sharded.
- Generated services must make mailbox capacity inspectable at construction.
- Tokio `async` convenience must live in bridge crates, not the Tina-native core.
- The raw byte API remains available and tested as the semantic truth.

Grug boundary:

> macro may hide bytes. macro may not hide backpressure.

## Rocks

1. **Service Topology Sketch**

   Decide what a service is before macros ossify it.

   Requirements:

   - document three supported shapes:
     - single service isolate;
     - bounded service pool;
     - sharded service set;
   - say what each shape means for mailbox capacity and pressure;
   - say how the registry routes to each shape;
   - say which shape is first-form implementation;
   - leave room for 053 sharded primitives without coupling too hard.

   Preferred first form:

   - macro emits typed handler/dispatch code;
   - adapters wrap that handler into single, pool, or sharded service shapes;
   - first PR may implement single only, but it must ship code/doc stubs for
     pool and sharded adapters so the adapter point is real, not vibes;
   - sketch the adapter trait or helper shape before the macro lands.

   Sketch target:

   ```rust
   // Names are negotiable. The adapter point is not.
   trait ServiceAdapter<H, S>
   where
       S: tina::Shard,
   {
       type Isolate: tina::Isolate<
           Message = ServiceCall,
           Reply = ServiceReply,
           Shard = S,
       >;

       fn wrap(handler: H, config: ServiceAdapterConfig) -> Self::Isolate;
   }
   ```

   The important thing: generated handler code should be reusable by
   `SingleService`, `PooledService`, and `ShardedService` adapters.

2. **Typed Service Dispatch Core**

   Build the boring runtime-free pieces before proc-macro magic.

   Requirements:

   - typed method table or generated dispatch function shape;
   - method name to handler dispatch;
   - request decode with payload size cap;
   - response encode with payload size cap;
   - typed mapping to `ServiceReply::UnknownMethod`, `Decode`, `Internal`;
   - explicit domain-error mapping shape, not accidental `Internal`;
   - tests without macros where possible.

   This core should be simple enough that the macro is mostly syntax.

3. **`#[tina_rpc::service]` Macro First Form**

   Hide byte soup for service authors.

   Target shape:

   ```rust
   #[tina_rpc::service(encoding = Json)]
   pub trait Billing {
       fn charge(&mut self, amount: Cents) -> Result<Receipt, BillingError>;
       fn refund(&mut self, id: ReceiptId) -> Result<(), BillingError>;
   }
   ```

   Requirements:

   - generate a Tina-compatible service wrapper using `ServiceCall` /
     `ServiceReply`;
   - string method dispatch generated by macro, not user-written;
   - JSON decode/encode generated by macro;
   - unknown method maps to `ServiceReply::UnknownMethod`;
   - decode failure maps to `ServiceReply::Decode`;
   - handler domain errors do **not** silently collapse to
     `ServiceReply::Internal`;
   - require an explicit error mapping choice:
     - encode domain errors as part of the reply payload; or
     - map selected errors to server-reported RPC errors through a user-provided
       conversion;
   - if no mapping is supplied, macro compile error is better than silent
     `Internal`;
   - service mailbox capacity is supplied by adapter config or an explicit macro
     attribute, and generated docs show where it lives;
   - generated docs say what capacity and timeout still mean.

   First form can be sync trait methods only.

4. **Typed Client Stub First Form**

   Generate client request builders that preserve the existing client isolate.

   Requirements:

   - typed methods construct service/method/payload requests;
   - caller still supplies or configures deadline;
   - caller still sees `Full`, `Timeout`, `ConnectionClosed`, server error;
   - generated client does not own hidden retry;
   - request ids remain owned by `Client`;
   - tests cover ok, unknown method, decode, full, timeout.

   Native Tina shape may still be message/response-address based. The typed
   stub removes payload/method boilerplate, not Tina's actor model.

5. **Tokio Bridge Async Client**

   Add adoption-friendly `await` at the edge.

   Requirements:

   - decide home up front: prefer `tina-rpc-tokio` if the surface is mostly RPC,
     otherwise explicitly document why it lives in `tina-tokio-bridge`;
   - uses existing `tina-rpc::Client` underneath;
   - one request maps to one bounded reply slot / oneshot;
   - timeout, full, closed, server error map into typed async error;
   - dropped Tokio future follows 052 cancellation semantics:
     - no cancel frame;
     - server may complete;
     - late reply is discarded;
     - in-flight slot is released when the underlying client observes reply,
       timeout, close, or shutdown;
   - dropped future must not leak a reply slot forever;
   - no Tokio runtime inside Tina-native service state;
   - docs say this is an edge adapter.

   Target edge shape:

   ```rust
   let receipt = billing
       .charge(amount)
       .deadline(Duration::from_millis(250))
       .await?;
   ```

6. **Explicit Retry Wrapper**

   Make retry easy without making pressure disappear.

   Requirements:

   - no retry by default;
   - retry policy type with fixed delay and jittered delay first forms;
   - retry only for configured outcomes (`Full`, maybe `Timeout`);
   - attempts are bounded;
   - each retry is trace-visible;
   - docs warn that retry changes load behavior.

   Prefer a wrapper/helper around typed client calls before baking retry into
   the core `ClientRequest`.

7. **RPC Tracing Fields**

   Make RPC useful in existing ops tools.

   Requirements:

   - service name;
   - method name;
   - request id as trace correlation, not a metrics label;
   - result kind;
   - full/closed/timeout distinction;
   - retry attempt when present;
   - bridge async request correlation.

   Warning:

   - never put high-cardinality request id into Prometheus-style labels;
   - request id belongs in spans/events/log fields, not aggregate metric
     dimensions.

   This can build on future tracing work if present. If tracing is not ready,
   leave a small adapter behind a feature or a documented follow-up.

8. **Registry Ping / Readiness First Form**

   Add the tiny built-in health probe users will otherwise reinvent badly.

   Requirements:

   - explicit registry or service readiness method;
   - no hidden service discovery;
   - bounded call path;
   - reports full/closed/timeout distinctly;
   - useful from the Tokio bridge.

   Keep it small. This is a liveness/readiness probe, not admin RPC.

9. **Specimen Typed RPC Comparison**

   Update the RPC comparison to use the typed surface.

   Requirements:

   - keep raw 052 comparison or one low-level test so byte API stays proved;
   - add typed service implementation;
   - add typed client or bridge client variant;
   - compare boilerplate before/after in README;
   - update findings with remaining pain.

## Required Proof

- A service author can expose two typed methods without manual byte decode,
  byte encode, or method string matching.
- Generated service preserves `UnknownMethod`, `Decode`, `Internal`, `Full`,
  `Closed`, and `Timeout` semantics.
- Typed client preserves local-vs-wire outcome distinction.
- Async bridge awaits one typed result without adding unbounded queues.
- Dropping an async bridge future does not leak a request slot and does not
  invent a cancel frame.
- Retry is opt-in, bounded, and trace-visible.
- Service topology docs exist before macro API is declared stable.
- Service mailbox capacity is explicit and inspectable in generated service
  construction.
- Domain errors do not silently collapse to `Internal`.
- Request id is trace correlation only, not a metric cardinality bomb.
- Specimen typed RPC example runs live.
- Simulator still covers the raw protocol path.

## Done Means

- `tina-rpc` still has honest bounded bones.
- Common users stop touching byte soup.
- Bridge users can `await` RPC from Tokio edges.
- Retry and tracing help production use without lying.
- The docs clearly say where convenience stops.
