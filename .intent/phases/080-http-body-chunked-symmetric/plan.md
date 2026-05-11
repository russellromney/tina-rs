# 080 HTTP Body Chunked Symmetric

## Status

- Done: plan exists.
- In progress: not started.
- Open: code, tests, docs, specimen.
- Deferred: HTTP/2, gRPC, trailers, compression, proxies, redirects,
  cookies, broad web framework surface.

## Goal

Finish HTTP/1 body truth.

Already shipped:

- server can send chunked responses;
- known-length request/response streaming exists;
- body metrics exist;
- response-source cancel exists.

Still missing:

- client can read chunked responses;
- server can accept chunked requests.

Grug truth:

```text
chunk says size.
read size bytes.
then CRLF.
zero chunk means done.
bad chunk means error.
do not store whole body unless bounded API already says so.
```

## Hard Rules

- No HTTP/2.
- No gRPC.
- No trailers. Reject them unless this phase explicitly decides and
  tests another rule.
- No gzip/deflate.
- No request pipelining.
- No hidden unbounded `Vec`.
- No "unknown length means 0".
- No clean EOF for malformed chunked wire.
- No docs that say "chunked works" without saying which side.

The existing client may still return a buffered decoded body. That is
okay only because `max_body_bytes` bounds it.

## PR Shape

One PR if boring.

Split into two PRs if it grows:

1. shared chunked decoder + client response decode;
2. server chunked request bodies + specimen/docs.

Do not wander into HTTP/2 because "frames feel related." Different
thing.

## Rock 0 — Read Current Code

Read before editing:

- `tina-http/src/parse.rs`
- `tina-http/src/connection.rs`
- `tina-http/src/client.rs`
- `tina-http/src/streaming.rs`
- `tina-http/tests/streaming_v2.rs`
- `examples/specimen_http_body_streaming`

Then write a short status note in this plan:

- where chunked is rejected today;
- which caps apply today;
- which metrics apply today;
- which request-stream type changes, if any, are needed.

## Rock 1 — One Chunked Decoder

Build one small incremental decoder.

Do not copy little decoders into tests/specimens.

It must:

- accept partial bytes;
- keep state between calls;
- emit decoded data chunks;
- enforce decoded-body cap;
- track partial size line, data, and CRLF;
- return "need more bytes" vs "bad wire";
- finish only after final `0\r\n\r\n`.

Accepted first-form wire:

```text
HEX\r\n
DATA\r\n
HEX\r\n
DATA\r\n
0\r\n
\r\n
```

Reject:

- bad hex;
- missing CRLF after size;
- missing CRLF after data;
- EOF before final zero chunk;
- decoded body over cap;
- unknown transfer codings;
- `Content-Length` plus `Transfer-Encoding`;
- trailers unless this phase explicitly changes that rule.

Chunk extensions:

- Either reject `4;foo=bar`.
- Or parse `size;...` and ignore extensions on purpose.
- Pick one. Test it.

Header rule:

- header names are case-insensitive;
- transfer coding values are case-insensitive;
- Tina accepts only `Transfer-Encoding: chunked` in first form;
- `gzip, chunked`, `chunked, gzip`, and unknown codings are rejected.

## Rock 2 — Client Reads Chunked Responses

Today client rejects chunked.

Change client so chunked response becomes the same user body bytes as
Content-Length response.

Rules:

- count decoded bytes against `max_body_bytes`;
- do not count chunk framing as app body;
- missing final zero chunk is error;
- malformed chunk is typed error;
- `Content-Length` plus `Transfer-Encoding` follows Rock 1 rule;
- keep client response buffered for now; do not invent streaming client
  API here.

Tests:

- happy chunked response;
- chunk header split across reads;
- chunk data split across reads;
- bad hex;
- missing CRLF;
- missing final zero chunk;
- decoded body cap exceeded;
- `Content-Length` plus `Transfer-Encoding` rejected.

## Rock 3 — Server Accepts Chunked Requests

Today server rejects chunked requests.

Change server so service pulls decoded chunks.

Preferred shape:

```text
HttpRequestBody::Stream(RequestStream)
service calls body_next()
body_next replies Chunk(bytes) / Eof / Error
```

Rules:

- chunked requests require `HttpLimits::inbound_stream_chunk_size =
  Some(n)`;
- if streaming is disabled, reject chunked request loudly;
- each `body_next()` returns decoded bytes, not wire bytes;
- resident decoded bytes are bounded by `inbound_stream_chunk_size`;
- decoded total is bounded by `max_body_bytes`;
- bad wire becomes `RequestChunkReply::Error(...)` if service owns the
  stream;
- bad wire before dispatch becomes parser error response;
- final zero chunk becomes `RequestChunkReply::Eof`.

Important type rule:

- Current `RequestStream` has `content_length`.
- Chunked request has unknown length.
- If needed, change the type to make known vs unknown explicit.
- Do not encode unknown as `0`.

Tests:

- happy chunked upload;
- chunk header split across reads;
- chunk data split across reads;
- slow service pull shows backpressure;
- bad hex;
- missing CRLF;
- missing final zero chunk;
- decoded body cap exceeded;
- metrics drain after success and error.

## Rock 4 — HTTPS Parity

One TLS proof is required.

Pick one:

- client decodes chunked response over HTTPS; or
- server accepts chunked request over HTTPS.

Do not duplicate every HTTP test over HTTPS. One parity test is enough
unless TLS needs special code.

## Rock 5 — Metrics Stay True

Body metrics count app bytes, not framing bytes.

Must prove:

- high-water is decoded bytes resident in Tina;
- malformed chunk increments body error counter;
- body cap/full/timeout counters keep old meaning;
- all charges drain after success;
- all charges drain after parse error;
- all charges drain after peer close/cancel.

Do not migrate to 082 capacity reports here. If one field is needed,
add one field. Otherwise leave metrics boring.

## Rock 6 — Specimen And Docs

Update `specimen_http_body_streaming`.

It should show:

- known-length streaming download;
- chunked streaming download;
- chunked streaming upload.

README lesson:

```text
unknown length uses chunked.
service still pulls chunks.
Tina still bounds resident bytes.
bad chunk is error.
```

Update docs/crate docs:

- client chunked response support;
- server chunked request support;
- known vs unknown request body length;
- deferred: trailers, compression, HTTP/2.

## Required Tests

Before PR:

```text
cargo fmt --all --check
cargo test -p tina-http --tests
cargo clippy -p tina-http --tests -- -D warnings
```

If docs changed:

```text
RUSTDOCFLAGS=-D warnings cargo doc --workspace --no-deps
```

Minimum coverage:

- decoder unit tests;
- client integration tests;
- server integration tests;
- body metrics test;
- one HTTPS parity test;
- specimen smoke test.

## Hostile Review Checklist

Ask:

- Did any path buffer unbounded body bytes?
- Can bad chunk input become clean EOF?
- Did unknown length become `0` anywhere?
- Are metrics counting decoded bytes, not framing bytes?
- Do metrics drain on every error path?
- Did we accept trailers/extensions by accident?
- Does the specimen teach the copied path?

## Done Means

- Client consumes chunked responses.
- Server accepts chunked requests.
- Decoder is shared, incremental, and bounded.
- Bad chunked wire is a typed failure.
- Metrics prove bounded resident body bytes.
- Docs no longer say chunked is one-sided.
