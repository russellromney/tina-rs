# Hostile Review

## Finding 1 [P2] The plan could skip substrate proof and jump to gRPC API

Bidi gRPC can appear to work while HTTP/2 still deadlocks when one direction is
flow-control blocked.

Resolution: Rock 1 is now substrate-first and requires full-duplex pressure
proof before gRPC bidi semantics.

## Finding 2 [P2] Client story can blur into server story

"Finish gRPC" can accidentally include a half-built production client or imply
one exists.

Resolution: the goal says server-side readiness. Rock 5 explicitly defers
production pooled Tina gRPC client unless the HTTP/2 client state machine
already exists.

## Finding 3 [P2] Interop can be vibes

Without concrete tonic/grpcurl commands, "gRPC compatible" is too soft.

Resolution: Rock 4 requires tonic h2c client tests for claimed modes and either
grpcurl commands or an explicit reflection/descriptor deferral.

## Finding 4 [P2] Final status ownership is easy to get wrong

Bidi has many endings: request EOF, response EOF, service error, peer reset,
deadline. Sending trailers twice or never is the common bug.

Resolution: Rocks 2 and 3 require one final-status owner and tests for early
finish, reset, malformed frame, deadline, and unrelated stream survival.

## Finding 5 [P3] The plan is still large

HTTP/2 full-duplex proof plus bidi plus interop is a lot.

Resolution: it remains one PR because the files and semantics are tightly
coupled, but Rock 0 has cut lines. If client/TLS/reflection grows, split it.
