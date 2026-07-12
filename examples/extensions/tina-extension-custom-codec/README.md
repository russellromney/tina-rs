# tina-extension-custom-codec

A **custom codec** implementing the public `tina_codec::SyncCodec` seam, driving
a tiny socket service — with only public APIs.

## The hook

`SyncCodec` is the open sync-codec extension trait:

```rust
fn feed(&mut self, bytes: &[u8]) -> usize;
fn next_frame(&mut self) -> FrameDecision<Self::Frame, Self::Malformed>;
```

The built-in `Framer` trait is sealed; `SyncCodec` is how a third-party crate
adds its own codec. This crate's `SemicolonCodec` frames on `;` with a maximum
frame length and rejects an embedded NUL.

## What it proves

- **No I/O.** The codec is plain state on the server isolate. Tina owns the
  sockets, capacity, cancellation, and replay; the codec only turns bytes into
  frames.
- **Bounded.** A frame that exceeds `max_frame` before a delimiter surfaces
  `FrameDecision::Full` before allocating further — no unbounded buffer.
- **Replayable.** `feed` + `next_frame` are pure over the bytes seen, so the
  service runs identically on the simulator's Unix-domain rails (used here) and
  on a live socket. The smoke test runs the same bytes twice and asserts
  identical frames.

The service runs over `tina_sim`'s deterministic Unix-domain rails, so the test
is reproducible.

## Authoring status

The codec hook and both actors use the canonical public surface. Server and
client are event-only services registered through `register_event_service` and
started through `try_send_event`. One-shot Unix calls use
`then_service_event`; `UnixWriteAll` uses `next_service_event` and
`advance_service_event`. Application code never constructs a service envelope.

Bind, accept, connect, read, write, and close failures retain the exact
`CallError` and endpoint/stage in `CodecIoFailure`. Codec policy rejection
(`Full` or `Malformed`) remains distinct from transport failure.

## Run the smoke test

```sh
cargo test --manifest-path examples/extensions/tina-extension-custom-codec/Cargo.toml
```
