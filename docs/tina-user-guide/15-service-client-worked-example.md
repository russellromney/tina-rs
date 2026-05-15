# Service Client Worked Example

This page shows the important pattern:

```text
one caller call
client service does many runtime I/O turns
client service replies once
caller sees CallOutcome
```

This is the shape for HTTP clients, RPC clients, database clients, and other
outbound services.

Native gRPC is currently server-first. `tina-http::GrpcRouter` serves unary
`prost` messages over the native HTTP/2 h2c listener, and
`grpc_unary_call_h2c` is only a tiny specimen/test helper to prove the wire
path without Tokio. A production gRPC client should follow the service-client
state-machine shape below once the native HTTP/2 client grows into a real Tina
client service.

## Public Call Shape

Caller code should look boring:

```rust
call(http_client, HttpClientMsg::Fetch(request), Duration::from_secs(2))
    .then(AppMsg::HttpReturned)
```

The caller does not spawn a temporary child. It calls a service.

## Client Service State Machine

Simplified sketch:

```rust
#[derive(Debug, Clone)]
enum HttpClientMsg {
    Fetch(HttpRequest),
    Connected(Result<StreamId, CallError>, HttpRequest),
    Wrote(Result<usize, CallError>),
    Read(Result<Vec<u8>, CallError>),
}

#[derive(Debug, Clone)]
enum HttpClientReply {
    Response(HttpResponse),
    Failed(HttpClientError),
}

struct HttpClient {
    target: SocketAddr,
    stream: Option<StreamId>,
    response_buf: Vec<u8>,
}

#[tina_runtime::isolate(
    message = HttpClientMsg,
    reply = HttpClientReply,
    shard = AppShard
)]
impl HttpClient {
    fn handle(
        &mut self,
        msg: HttpClientMsg,
        _ctx: &mut Context<'_, AppShard>,
    ) -> Effect<Self> {
        match msg {
            HttpClientMsg::Fetch(request) => {
                tcp_connect(self.target)
                    .then(|result| HttpClientMsg::Connected(result, request))
            }

            HttpClientMsg::Connected(Ok(stream), request) => {
                self.stream = Some(stream);
                let bytes = encode_request(request);
                tcp_write(stream, bytes).then(HttpClientMsg::Wrote)
            }

            HttpClientMsg::Connected(Err(_), _request) => {
                reply(HttpClientReply::Failed(HttpClientError::Connect))
            }

            HttpClientMsg::Wrote(Ok(_)) => {
                let stream = self.stream.expect("connected before write");
                tcp_read(stream, 8192).then(HttpClientMsg::Read)
            }

            HttpClientMsg::Wrote(Err(_)) => {
                reply(HttpClientReply::Failed(HttpClientError::Write))
            }

            HttpClientMsg::Read(Ok(bytes)) => {
                self.response_buf.extend(bytes);
                match try_parse_response(&self.response_buf) {
                    Ok(Some(response)) => reply(HttpClientReply::Response(response)),
                    Ok(None) => {
                        let stream = self.stream.expect("connected before read");
                        tcp_read(stream, 8192).then(HttpClientMsg::Read)
                    }
                    Err(_err) => reply(HttpClientReply::Failed(HttpClientError::Parse)),
                }
            }

            HttpClientMsg::Read(Err(_)) => {
                reply(HttpClientReply::Failed(HttpClientError::Read))
            }
        }
    }
}
```

The example is intentionally incomplete. It omits close, body limits, write-all
state, redirects, keep-alive, and pooling.

The important truth is complete: the original caller can get a reply after
`tcp_connect`, `tcp_write`, and one or more `tcp_read` continuations.

## Timeout Still Belongs To Caller

The caller owns the outer deadline:

```rust
call(http_client, HttpClientMsg::Fetch(request), Duration::from_secs(2))
    .then(AppMsg::HttpReturned)
```

If the client service takes too long, the caller receives `CallOutcome::Timeout`.

The client service may later finish. Tina should discard the late reply visibly.

## Pool Shape

One client isolate is sequential. That is fine.

For parallel outbound work, use explicit topology:

```text
App -> ClientPoolFrontend -> HttpClient worker 0
                        -> HttpClient worker 1
                        -> HttpClient worker N
```

The pool frontend is still one address:

```rust
call(pool, PoolMsg::Fetch(request), timeout).then(AppMsg::HttpReturned)
```

Pressure remains visible at the pool mailbox, worker mailboxes, in-flight
limits, and caller timeout.

## What Not To Do

Avoid this as the default service-client shape:

```text
spawn one temporary client child
give it a callback address
route result back manually
```

That may be useful for a special topology. It is not the normal Tina service
shape.
# Service Client Worked Example

For a full HTTP + DB + outbound client service shape, start with
`examples/systems/mini_saas_api`.

The key path is:

```text
POST /items/{id}/notify
  -> controller uses call_ctx.defer(SQLite query).reply(...)
  -> acquire native tina-http keepalive lease
  -> POST /notify upstream
  -> release lease as Reuse
  -> reply to original HTTP caller
```

Exact commands:

```sh
cargo test --manifest-path examples/systems/mini_saas_api/Cargo.toml
cargo run --manifest-path examples/systems/mini_saas_api/Cargo.toml -- smoke
cargo run --manifest-path examples/systems/mini_saas_api/Cargo.toml -- pressure
```

The system README contains the route table, capacity table, readiness meanings,
shutdown order, and out-of-scope list.
