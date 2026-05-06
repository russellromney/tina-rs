# Phase 048: Native HTTP Service Stack

## Goal

Make Tina speak HTTP itself.

Today Tina HTTP services are really Tokio edge services with Tina behind them.
That is useful, but it is not the big goal.

048 answers:

> Can a small HTTP/1.1 service run on Tina without Tokio owning the server edge?

This phase starts with native HTTP/1.1, not HTTP/2, not gRPC, not Tower
middleware, not a web framework.

Near-grug:

> Parse request. Run isolate. Write response. Shed load when full. Shutdown
> clean. No Tokio edge.

## Slices

The phase has 12 rocks. They do not all need to land together. Implementing
them as one undifferentiated chunk risks scope sprawl and a "first form"
framing that loses teeth. The phase ships in three ordered slices, each
testable on its own:

| Slice | Rocks | Closes |
|---|---|---|
| **048a — server first form** | 1, 2, 3, **5a**, 6, 7, 8, 12 | The headline question: native HTTP server without a Tokio edge. Listener, connection, parser, service dispatch, *typed-mapping overload* (5a), shutdown, example, DST, observability. |
| **048b — HTTP client + pool + 5b** | 9, 10, **5b** | Outbound shape, plus rock 5b: max connection count, per-connection metrics, deterministic wire-level 503. The pool is a general primitive 055 native DB also needs and naturally produces overload scenarios. |
| **048c — streaming and routing** | 4, 11 | Sharp-edges polish. Defer until 048a's first endpoint is proven. The plan already says rock 4 must not swallow the phase. |

Rock 5 is **split** into 5a (typed-mapping overload visibility, shipped
in 048a) and 5b (admission limits + metrics + deterministic wire-level
503/Full coverage, shipped in 048b alongside the connection pool). The
split is honest about the limitation that constructing
`CallOutcome::Full` reliably on a single shard requires either a
delayed-reply primitive Tina does not yet expose, or the 048b
connection pool's natural overload scenarios. 048a does ship
deterministic wire-level coverage of the *Timeout* (504) path via a
service that never replies — that is the integration-test arm of 5a.

048a closes the phase's headline. 048b lets the slice eat its own dogfood
(client fetches from server). 048c is incremental.

## User-Facing Shape (First Form)

Two reasonable surfaces:

- **A: builder/router.** `HttpServer::builder().bind(addr).route(GET, "/x", addr).build()`.
  Familiar to axum users, fewer lines per service.
- **B: assemble-it-yourself.** User registers a `HttpListener` isolate that
  knows the bind address and the dispatcher address; the listener spawns
  one connection isolate per accepted socket; user writes their service
  isolate as ordinary Tina code that handles `HttpRequest` messages and
  replies with `HttpResponse`.

First form ships as **B**. It preserves the property that Tina services are
ordinary Tina isolates with no framework magic, matches how `tcp_echo.rs`
already reads, and exposes the same `call(service, req, timeout).reply(...)`
ceremony users already know from the keyspace example.

A future 048-sugar slice can add a builder/router on top. The first slice
must not foreclose that path, but also must not block on it.

## Crate Placement

Native HTTP lives in a new workspace crate **`tina-http`** alongside
`tina-supervisor` and `tina-mailbox-spsc`, depending on `tina` and
`tina-runtime`. This keeps `tina-runtime` focused on the substrate and
makes the HTTP work an opt-in dependency for consumers who do not need
it.

## Coordination With 047

048a runs in parallel with 047 (Eiffel ergonomics harvest). The overlap
surface is small but real:

- 047's `tcp_write_all` / `tcp_read_to_eof` helpers are load-bearing for
  HTTP server response writes. Until 047 ships them, 048a hand-rolls a
  small write-all loop *inside the connection isolate* — same shape as
  `tcp_echo.rs` already does. **Do not publish a `tina-http::tcp_write_all`
  helper.** When 047's public helper lands, the swap is delete-the-local
  + call-the-public.
- 047's default mailbox factory simplifies the example boilerplate. Until
  it lands, the example carries the standard 40-line `Mailbox` + factory
  copy. When 047 ships it, the example shrinks. No API freeze either way.
- 047's stable trace fingerprint helps DST in rock 8. Until it lands, the
  DST work hashes the `Debug` projection — same shape as
  `eiffel_replay_dst`. When 047 ships it, swap.

Concrete file-conflict surface between 048a and 047: only the workspace
`Cargo.toml` `members = [...]` line where 048a registers `tina-http`.
Trivial merge.

## Baseline

Already exists:

- runtime-owned TCP bind/accept/read/write/close;
- isolate call timeouts;
- observed send pressure;
- shutdown reports;
- deterministic simulator;
- bridge/Axum comparison showing why bridge ergonomics are not enough;
- outbound fetch comparison showing raw TCP client loops are possible but
  clunky.

Expected 047 helpers, if landed before or during this phase:

- default mailbox factory;
- host observation handles;
- stable trace fingerprint;
- TCP write-all/read-to-eof or lower-level loop helpers;
- bridge lifecycle docs, as contrast.

048 may prototype before 047 is complete, but must not freeze bad HTTP API
around missing 047 primitives.

Use boring HTTP brain where it helps:

- prefer `httparse` or a similarly boring sync parser for HTTP/1.x bytes;
- prefer the `http` crate for `Request`, `Response`, headers, methods, and
  status codes if it fits;
- Tina still owns sockets, connection isolates, buffers, backpressure, service
  dispatch, trace, and shutdown.

Do not use a library that owns the socket, blocks on `Read`/`Write`, spawns
threads, owns a pool, or hides buffers. Borrow parser brain. Do not borrow
server body.

## Non-Goals

- No HTTP/2.
- No gRPC.
- No TLS termination unless the current Tina TLS rail makes it cheap and
  honest.
- No Tower clone.
- No Axum clone.
- No full routing framework.
- No native DB driver.
- No `io_uring` requirement. Portable backend first; North Sea can accelerate
  later.
- No broad production-ready HTTP claim.
- No HTTP/1.1 completeness claim.
- No pipelining in the first form unless explicitly chosen after the basic
  server works.
- No `Expect: 100-continue` in the first form.
- No chunked request bodies in the first form unless explicitly chosen. Start
  with `Content-Length` and bounded chunks.
- No transparent compression.

## Rules

- Native means Tina owns listener, connection, read, parse, write, close, and
  shutdown.
- Backpressure must surface as typed busy/full/closed/timeout outcomes.
- Request body streaming must not require buffering whole bodies by default.
- Response body streaming must preserve write backpressure.
- Parser failures return typed HTTP errors and trace events.
- Keep API small. A user should be able to read all public HTTP types in one
  sitting.
- If implementing HTTP reveals missing core primitives, record them against
  047 instead of hiding them inside HTTP code.

## Rocks

1. **HTTP/1.1 Parser And Framing**

   Build or select a small HTTP/1.1 parser path that fits Tina.

   Requirements:

   - request line;
   - headers;
   - `Content-Length`;
   - keep-alive / close decision;
   - bad request handling;
   - header size limit;
   - body size or streaming policy;
   - explicit unsupported response for first-form features such as pipelining,
     chunked request bodies, and `Expect: 100-continue`;
   - no unbounded header/body accumulation.

   Prefer a proven parser crate if it fits the model. Do not write a heroic
   parser unless needed.

2. **Connection Isolate**

   One connection is one isolate or one small isolate family.

   Requirements:

   - owns stream id;
   - owns read buffer;
   - parses complete requests;
   - dispatches to service isolate or handler isolate;
   - writes response;
   - supports keep-alive where simple;
   - closes on protocol error, overload, shutdown, or peer close;
   - all mailbox capacities explicit.

3. **Service Handler Shape**

   Define the smallest useful app surface.

   Candidate shape:

   - request message contains method, path, headers, body handle/chunks;
   - service replies with status, headers, and body;
   - call timeout maps to HTTP timeout or 503;
   - overload maps to 503 or 429 by policy;
   - handler remains normal Tina isolate code.

   Do not make routing clever yet. A simple match on method/path is enough.

4. **Streaming Bodies**

   Add the first bounded chunk body story needed for real services.

   Do not let body machinery swallow the phase. First make one boring server
   work. Then prove bodies larger than one TCP read can move through bounded
   chunks.

   Requirements:

   - bounded chunk reads;
   - visible backpressure between connection and service;
   - `Content-Length` request body may be consumed incrementally;
   - response body may be produced incrementally or sent from bounded chunks;
   - upload/download tests cover bodies larger than one TCP read.

   If full streaming is too large, land the minimal body handles and document
   the remaining limit honestly.

5. **Load And Overload Semantics**

   HTTP must show Tina's reason for existing.

   Requirements:

   - bounded listener/session/service mailboxes;
   - max connection count or visible admission pressure;
   - service full maps to typed HTTP response or connection close by policy;
   - slow reader does not grow unbounded response buffers;
   - slow request body sender does not starve the service;
   - metrics/report line includes accepted/full/closed/timeouts.

6. **Graceful Shutdown**

   Requirements:

   - stop accepting new connections;
   - drain or reject in-flight requests by policy;
   - close idle keep-alive connections;
   - report pending work in terminal report;
   - no `Arc::try_unwrap` bridge dance because no bridge is involved.

7. **Native HTTP Example**

   Add a root-level comparison or example, probably
   `examples/eiffel_native_http`.

   It should include:

   - Tina native HTTP server;
   - Tokio/hyper or axum reference server;
   - same simple endpoint behavior, at minimum one boring `GET /counter` or
     `POST /echo`;
   - same report format;
   - overload scenario that can later run under CPU/memory wrappers.

8. **DST And Replay**

   Add simulator proof for the HTTP state machine where feasible.

   Required:

   - parse good request;
   - parse bad request;
   - service full;
   - slow body or partial body;
   - shutdown while request in flight;
   - saved seed for at least one interesting interleaving.

9. **Native HTTP Client First Form**

   Build the matching outbound shape.

   Requirements:

   - Tina owns `tcp_connect`, write, read, timeout, close;
   - request uses the same `http`/serialization shape as server where practical;
   - response head parsed with the same boring parser path;
   - `Content-Length` body first;
   - timeout/full/closed/error outcomes are typed;
   - comparison against a small Tokio/reqwest or hyper client reference.

   No reqwest inside Tina-owned client. Reqwest belongs in bridge/adapters.

10. **Bounded Connection Pool Primitive**

   HTTP client and DB adapters both need this.

   Requirements:

   - fixed pool capacity;
   - bounded waiter/admission policy or no waiter queue;
   - acquire timeout;
   - full/closed outcomes;
   - idle reuse;
   - bad connection drop;
   - shutdown closes idle and in-flight connections visibly.

   This may live as a general Tina primitive if it is not HTTP-specific.

11. **Tiny Routing Shape**

   Not Axum. Not Tower. Just enough grug routing.

   First shape:

   - method + static path maps to service address or handler;
   - route miss gives 404;
   - service full gives typed busy response by policy;
   - no middleware empire;
   - path params only if the first examples prove they are needed.

12. **HTTP Observability And Bad Input Suite**

   HTTP should count pressure and reject bad bytes cleanly.

   Required counters/events:

   - accepted;
   - rejected_full;
   - parse_error;
   - header_too_large;
   - body_too_large;
   - handler_timeout;
   - response_written;
   - connection_closed.

   Bad input cases:

   - malformed request line;
   - huge headers;
   - unsupported transfer encoding;
   - missing `Content-Length` where required;
   - peer closes mid-request;
   - slowloris-ish partial header with timeout;
   - keep-alive close behavior.

## Required Proof

- Native Tina HTTP example runs without Tokio owning the server edge.
- HTTP parser/framing tests pass.
- At least one request/response happy path over real TCP.
- At least one boring endpoint works end-to-end (`GET /counter` or
  `POST /echo`).
- At least one overload path returns visible HTTP pressure.
- At least one body larger than one read works or the limit is explicitly
  documented.
- First-form unsupported HTTP features are documented and tested as rejected or
  closed cleanly.
- Graceful shutdown test proves terminal truth.
- Native HTTP client first form can fetch from the native server or a tiny
  reference server.
- Bounded pool primitive is either landed or explicitly split into a follow-up
  with the reason recorded.
- Bad input suite covers malformed head, huge headers, unsupported transfer
  encoding, and slow partial header.
- Docs say this is HTTP/1.1 first form, not a full web framework.

## Done Means

- Tina can host a tiny HTTP/1.1 service on its own runtime.
- The bridge remains useful for Axum/Tower ecosystem integration, but is no
  longer the only HTTP story.
- Native HTTP has enough shape to pressure future DB, HTTP client, connection
  pool, streaming, and North Sea work.
- Tina is closer to "as easy as Tokio for normal services," but still does not
  claim full Tokio ecosystem replacement.

## 048b Slice Design Notes

048b lands rocks 9 (HTTP client first form), 10 (bounded connection pool),
and 5b (wire-level admission overload — naturally constructible once the
pool exists).

### Shape decisions

**Client is service-shaped via `call(...).reply(...)`.** Earlier analysis
claimed `Effect::Reply` was usable only during the handler turn for the
current message. That was wrong: `continuation_context` propagates
through runtime-call `.reply(continuation)` chains
([tina-runtime/src/lib.rs:1952](tina-runtime/src/lib.rs#L1952),
[tina-runtime/src/lib.rs:2097](tina-runtime/src/lib.rs#L2097)),
demonstrated by `tina-runtime/tests/portable_service.rs` where a
durable-store worker defers its reply across a journal append. So
`HttpClient` can be a long-lived isolate that takes an `HttpClientMsg`
via `call`, kicks off `tcp_connect/read/write` continuations across many
turns, and finally `Effect::Reply`s the original caller. The user-side
call site is one expression:

```rust
call(http_client, HttpClientMsg::call(target, request), timeout)
    .reply(MyMsg::HttpReturned)
```

No fn-pointer mapper, no spawn-and-route-back, no generic over user
message type. The same pattern Tina uses everywhere else.

**Direct vs pooled stays visible.** Calling `http_client` directly does
one TCP connect per call. Calling `pool.Submit` adds bounded admission
control — when the pool's slot is busy, `Submit` returns
`Err(HttpClientError::PoolFull)` immediately rather than waiting. Both
paths are tested separately because their failure modes differ: direct
surfaces `Connect`/`Read`/`Write`/`Timeout`; pooled adds `PoolFull`.

**Bootstrap helpers stay runtime-neutral.** No
`spawn_http_listener(runtime, ...)` helper that takes a runtime by
reference — that couples to a specific runtime flavor (threaded vs local
vs simulator). Instead:

- `HttpServerConfig::dev() / ::pressure()` — Copy struct of limits +
  timeouts + mailbox capacities.
- `HttpListener::with_config(addr, service, config)` — 3-arg constructor
  that absorbs the config struct.
- User still calls `runtime.register_with_capacity(...)` +
  `try_send(Start)` themselves. Three lines instead of five; runtime
  coupling stays out of `tina-http`.

**Symmetric presets across server/client/pool.** `HttpServerConfig`,
`HttpClientConfig`, `PoolConfig` each ship `::dev()` and `::pressure()`
presets. Examples should never need to hand-roll a timeout/limit triple.

### Deferred to 048c (or later)

- JSON helpers (`Response::ok_json`, `req.json::<T>()`) — defer the serde
  dependency decision; ship `body_str()` / `with_text()` first and choose
  between feature-gated serde and a `tina-http-json` extension once the
  example demand is clear.
- Fluent server builder (`HttpServer::bind().route().spawn()`) — couples
  to rock 11 (routing), comes in 048c.
- Internal close/drain rename inside `connection.rs` — pure rename diff,
  defer until the names actually annoy us in code review.
- Per-connection client keep-alive — first form is one connection per
  request; pool reuse covers the keep-alive *use case* without the
  state-machine complexity. Add only if the example demands it.

### First-form pool scope

- **Capacity = 1.** Multi-slot pools require multiple `HttpClient`
  instances and a placement policy; that's an honest 048c slice once
  the call-shaped primitive proves out. Constructing a pool with
  `capacity != 1` panics.
- **No idle reuse.** Each `Submit` call results in a fresh
  `tcp_connect`. Idle reuse needs the client to hand the `StreamId`
  back to the pool on completion — natural in the call chain when we
  generalise the reply type, but a separate slice.
- **No waiter queue.** Submits arriving while the slot is busy return
  `Err(PoolFull)` immediately. Acquire-timeout and waiter queue come
  with multi-slot.

### Required proof for 048b specifically

- `HttpClient` fetches from the native server
  (`client_against_native.rs`) and from a stdlib `TcpListener` reference
  (`client_smoke.rs`).
- Bad-input client suite covers malformed response head, oversized
  headers, unsupported transfer encoding, missing `Content-Length`.
- Pool tests cover slot acquisition, immediate `PoolFull` when busy,
  and pass-through correctness against the native server.
- Wire-level 503 (`pressure_503.rs`) — service mailbox of capacity 1
  hammered by concurrent inbound TCP requests in threaded mode produces
  `CallOutcome::Full` at the connection isolate's call into the
  service, mapped to `503 Service Unavailable` over the wire.
- Paired comparison `examples/eiffel_outbound_http` — axum + reqwest
  on Tokio side, native `HttpListener` + service-shaped `HttpClient`
  on Tina side, same scripted endpoint sequence.

### Connection-isolate service-shape note

`HttpListener`'s service address remains `Address<HttpRequest,
HttpResponse>` — sync-reply, single-variant message. Multi-turn user
services (services that need to call upstream HTTP via the new
`HttpClient`) can't fit that signature directly because their handler
for `HttpRequest` would need to issue `.reply(continuation)` with a
continuation type matching the service's own message type, which is
fixed to `HttpRequest`. For 048b we leave the connection isolate's
service shape unchanged and document the limitation. A flexible
service shape — generic over user message/reply types with conversions
to/from HTTP types — is a 048c candidate.
