# Outbound Clients: Endpoint → Policy → Manager

A single resolved socket address is not a production client. A real client
says "connect to this host," then gets bounded DNS, bounded connect attempts,
clear TLS/SNI/authority truth, a reconnect policy, and a report when it fails.

Tina's outbound shape has three layers. Pick the layer that matches the job.

## 1. Endpoint — user intent

An **endpoint** owns intent: host, port, authority/`Host:`, SNI/server name,
trust roots, path, ALPN. It is unresolved — it does not name a `SocketAddr`.

```rust
use tina_http::{HttpEndpoint, Http2Endpoint, GrpcEndpoint, WebSocketEndpoint, TlsTrustRoots};

let api = HttpEndpoint::https("api.example.com", 443, roots).server_name("api.example.com");
let h2  = Http2Endpoint::tls("api.example.com", "api.example.com", 443, "api.example.com", der_roots);
let grpc = GrpcEndpoint::h2c("billing.svc", "billing.svc", 50051);
let ws  = WebSocketEndpoint::wss("rt.example.com", 443, "/ws", "rt.example.com", roots);
```

Each endpoint `resolve(addr)`s into the low-level resolved target
(`HttpTarget`, `Http2Target`, `GrpcTarget`, `WebSocketTarget`) once a winning
address is chosen. The resolved targets still exist as escape hatches when you
already have a `SocketAddr` and want to skip DNS.

The endpoint never loses truth in a string: the authority, SNI, trust roots,
and ALPN you set are exactly what the connect dials and what the report shows.

## 2. ConnectPolicy — bounded DNS + connect

A **`ConnectPolicy`** names every cap up front, and validates before first use:

```rust
use tina_http::{ConnectPolicy, AddressFamilyPolicy, HappyEyeballsPolicy};

let mut policy = ConnectPolicy::balanced();
policy.address_family = AddressFamilyPolicy::Ipv6First; // or Ipv4First / PreserveOrder
policy.max_resolved_addresses = 3;                      // how many DNS answers to try
policy.happy_eyeballs.max_concurrent_attempts = 2;      // bounded concurrency
policy.happy_eyeballs.delay = std::time::Duration::from_millis(250);
policy.max_total_attempts = 6;                           // hard cap over the whole connect
policy.validate().expect("zero caps and overcommit are rejected");
```

`validate()` rejects zero attempt caps, zero deadlines, and a concurrency cap
above the total cap. Every cap can be named as a `BudgetSurface`, so a manifest
row and a live pressure row describe the same bound.

DNS is one bounded runtime call. A full DNS lane (`DnsOutcome::Full`) is a
distinct fact from a refused TCP connect — they are never collapsed.

## 3. Manager — sessions, reconnect, reports

The connect helper (`ConnectAttempts`) runs a bounded Happy-Eyeballs race over
the resolved addresses: it admits attempt slots before building any connect
effect, cancels losers when a winner appears, and tombstones any loser that
completes late so it can never become the user's success. You rarely call it
directly — a manager owns it.

`WebSocketClientManager` is the reconnecting WebSocket client:

```rust
use tina_http::{WebSocketManagerConfig, WebSocketManagerMsg, build_websocket_client_manager};

let mut config = WebSocketManagerConfig::new(policy);
config.max_reconnects = 3;     // bounded — no hidden reconnect loop
config.retained_reports = 4;   // bounded retained closed/stale reports
config.validate().unwrap();

let handles = build_websocket_client_manager(&runtime, ws, config, 32, 16)?;
// call(handles.manager, WebSocketManagerMsg::Connect, timeout) → WebSocketConnectOutcome
```

The manager returns typed connect outcomes: `Connected`, `ConnectFailed`,
`NoHealthyEndpoint`, `TimedOut`, `Full`, and `Closed` when shutdown wins a
connect in progress. Session operations return `NotConnected`, `Busy`,
`TimedOut`, or `Closed` instead of hanging. A generation guard drops any reply
from an old session, so a stale session can never replace the current one. Each
`Report` folds the current session's outbound queue/byte pressure. Shutdown
drains and stops sessions.

For a fixed list of backends, `Http2ClientPool` and `GrpcClientPool` round-robin
over healthy endpoints with a per-connection stream cap and `NoHealthyEndpoint`
when every endpoint is down. The gRPC pool keeps the gRPC final status
first-class: a non-OK status means the server answered (endpoint stays healthy),
while an HTTP/2 reset / GOAWAY / ALPN failure marks the endpoint down.

## Budget truth

Manager, pool, and connect caps all expose `budget_surfaces(prefix)`. Join them
against a live pressure report with `manifest.report(&pressure)` — a stale or
missing manifest row fails the consistency check. Caps are not decoration; they
are the same numbers your manifest declares and your reports observe.
