# Phase 131: Outbound Connect And Session Managers

## Status

- Future implementation phase.
- One PR.
- Can run beside 132/133/134 if it owns only `tina-http` outbound client
  code, specimens, and docs.

## Grug Truth

Real services are clients too.

A single resolved socket address is not a production client. A user wants to
say "connect to this host", then get bounded DNS, bounded connect attempts,
clear TLS/SNI/authority truth, reconnect policy, stale-session truth, and a
report when it fails.

## Current Code Facts

- `tina-http::HttpTarget`, `Http2Target`, and `WebSocketTarget` already require
  a resolved `SocketAddr`.
- HTTP/1 keepalive pooling exists, but it also takes a resolved target.
- HTTP/2 and gRPC native clients exist; gRPC is a wrapper over one HTTP/2
  connection address.
- `tina_runtime::dns_lookup(host, port, timeout)` exists. The live runtime has
  a bounded DNS lane. The simulator has `ScriptedDnsConfig`.
- WebSocket client is explicitly one session, not a reconnecting manager.

So this phase must add host/authority endpoint APIs, not only policy structs
beside the existing clients.

## Goal

Ship the copied outbound-client path:

- unresolved endpoint types for HTTP/1, HTTP/2, gRPC, and WebSocket;
- `ConnectPolicy` over runtime DNS + TCP/TLS connect;
- bounded Happy Eyeballs first form;
- WebSocket reconnecting client manager;
- HTTP/2 and gRPC fixed-endpoint pools;
- endpoint generations and stale-session reports;
- typed pressure/lifecycle reports;
- sim/replay facts or explicit unsupported facts.

## Does Not Include

- no dynamic service discovery;
- no global session manager;
- no hidden infinite reconnect loop;
- no unbounded client pool;
- no HTTP/3/QUIC;
- no fake cancellation of external work that Tina does not own.

## Names And Homes

- Add `tina-http::connect`.
- Keep existing resolved targets as low-level escape hatches:
  `HttpTarget`, `Http2Target`, `WebSocketTarget`.
- Add unresolved user-facing endpoint types:
  - `HttpEndpoint`
  - `Http2Endpoint`
  - `GrpcEndpoint`
  - `WebSocketEndpoint`
- Add shared connect types:
  - `ConnectPolicy`
  - `AddressFamilyPolicy`
  - `HappyEyeballsPolicy`
  - `ResolvedEndpoint`
  - `ConnectAttemptReport`
  - `ConnectReport`
  - `EndpointId`
  - `EndpointGeneration`
- Add managers:
  - `WebSocketClientManager`
  - `Http2ClientPool`
  - `GrpcClientPool`

The endpoint owns user intent: host, port, authority/Host, SNI/server name,
trust roots, path, ALPN. A resolved target owns one chosen `SocketAddr`.

## Implementation

### Rock 1: Endpoint And Connect Policy

Add endpoint types that can resolve into existing targets:

- HTTP/1:
  - `HttpEndpoint::http(host, port)`
  - `HttpEndpoint::https(host, port, trust_roots)`
  - optional explicit `host_header(...)`
  - optional explicit `server_name(...)` for HTTPS
- HTTP/2:
  - `Http2Endpoint::h2c(authority, host, port)`
  - `Http2Endpoint::tls(authority, host, port, server_name, trust_roots)`
- gRPC:
  - `GrpcEndpoint` wraps `Http2Endpoint` plus gRPC limits/service metadata.
- WebSocket:
  - `WebSocketEndpoint::ws(host, port, path)`
  - `WebSocketEndpoint::wss(host, port, path, server_name, trust_roots)`

Use `dns_lookup` for unresolved host endpoints. Do not add a background
resolver. Do not make live DNS pluggable in this phase; sim tests use
`ScriptedDnsConfig`, live tests use localhost/closed ports.

`ConnectPolicy` must name:

- DNS timeout;
- connect timeout;
- max resolved addresses to try;
- address-family policy: IPv6 first, IPv4 first, or preserve resolver order;
- Happy Eyeballs delay;
- max concurrent connect attempts;
- max total attempts.

`ConnectReport` must include:

- endpoint id and generation;
- host, port, authority, SNI/server name;
- resolved addresses;
- attempted addresses in order;
- winner, if any;
- per-attempt family and terminal reason;
- DNS full/closed/timeout;
- TCP refused/io/timeout;
- TLS failure/ALPN mismatch/certificate/name truth;
- cancelled loser truth;
- late completion/tombstone count.

### Rock 2: Bounded Connect Helper

Build one Tina-shaped helper that managers call:

- DNS is one runtime call.
- TCP/TLS connect attempts are ordinary Tina calls.
- Happy Eyeballs uses bounded call handles and `cancel_call` for losers.
- Losers can complete late; late completions are counted and ignored, not
  converted into success.
- The helper returns a resolved low-level target or a typed `ConnectReport`.

No helper may dispatch a connect effect until its attempt slot is admitted.

### Rock 3: WebSocket Client Manager

Build `WebSocketClientManager` over `WebSocketClientConnection`:

- owns at most `max_sessions`;
- keeps at most one current session per endpoint unless config says more;
- reconnects only up to policy limits;
- exposes connect/send/receive/report/close;
- returns `Full(report)`, `Closed(report)`, `NoHealthyEndpoint(report)`,
  `ConnectFailed(report)`, `TimedOut(report)`, and `Stale(report)`;
- retains the last N closed/stale session reports under a cap;
- drains/close-stops sessions on shutdown;
- reports per-session outbound queue/bytes pressure from the underlying
  WebSocket report.

Endpoint generation must prevent an old session reply from replacing the
current session.

### Rock 4: HTTP/2 And gRPC Pools

Add fixed-endpoint pools. Keep this boring:

- fixed endpoint list at construction;
- round-robin over healthy endpoints;
- max connections;
- max in-flight streams per connection;
- pre-connect queue cap;
- idle close;
- stale connection retire;
- `NoHealthyEndpoint` when every endpoint is closed/unhealthy;
- separate HTTP/2 reset truth from gRPC status truth.

Do not add dynamic membership. Do not collapse HTTP/2 reset, GOAWAY, TLS/ALPN,
and gRPC final status into one generic error.

### Rock 5: Specimens And Docs

Update/add:

- `system_realtime_rooms`: outbound WebSocket manager path with reconnect.
- One small gRPC client service using `GrpcClientPool`.
- One closed-port reconnect-storm test.
- User guide outbound-client section showing endpoint -> policy -> manager.

## Required Proof

- Host-only endpoints resolve through runtime DNS before connecting.
- Existing resolved-target APIs still work.
- Sim scripted DNS success/failure/timeout maps to distinct report rows.
- DNS lane `Full` is distinct from TCP connect failure.
- Happy Eyeballs attempts are bounded; the loser is cancelled and cannot
  become user success.
- Late loser completion is tombstoned and counted.
- HTTPS/WSS/H2 TLS reports preserve authority, Host, SNI, trust roots, and
  ALPN truth.
- WebSocket manager reconnects after peer close and marks the old session
  stale.
- Slow peer fills bounded outbound queue/bytes and returns typed pressure.
- Reconnect storm against a closed port is deterministic and does not leak
  sessions or attempts.
- HTTP/2 pool returns `NoHealthyEndpoint` when every endpoint is unhealthy.
- gRPC pool preserves gRPC final status separately from transport failure.
- Shutdown closes/drains sessions and returns a manager report.
- Request-scope cancellation of a connect/session operation cancels Tina-owned
  waits and reports late completions.
- Sim/replay either reproduces supported facts or records explicit unsupported
  facts. No exact replay lie.
