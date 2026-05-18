# Adversarial Review Playbook

Use this when you want a serious bug hunt of Tina, especially after large
LLM-written or LLM-assisted Rust changes.

This is not product documentation. This is an agent prompt/playbook.

## Copy-Paste Invocation

```text
Read .intent/review/adversarial-review-playbook.md, then run the full Tina
adversarial review process against the current checkout. Produce findings using
the output contract. Include a track coverage map and a ranked top-10 fix list.
```

## Goal

Find real bugs.

Do not summarize the repo. Do not bikeshed style. Do not stop after grepping
for `unwrap`, `expect`, `panic`, or `todo`.

Assume some code may be LLM-written: plausible, idiomatic-looking, and tested on
happy paths, but wrong at boundary conditions, protocol law, failure paths,
capacity semantics, or cross-module invariants.

## Lessons From Prior Reviews

The useful prior reviews were PR #134, PR #135, and PR #136.

They found the most important bugs at these boundaries:

- HTTP/1 keepalive, chunked parsing, WebSocket framing, and HTTP/2 conformance.
- Cross-shard call routing, terminal replies, and local-command fairness.
- Bridge timeout/cancel/retry/backpressure semantics.
- Resource ownership on panic, drop, shutdown, and restart.
- Trace/replay determinism and proof-harness truth.
- Persistence, process, TLS, signal, and filesystem failure paths.

The second pass mattered. It found HTTP/2 `content-length` lies, duplicate
pseudo-header handling, core frame validation gaps, and multi-shard starvation.
Future reviews must include a second pass for "truth gaps", not just a broad
first pass.

## Review Process

### Phase 1: Find The Invariants

Before hunting individual bugs, write down the invariants the code claims or
implies. Check at least these:

- Every call settles exactly once with a typed terminal cause.
- `Full`, `Closed`, `Rejected`, timeout, and cancellation are never silently
  converted into each other.
- Bounded capacity means the real thing is bounded, not just a visible handle.
- Timeout and cancellation settle caller authority, but do not lie about
  external work that may still be running.
- Shutdown eventually settles, even when user code or external processes wedge.
- Parser output is only what downstream code can safely consume.
- Protocol headers and body lengths tell the truth.
- Live and simulator behavior match where the project says they match.
- Replay hashes are deterministic where they are used as proof.

### Phase 2: Run Specific Tracks

Review by track. Each track should trace real data/control flow across module
boundaries.

Track A: HTTP/1, chunked, WebSocket parser strictness

- Look for smuggling, ambiguous framing, non-minimal encodings, unchecked
  arithmetic, split-read state bugs, UTF-8 fragmentation bugs, and cap bypasses.

Track B: HTTP/2 and gRPC protocol law

- Check DATA, HEADERS, SETTINGS, CONTINUATION, PRIORITY, RST_STREAM,
  flow-control, pseudo-headers, `content-length`, `:authority`, forbidden
  connection headers, duplicate headers, stream lifecycle, and response body
  length truth.
- Treat RFC rules as an oracle. Ignored flags and "unknown extension" handling
  on core frames are suspicious.

Track C: Runtime calls, cross-shard delivery, and fairness

- Trace every call/send path to a terminal outcome.
- Look for dropped terminal replies, saturated reverse queues, starvation of
  local commands by remote traffic, timeout misattribution, exactly-once
  violations, shutdown starvation, and simulator/live drift.

Track D: Bridges and external work

- Review sqlx, AWS, reqwest, tokio/tower, RPC, and other bridge crates.
- Separate caller authority, admission capacity, external physical work, late
  results, retries, cancellation, and terminal classification.
- Look for bridge crates teaching different meanings for the same words.

Track E: Resource ownership and drop paths

- Review pools, leases, guards, permits, mailboxes, pending maps, cancellation
  cells, and restart paths.
- Look for leaked capacity, double release, stale tickets, ABA, panic leaks,
  and OneForAll restart leaks.

Track F: Persistence, process, filesystem, signals, and TLS

- Review crash truncation, append validation, snapshot rename/cleanup, process
  groups/job objects, inherited pipes, bounded kill/reap, global signal handler
  state, TLS worker serialization, TLS verification, and blocking syscalls under
  locks.

Track G: Determinism, simulation, and proof harness

- Look for wall-clock leakage, unstable ordering, fake randomness,
  poisoned-lock behavior, test harnesses that claim one failure shape but create
  another, reports that overwrite earlier errors, and saved replay artifacts
  that are not actually verified.

Track H: Macros and public API contracts

- Review proc-macro hygiene, crate rename support, `no_std` assumptions,
  textual heuristics, generated ABI stability, compile-fail coverage, and
  diagnostics that can drift.

Track I: Performance as correctness

- Look for hot-path O(n), synchronous observers on shard turns, unbounded
  queues, linear scans under restart churn, blocking sleeps in async contexts,
  and caps whose names imply more concurrency than exists.

### Phase 3: Second-Pass Truth Gap Review

After the first findings list, run a narrower second pass. Ask:

- What protocol rule did the code assume instead of enforce?
- Where does a field name promise truth that code never checks?
- Where does "bounded" mean bounded callers but not bounded work?
- Where does a terminal error become a timeout?
- Where can one traffic class starve another?
- Where do tests prove helper internals but not user-visible behavior?
- Where does live behavior differ from simulator behavior?
- What happens with duplicate, malformed, repeated, early, late, oversized,
  zero-sized, or split input?

This pass should be adversarial and specific. Prior Tina reviews found major
bugs here.

## Output Contract

For each finding, include:

1. Severity: Critical, High, Medium, or Low.
2. Confidence: High, Medium, or Low.
3. File and line reference.
4. Violated invariant or protocol rule.
5. Concrete bug.
6. Why it can happen in real use.
7. Minimal reproduction or failing test idea.
8. Small idiomatic Rust fix.
9. Whether this looks like an LLM-style bug pattern.

Do not include style-only findings unless they hide a real bug.

If a suspected issue is false or already fixed, record the proof and the test
name. Do not silently drop it.

## Final Report Shape

Produce:

- Short summary by risk boundary.
- Ranked top 10 fixes.
- Full findings list.
- Invariants violated.
- Areas needing deeper review.
- Suggested fuzz, property, and integration tests.
- Track coverage map: which track found which findings.

Keep the report concrete. The best finding names the exact bad state, the line
that allows it, and the test that would fail.
