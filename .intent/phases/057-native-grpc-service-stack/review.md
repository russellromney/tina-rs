# 057 Hostile Review

## Verdict

Plan is intentionally narrow. Good.

gRPC can explode into tonic clone, streaming framework, codegen story, health,
reflection, interceptors, auth, compression, load balancing. This plan rejects
that.

## Required Guardrails

- Unary must land before streaming.
- If HTTP/2 client is not ready, do not fake a gRPC client with Tokio.
- If HTTP/2 trailers are missing, add real minimal trailer support. Do not fake
  `grpc-status` in the body.
- Compression flag must reject loudly.
- Message length cap must be checked before allocation.
- HTTP/2 stream reset must become gRPC cancel/status truth, not generic I/O.
- Do not hide HTTP/2 `Full` / stream cap behind broad `Internal`.
- Multi-turn services must use `RequestContext`; this phase must not revive the
  old broken continuation-context story.

## Missing But Okay To Defer

- Code generation from `.proto`.
- Client streaming and bidi streaming.
- Tonic interoperability matrix.
- gRPC health/reflection.
- TLS/ALPN polish.

## Review Focus

Look hardest at message buffering, trailer/status mapping, and h2c-vs-TLS
wording. Those are where a "small helper" can lie.
