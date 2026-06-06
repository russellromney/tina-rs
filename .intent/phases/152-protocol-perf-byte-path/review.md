# Phase 152 Review

## Plan Review 1

Findings:

- [P2] The first plan could have become "add more rows" without changing any
  code. That would not be enough. The plan now requires byte-path migration for
  real protocol paths where compatibility helpers still copy or clone.
- [P2] "Equivalent workload" can become dishonest if the baseline is not
  semantically equivalent. The plan now allows Tina-only rows with a clear
  shape when a fair external baseline would be too large, and forbids fake
  semantic equality.
- [P2] Connection setup could be mistaken for a regression after Phase 151 made
  it visible. The plan now requires explicit setup vs steady-state rows and
  stage naming.
- [P2] WebSocket perf rows could accidentally test only frame helpers. The plan
  now requires the normal public session/app path.
- [P3] Linux proof could be implied by old Phase 151 evidence. The plan now
  requires at least one Linux/x86 sample for this phase, or a named pre-merge
  gap.

Decision:

- Plan is implementation-ready. It is not a planning phase. It builds rows,
  migrates byte paths, records setup cost, and updates docs with honest
  non-claims.

## Plan Review 2

Findings:

- [P2] The plan still had a stale premise: HTTP/2 and standalone WebSocket
  already use `tcp_read_buf` / `tls_read_buf` and `tcp_write_owned` /
  `tls_write_owned` on current `main`. The actual remaining byte-path work is
  protocol-internal allocation/copy, not broad migration off plain
  `tcp_read`/`tcp_write`. The plan now includes a current inventory and changes
  Rock 3 to reduce measured protocol-internal byte cost.
- [P2] "Find protocol paths" was a planning/audit step inside an implementation
  phase. The plan now pins the known files and copy/allocation families:
  HTTP/2 frame payload copies, header/trailer churn, gRPC frame allocation, and
  WebSocket payload/close copies.
- [P2] Row requirements were too soft. A worker could add one vague line and
  call it done. The plan now names first-form row labels for HTTP/2 and
  WebSocket, requires stable schema shape, and requires the perf test's label
  assertions to be updated so rows cannot silently disappear.
- [P2] Byte-path changes had weak direct proof. The plan now requires allocation
  ceilings/evidence for changed rows plus adversarial protocol proof for the
  exact edge the optimization could break: partial frames, flow control,
  trailers/status, fragmented WebSocket messages, close frames, or slow-peer
  pressure.
- [P2] "Linux proof if possible" was too weak for a performance phase. The plan
  now says Linux/x86 sample is required before merge readiness; if the builder
  cannot run it, the PR must remain non-final until the orchestrator does.
- [P3] Non-change guarantees were too broad. The plan now names what must not
  change: HTTP/2 flow control/reset/GOAWAY/trailers, gRPC status truth,
  WebSocket close/ping/pressure/stale-session truth, TLS half-duplex rules,
  `Runtime::step()` nonblocking behavior, and Phase 151 worker park behavior.

Decision:

- Plan is stronger and more Tina-like now. It is still big enough to matter,
  but it no longer sends the implementer on an open-ended audit. The phase must
  produce rows, reduce or honestly localize byte cost, and prove protocol
  semantics did not regress.
