# Eiffel Examples

`examples/` is the home for Eiffel: paired Tokio-vs-Tina implementation
comparisons for common use cases. These are not Tina crate tests and not
benchmarks. They are ergonomics and functionality probes that should help us
discover where Tina is safer, awkward, incomplete, broken, or pointing at a
better model.

Cross-cutting ergonomic findings — patterns that show up in more than one
comparison and the API/runtime suggestions they imply — live in
[`FINDINGS.md`](FINDINGS.md). Per-comparison ergonomic notes stay in each
comparison's own `README.md`.

Each comparison gets its own directory and should aim for:

- separate Tokio and Tina implementations;
- separate runnable process modes for each side;
- a shared protocol and report format;
- a load-driver mode;
- overload knobs that work on macOS for the ergonomics pass;
- later wrapper support for Linux/Fly memory and CPU constraints;
- notes about what the comparison teaches.

Current comparisons:

| Directory | Status | Purpose |
|---|---|---|
| `eiffel_real_io_chat` | First specimen | Slow-consumer chat/fanout over real TCP. |
| `eiffel_mini_keyspace` | Built | Tiny Redis-shaped key/value service over real TCP; tests request/reply continuations and store-isolate ergonomics. |
| `eiffel_axum_counter` | Built | Stateful HTTP counter over axum; tests `tina-tokio-bridge` ergonomics and HTTP-shaped pushback. |
| `eiffel_ws_room` | Built | WebSocket broadcast room with two clients; tests bridge-hosted bidirectional sessions and subscriber pruning. |
| `eiffel_mux_client` | Built | Tina as a multiplexed *client* against a Tokio TCP responder; proves out-of-order arrival and tests correlation/parsing in an isolate. |
| `eiffel_cpu_run` | Built | Wrapper runner that re-executes any built comparison under N CPU-busy spinner threads; reports baseline vs contended wall-clock and exit status. |
| `eiffel_mem_run` | Built | Wrapper runner that re-executes any built comparison under a series of `RLIMIT_AS` caps (Linux real, macOS best-effort no-op); reports per-tier duration and exit status. |
| `eiffel_supervised_worker` | Built | Worker that panics on poison messages; compares Tokio's hand-rolled `catch_unwind`/respawn loop against Tina's supervisor + restart budget. |
| `eiffel_persistent_counter` | Built | Counter that survives restart via runtime-owned snapshot + journal; compares against a Tokio file-write story. |
| `eiffel_replay_dst` | Built | Same workload run twice under `tina-sim` with one seed; demonstrates deterministic replay versus the Tokio shape that cannot answer the question. |
| `eiffel_outbound_fetch` | Built | "Go fetch these endpoints and aggregate" — Tina as a TCP/DNS *client*, compared to `reqwest`/`hyper`. |
| `eiffel_graceful_shutdown` | Built | Long-lived service with in-flight work receives SIGINT; compares `tokio::signal` + manual drain against Tina's signal capture and bounded shutdown story. |

All seven items from the original ROADMAP Eiffel backlog are built, and
all five forward-backlog items added in this round are also built.
