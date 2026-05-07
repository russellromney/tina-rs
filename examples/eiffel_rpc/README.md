# eiffel_rpc — Tina-vs-Tokio RPC overload comparison

Same wire protocol on both sides ([`tina_rpc`] frame format), same workload
(one client connection, M parallel requests against a server with bounded
in-flight). Two implementations:

- `tina_impl` — Tina framed RPC, raw byte API. Connection isolate has
  `max_in_flight = 1`; over-cap requests get a server-reported wire
  `Error(Full)` frame immediately.
- `tokio_impl` — Tokio reference. Server decodes frames, dispatches each
  one to an unbounded `mpsc` channel. A single worker drains the channel
  in order. The unbounded queue accepts every request and the worker
  replies to all of them; the wire never carries a `Full` error.

The point: framed RPC makes overload **wire-visible**. The Tokio
reference silently buffers behind an unbounded queue. Same workload,
observably different behavior.

## Run

This example is workspace-excluded (matching the other `eiffel_*`
examples), so run it via `--manifest-path` from the repo root or `cd`
in first:

```sh
# From the repo root:
cargo run --manifest-path examples/eiffel_rpc/Cargo.toml                       # both sides
cargo run --manifest-path examples/eiffel_rpc/Cargo.toml -- tina               # tina only
cargo run --manifest-path examples/eiffel_rpc/Cargo.toml -- tokio              # tokio only
cargo run --manifest-path examples/eiffel_rpc/Cargo.toml -- compare 8          # 8-burst

# Or from inside the example dir:
cd examples/eiffel_rpc && cargo run -- compare 8
```

Each side prints one line of the same shape:

```
comparison=eiffel_rpc side=tokio burst=4 ok=4 full=0 other=0
comparison=eiffel_rpc side=tina burst=4 ok=1 full=3 other=0
```

`ok` counts wire `Reply` frames received. `full` counts server-reported
`Error(Full)` frames. `other` counts any unexpected frame kind / decode
error. The interesting difference is on the `tina` row: `full=3` is the
visible-overload signal that the `tokio` row never produces.

## Typed surface (`tina_typed_impl`)

The `#[tina_rpc::service]` macro removes hand-rolled byte plumbing.
The same Echo workload, written two ways:

**Raw bytes** (`tina_impl.rs::EchoService`):

```rust
struct EchoService;

#[tina_runtime::isolate(message = ServiceCall, reply = ServiceReply, shard = EiffelShard)]
impl EchoService {
    fn handle(&mut self, msg: ServiceCall, _ctx: &mut Context<'_, EiffelShard>) -> Effect<Self> {
        reply(ServiceReply::Ok(msg.payload))
    }
}
```

The handler ignores `msg.method`; adding a second method means
`match msg.method.as_str()` plus per-arm decode/encode/error mapping.

**Typed** (`tina_typed_impl.rs::Echo`):

```rust
#[tina_rpc::service]
trait Echo {
    fn ping(&mut self, payload: Vec<u8>) -> Vec<u8>;
}

struct EchoState;
impl Echo for EchoState {
    fn ping(&mut self, payload: Vec<u8>) -> Vec<u8> {
        payload
    }
}
```

Method name comes from the `fn`. Per-method JSON encode/decode is
generated. `ServiceReply::UnknownMethod` / `Decode` / `Internal`
mappings are generated. Adding a second method is one line.

The typed module compiles end-to-end and registers with the same
`SingleService` adapter and `Registry` as the raw side; the contract
test in `tests/contract.rs` continues to pin the raw side's
`ok=1, full=N-1` shape so the byte API stays proved.

**Remaining pain**:

- Wire shape is positional tuples (`(arg1, arg2, ..)` → JSON `[a, b]`).
  Adding/removing args changes the array length and breaks existing
  clients silently. A struct-shaped, name-keyed payload would be
  additive-friendly; the wire carries no public compatibility promise
  today.
- The typed `Echo::ping(payload: Vec<u8>) -> Vec<u8>` JSON-wraps the
  bytes, so the typed-side wire bytes differ from the raw side. The
  comparison harness's shared `drive_client` sends raw frame payloads;
  to round-trip the typed side end-to-end, callers should reach for
  the `tina-rpc-tokio` `BridgeClient` which uses the macro-generated
  `*_request` / `*_decode_reply` helpers.
- Bridge `await` API lives in `tina-rpc-tokio::BridgeClient`. Retry
  policy is in `tina-rpc-tokio::call_with_retry`. Tracing fields are
  emitted from the bridge under the `tracing` feature.
