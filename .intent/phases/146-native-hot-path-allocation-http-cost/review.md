# Review: Phase 146 Native Hot-Path Allocation And HTTP Cost Reduction

## Plan Review 1

Reviewed against `/Users/russellromney/Documents/Github/idd` on
2026-06-02.

### Strong

- The phase names a concrete build target: owned reusable I/O buffers,
  HTTP/1 server/client/keepalive migration, perf evidence, and boundedness
  proof.
- It avoids a borrowed-buffer API that would cross Tina's owned effect
  boundary.
- It names the main user-shaped proof: `examples/systems/perf_native` rows, not
  helper-only unit tests.
- It keeps the performance claim honest: local evidence, semantic labels,
  before/after rows, and no production brag.

### Findings

**P1: Non-change rules were implicit.**  
The first draft said "do not cheat," but IDD asks every phase to say what must
not change. That matters here because an optimization could accidentally weaken
HTTP parser strictness, body-pressure accounting, TLS verification, or replay
tags. Fixed by adding `What Will Not Change`.

**P2: Blast-radius proof needed its own section.**  
The first draft listed many tests, but did not separate direct proof from
blast-radius proof. Fixed by adding `Proof Matrix`, including HTTP bad-input,
chunked/body lifecycle/WebSocket, TLS, and proof-fast coverage.

**P3: File ownership was too implicit.**  
The first draft named rocks but not enough likely files. Fixed by adding
`Likely Files` so implementation sessions know the expected blast radius.

**P4: Post-proof artifact updates were not named.**  
IDD says `SYSTEM.md` is current proved truth and should update only after
proof. Fixed by adding `After Proof`, including changelog, perf README,
SYSTEM, and `commits.txt`.

### Remaining Review Focus

Implementation review should especially check:

- whether the new I/O buffer API returns/drops buffer ownership consistently on
  errors;
- whether HTTP write optimization still handles partial writes;
- whether faster paths still expose service `Full`, body-cap `Full`, timeout,
  and shutdown truth;
- whether TLS, if touched, preserves cert/SNI/ALPN verification;
- whether perf output distinguishes worker-thread allocation from process-wide
  allocation.
