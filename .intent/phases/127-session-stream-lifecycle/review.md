# Plan Review 1

- [fixed] The plan could have changed protocol wire behavior while only proving
  lifecycle reports. Added non-change rules for HTTP/1, HTTP/2, gRPC, and
  WebSocket success paths and error mappings.
- [fixed] Request-scoped cancellation was not tied into session lifecycle. The
  plan now requires caller disconnect/cancel to feed the same session report
  vocabulary and avoid orphan body/source/session work.
- [fixed] Blast-radius proof was implicit. Added existing protocol test
  families as required regression proof.

Remaining risk: "one vocabulary" can become a fake abstraction. Implementation
review must check that protocol-specific facts remain typed where they matter.

# Plan Review 2

- [fixed] Native protocol completion was too coupled to session lifecycle to
  live as a separate phase. It is now folded in so WebSocket client, HTTP/2
  ALPN, gRPC client polish, pooled clients, and client protocol facts all use
  the same session lifecycle words.
- [fixed] The plan now clearly follows Phase 116 instead of redoing first-form
  clients.
- [fixed] Interop and blast-radius proof now include both server and client
  paths.

Remaining risk: protocol completion can balloon into a web framework. Keep it
native protocol sessions only: no HTTP/3, no broad framework, no transparent
reconnect, no unbounded client pool.
