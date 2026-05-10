# 074 HTTP Body Streaming And Backpressure

## Status

- Done:
  - plan drafted after the native HTTPS slice;
  - Rock 0 audit (see findings below);
  - body-pressure counters (`BodyMetrics` + `BodyPressureReport`)
    wired through `HttpListener` and `HttpsListener`;
  - HTTP/HTTPS parity proofs and lifecycle/drain tests;
  - first-form streaming surface: `Content-Length` only on both
    sides, mid-body errors typed, parser-level body cap;
  - first specimen: `specimen_http_body_streaming` with
    in-flight body pinned at one chunk while Tokio side holds
    the whole `Vec`.
  - **Upward shift (this commit):**
    - `IterBodySource<S>` adapter — wraps any
      `Iterator<Item = Vec<u8>>` into a chunk source so the
      common case needs no custom `Isolate` impl.
    - Loud-API constructors: `HttpResponse::stream_known_length`
      (`Content-Length` framing) and `HttpResponse::stream_chunked`
      (`Transfer-Encoding: chunked` framing). Both are typed
      compile-time choices; there is no "guess a length" path.
    - Narrow chunked transfer-encoding implemented for the
      response side. Connection isolate frames each chunk as
      `size CRLF data CRLF` and writes `0 CRLF CRLF` on `Eof`.
      `encode_response_head` switches on `body.declared_length()`
      to pick `Content-Length` vs `Transfer-Encoding: chunked`.
      Body charge counts data bytes only, not framing overhead.
    - `streaming_v2` integration tests: chunked encoding wire
      correctness, iterator-source over both framings,
      mid-stream client close visible via `body_io_error_count`.
    - Specimen rewritten to use the blessed shape (`IterBodySource`
      + `stream_known_length`); a second route demonstrates
      `stream_chunked` for unknown-length.
- Open: a live-tick metrics emitter (timer-driven snapshot); the
  current `metrics.snapshot()` is already callable any time but
  there's no built-in periodic emit. Recorded as a follow-up
  observability slice rather than this phase.
- Deferred:
  - HTTP/2;
  - gRPC;
  - redirects, cookies, proxies;
  - ACME, mTLS, cert reload, SNI routing;
  - broad web-framework surface;
  - chunked transfer-encoding on the **request** side (server
    still rejects chunked requests as
    `UnsupportedTransferEncoding`); the response side now
    supports chunked emit;
  - chunked decoding on the HTTP/1 client (server-side
    chunked-emit only);
  - cancel signal from connection back to chunk source on wire
    failure (today the source is left idle; failure is visible
    via `body_io_error_count`);
  - live-tick metrics emitter (snapshot is live; there's no
    built-in periodic emit);
  - migrating body-pressure counters into the shared
    capacity-report shape when that lands.

### Rock 0 — Audit findings

**Request body storage (`tina-http/src/connection.rs`).**

- Buffered path is the default. `read_buf` accumulates head + body;
  `dispatch_to_service` truncates to `head_len + content_length`,
  drains the head, and hands the service a
  `HttpRequestBody::Buffered(Vec<u8>)`. Whole body is resident before
  dispatch.
- Streaming path is opt-in via
  `HttpLimits::inbound_stream_chunk_size = Some(N)`. `dispatch_to_service`
  fires after head parse; body bytes are pulled lazily by the service
  via `call(stream.source, HttpConnectionMsg::body_next(), timeout)`.
  The connection issues `tcp_read`/`tls_read` only when the buffer is
  empty and more body is owed — slow service applies real
  backpressure to the kernel.
- Per-pull chunk size is bounded by `inbound_stream_chunk_size` (cap
  on a single `Chunk` reply) and by `READ_CHUNK = 4096` (ceiling on
  any single `tcp_read`/`tls_read`).
- Per-pull timeout is the deadline on the service's outer
  `body_next()` call. For TLS that deadline is enforced at the
  runtime IO call too (`tls_read` takes `tls_io_timeout`); for TCP
  the runtime has no per-call deadline today, so a slow peer mid-body
  is bounded only by the service's call timeout, not by the IO call
  itself.

**Response body storage.**

- Buffered: `HttpResponseBody::Buffered(Vec<u8>)`. Head + body merged
  into one `pending_response`; `tcp_write` is reissued on partial
  acceptance until drained.
- Streaming: `HttpResponse::with_stream(status, ResponseStream {
  content_length, source })`. Pull-based via `ResponseChunkMsg::Next`
  calls. `Content-Length` framing — declared length pinned in the
  head, declared length truthful on the wire even if source
  under-produces (early `Eof` truncates wire body).
- Per-chunk write timeout: `stream_call_timeout = service_call_timeout`.
  TCP `tcp_write` has no per-call deadline; TLS `tls_write` uses
  `tls_io_timeout`.

**Existing body caps and what they measure.**

| Knob                                  | Measures                          |
|---------------------------------------|-----------------------------------|
| `HttpLimits::max_body_bytes`          | declared `Content-Length` ceiling |
| `HttpLimits::inbound_stream_chunk_size` | per-`Chunk` reply ceiling       |
| `READ_CHUNK = 4096`                   | per-syscall read ceiling          |
| `HttpsServerConfig::tls_io_timeout`   | TLS read/write/close per-call deadline |

No high-water tracking. No `body_full_count`. No `body_timeout_count`.
No body pressure report at all today.

**Errors that currently collapse into clean EOF.**

- Buffered request: a mid-body `Read(Err(_))` calls `begin_close()`,
  with no typed signal to the service — the service simply does not
  receive the dispatch. (Acceptable in first form because dispatch
  hasn't happened yet.)
- Buffered request: a mid-body `Read(Ok(empty))` is treated as a
  clean peer close and silently drops the request. **Truncation
  is invisible to the service.**
- Streaming request: mid-body IO error already routes to
  `RequestChunkReply::Error(CallError)` — service can distinguish
  truncation from clean `Eof` (post-068 round-5 fix).
- Streaming request: a mid-body `Read(Ok(empty))` (peer FIN before
  declared bytes arrived) replies `Eof` to the service, not `Error`.
  Service detects truncation by comparing `delivered` to declared
  length — but the call shape collapses peer FIN into clean Eof.
- Streaming response source `Full | Closed | Timeout` →
  `begin_close()` silently truncates the wire. No typed surface to
  the service that the wire died mid-body. Source isolate is left
  with no notice that the wire was abandoned.
- Streaming response `Wrote(Err(_))` → `begin_close()`. Source
  isolate is not notified the wire write failed.

**HTTP vs HTTPS transport differences.**

- Read/write/close dispatch is unified through `HttpTransport::{Tcp,
  Tls}`. Same isolate, same handlers.
- TLS path threads `tls_io_timeout` into every `tls_read`,
  `tls_write`, `tls_close`. TCP path passes nothing — TCP read/write
  have no per-call runtime deadline.
- Slow-loris guard covers head-read on both transports (via a
  `sleep` race). Body-read has no equivalent guard on TCP.
- Above the transport layer, dispatch, framing, and error mapping
  are identical.

**071 capacity reports.**

- 071 plan is open and unimplemented. No `CapacitySurfaceReport`,
  no `CapacityScopeReport`, no `MailboxBudget`. 074 ships its own
  small `BodyPressureReport` and adds a TODO to migrate to the 071
  shape when 071 lands.

### Framing decision

`Content-Length` framing only, on both request and response.

- Request side: chunked transfer-encoding stays rejected as
  `RequestParseError::UnsupportedTransferEncoding` (501 Not
  Implemented). Already enforced by the parser.
- Response side: the public response API only frames known length
  (`HttpResponseBody::Buffered(Vec<u8>)` and
  `ResponseStream { content_length, source }`). There is no
  unknown-length variant — callers cannot accidentally pretend-stream
  by buffering a `Vec` to compute its length. Building a streamed
  response without declaring length is a type error, not a runtime
  surprise.

We deliberately do not implement chunked encoding in first form.
Chunked is a real protocol, not a one-line addition; it deserves
its own slice. Today's narrow `Content-Length`-only contract is
honest about what we ship.

## Grug Truth

Headers are small. Bodies are where lies live.

```text
do not buffer whole body unless user asked
reader pulls chunks
writer sees slow peer
body cap says bytes-ish, not vibes
truncated body is error, not eof
HTTP and HTTPS same semantics
```

## Goal

Make Tina HTTP/1 bodies production-shaped under pressure.

068 proved the native HTTP/1 + HTTPS rails. This phase proves large and
slow bodies do not smuggle in hidden unbounded buffers.

First form:

- request body is demand-driven chunk reads;
- response body can stream bounded chunks;
- slow client/server pressure is visible as typed `Full` / `Timeout` /
  `Closed` / body error;
- clean EOF and truncated/error EOF are distinct;
- HTTP and HTTPS use the same body semantics;
- body limits show up in tests and docs;
- at least one specimen moves a large or slow body without buffering it
  whole.

## Non-Goals

- No HTTP/2 flow control.
- No gRPC.
- No broad transfer-coding feature sprawl. If chunked is needed for
  honest unknown-length streaming, implement the narrow chunked subset
  and test it. Otherwise require explicit `Content-Length`.
- No async stream abstraction.
- No hidden task that buffers the body for convenience.
- No "just Vec the whole response" as the copied path.
- No complete web framework.

## Rock 0: Audit Current Body Shape

Read current `tina-http` request/response body code and tests.

Answer:

- where does a request body live today?
- where does a response body live today?
- what caps exist?
- what caps are count vs bytes-ish?
- what errors collapse into EOF?
- what differs between TCP and TLS transport?
- where does the user see pressure?

Write the audit into this plan before coding.

## Rock 1: Request Body Pull API

The service should ask for the next body chunk when ready.

Candidate shape:

```rust
RequestBody::next_chunk(max_len, timeout).reply(Msg::BodyChunk)
```

or whatever matches current `tina-http` style.

Rules:

- no automatic full-body buffering;
- max chunk size is explicit;
- body read timeout is explicit;
- body framing is explicit: fixed `Content-Length`, chunked, or
  close-delimited;
- clean EOF is distinct from I/O/TLS/closed/truncated error;
- user can stop reading and close/drain visibly.

Proof:

- small body reads one chunk then EOF;
- large body reads many chunks;
- user stops early and connection cleanup is visible;
- mid-body transport error is not reported as clean EOF.

First form may support only fixed `Content-Length` request streaming if
that is what current `tina-http` can honestly prove. If chunked request
bodies are not implemented, reject them with a typed unsupported/body
framing error rather than buffering or pretending EOF.

## Rock 2: Streaming Response API

The service should be able to send a response body in chunks.

Candidate shape:

```rust
ResponseBody::start(status, headers)
ResponseBody::write_chunk(bytes, timeout).reply(Msg::Wrote)
ResponseBody::finish(timeout).reply(Msg::Finished)
```

Rules:

- each write is a runtime-owned effect/call;
- framing is explicit:
  - known length: set `Content-Length`, write exactly that many bytes;
  - unknown length: either implement narrow chunked response framing, or
    reject unknown-length streaming in first form;
- slow peer can timeout;
- closed peer is typed;
- write capacity is bounded;
- failed write does not pretend response finished.

Proof:

- many chunks reach a normal client;
- slow/non-reading client causes timeout or pressure;
- peer close surfaces typed close/write failure;
- no whole response buffer is required in the service isolate.

Do not choose "buffer the whole response to compute `Content-Length`" as
the copied streaming path. That is not streaming.

## Rock 3: Body Capacity Truth

Body pressure needs a report even if 071 has not landed yet.

First form can be local:

```text
request_body_current
request_body_high_water
response_body_current
response_body_high_water
body_full_count
body_timeout_count
```

If 071 is already available, use the 071 report shape. If not, keep the
local report small and mark the migration.

Rules:

- cap is explicit;
- high-water is recorded;
- `Full` says body cap, not generic full;
- report does not claim exact heap memory.
- charge on chunk admission/read/write;
- release on delivery/drop/close/cancel.

Proof:

- oversized request rejected with body-cap reason;
- streaming response under cap succeeds;
- pressure report names high water and full count.

## Rock 4: HTTP / HTTPS Parity

Body semantics must not depend on transport.

Run the same body proof over:

- plain HTTP;
- HTTPS.

Allowed differences:

- TLS name/cert/handshake errors;
- TLS transport error source.

Not allowed:

- HTTPS buffers more;
- HTTPS turns body error into EOF;
- HTTPS close leaks a stream resource;
- timeout vocabulary differs without reason.

## Rock 5: Lifecycle And Drain

Listener and connection shutdown must stay honest while bodies are in
flight.

Proof:

- stop listener while request body is being read;
- close connection while response body is being written;
- graceful drain lets admitted body work finish within budget;
- force close settles pending body reads/writes visibly;
- terminal report does not lose body pressure/resource truth.

## Rock 6: Specimen

Add or update one specimen.

Preferred shape:

```text
slow upload -> Tina service reads chunks -> computes digest/count
large response -> Tina service streams chunks -> slow client applies pressure
```

The README must compare against Tokio honestly:

- Tokio linear `AsyncRead` / `AsyncWrite` is shorter;
- Tina names each suspension and pressure point;
- Tina does not hide the body buffer;
- failure shape is visible.

Do not make a toy that only sends `"hello"`.

## Rock 7: Docs

Update user guide:

- when to use full body helper, if any;
- when to stream;
- what body framing is supported in first form;
- how body caps work;
- how clean EOF differs from truncated/error;
- how HTTP and HTTPS share the same body model;
- what is still not HTTP/2/gRPC.

Docs should say:

```text
Small body can be buffered if you choose.
Large body should be pulled/written in chunks.
Every chunk is a pressure point.
```

## Proof Targets

- Unit/parser tests for body framing edge cases.
- Runtime HTTP tests for request chunking.
- Runtime HTTP tests for streaming response.
- Slow-reader/slow-writer tests.
- Oversized-body rejection test.
- Mid-body transport error test.
- HTTP and HTTPS parity tests.
- Shutdown/drain with body in flight.
- Specimen smoke test.
- Clippy/fmt.

## Done Means

- A user can write an HTTP/1 upload/download service without buffering
  the whole body.
- Body pressure is typed and reported.
- Clean EOF, truncated body, timeout, closed peer, and cap full are not
  collapsed.
- HTTP and HTTPS behave the same above the transport layer.
- At least one specimen proves a large/slow body.
- HTTP/2/gRPC remain clearly deferred, not accidentally started.
