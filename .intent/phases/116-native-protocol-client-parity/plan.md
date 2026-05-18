# Phase 116: Native Protocol Client Parity

## Status

- Future IDD outline for Wave A.
- Can run in parallel with phases 117 and 118 if ownership stays mostly in
  `tina-http`, `tina-runtime` protocol facts, and protocol specimens.
- Runs before Phase 119 resource maturity. HTTP/2/gRPC client pooling needs
  the real client connection shape first.

## Purpose

Make Tina a native client, not only a native server.

The user story:

```text
my Tina service calls another HTTP/2/gRPC service without Tokio
```

## Includes

- split HTTP/2 frame/header helpers out of server-only `http2.rs` into shared
  internal code used by server and client
- native HTTP/2 client connection isolate
- bounded HTTP/2 client stream-slot admission; do not model one request as one
  leased connection
- native gRPC client surface over that HTTP/2 client connection
- unary, server-streaming, client-streaming, and bidi client paths
- TLS ALPN rail support for `h2`:
  - ALPN protocols on TLS bind/connect config
  - selected protocol in TLS connect/accept output
  - typed ALPN mismatch/failure truth
- authority/SNI/Host rules copied from the HTTP/1/TLS lessons
- h2c and h2/TLS target types; no string bag that can forget SNI/authority
- client connection reuse keyed by authority plus TLS/root config
- received gRPC status as protocol fact after Phase 112
- live interop tests against a real tonic/h2c or h2 server
- simulator/replay support or explicit unsupported fact for live-only paths

## Does Not Include

- no gRPC reflection
- no load balancing
- no interceptor framework
- no broad web framework
- no hidden Tokio client
- no generic resource pool policy; Phase 119 owns idle/max-lifetime/health
- no HTTP/2 server rewrite beyond sharing frame/header helpers

## Proof Shape

- live HTTP/2 client happy path
- live HTTP/2 flow-control/timeout/reset paths
- gRPC client unary and streaming interop
- TLS ALPN success and failure truth
- connection reuse/retire/close truth
- protocol facts emitted for received statuses and stream lifecycle
- compile-fail tests for wrong client config typestate where practical
