# Phase 115: Native Protocol Client Parity

## Status

- Future IDD outline for Wave A.
- Can run in parallel with phases 116 and 117 if ownership stays mostly in
  `tina-http`, `tina-runtime` protocol facts, and protocol specimens.

## Purpose

Make Tina a native client, not only a native server.

The user story:

```text
my Tina service calls another HTTP/2/gRPC service without Tokio
```

## Includes

- native HTTP/2 client isolate
- native gRPC client isolate
- unary, server-streaming, client-streaming, and bidi client paths
- TLS ALPN for HTTP/2 where the current TLS stack can support it
- authority/SNI/Host rules copied from the HTTP/1/TLS lessons
- client connection pooling keyed by authority plus TLS/root config
- received gRPC status as protocol fact after Phase 112
- live interop tests against a real tonic/h2c or h2 server
- simulator/replay support or explicit unsupported fact for live-only paths

## Does Not Include

- no gRPC reflection
- no load balancing
- no interceptor framework
- no broad web framework
- no hidden Tokio client

## Proof Shape

- live HTTP/2 client happy path
- live HTTP/2 flow-control/timeout/reset paths
- gRPC client unary and streaming interop
- TLS ALPN success and failure truth
- pool reuse/retire/close truth
- protocol facts emitted for received statuses and stream lifecycle
- compile-fail tests for wrong client config typestate where practical

