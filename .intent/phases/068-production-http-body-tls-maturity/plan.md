# 068 — Native HTTPS First Form

## Status

- Done:
  - Rock 0 audit (see findings below).
  - Rock 1 — `HttpTransport`/`HttpListenerTransport` enums + transport
    helpers; `HttpClient` and `HttpConnection` lifted onto the rail.
  - Rock 2 — `HttpsListener` with call-shaped typed startup
    (`Result<HttpsReady, HttpsStartupError>`), `HttpsServerConfig` with
    split `tls_accept_timeout` / `tls_io_timeout`, `HttpConnection`
    parameterised by transport. Real-rustls happy-path test +
    typed-bad-key test green.
  - Rock 3 — `HttpTarget::Http`/`Https`, `TlsTrustRoots`,
    `HttpHostPolicy`, `HttpClientError::Transport { phase, source }`,
    `DuplicateHostHeader`. Five client tests cover round-trip, default
    Host, explicit Host, duplicate Host, bad name, untrusted root.
  - Rock 4 — `HttpConnectionPool` admits HTTPS targets unchanged
    (target-agnostic). One pool-over-HTTPS smoke test added.
  - Rock 5 — DST proof: HttpClient over scripted TLS replays
    deterministically (saved hash matches across two sim runs). Sim
    can script TLS connect outcomes including bytes; bind/accept-side
    full HTTP-byte replay deferred.
  - Rock 6 — `examples/eiffel_native_https` lands. Tina
    `HttpsListener` + Counter vs hand-rolled `tokio + tokio-rustls`,
    same scripted rustls client. Both sides report identical counter
    behaviour.
  - Rock 7 — docs: `tina-http` crate header, `12-io-model.md`,
    `18-bridge-crates.md`, `examples/README.md`, `FINDINGS.md` (new
    finding 16: multi-worker TLS lane).
- In progress: none.
- Post-review fixes (PR feedback round 4 — broader sweep):
  - **Symmetry fix**: applied the same idempotency / state-machine
    fixes to plain `HttpListener` after a sweep showed the TCP twin
    had identical bugs. `Start` is now idempotent (no-op the second
    time, since reply type is `()` and we can't surface
    `AlreadyStarted` without a breaking API change). `Stop` racing
    with a pending `tcp_bind` now closes the just-bound listener
    via the `Bound(Ok)` stopping branch. `expect("listener set
    after bind")` replaced with defensive `if let` (orphan-close
    stream).
  - Plain HTTP did NOT have the Stop-with-pending-accept leak
    because the TCP lane uses "close wins" — pending accepts get
    cancelled when `tcp_close_listener` runs. Recorded as runtime
    asymmetry (TCP close wins, TLS rejects with `ResourceBusy`)
    that motivated the deferred-close pattern in `HttpsListener`.
  - Test added: `second_start_does_not_rebind_or_leak_listener` in
    `server_smoke.rs` — sends a second Start to an already-running
    listener and asserts the trace shows exactly one `TcpBind`.

  Round 5 — fix the cross-cutting items the round-4 sweep
  surfaced:

  - **Runtime: TLS lane close-wins for listeners.** `submit_listener_close`
    now cancels any pending `tls_accept` on the listener and
    proceeds with close, mirroring `do_listener_close` on the TCP
    lane. Cancelled accepts surface as synthetic
    `Failed(TargetClosed)` completions via a new
    `cancelled_by_close` queue drained in `advance()`. The
    deferred-close workaround in `HttpsListener::Stop` is removed;
    Stop now mirrors `HttpListener::Stop` exactly.
  - **Runtime: TLS lane close-wins for streams.** `submit_close`
    now cancels any pending read/write on the stream and removes
    the stream from `self.streams` synchronously. The worker still
    flushes through its `Arc<Mutex<TlsRuntimeStream>>` clone, but
    the lane table entry is freed immediately — so a close that
    fails with `Failed(Timeout|Io)` no longer leaks the stream
    resource. `finish_completion` no longer needs to remove on
    success either.
  - **`RequestChunkReply::Error(CallError)`.** `handle_body_chunk_read`
    now surfaces an I/O error mid-body as a typed `Error` reply
    instead of fabricating `Eof`. The service can distinguish a
    clean short delivery from a truncated request caused by I/O or
    peer error. Match-exhaustiveness compile-checks ensure every
    consumer handles the new variant.
  - **`HttpClient::Closed(_) => noop()`** — comment added: the
    runtime now removes the stream resource synchronously at
    submit-close time, so a failed close doesn't leak. The
    runtime trace records the outcome.
  - **`tls.rs: let _ = tcp.set_*_timeout(...)`** — comments added
    explaining these can only fail for `Some(zero)`/`None` (which
    we never pass), so the `let _` is explicit acknowledgement,
    not a silent swallow.

- Post-review fixes (PR feedback round 3):
  - **Bug fix**: Stop racing with a pending `tls_accept` previously
    surfaced `CallFailed { call_kind: TlsListenerClose, reason:
    ResourceBusy }` because the lane rejects close while accept is
    pending. The listener swallowed the failure and stopped, so the
    `TlsListenerId` orphaned in the lane until full shutdown. Fix:
    `Stop` now sets `stopping=true` and defers the listener-close.
    The `Accepted(...)` handler observes `stopping` once accept lands
    and triggers `tls_close_listener` from there, by which point the
    lane is free.
  - **Bug fix**: `Start` is now idempotent. A second `Start` replies
    `HttpsStartupError::AlreadyStarted` rather than issuing a second
    `tls_bind` that would leak the first listener.
  - **Bug fix**: Stop sent before `Bound(Ok)` lands now closes the
    listener cleanly via the new `Bound(Ok)` stopping branch — used
    to leak in this race.
  - **Bug fix**: `apply_host_policy` rejects empty policy values
    (`""`) as `InvalidHostHeaderValue` instead of letting them ride
    onto the wire as `Host: \r\n`.
  - **Smell fix**: replaced fragile `expect("listener set...")`
    invariants with defensive `if let` branches (orphan-close stream
    or `stop()` if the invariant is ever violated).
  - **Eiffel fix**: `Driver` previously fabricated a fake
    `Bind { Timeout }` for any non-`Replied` `CallOutcome`. Now it
    forwards the full `CallOutcome` and `run()` discriminates
    `Full`/`Closed`/`Timeout` honestly via `anyhow::bail!`.
  - Tests added:
    `second_start_returns_already_started_without_rebinding`,
    `stop_with_pending_accept_closes_listener_cleanly`,
    `stop_before_bound_completes_does_not_leave_listener_held`,
    `empty_host_policy_value_is_typed_error`,
    `empty_http_host_policy_is_typed_error`.

- Post-review fixes (PR feedback round 2):
  - Boxed `HttpClientMsg::Call(Box<OutboundCall>)` to silence
    `clippy::large_enum_variant` — `OutboundCall` carries an
    `HttpRequest` whose `HeaderMap` + body dwarfed every other
    variant.
  - Added `Runtime::observe_next_tls_bound` (and the `ThreadedRuntime`
    forward) that mirrors the existing `observe_next_bound` but
    fires on `CallKind::TlsBind` completion. The runtime now also
    exposes `pending_tls_bound` in the `ObservationRegistry` debug.
  - Symmetric Host policy on `HttpTarget::Http`: variant carries an
    optional `host: Option<String>`. `None` keeps the existing
    caller-managed-Host behavior; `Some(value)` populates the wire
    `Host:` header (and rejects caller-set Host via
    `DuplicateHostHeader` like HTTPS does). New constructor:
    `HttpTarget::http_with_host`. `From<SocketAddr>` still produces
    `Http { host: None }` so existing call sites keep working.

- Post-review fixes (PR feedback round 1):
  - `HttpsListener` accept-error classification: re-accept only on
    `Timeout` and `TlsHandshake` (transient); close out on
    `TlsClosed`, `Io`, `InvalidResource`, `TlsFull`, etc. Avoids
    busy-loop on terminal errors.
  - Removed dead `HttpListenerTransport` type from public API.
  - Added `TLS_DEADLINE_UNUSED` const sentinel; replaced raw
    `Duration::ZERO` magic at TCP-only call sites.
  - Split `DuplicateHostHeader` into `DuplicateHostHeader` (caller
    set Host) + `InvalidHostHeaderValue` (policy bytes invalid).
  - Tests added: `invalid_host_policy_value_is_typed_error`,
    `pool_refuses_with_full_when_https_slot_busy` (closes the Rock 4
    Full/Busy gap), three DST tests for scripted
    `TlsCertificate`/`TlsName`/`Timeout` connect failures mapping to
    `Transport(Connect, _)`.
  - Cleanup pass: semi-grug comments throughout new code.
  - Eiffel `Counter` handler: dropped redundant match wrapper.
- Open: make `tina-http` speak HTTPS with existing Tina TLS rails.
- Deferred: HTTP/2, gRPC, ALPN, ACME, cert reload, mTLS, SNI routing,
  system-root defaults, proxies, redirects, cookies, chunked transfer, broad
  web framework surface, production body-streaming polish.

### Rock 0 — Audit findings

Reuse:

- Runtime TLS rails are already there: `tls_bind`, `tls_accept`,
  `tls_connect`, `tls_read`, `tls_write`, `tls_close`,
  `tls_close_listener` (`tina-runtime/src/call.rs:2201–2278`). All take
  DER bytes — `Vec<Vec<u8>>` cert chain, `Vec<u8>` PKCS#8 private key,
  `Vec<Vec<u8>>` root certs. `tls_bind` returns
  `(TlsListenerId, SocketAddr)` so the bound local addr is in the reply,
  not a side-channel fact.
- TLS-flavoured `CallError` variants exist already: `TlsCertificate`,
  `TlsHandshake`, `TlsName`, `TlsFull`, `TlsClosed`, plus `Timeout`.
  `HttpConnection`'s service-error mapping already lists every TLS
  variant exhaustively (`tina-http/src/connection.rs:641–661`), so the
  inbound mapping does not need new variants.
- `LocalSystemConfig::tls_lane_capacity`
  (`tina-runtime/src/local_system.rs:50`) — TLS lane capacity is wired
  the same way as TCP/DNS/storage. Nothing new to plumb. Default
  `DEFAULT_TLS_LANE_CAPACITY` lives in `tina-runtime/src/driver/mod.rs`.
- Cert helper convention: `rcgen::generate_simple_self_signed(["localhost"])`
  inline. Six TLS tests in `tina-runtime/tests/local_system.rs:2003–2461`
  already cover bind / accept / connect / read / write / close plus
  `TlsCertificate`, `TlsHandshake`, `TlsFull`, `Timeout`, and
  shutdown-with-stuck-handshake reporting. We follow the same inline
  pattern for `tina-http` integration tests.
- `HttpServerConfig`, `HttpClientConfig`, `HttpLimits` compose cleanly
  inside `HttpsServerConfig` and the new `HttpTarget::Https` variant; no
  duplication of body-limit knobs.
- `encode_request`, `parse_response_head`, `parse_request_head`,
  `encode_response_head` are transport-agnostic and reused as-is.

Stay separate:

- New `HttpsListener` isolate. Re-skinning `HttpListener` would conflate
  `tcp_*` and `tls_*` continuation variants in one match — bigger isolate,
  weaker types, blurry trace. A second isolate keeps `TcpRead`/`TcpWrite`
  vs `TlsRead`/`TlsWrite` cleanly distinct in the trace and lets the
  message enum stay narrow.
- `HttpConnection` is parameterised by `HttpTransport`; one isolate
  serves both transports. Per-call TCP and TLS read/write/close are
  dispatched through the transport rail, so `TcpRead` vs `TlsRead`
  (and the rest) stays distinct in the trace without conditional
  dispatch in match arms. Avoids duplicating ~700 lines of parser /
  streaming-body / service-call code.
- `HttpClient` is generalised to one client over a small internal
  `HttpTransport { Tcp(StreamId), Tls(TlsStreamId) }`. The transport is
  decided when `OutboundCall` lands by inspecting `HttpTarget`.
  Continuation variants gain TLS analogues (`TlsConnected`, `TlsRead`,
  `TlsWrote`, `TlsClosed`) so the trace stays distinct and the match
  stays exhaustive.
- `HttpClientError` grows a typed transport variant
  (`Transport { phase: HttpTransportPhase, source: CallError }`).
  Existing TCP-shaped `Connect`/`Write`/`Read`/`Closed`/`Timeout`
  variants are kept for source compat — the new variant carries TLS
  reasons (`TlsName`, `TlsCertificate`, `TlsHandshake`, `TlsFull`,
  `TlsClosed`, `Timeout`) so callers can match precisely.
- Sim TLS rail is client-side only today
  (`tina-sim/src/config.rs` `ScriptedTlsConnectConfig`,
  `ScriptedTls{Read,Write}Result`). Bind/accept are not scriptable. So
  Rock 5 can prove HTTPS-client behaviour over a scripted TLS stream
  (one connect → scripted reads/writes), but server-side HTTPS proof
  lives in real local_system-style integration tests, not sim. This is
  acceptable for 068 and recorded as the missing primitive.
- Tina HTTPS client and Tina HTTPS server cannot share a runtime: the
  TLS lane has one worker thread per shard, and both sides of a TLS
  handshake need to drive that worker concurrently — they deadlock.
  Same constraint applies to client + server in the same shard. Both
  the smoke tests and the eiffel example respect this by running the
  counterparty in a raw OS thread (rustls directly). A multi-worker
  TLS lane is deferred.

Chosen startup API shape: **call-shaped `Start`**.

- `HttpsListener` reply type is
  `Result<HttpsReady { local_addr: SocketAddr }, HttpsStartupError>`.
- User waits for ready/failed via:
  `call(listener, HttpsListenerMsg::Start, deadline).reply(MyMsg::HttpsReady)`.
- The `Start` handler issues `tls_bind`. The `Bound` continuation either
  emits `reply(Ok(HttpsReady { local_addr }))` and proceeds to
  `tls_accept`, or `reply(Err(HttpsStartupError::Bind { source: CallError }))`
  and stops without spawning any child.
- Trace is evidence, not the API: the bound-address fact is still
  published for tests that prefer the existing `observe_next_bound`
  shape, but typed failure flows through the call reply.
- This shape extends cleanly to plain `HttpListener` later — out of
  scope for 068, so the existing fire-and-forget `Start` send keeps
  working for non-HTTPS callers.

## Goal

Tina can host and call a small HTTPS/1.1 service without Tokio owning the edge.

Grug truth:

```text
HTTP should not care if bytes came from TCP or TLS.
TLS config is explicit.
startup returns typed ready/error.
TLS errors stay TLS errors.
TLS lane pressure is visible.
SNI, cert name, and HTTP Host do not drift by accident.
```

This phase is HTTPS first. Do not let keep-alive, body streaming, or client
feature creep eat the TLS work.

## Boundaries

- Use existing `tina-runtime` TLS calls: `tls_bind`, `tls_accept`,
  `tls_connect`, `tls_read`, `tls_write`, `tls_close`,
  `tls_close_listener`.
- Use existing `tina-sim` TLS scripts.
- No new TLS implementation. No `tokio-rustls`.
- No hidden HTTPS -> HTTP fallback.
- No collapsing `TlsFull`, `TlsCertificate`, `TlsName`, `TlsHandshake`,
  `TlsClosed`, or `Timeout`.
- 066 owns new cancellation/deadline semantics.
- 069 owns replay helper ergonomics.

## Rock 0 — Audit First

Read:

- `tina-http/src/listener.rs`
- `tina-http/src/connection.rs`
- `tina-http/src/client.rs`
- `tina-http/src/pool.rs`
- `tina-runtime/src/call.rs`
- `tina-runtime/src/driver.rs`
- `tina-sim/tests/io_simulation.rs`
- `tina-runtime/tests/local_system.rs`

Then update Status with:

- what can be reused;
- what must stay separate;
- chosen startup API shape.

No code before this.

## Rock 1 — Transport Rail

Lift HTTP over a tiny transport enum. Keep the rail visible.

```rust
enum HttpTransport {
    Tcp(StreamId),
    Tls(TlsStreamId),
}

enum HttpListenerTransport {
    Tcp(ListenerId),
    Tls(TlsListenerId),
}
```

Rules:

- names say transport, not socket;
- trace keeps `TcpRead` vs `TlsRead`, `TcpWrite` vs `TlsWrite`;
- zero-progress read/write stays strict;
- no trait-object async abstraction;
- TCP tests still pass.

## Rock 2 — HTTPS Server

Add explicit server config.

```rust
pub struct HttpsServerConfig {
    pub http: HttpServerConfig,
    pub identity: TlsServerIdentity,
    pub tls_accept_timeout: Duration,
}

pub struct TlsServerIdentity {
    pub certificate_chain_der: Vec<Vec<u8>>,
    pub private_key_der: Vec<u8>,
}
```

`HttpsListener` may be its own isolate, or `HttpListener::https(...)` may reuse
the existing isolate with a transport mode. Pick the boring shape.

Startup must be typed. User waits for `Ready { local_addr }` or
`Failed { source }`. Trace is evidence, not the API.

Choose exactly one startup shape:

- call-shaped `Start -> Result<HttpsReady, HttpsError>`;
- `HttpsListener::install(...)` returns a ready/error waiter;
- `stop_with(HttpsListenerReport)`.

The shape should also work for plain `HttpListener` later.

Required proof:

- valid cert/key binds `127.0.0.1:0` and returns actual local addr;
- real rustls client gets HTTP `200`;
- invalid cert/key returns typed failure and leaks no listener;
- failed handshake does not spawn an HTTP connection isolate and leaks no
  `TlsStreamId`;
- TLS lane capacity 1 produces visible `TlsFull`;
- stop closes TLS listener; accepted children finish or close through existing
  cleanup.

## Rock 3 — HTTPS Client

Add explicit HTTPS target support.

```rust
pub enum HttpTarget {
    Http(SocketAddr),
    Https {
        addr: SocketAddr,
        server_name: String,
        trust_roots: TlsTrustRoots,
        host: HttpHostPolicy,
    },
}

pub struct TlsTrustRoots {
    pub root_certificates_der: Vec<Vec<u8>>,
}

pub enum HttpHostPolicy {
    UseServerName,
    Explicit(String),
}
```

Rules:

- roots are explicit; no system roots by default;
- runtime TLS validates `server_name`;
- default HTTP `Host` comes from `server_name`;
- explicit Host override is a visible target/request-builder choice;
- duplicate Host is typed error or deterministic documented overwrite;
  prefer reject;
- no redirects, proxy, cookies, HTTP/2, or ALPN.

TLS errors must stay matchable. Add something like:

```rust
pub enum HttpTransportPhase {
    Connect,
    Bind,
    Accept,
    Read,
    Write,
    Close,
}

pub enum HttpClientError {
    Transport {
        phase: HttpTransportPhase,
        source: CallError,
    },
    // parse/busy/pool variants...
}
```

Names may differ. Truth may not: callers can match `TlsName`,
`TlsCertificate`, `TlsHandshake`, `TlsFull`, `TlsClosed`, and `Timeout`.

Add copyable config constructors:

```rust
impl TlsTrustRoots {
    pub fn from_der(roots: Vec<Vec<u8>>) -> Self;
}

impl TlsServerIdentity {
    pub fn from_der(certificate_chain: Vec<Vec<u8>>, private_key: Vec<u8>) -> Self;
}
```

PEM helpers are optional test/file convenience. They do not imply system roots.

Required proof:

- HTTPS client fetches from native HTTPS server;
- default Host comes from server name;
- explicit Host override works;
- bad name -> `TlsName`;
- bad root -> `TlsCertificate`;
- protocol failure -> `TlsHandshake`;
- lane full -> `TlsFull`;
- timeout -> `Timeout`;
- client timeout/failed handshake closes or tombstones TLS resource visibly.

## Rock 4 — Pool Truth

Do not invent a keyed keep-alive pool here.

Current `HttpConnectionPool` is capacity-1 admission in front of one
`HttpClient`. It is not an idle connection cache.

Either:

- let the serial pool front HTTPS with no reuse; prove one success and one
  visible concurrent `Full`/`Busy`; or
- record pool support as HTTP-only until 067/keyed pools.

No root-bundle pool key work unless a real reusable pool exists.

## Rock 5 — Simulator

Add one HTTPS replay proof if current simulator TLS rail is enough.

Rules:

- scripted TLS read/write bytes are HTTP/1.1 bytes;
- saved hash pins TLS call kinds and HTTP outcome;
- cert validation is a scripted TLS outcome, not HTTP parsing.

If 069 has landed, use `ReplayCase`; otherwise use existing saved
seed/fingerprint and leave a migration note.

## Rock 6 — Eiffel Specimen

Add `examples/eiffel_native_https`.

Shape:

- Tokio side: tiny rustls HTTPS server/client;
- Tina side: `tina_http::HttpsListener` and `HttpClient` over HTTPS;
- same small counter route.

README says:

- TLS cert/root config is explicit;
- TLS failures are typed values;
- native HTTPS removes reqwest bridge for simple HTTPS/1.1;
- reqwest bridge still wins for mature web-client behavior.

## Rock 7 — Docs

Update:

- `tina-http` crate docs;
- `docs/tina-user-guide/18-bridge-crates.md`;
- `docs/tina-user-guide/12-io-model.md`;
- `examples/README.md`;
- `examples/FINDINGS.md` only for new product findings.

Docs must say:

- native HTTPS is HTTP/1.1 over Tina TLS rails;
- DER cert chain/key/root inputs are explicit first form;
- no system roots by default;
- no HTTP/2/ALPN claim;
- reqwest bridge remains the mature outbound web-client escape hatch.

## Proof Matrix

| Area | Works | Fails visibly |
|---|---|---|
| Server startup | ready with local addr | bad identity, no leak |
| Server request | rustls client gets HTTP 200 | bad handshake does not reach service |
| Client | fetch native HTTPS server | name/cert/handshake/full/timeout typed |
| Host | default Host and explicit override | duplicate/malformed Host typed |
| Transport | TCP tests still pass | TCP/TLS errors distinguishable |
| Pool | serial HTTPS submit if supported | concurrent pressure visible |
| Sim | HTTP bytes over scripted TLS replay | scripted cert failure maps to HTTPS error |
| Docs/Eiffel | simple native HTTPS shown | reqwest escape hatch named |

## Done Means

- Native HTTPS server and client work.
- Startup success/failure is typed.
- TLS errors and lane pressure are typed.
- Host/SNI/cert-name behavior is explicit and tested.
- Pool support is honest: serial-admission only or deferred.
- Sim has HTTPS-over-TLS replay, or records exact missing primitive.
- Eiffel specimen lands, or README says why not.
- Docs stop saying TLS is out of scope without pointing here.

## Hostile Review

- Did TLS config hide behind defaults?
- Did HTTPS silently fall back to HTTP?
- Did TLS errors become generic `Io`/`Connect`/`Read`?
- Did listener startup force trace scraping?
- Did callers have to hand-write Host?
- Did pool work pretend to be keyed keep-alive?
- Did docs imply HTTP/2, ALPN, system roots, redirects, or proxies?
- Did sim test HTTP over TLS, not TLS alone?
- Did this invent 066 cancellation?
- Did body streaming eat the phase?
