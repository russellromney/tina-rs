# specimen_native_https

A tiny HTTPS/1.1 counter service. Tokio side: `tokio +
tokio-rustls`, hand-rolled HTTP/1.1 over a TLS-wrapped socket. Tina
side: `tina_http::HttpsListener` + a `Counter` isolate. The shared
`scripted_client` is a stdlib rustls client that hits both sides
identically: `GET /counter → POST × 3 → GET /counter → GET /missing`.

The complement of [`specimen_native_http`](../specimen_native_http/README.md);
this is the *server* comparison for HTTPS specifically. The Tina HTTPS
**client** (`HttpClient` over `HttpTarget::Https`) is exercised by the
integration tests in `tina-http/tests/client_tls_smoke.rs` — it cannot
share a runtime with `HttpsListener` in first form because the
runtime's TLS lane has one worker thread and both sides of a
handshake would deadlock.

## Run

```sh
cargo run --manifest-path examples/specimen_native_https/Cargo.toml -- both
cargo run --manifest-path examples/specimen_native_https/Cargo.toml -- tokio
cargo run --manifest-path examples/specimen_native_https/Cargo.toml -- tina
```

Both sides:

```
side=tokio successful_get=2 successful_post=3 final_counter_value=3
           got_404_for_missing=true exit_clean=true
side=tina  successful_get=2 successful_post=3 final_counter_value=3
           got_404_for_missing=true exit_clean=true
```

## Read

- [`src/tokio_impl.rs`](src/tokio_impl.rs)
- [`src/tina_impl.rs`](src/tina_impl.rs)
- [`src/lib.rs`](src/lib.rs) — the shared scripted rustls client.
- [`src/tls_identity.rs`](src/tls_identity.rs) — rcgen self-sign.

## Shape

Both sides:

- generate a self-signed `localhost` cert via `rcgen`;
- bind `127.0.0.1:0` and accept TLS connections backed by that cert;
- on each accepted TLS stream, parse one HTTP/1.1 request and write a
  `Connection: close` response;
- count `POST /counter` increments, return the value on `GET
  /counter`, return `404` on `/missing`.

The Tokio side hand-rolls the HTTP/1.1 read loop in `tokio::spawn`
tasks; the Tina side delegates that to the connection isolate the
listener spawns per accept. Counter state is `Arc<AtomicU32>` on the
Tokio side, `value: u32` on the Tina `Counter` isolate (no `Mutex`,
no atomics — the isolate owns it).

## What feels different from plain HTTP

- **TLS config is explicit DER.** `TlsServerIdentity::from_der(chain,
  key)` is the only way to build a server identity. There is no PEM
  default and no system roots. Same shape on both sides.
- **Startup is typed.** `call(listener, HttpsListenerMsg::Start, t)`
  returns `Result<HttpsReady { local_addr }, HttpsStartupError>`. A
  bad cert/key surfaces as
  `Err(HttpsStartupError::Bind { source: TlsCertificate })`. The
  Tokio side relies on the `with_single_cert` builder erroring at
  startup; the Tina side surfaces the same outcome via the runtime's
  typed `CallError`.
- **TLS errors stay TLS errors.** When `HttpClient` is the caller (in
  the integration tests), TLS reasons surface as
  `HttpClientError::Transport { phase, source }` carrying
  `TlsName`, `TlsCertificate`, `TlsHandshake`, `TlsFull`,
  `TlsClosed`, or `Timeout`. They do *not* collapse into a generic
  `Connect`/`Read`/`Write`.
- **The HTTPS client side is in tests, not the example.** Tina's
  TLS lane is single-worker; running an HTTPS server *and* an HTTPS
  client on the same shard deadlocks the handshake. The integration
  tests prove the client side end-to-end against a thread-spawned
  rustls server. A multi-worker TLS lane is deferred.

## Where the boring deferred work lives

- HTTP/2, ALPN, system roots, certificate reload, mTLS — none of
  them. Pulling in any of those means changing the runtime's TLS
  rails, not `tina-http`.
- `reqwest` remains the mature outbound web-client escape hatch for
  redirects, cookies, proxies, and HTTP/2. See
  `docs/tina-user-guide/18-bridge-crates.md`.
