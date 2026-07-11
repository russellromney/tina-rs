# Service Client Worked Example

A service client may need several runtime I/O turns before it can answer one
caller. The caller authority must move through those continuation messages; an
ordinary `then(...)` continuation does not preserve it automatically.

The shape is:

```text
one caller call
client captures caller authority while starting I/O
each continuation carries the RequestContext
one terminal continuation replies or rejects
caller still owns the outer timeout
```

## Public Call

Caller code remains a normal bounded isolate call:

```rust
call(http_client, HttpClientMsg::Fetch(request), Duration::from_secs(2))
    .then(AppMsg::HttpReturned)
```

The client does not spawn a temporary child or retain an application-owned
oneshot channel.

## Client State Machine

This sketch omits HTTP framing details so the authority path stays visible:

```rust
#[derive(Debug)]
enum HttpClientMsg {
    Fetch(HttpRequest),
    Connected(
        RequestContext<HttpClientReply>,
        Result<(StreamId, SocketAddr, SocketAddr), CallError>,
        HttpRequest,
    ),
    Wrote(
        RequestContext<HttpClientReply>,
        StreamId,
        Result<usize, CallError>,
    ),
    Read(
        RequestContext<HttpClientReply>,
        StreamId,
        Result<Vec<u8>, CallError>,
    ),
}

#[derive(Debug)]
enum HttpClientReply {
    Response(HttpResponse),
    Failed(HttpClientError),
}

struct HttpClient {
    target: SocketAddr,
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
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            HttpClientMsg::Fetch(_) => noop(),

            HttpClientMsg::Connected(request, Ok((stream, _, _)), outbound) => {
                tcp_write(stream, encode_request(outbound)).then_with_request(
                    request,
                    move |request, result| HttpClientMsg::Wrote(request, stream, result),
                )
            }
            HttpClientMsg::Connected(request, Err(_), _) => {
                reply_to(request, HttpClientReply::Failed(HttpClientError::Connect))
            }

            HttpClientMsg::Wrote(request, stream, Ok(_)) => {
                tcp_read(stream, 8192).then_with_request(
                    request,
                    move |request, result| HttpClientMsg::Read(request, stream, result),
                )
            }
            HttpClientMsg::Wrote(request, _stream, Err(_)) => {
                reply_to(request, HttpClientReply::Failed(HttpClientError::Write))
            }

            HttpClientMsg::Read(request, _stream, Ok(bytes)) => match decode_response(bytes) {
                Ok(response) => reply_to(request, HttpClientReply::Response(response)),
                Err(_) => reply_to(
                    request,
                    HttpClientReply::Failed(HttpClientError::Parse),
                ),
            },
            HttpClientMsg::Read(request, _stream, Err(_)) => {
                reply_to(request, HttpClientReply::Failed(HttpClientError::Read))
            }
        }
    }

    fn handle_call(
        &mut self,
        msg: HttpClientMsg,
        call: CallContext<'_, Self>,
    ) -> Effect<Self> {
        match msg {
            HttpClientMsg::Fetch(request) => call
                .defer(tcp_connect(self.target))
                .reply(move |caller, result| {
                    HttpClientMsg::Connected(caller, result, request)
                }),
            _ => call.reject(CallRejectedReason::UnsupportedMessage),
        }
    }
}
```

The first `call.defer(...).reply(...)` consumes the current `CallContext` and
creates the first continuation carrying `RequestContext<HttpClientReply>`.
Later runtime calls use `then_with_request(...)` to move that same one-shot
authority forward. The corresponding `StreamId` moves through those messages
too; it is not stored in one shared client slot that concurrent calls could
overwrite. Every terminal branch consumes the request with `reply_to(...)`.

For a single runtime call, prefer the shorter
`call.defer(work).reply(Continuation)` form. For longer workflows, keep the
request context in every continuation or use `tina::flow!` when its generated
step vocabulary fits.

## Timeout And Cancellation

The caller's timeout is still mandatory. If it expires, a later `reply_to`
cannot resurrect the call; the runtime records the late terminal outcome. A
client that needs to stop physical work too should retain the matching
cancelable call handle and close it through the explicit cancellation path.

Do not interpret a Tina call timeout as proof that the kernel, a database
library, or a bridged SDK stopped executing. Each runtime rail and bridge
documents its cancellation strength separately.

## Production Details

A real HTTP client must additionally own:

- bounded request and response bodies;
- partial-write and incremental-read loops;
- connection reuse and retirement;
- per-stage and outer deadlines;
- close and shutdown reporting;
- protocol facts and simulator support.

Use the native `tina-http` client for HTTP rather than reproducing this sketch.
The sketch exists to explain caller authority across multiple turns.
