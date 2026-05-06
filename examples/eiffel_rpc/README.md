# eiffel_rpc — phase 052 Rock 7 RPC overload comparison

Same wire protocol on both sides ([`tina_rpc`] frame format), same workload
(one client connection, M parallel requests against a server with bounded
in-flight). Two implementations:

- `tina_impl` — Tina framed calls first form. Connection isolate has
  `max_in_flight = 1`; over-cap requests get a server-reported wire
  `Error(Full)` frame immediately.
- `tokio_impl` — Tokio reference. Server decodes frames, dispatches each
  one to an unbounded `mpsc` channel. A single worker drains the channel
  in order. The unbounded queue accepts every request and the worker
  replies to all of them; the wire never carries a `Full` error.

The point: framed first-form makes overload **wire-visible**. The Tokio
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
