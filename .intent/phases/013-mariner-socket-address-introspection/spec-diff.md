# 013 Mariner Socket Address Introspection

Session:

- A

## In Plain English

Phase 012 proved that `tina-rs` can drive real TCP work through a
runtime-owned call contract on a completion-driven backend without hiding an
async runtime in the user model.

The biggest honest gap left behind is socket address introspection.

Today:

- `TcpBind { addr: 127.0.0.1:0 }` is rejected as `Unsupported`
- `CallResult::TcpAccepted` does not include `peer_addr`
- the echo proof uses a test-selected concrete loopback port instead of
  runtime-owned ephemeral binding

That is an acceptable narrowing for 012, but it should not become permanent.

This slice closes that gap by teaching the Betelgeuse-backed runtime to obtain
and report real bound / peer socket addresses honestly. Once that works, the
runtime can support `:0` binding truthfully and the TCP call family stops
having this visible limitation.

## What Changes

- The Betelgeuse substrate used by `tina-runtime-current` gains an honest way
  to query:
  - a listener or stream's local socket address
  - an accepted stream's peer socket address
- `tina-runtime-current` uses that surface instead of narrowing or faking:
  - `TcpBind { addr: 127.0.0.1:0 }` becomes supported again
  - `CallResult::TcpBound { local_addr }` carries the actual bound address
  - `CallResult::TcpAccepted` regains `peer_addr`
- The TCP echo proof and example return to binding `127.0.0.1:0` honestly.

## What Does Not Change

- `tina` remains substrate-neutral. No backend-specific API leaks into the
  trait crate.
- Handlers remain synchronous.
- `CurrentRuntime::step()` remains synchronous and caller-driven.
- Runtime-owned sleep / timer wake is still out of scope for this slice.
- Multi-connection accept re-arm is still out of scope for this slice.

## Acceptance

- A `TcpBind { addr: 127.0.0.1:0 }` request succeeds on the Betelgeuse-backed
  runtime and returns a `local_addr` whose port is non-zero.
- The returned `local_addr` is the real address the client can connect to.
- `CallResult::TcpAccepted` includes the accepted stream's `peer_addr`.
- The TCP echo integration test binds to `127.0.0.1:0` again and still proves:
  - bind
  - accept
  - read
  - write
  - stream close
- The runtime does not use a side probe, TOCTOU reconstruction, or fake
  placeholder address values anywhere in the call path.
- `make verify` passes.

## Notes

- If this requires a small upstream Betelgeuse change, that is part of the
  slice. `tina-rs` is allowed to depend on a pinned fork or commit while the
  upstream change is in flight, as long as the runtime behavior is honest and
  the delta is documented.
