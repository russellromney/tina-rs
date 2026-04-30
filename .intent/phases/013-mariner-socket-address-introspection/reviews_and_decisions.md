# 013 Reviews And Decisions

Session:

- A

## Review Log

### Round 1

Headline: ready to close. The socket-address honesty gap is now closed on
the Betelgeuse-backed runtime.

What landed:

- `tina-runtime-current` now depends on a vendored Betelgeuse copy in
  `vendor-betelgeuse/` so the runtime can add the smallest missing
  substrate surface locally.
- The vendored `IOSocket` trait now exposes `local_addr()` and
  `peer_addr()`, implemented on both Darwin and Linux socket backends via
  `getsockname` / `getpeername`.
- `CallRequest::TcpBind { addr: 127.0.0.1:0 }` is supported again
  honestly: the runtime binds through Betelgeuse and reports the actual
  bound `local_addr`.
- `CallResult::TcpAccepted` includes the accepted stream's real
  `peer_addr`.
- The live `tcp_echo` proof and runnable example both returned to
  `127.0.0.1:0`.
- Focused call-dispatch proofs now cover:
  - successful port-0 bind returns a non-zero real local port
  - accepted stream reports the connecting client's actual peer address
  - pending `TcpAccept` still rejects late completion with
    `CallCompletionRejected { RequesterClosed }`

Verification:

- `cargo +nightly test -p tina-runtime-current --test call_dispatch`
- `cargo +nightly test -p tina-runtime-current --test tcp_echo`
- `cargo +nightly run -p tina-runtime-current --example tcp_echo`
- `make verify`

All passed.

Decision:

- Close 013.
