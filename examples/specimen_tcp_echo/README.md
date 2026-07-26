# specimen_tcp_echo

A standing TCP echo server built from Tina isolates: one connection is
one isolate, each isolate reads a chunk and writes the identical bytes
back (retrying partial writes) until EOF. The same connection source
runs live on `LocalSystem` over a real loopback socket and
deterministically inside `tina_sim::Simulator` replayed from a seed.

## Run

Start the standing server (binds an ephemeral loopback port, serves
until you press Enter):

```sh
cargo run --manifest-path examples/specimen_tcp_echo/Cargo.toml
# then, in another terminal:
#   nc 127.0.0.1 <port>
```

Run the self-terminating bounded-mailbox demo instead: a host producer
bursts a bounded worker and the surplus comes back as a typed `Full`:

```sh
cargo run --manifest-path examples/specimen_tcp_echo/Cargo.toml -- load-shed
```

Run the live loopback, simulator replay, and README-sync tests:

```sh
cargo test --manifest-path examples/specimen_tcp_echo/Cargo.toml
```

## Read

- [`src/lib.rs`](src/lib.rs) — the echo listener/connection isolates,
  the `echo_round_trip` live path, and the `run_load_shed` bounded
  producer demo.
- [`src/main.rs`](src/main.rs) — the standing-server and load-shed
  entry points on `LocalSystem`.
- [`tests/sim_echo.rs`](tests/sim_echo.rs) — the same connection source
  inside the deterministic simulator.
