# 098 HTTP/2 And gRPC Streaming Finish

## Status

- IDD phase.
- One PR unless interop tooling forces a tiny follow-up.
- Builds on shipped Phase 095 HTTP/2 streaming substrate and Phase 096 native
  gRPC first streaming modes.
- Owns remaining HTTP/2/gRPC streaming truth: bidirectional gRPC, fuller
  full-duplex HTTP/2 proof, grpcurl/tonic interop where practical, and clear
  client/TLS deferrals.
- Do not run beside broad `tina-http` protocol rewrites without file ownership.

## Grug Truth

HTTP/2 streaming mostly exists.

gRPC unary, server-streaming, and client-streaming mostly exist.

The remaining gap is the hard one:

- both directions active;
- one peer stalls;
- one peer resets;
- service finishes one side first;
- final status still happens once;
- no hidden queue grows.

Do not paper over this with a gRPC-only shortcut.

HTTP/2 owns bytes and windows.

gRPC owns protobuf frames and status.

Tina owns capacity, cancellation, and lifecycle truth.

## Goal

After this phase, Tina can honestly claim:

```text
native server-side gRPC unary, server-streaming, client-streaming, and
bidirectional streaming over Tina-owned HTTP/2 h2c, with bounded messages,
visible pressure, peer reset cancellation, and interop proof for claimed modes.
```

This is server-side readiness. A production Tina gRPC client is a separate
phase unless the needed HTTP/2 client state machine already exists.

## Non-Goals

- no tonic runtime inside Tina;
- no hyper/h2 async runtime inside Tina;
- no production pooled gRPC client in this phase;
- no TLS ALPN unless it is already tiny and clearly separate;
- no reflection unless grpcurl proof requires a very small descriptor path;
- no compression support unless already implemented and tested;
- no load balancing;
- no retry/reconnect framework;
- no unbounded request/response queues;
- no hidden automatic retry on flow-control pressure.

## Rock 0: Read First, Freeze The Claim

Read:

- `.intent/phases/095-http2-streaming-substrate/plan.md`;
- `.intent/phases/096-finish-native-grpc-streaming/plan.md`;
- `tina-http/src/http2.rs`;
- `tina-http/src/grpc.rs`;
- `tina-http/tests/http2_live.rs`;
- `tina-http/tests/grpc_live.rs`;
- `examples/specimen_grpc_counter`;
- `docs/tina-user-guide/12-io-model.md`;
- `docs/tina-user-guide/18-bridge-crates.md`.

Before coding, update this status with:

- current shipped HTTP/2 streaming facts;
- current shipped gRPC modes;
- exact bidi public API names;
- exact request/response stream ownership model;
- exact final-status ownership rule;
- exact interop targets;
- exact deferrals.

Cut line:

- if HTTP/2 cannot make both directions progress independently, stop and fix
  HTTP/2 before adding more gRPC surface;
- if bidi needs a new scheduler, stop;
- if client work starts growing a real HTTP/2 client state machine, split it.

## Rock 1: HTTP/2 Full-Duplex Proof

Prove the substrate before layering gRPC bidi.

Required behavior:

- inbound DATA can be consumed while outbound DATA is flow-control blocked;
- outbound DATA can resume after `WINDOW_UPDATE`;
- peer `RST_STREAM` cancels accepted service work and response source;
- request EOF does not force response EOF unless the service says so;
- response EOF does not require request EOF unless the route says so;
- malformed DATA/trailer order resets the stream without killing unrelated
  streams;
- connection report still works after stream reset/cancel.

Required tests:

- one stream response blocked, another stream completes;
- inbound request chunks continue while outbound side is blocked;
- reset during blocked response cancels source;
- reset during request streaming cancels service call;
- content-length overrun/underrun remains pinned;
- trailers-after-end and DATA-after-end are rejected/reset visibly.

No sleeps as proof. Use barriers, socket deadlines, reports, or trace facts.

## Rock 2: Bidi gRPC API

Add the smallest Tina-shaped bidi surface.

Candidate shape:

```rust
router.bidi_streaming(path, |stream| { ... })
```

The service must see explicit handles, not an async stream illusion:

- request stream handle/source;
- response sink/source;
- final status owner;
- per-message caps;
- per-stream caps;
- cancel/deadline outcome.

Rules:

- request and response lifecycles are independent;
- final gRPC status is sent once;
- service can finish response before request EOF only by explicit policy;
- request EOF does not auto-finish response;
- peer reset cancels request and response work;
- service error maps to typed `GrpcStatus`;
- no hidden per-message `Vec` grows without a cap.

If the API wants too many clever types, use one explicit state-machine specimen
first and extract only the dull names.

## Rock 3: Bidi gRPC Semantics

Implement full-duplex server-side gRPC over the HTTP/2 substrate.

Required proof:

- echo bidi: client sends N messages, server sends N messages;
- server sends before request EOF;
- client stops sending while server continues, if policy allows;
- server ends early with status and cancels/drains remaining request stream;
- peer reset cancels both sides;
- request message too large fails before user service sees decoded message;
- response message too large returns `ResourceExhausted`;
- malformed gRPC frame returns `InvalidArgument`;
- deadline maps to `DeadlineExceeded`;
- unrelated concurrent stream survives reset/failure.

Keep unary, server-streaming, and client-streaming tests green.

## Rock 4: Interop

Interop is required for any compatibility claim.

Minimum target:

- tonic h2c client -> Tina unary/server-streaming/client-streaming/bidi server
  for shipped modes.

Try grpcurl too:

- if grpcurl works without reflection, add a script/test command;
- if it needs reflection or descriptor plumbing, record exact reason and defer
  reflection.

Docs must say exactly what was tested:

- h2c or TLS;
- tonic version if pinned;
- grpcurl command if present;
- modes covered.

Do not claim broad gRPC ecosystem replacement without these tests.

## Rock 5: Client And TLS Deferral

Write down the line:

- server-side h2c streaming is in scope;
- production pooled Tina gRPC client is out of scope unless a native HTTP/2
  client state machine already exists;
- TLS ALPN / h2 over HTTPS is out of scope unless landed intentionally;
- tonic client interop can still prove server behavior.

If a tiny client helper exists only for tests, label it test-only. Do not let a
blocking helper pretend to be the production client.

## Rock 6: Docs And Specimen

Update:

- `docs/tina-user-guide/12-io-model.md`;
- `docs/tina-user-guide/18-bridge-crates.md` if bridge wording mentions gRPC;
- `examples/specimen_grpc_counter/README.md`;
- phase status.

Specimen must show:

- unary;
- server-streaming;
- client-streaming;
- bidi;
- one pressure/cancel case.

## Required Checks

Run at least:

```text
cargo fmt --all --check
cargo test -p tina-http --test http2_live -- --nocapture
cargo test -p tina-http --test grpc_live -- --nocapture
cargo test --manifest-path examples/specimen_grpc_counter/Cargo.toml
cargo clippy -p tina-http --tests -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc -p tina-http --no-deps
```

If tonic/grpcurl tests are added, include exact commands in the status block.

## Success

No gRPC streaming mode depends on hidden buffering.

Peer reset and deadline reach the Tina service.

Flow-control pressure is visible.

Interop claims are backed by commands.

Production client and TLS ALPN are honestly deferred unless actually shipped.
