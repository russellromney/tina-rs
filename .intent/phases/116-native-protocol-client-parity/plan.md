# Phase 116: Native Protocol Client Parity

## Status

- In progress. Wave A. Phase 115 and Phase 124 have landed.
- **Checkpoint 1 (server-only module split) landed.** The single-file
  `tina-http/src/http2.rs` is now `tina-http/src/http2/{mod,frame,
  headers,errors,server,target,client}.rs`. The frame/header/error
  helpers are internal (`pub(super)`) and shared between the server
  and the new native client. Public exports are unchanged for the
  server surface; existing HTTP/2 and gRPC server tests pass.
- **Native HTTP/2 client first form landed.** New typed
  `Http2Target::H2c { authority, addr }` / `Http2Target::Tls
  { authority, addr, server_name, trust_roots, alpn }`,
  `AlpnProtocols::h2()` / `none()`, and the
  `Http2ClientConnection<S>` isolate with bounded admission, typed
  `Http2ClientOutcome` (Replied, Full, Closed, FlowControlBlocked,
  Timeout, Reset, LocalCancel, ProtocolError, TlsAlpnMismatch), and
  outbound client-stream protocol facts. Live tests in
  `tina-http/tests/http2_client_live.rs` prove h2c GET, h2c POST,
  typed TlsAlpnMismatch, route-key shape, and method/path round-trip.
- **Native gRPC unary client landed.** `GrpcClient` over
  `Http2ClientConnection` with `GrpcTarget` and a typed
  `GrpcUnaryOutcome` (Ok / Status / Transport / Malformed); a non-OK
  status is the caller outcome, never a hidden success. Received status
  is emitted as `ProtocolFact::GrpcFinalStatusReceived` (trace tag 9,
  paired with `GrpcFinalStatusSent`). Live tests, two compile-fail
  proofs, specimen rewritten onto the native client (OK + non-OK +
  cancel), and docs updated off `grpc_unary_call_h2c_blocking`.
- **`Http2ClientMsg::Cancel { stream_id }` landed** with outbound
  RST_STREAM(CANCEL), `LocalCancel` outcome, and an outbound reset fact
  (parts 3/5).
- **Remaining work in this phase** (still future slices, named in
  *Includes* and *Proof Shape* below):
  - Streaming gRPC client (`server_streaming` / `client_streaming` /
    `bidi`), gated on HTTP/2 client streaming bodies below.
  - HTTP/2 client streaming request and response bodies (today's
    client buffers both under explicit caps).
  - TLS ALPN on the runtime: thread `AlpnProtocols` through
    `CallInput::TlsConnect` / `TlsBind`, return `selected_alpn` in
    `CallOutput::TlsConnected` / `TlsAccepted`, plumb the bytes
    through rustls. The client surface already takes
    `AlpnProtocols::h2()`; today's TLS rail does not carry the bytes.
  - DST replay for live client socket work (typed unsupported fact
    + saved replay case once the typed-ALPN rail lands).
  - Connection-reuse pool (idle eviction, max lifetime, health
    policy) — Phase 119, using the new
    `Http2Target::route_key()` shape.
  - Tina-client → tonic-server interop (needs tonic as a `tina-http`
    dep; deferred). Tina-client ↔ Tina-server gRPC is proven.
  - HTTPS/2 client compile-fail proofs for the typed gates, and the
    gRPC streaming compile-fail proofs (gated on streaming).
- Can run in parallel with phases 117 and 118 if ownership stays in `tina-http`,
  TLS ALPN rail data, protocol facts, docs, and protocol specimens.
- Runs before Phase 119 resource maturity. HTTP/2/gRPC client pooling needs
  the real client connection shape first.

## Layering

Phase 115 separated core from batteries (see
`docs/tina-user-guide/23-core-and-batteries.md`). This phase respects that
line:

- **Core** (`tina`, `tina-runtime`, `tina-sim`): publish the small public
  hooks the HTTP/2 client needs — TLS ALPN on the TLS rail, received-status
  protocol facts already added in Phase 112. No new runtime semantics.
- **Official battery** (`tina-http`): all HTTP/2 / gRPC client code lives
  here. New `Http2ClientConnection`, gRPC client wrappers, and pooled
  client connection helpers are battery code on top of public core hooks.
- **No bridge** is involved; the native client replaces the
  `tina-reqwest-bridge`-style escape hatch for the protocols it covers.

If the design needs a hook that does not yet exist as a public Tina core
surface (for example, ALPN selection on the TLS rail), promote that hook in
core first, then build the battery on top. Do not reach into runtime
internals.

## Purpose

Make Tina a native client, not only a native server.

The user story:

```text
my Tina service calls another HTTP/2/gRPC service without Tokio
```

## Starting Facts

- Server-side HTTP/2 h2c already exists in `tina-http/src/http2.rs`.
- Server-side gRPC already exists in `tina-http/src/grpc.rs` with unary,
  server-streaming, client-streaming, bidi-shaped routes, and tonic h2c tests.
- `grpc_unary_call_h2c_blocking` is only a blocking specimen/test helper. It
  is not a Tina client isolate and emits no client runtime lifecycle truth.
- HTTP/2 frame decode/encode and HPACK helpers are private inside server
  `http2.rs`. Client work must split those into shared internal modules first.
- `http2.rs` is one flat file today. The split is a real module-tree move,
  not just adding sibling files.
- HTTP/2 flow/window state is woven through `ActiveStream`,
  `Http2Connection`, pending response bodies, request-body credit, and
  outbound writes. Share frame/header/error code first; keep flow logic local
  until client duplication proves the right extraction.
- TLS has no ALPN today. `tls_connect` / `tls_accept` return only stream ids.
  This phase owns ALPN config and selected-protocol truth.
- The TLS worker command and pending table share one cancellation flag via
  `submit_command`. ALPN edits must preserve that exact contract.
- Protocol facts now exist as runtime/sim facts. Client-received gRPC status
  and HTTP/2 lifecycle facts should use that path, not private counters only.

## Includes

- move HTTP/2 into a module tree and split frame/header helpers out of the
  current server-only file into shared internal code used by server and client:
  - frame encode/decode
  - SETTINGS / PING / GOAWAY / RST_STREAM / WINDOW_UPDATE builders
  - HPACK header encode/decode helpers
  - protocol error mapping to HTTP/2 wire error codes
- native HTTP/2 client connection isolate
- bounded HTTP/2 client stream-slot admission; do not model one request as one
  leased connection
- one caller request owns one HTTP/2 stream slot until response trailers/EOF,
  reset, timeout, or caller cancellation
- native gRPC client surface over that HTTP/2 client connection
- unary, server-streaming, client-streaming, and bidi client paths
- TLS ALPN rail support for `h2`:
  - named `AlpnProtocols` config, not raw byte bags:
    `AlpnProtocols::h2()` and `AlpnProtocols::none()`
  - ALPN protocols on TLS bind/connect config
  - selected protocol in TLS connect/accept output
  - typed ALPN mismatch/failure truth
- authority/SNI/Host rules copied from the HTTP/1/TLS lessons
- h2c and h2/TLS target types; no string bag that can forget SNI/authority
- client connection reuse keyed by authority plus TLS/root config
- first-form reuse is "one client connection isolate can carry many admitted
  streams." Idle eviction, max lifetime, health policy, and pool ownership are
  Phase 119.
- received gRPC status as protocol fact after Phase 112
- live interop tests against a real tonic/h2c or h2 server
- simulator returns typed unsupported facts for live HTTP/2 client socket work
  until a scripted HTTP/2 client simulator lands; protocol status facts still
  use stable names for later replay plumbing
- update docs that currently say native gRPC is server-first
- replace the blocking helper docs so users copy the client isolate path, not
  the test helper

## Does Not Include

- no gRPC reflection
- no load balancing
- no interceptor framework
- no broad web framework
- no hidden Tokio client
- no generic resource pool policy; Phase 119 owns idle/max-lifetime/health
- no HTTP/2 server rewrite beyond sharing frame/header helpers
- no client load balancing; one configured authority is one client target

## Blast Radius

Big blast radius. Keep it fenced.

- Allowed: `tina-http` HTTP/2/gRPC internals, TLS ALPN call inputs/outputs,
  protocol facts, focused docs/specimens.
- Allowed: `tina-runtime` and `tina-sim` TLS call shapes for selected ALPN.
- Not allowed: broad HTTP/1 rewrites, WebSocket rewrites, pool policy, new async
  bridge, or public HTTP/2 frame API.
- Do the internal HTTP/2 module split first and prove server behavior is
  unchanged before adding client behavior.
- The PR/commit sequence must leave a clean review checkpoint after the
  server-only split: moved files, shared helpers, tests green, and no client
  behavior or ALPN behavior yet.

## Implementation Shape

- `tina-http` gains shared internal HTTP/2 modules before behavior changes:
  - move today's `http2.rs` server implementation to `http2/server.rs`
  - add `http2/mod.rs` that preserves the existing public exports
  - `http2/frame.rs`
  - `http2/headers.rs`
  - `http2/errors.rs`
- Keep flow/window accounting in the server/client modules for this phase. Do
  not add `http2/flow.rs`; today flow is tied to connection state, response
  pulls, request-body credit, write queues, and protocol facts. Split it later
  only if both sides duplicate the same small state machine after the client
  exists.
- Existing server tests must stay green after the split before client behavior
  lands.
- Commit/checkpoint 1 is server unchanged:
  - `http2.rs` is split into the module tree;
  - shared frame/header/error helpers are internal;
  - no new client structs;
  - no ALPN edits;
  - server behavior and public exports are unchanged.
  Run the existing HTTP/2/gRPC server tests at this checkpoint so reviewers can
  diff the move apart from the new client.
- Do not expose these frame/header modules as public API. They are internal
  shared code for server/client.
- `Http2ClientConnection` is an isolate over one TCP/TLS stream:
  - sends client preface and SETTINGS;
  - opens odd-numbered streams;
  - enforces `max_concurrent_streams`;
  - tracks per-stream request body, response body, trailers, reset, timeout,
    and caller cancellation;
  - reports pressure and lifecycle.
- User-facing names should be target-shaped:
  - `Http2Target::H2c { authority, addr }`
  - `Http2Target::Tls { authority, addr, server_name, trust_roots }`
  - `GrpcTarget` wraps `Http2Target` plus service defaults, not a string URL
    bag.
- HTTP/2 client admission returns typed outcomes:
  - `Admitted`
  - `Full`
  - `Closed`
  - `Timeout`
  - `Reset`
  - `ProtocolError`
  - `TlsAlpnMismatch`
- gRPC client wrappers sit above `Http2ClientConnection`; they do not own a
  hidden queue or hidden runtime.
- gRPC copied path:
  - `GrpcClient::unary(...)`
  - `GrpcClient::server_streaming(...)`
  - `GrpcClient::client_streaming(...)`
  - `GrpcClient::bidi(...)`
  each returns one Tina call/effect shape with explicit status outcome.
- TLS ALPN extends the existing TLS rail:
  - no ambient default;
  - h2 target asks through `AlpnProtocols::h2()`;
  - explicit non-ALPN TCP/TLS paths use `AlpnProtocols::none()`;
  - h2c target never touches TLS;
  - selected ALPN is visible in typed connect/accept output;
  - simulator TLS config/history includes offered/selected ALPN so saved cases
    do not replay under ambient defaults;
  - stable trace tags are appended, never renumbered.

## Proof Shape

- server split proof:
  - existing HTTP/2 and gRPC server tests pass unchanged after frame/header
    helper extraction
- live HTTP/2 client proof:
  - h2c GET/POST happy path against Tina server
  - response DATA arrives in bounded chunks
  - request DATA streaming is bounded
  - concurrent streams on one client connection do not cross replies
  - server RST_STREAM maps to typed reset
  - GOAWAY closes new admission but lets admitted streams settle visibly
  - flow-control blocked path is counted/reported
  - timeout/cancel frees stream slot and rejects late response truth visibly
  - malformed response frame closes the affected stream/connection with typed
    protocol truth, not a panic
- interop proof:
  - Tina HTTP/2 client talks to Tina HTTP/2 server
  - Tina gRPC client talks to Tina gRPC server
  - Tina gRPC unary talks to tonic h2c
  - tonic/h2 client talks to Tina server still passes after shared-code split
- TLS ALPN proof:
  - h2/TLS success selects `h2`
  - no shared protocol / wrong ALPN returns typed mismatch
  - cert/name failures remain distinct from ALPN failure
  - h2c target does not touch TLS rails
- gRPC client proof:
  - unary client against Tina server
  - unary client against tonic h2c server
  - server-streaming client receives all messages and final status
  - client-streaming sends multiple messages and receives final status
  - bidi client proves request and response streams progress independently
  - non-OK gRPC status is the caller outcome, not an HTTP transport success
  - oversized received message is `ResourceExhausted`/typed cap failure before
    unbounded allocation
- reuse/close proof:
  - N unary calls reuse one HTTP/2 client connection when healthy
  - closed/reset/protocol-bad connection rejects new admission visibly
  - caller cancellation does not poison unrelated streams
- replay/protocol fact proof:
  - protocol facts emitted for received statuses and stream lifecycle
  - simulator returns typed unsupported fact for live client socket work, not a
    silent no-op or fake replay
- compile-fail proof:
  - h2c target cannot carry TLS roots/SNI
  - h2/TLS target must carry server name/root policy
  - unary helper cannot accept a streaming request body
  - gRPC client status outcome must be handled as status, not collapsed into
    HTTP success

## User Specimens

- Add/update one protocol specimen that calls a Tina gRPC server from a Tina
  gRPC client without Tokio.
- Add/update one system specimen that uses outbound gRPC client calls from a
  service handler and shuts down cleanly.
- README must show copied path, not `grpc_unary_call_h2c_blocking`.
- Specimen must include one non-OK gRPC status and one client cancellation.

## Hostile Review Notes

- Do not build a second HTTP client that owns one TCP connection per request.
- Do not put ALPN in docs only. Selected protocol must be typed runtime truth.
- Do not let gRPC status disappear inside `Ok(HttpResponse)`.
- Do not add broad pool policy here. Reuse is connection capability; resource
  lifecycle policy is Phase 119.
