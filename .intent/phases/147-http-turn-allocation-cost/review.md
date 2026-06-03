# Phase 147 Review

## Hostile Pass

- Evidence shape is now less lie-prone: process rows include allocated bytes,
  and history rows include platform, arch, and profile.
- HTTP hotpath reports are real public-path probes: listener, socket client,
  service isolate, and trace observer.
- The first allocation cleanup is intentionally small: remove benchmark-client
  request formatting noise and pre-size common HTTP/1 request/response encoder
  buffers. It does not claim production performance.
- Small no-metrics buffered HTTP/1 responses now coalesce head+body into one
  TCP write. The guardrail is explicit: larger buffered bodies stay split, and
  metrics-enabled bodies stay split so body-pressure accounting does not lie.
- Deep hostile review found one correctness hole in the terminal write-close
  rail: it closed the stream without cancelling sibling pending reads the way
  `tcp_close_stream` does. Fixed live and sim so terminal write-close is a real
  close operation: sibling pending reads get `ResourceClosed`, the terminal
  write-close call itself completes normally, and in-flight call state is
  reclaimed.
- Pressure proof is attached: oversized HTTP bodies produce typed `full`,
  body-pressure surfaces, and drained final current.

## Still True

- HTTP close/fixed-body paths still take dozens of observed stages.
- Four sequential keepalive requests still take many observed stages. The next
  big cost is turn/scheduling shape, not the old read/write buffer clone.
- Linux perf rows were not produced in this local pass.
- `make perf-check` will warm history until enough Phase 147 rows exist for the
  current platform.
