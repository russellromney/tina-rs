# Phase 153 Review

## Plan Review 1

Findings:

- [P2] The plan could still turn into another harness phase if "measure" is
  treated as the work. The Done section now requires at least three real
  protocol code paths to allocate/copy less, and Rock 1-4 name the exact files
  and transformations.
- [P2] HTTP/2 flow-control accounting is easy to break while moving payloads.
  Rock 1 now requires the owned DATA helper to preserve both unpadded payload
  length and wire payload length, with bad-padding tests.
- [P2] Consuming `HttpResponse` by value could accidentally break streaming
  response setup or fallback error responses. Rock 2 now names every response
  body variant and requires Stream/ChunkedStream/WebSocket behavior to stay
  intact.
- [P2] Replacing `VecDeque<u8>` with a cursor buffer could leak resident memory
  after a large request body. Rock 3 requires a bounded owned buffer shape and
  tests multi-frame POST plus flow-control resume.
- [P2] WebSocket dual-delivery is a compatibility/ergonomics choice, not just a
  copy bug. Rock 4 now chooses the Tina-shaped fix: the protocol owner emits
  one session-rich app event per wire event and stops sending duplicate legacy
  compatibility messages.
- [P2] Turn-count work can become "skip service truth." Rock 5 now only allows
  removing turns that do not cross a policy boundary and repeats that typed
  `Full` / `Closed` / `Timeout` outcomes must remain visible.
- [P2] Rock 5 allowed a no-op outcome by saying a PR could merely prove all
  remaining turns were real. That is Phase 152 behavior, not Phase 153. The
  plan now requires at least one stage-count reduction.
- [P3] Performance evidence can overclaim from one macOS run. Rock 6 requires
  macOS and Linux release rows and tells docs not to make a production
  performance claim.

Decision:

- Implementation-ready. This is not a planning phase. It names the current hot
  paths, the changes to make, and the proof required to call them real.
