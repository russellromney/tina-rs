# specimen_native_https

A tiny HTTPS/1.1 counter service, run two ways through the same scripted flow:
`GET /counter → POST × 3 → GET /counter → GET /missing`.

- **Tokio side:** a hand-rolled `tokio + tokio-rustls` server, hit by the
  stdlib-rustls `scripted_client` — the interop counterparty.
- **Tina side:** a `tina_http::HttpsListener` server **and** a
  `tina_http::HttpClient` HTTPS client in **one runtime, on one shard**. TLS
  rides Tina's Betelgeuse TCP rail (rustls sans-I/O on the shard thread), so a
  Tina HTTPS client and server share a runtime. The old single-worker TLS lane
  deadlocked both sides of one handshake — which is why the client used to live
  in a separate stdlib-rustls process; it no longer has to.

The complement of [`specimen_native_http`](../specimen_native_http/README.md).

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

- [`src/tokio_impl.rs`](src/tokio_impl.rs) — the tokio+tokio-rustls counterparty.
- [`src/tina_impl.rs`](src/tina_impl.rs) — the all-Tina client+server in one runtime.
- [`src/lib.rs`](src/lib.rs) — the stdlib-rustls `scripted_client` for the tokio side.
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
- **TLS errors stay TLS errors.** With `HttpClient` as the caller, TLS reasons
  surface as `HttpClientError::Transport { phase, source }` carrying `TlsName`,
  `TlsCertificate`, `TlsHandshake`, `TlsFull`, `TlsClosed`, or `Timeout`. They
  do *not* collapse into a generic `Connect`/`Read`/`Write`.
- **Client and server share a runtime.** The Tina side runs both ends on one
  shard: TLS is a layer over the runtime's TCP rail, not a separate
  blocking-socket subsystem on a worker thread. `tls_lane_capacity` bounds the
  shard-total count of in-flight TLS ops (handshakes, reads, writes, closes),
  not a per-stream worker slot.

## Where the boring deferred work lives

- HTTP/2, ALPN, system roots, certificate reload, mTLS — none of
  them. Pulling in any of those means changing the runtime's TLS
  rails, not `tina-http`.
- `reqwest` remains the mature outbound web-client escape hatch for
  redirects, cookies, proxies, and HTTP/2. See
  `docs/tina-user-guide/18-bridge-crates.md`.
