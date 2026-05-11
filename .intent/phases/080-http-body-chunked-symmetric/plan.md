# 080 HTTP Body Chunked Symmetric

## Status

- Done: plan created after 074 body streaming, 076 server keepalive,
  079 response-source cancel, and the SQL/DB pool tranche.
- In progress: not started.
- Open: implementation, proofs, docs, specimen updates.
- Deferred: HTTP/2, gRPC, trailers, compression, proxies, redirects,
  cookies, broad framework surface.

## Goal

Finish honest HTTP/1 body semantics.

Tina can already emit chunked responses. The missing half is:

```text
client can read chunked response.
server can accept chunked request.
decoder does not buffer whole body.
bad chunks fail loudly.
pressure stays bounded.
```

Grug truth:

```text
chunk says size.
read at most that much.
zero chunk means done.
bad chunk means error.
no secret unbounded Vec.
```

## Non-Goals

- No HTTP/2.
- No gRPC.
- No trailers in first form. If trailers appear, reject or ignore only
  if the rule is documented and tested. Prefer reject.
- No chunk extensions unless the implementation explicitly parses and
  ignores them. Do not silently accept malformed size lines.
- No transparent gzip/deflate.
- No request pipelining upgrade.
- No unbounded whole-body buffering just to make chunked easier. The
  existing client may still return a buffered decoded body, but that
  buffer is bounded by `max_body_bytes`.

## Shape

One PR if it stays boring. Two PRs if shared decoder + one side already
gets large.

Preferred split if needed:

1. Shared chunked decoder and client-side chunked response decode.
2. Server-side chunked request bodies plus specimen/docs.

Do not start HTTP/2 because chunked feels close to framing. It is not
the same product.

## Rock 0 — Audit Current HTTP Body Paths

Read first:

- `tina-http/src/parse.rs`
- `tina-http/src/connection.rs`
- `tina-http/src/client.rs`
- `tina-http/src/streaming.rs`
- `tina-http/tests/streaming_v2.rs`
- `examples/specimen_http_body_streaming`

Record what changes in this plan before code:

- current rejection paths for `Transfer-Encoding: chunked`;
- current body caps;
- current body metrics;
- current client response buffering shape;
- whether server request streaming already has enough pull surface.

## Rock 1 — Shared Chunked Decoder

Add a small HTTP/1 chunked state machine.

It must be incremental:

- accepts bytes as they arrive;
- returns decoded data chunks as they become available;
- keeps partial size/data/CRLF state between reads;
- never needs the whole wire body resident;
- enforces configured body caps;
- distinguishes incomplete input from malformed input.

First-form accepted wire:

```text
HEX\r\n
bytes\r\n
...
0\r\n
\r\n
```

Reject:

- invalid hex size;
- chunk larger than configured cap / remaining body cap;
- missing CRLF after size or data;
- EOF before final zero chunk;
- trailers, unless this phase deliberately implements a tiny
  documented trailer rule.

If chunk extensions are accepted, they must be parsed as
`size;extension...` and ignored deliberately. If not, test that they
are rejected.

Header matching rule:

- `Transfer-Encoding` matching is case-insensitive;
- `chunked` must be the only/final coding Tina accepts in first form;
- unknown transfer codings are rejected;
- `Content-Length` plus `Transfer-Encoding` is rejected unless this
  phase writes a different explicit rule and proves it.

## Rock 2 — Client-Side Chunked Response Decode

Today the client rejects chunked responses.

Change it so `Transfer-Encoding: chunked` decodes to the existing
client response body shape, under the same body cap rules as
`Content-Length`.

Rules:

- decoded body bytes count toward `max_body_bytes`;
- wire framing bytes do not count as app body;
- malformed chunked body returns a typed parse/body error;
- missing terminator is an error, not clean EOF;
- `Content-Length` + `Transfer-Encoding: chunked` follows the shared
  header rule above.

This is still a buffered client response unless a streaming client body
surface already exists. Do not invent a new client streaming API in
this rock.

Proof:

- happy chunked response from a native server;
- fragmented chunk headers and fragmented data;
- malformed size;
- missing data CRLF;
- missing final zero chunk;
- body cap exceeded by decoded bytes;
- `Content-Length` plus `Transfer-Encoding` behavior pinned.

## Rock 3 — Server-Side Chunked Request Bodies

Today the server rejects chunked requests.

Change it so services can receive chunked uploads through the existing
request-stream pull model.

Rules:

- `HttpRequestBody::Stream(RequestStream)` is the preferred shape for
  chunked requests;
- there is no declared `content_length`; add an explicit unknown-length
  representation if needed;
- chunked requests require request streaming to be enabled. If
  `HttpLimits::inbound_stream_chunk_size` is `None`, reject the request
  loudly instead of buffering it all;
- each `body_next()` yields decoded app bytes, not wire framing;
- resident bytes are bounded by `inbound_stream_chunk_size` and body
  caps;
- parse/truncation errors surface as `RequestChunkReply::Error(...)`;
- final zero chunk surfaces as `RequestChunkReply::Eof`;
- malformed chunks close the connection after the service observes the
  error, or before dispatch if no service owns the stream yet. Pick one
  rule and test it.

If the existing `RequestStream { content_length }` cannot represent
unknown length honestly, change the type. Do not encode unknown as `0`.

Proof:

- happy chunked upload, service pulls all chunks;
- fragmented chunk headers/data;
- slow service applies backpressure;
- malformed size;
- missing terminator;
- body cap exceeded;
- metrics drain after close/cancel.

## Rock 4 — HTTP/HTTPS Parity

Chunked semantics must be transport-independent.

At least one proof should run through TLS:

- chunked response decode over HTTPS client path, or
- chunked request upload to `HttpsListener`.

Do not duplicate every HTTP test over HTTPS. One parity proof is enough
unless the TLS path needs different code.

## Rock 5 — Metrics And Capacity Truth

Body metrics must stay honest:

- high-water counts decoded body bytes resident in Tina, not chunk
  framing;
- body timeout/full/io-error counters still mean the same thing;
- malformed chunked body increments the right error counter;
- all charges drain after success, parse error, peer close, timeout,
  and cancel.

Do not migrate to 082 capacity-scope machinery here. If the existing
body report needs one new field for chunked, add it. Otherwise keep the
surface boring.

## Rock 6 — Specimen And Docs

Update the HTTP body specimen so it shows both directions:

- streaming download with known length;
- streaming download with chunked response;
- streaming upload with chunked request.

The README should name the lesson:

```text
unknown length uses chunked.
service still pulls chunks.
Tina still bounds resident bytes.
```

Update user guide / crate docs:

- request body shapes;
- response body shapes;
- what chunked supports;
- what is still deferred: trailers, compression, HTTP/2.

## Required Tests

Minimum tests before merge:

- shared decoder unit tests for happy, fragmented, malformed, cap
  exceeded, missing terminator;
- client chunked response integration test;
- server chunked upload integration test;
- body metrics drain/high-water test;
- one HTTP/HTTPS parity test;
- specimen smoke test.

Run:

```text
cargo fmt --all --check
cargo test -p tina-http --tests
cargo clippy -p tina-http --tests -- -D warnings
```

If docs changed:

```text
RUSTDOCFLAGS=-D warnings cargo doc --workspace --no-deps
```

## Hostile Review Checklist

Ask before PR:

- Did any path buffer the whole chunked body secretly?
- Can malformed chunk input look like clean EOF?
- Does unknown request length use an honest type, not `0`?
- Do body metrics drain on every error path?
- Does the client count decoded bytes, not wire bytes, against body
  cap?
- Did we accidentally accept trailers/extensions without deciding?
- Are docs/specimen teaching the copied path?

## Done Means

- Tina HTTP client can consume chunked responses.
- Tina HTTP server can receive chunked requests.
- Chunked decoding is incremental and bounded.
- Bad chunked wire shapes are typed failures.
- Metrics and tests prove resident body pressure stays bounded.
- HTTP/1 body docs no longer say chunked is one-sided.
