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

## Plan Review 2

Findings:

- [P2] "At least three changed protocol paths" was too easy to satisfy with
  helper-level wins while the public rows stayed ugly. The Goal/Done sections
  now require HTTP/2 steady-state, one gRPC row, and one WebSocket row to get
  cheaper in public API use.
- [P2] Rock 2 allowed an implementation to remove the response-body clone but
  keep per-DATA-frame `Vec` allocation through a generic "move chunks" phrase.
  The plan now requires a direct frame writer (`encode_frame_into` /
  `push_data_frame` shape) for multi-frame responses.
- [P2] The `VecDeque<u8>` replacement could keep huge consumed buffers resident
  forever. Rock 3 now requires compaction/drop when a stream finishes, with
  multi-frame POST proof.
- [P2] WebSocket compatibility could keep the bad path as the default. The plan
  now permits a breaking cleanup: one connection-owner event per wire event,
  session-rich by default, no duplicate legacy emission.
- [P2] Turn-count reduction could be faked in the perf harness. Rock 5 now says
  the removed turn must be in runtime/protocol code or a canonical public
  specimen path.
- [P2] Allocation wins can hide latency regressions. Rock 6 now requires a
  before/after table with process allocations, allocated bytes, p50/p90/p99,
  and stage count; if latency worsens, the PR must fix it or mark the phase
  incomplete.
- [P2] The plan assumed a gRPC perf row exists. If Phase 152 did not land one,
  that would let the implementer defer gRPC proof. Rock 3 now requires adding a
  minimal public unary gRPC row first, recording it as the before row, and then
  improving it in the same phase.
- [P3] "Compile-time proof if possible" was soft. Rock 4 now requires a
  compile/doctest-style proof for the session-rich WebSocket app path.

Decision:

- Stronger. The phase now demands public-row improvement, direct byte-writer
  changes, no compatibility-tax default, and honest latency reporting.

## Plan Review 3

Findings:

- [P3] The plan was strong but too wordy. Goal, Done, proof, and evidence
  repeated the same contract in different words. The plan is now shorter and
  uses one clear top-level rule: HTTP/2 steady-state, one gRPC row, one
  WebSocket row, and one stage row must improve.
- [P3] Several sections used soft phrases like "required shape" and long
  explanatory paragraphs. They now use simple `Rules` / `Proof` blocks.
- [P3] The plan buried the important grug line. It now says directly: rows are
  not performance; faster code is performance.

Decision:

- Simpler without weakening the gates. The implementation work is still large,
  but the plan now reads like a build checklist instead of a memo.
