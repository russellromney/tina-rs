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
