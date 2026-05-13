# Phase 056: Native HTTP/2 Service Stack

## Status

- Ready to implement.
- One PR if it stays small.
- Do not run beside WebSocket work. Both touch `tina-http`.

## Grug Truth

HTTP/2 is many streams on one connection.

Frames are bytes.

Flow control is capacity.

Stream state is lifecycle.

RST is cancellation-ish, but not magic rollback.

Tina owns sockets, queues, timers, trace, and shutdown.

Codec crates are okay. Async runtimes are not.

## Goal

Add native HTTP/2 first form to `tina-http`.

First form:

- server-side HTTP/2 over cleartext test transport first;
- TLS/ALPN only if it is tiny after the server path works;
- unary request/response streams;
- server-streaming if it is naturally small;
- bounded per-connection and per-stream state;
- visible flow-control, reset, closed, full, timeout, and protocol errors;
- enough user API that Phase 057 gRPC can build on it.

This is not a full web framework.

## Non-Goals

- no gRPC in this phase;
- no broad client crate unless needed for proof;
- no HTTP/3;
- no push promises;
- no priority tree first form;
- no pretending all HTTP/2 behavior is done;
- no Tokio/hyper runtime;
- no unbounded stream table;
- no hidden body buffer.

## Rock 0: Read First

Read:

- `tina-http/src/connection.rs`;
- `tina-http/src/client.rs`;
- `tina-http/src/types.rs`;
- HTTP body/chunked tests;
- server keepalive tests;
- `docs/tina-user-guide` HTTP sections;
- this plan.

Before coding, add a status note here:

- codec choice;
- API home;
- first-form TLS/ALPN choice;
- tests chosen.

Likely home:

- `tina-http/src/http2.rs`;
- re-export narrow public types from `tina-http/src/lib.rs`;
- tests in `tina-http/tests/http2_*.rs`;
- specimen only if a copied user path exists.

## Rock 1: Frame Codec

Use a sync codec crate if it fits. If not, write the smallest frame codec.

Required frame support:

- connection preface;
- SETTINGS;
- HEADERS;
- DATA;
- WINDOW_UPDATE;
- RST_STREAM;
- GOAWAY;
- PING if cheap.

Required truth:

- max frame size cap;
- max header bytes cap;
- max concurrent streams cap;
- protocol errors are typed;
- unknown/unsupported frame behavior is explicit.

No unbounded HPACK/header table. If dynamic table support is not first form,
disable or cap it visibly.

## Rock 2: Stream State

Add a bounded stream table.

Rules:

- stream ids are validated;
- stream lifecycle is explicit;
- closed streams reject late frames visibly;
- stream reset frees capacity;
- connection close drains or rejects streams with typed truth;
- no growing `HashMap`.

If fixed-cap storage is awkward, use a bounded vector/slab and document the cap.

## Rock 3: Flow Control

Treat flow control as capacity.

Required:

- connection window;
- stream window;
- body chunks respect windows;
- `Full`/blocked state is visible;
- WINDOW_UPDATE updates capacity;
- no hidden buffer while waiting for window.

Do not fake flow control by buffering everything and writing later.

## Rock 4: Request/Response API

Add one copied server shape.

It should feel like current HTTP/1 Tina routing where possible.

First form:

- request headers + bounded body;
- response headers + bounded body;
- optional server-streaming body if it stays small;
- typed `Http2Error` / `Http2Outcome`.

Keep HTTP/1 and HTTP/2 types close where that helps, but do not force a common
abstraction that hides stream state.

## Rock 5: Cancellation And Close

Pin these cases:

- client resets stream;
- service stops while stream is open;
- connection closes with open streams;
- body source is cancelled on abandoned wire;
- late frames after close/reset are typed protocol/lifecycle facts.

Use existing cancellation/resource lifecycle vocabulary where possible.

## Rock 6: Tests

Required tests:

- valid preface + SETTINGS handshake;
- bad preface rejects;
- simple unary request/response;
- concurrent streams up to cap;
- stream cap full/rejects visibly;
- oversized frame/header/body rejects;
- DATA obeys stream and connection windows;
- WINDOW_UPDATE unblocks;
- RST_STREAM frees stream capacity;
- GOAWAY stops new streams and drains or rejects old streams visibly;
- service stop closes/rejects open streams;
- deterministic parser/state unit tests;
- at least one simulator or replay-style test if the touched path has modeled
  facts.

Tests must assert typed outcomes, not only "connection ended".

## Docs

Update native HTTP docs with:

- what works;
- what is deferred;
- how HTTP/2 capacity differs from HTTP/1 keepalive;
- why gRPC waits for Phase 057.

## Required Checks

- `cargo fmt --all --check`
- `cargo test -p tina-http http2 --tests`
- `cargo clippy -p tina-http --tests -- -D warnings`
- `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` if rustdoc changed

## Hostile Review Notes

- Risk: this becomes a full HTTP/2 framework.
  Fix: unary server path first. gRPC later.
- Risk: flow control becomes hidden buffering.
  Fix: windows are capacity; blocked/full is visible.
- Risk: stream table grows forever.
  Fix: bounded storage and stream-close capacity proof.
- Risk: HPACK dynamic table hides memory.
  Fix: cap it or defer it.
- Risk: TLS/ALPN distracts from stream truth.
  Fix: cleartext server proof first; TLS/ALPN only if tiny.
