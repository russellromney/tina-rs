# Adversarial Review — 2026-06-09

Target: a clean worktree at `origin/main` HEAD `0cd6a31`. This review exists
because the 2026-06-08 review's deep dives ran over a tree 125 commits behind
`main`; re-verifying those findings filtered stale ones but never adversarially
read the ~125 commits of new code. This is that fresh read.

Method: nine parallel deep-dive agents over playbook tracks A–I
(`.intent/review/adversarial-review-playbook.md`), a second-pass truth-gap
agent aimed at the seams between tracks, three carve-out deep-dives covering
the 06-08 "areas needing deeper review" that no track owned, and a final
adversarial verification pass in which five independent verifier agents tried
to refute every Medium-or-higher finding.

Verification result: **23 of 23 Medium+ findings confirmed, zero refuted.**
One severity adjustment (E2 High→Medium on reachability), one magnitude
correction (I-NEW-3 memmove arithmetic), several findings strengthened
(extra leak sites, an empirical CPU probe). Every finding below survived a
dedicated refutation attempt.

Full per-track findings: `.intent/review/2026-06-09-tracks/track-{A..I}.md`,
`second-pass.md`, `carveout-{http2,tracing,process-aws}.md`.

## Summary by risk boundary

- **The perf wave fixed each bug at one site and missed its twin (the
  dominant cluster).** The multi-shard worker traded its 1 ms sleep for a
  literal busy-spin — no blocking wait is reachable while any work is merely
  *pending*; one idle listener or sleep timer burns a core, and the in-code
  comment claiming it "blocks inside the betelgeuse io_loop (verified)" is
  false (C-1, empirically proven). The send/call ingress path still does an
  O(N) scan per message while the `entry_indexes` map sits unused beside it
  (C-5). Caller-build, deferred-settle, and stop-churn have the same
  indexed-on-one-side shape (I-NEW-4/5/6). The HTTP/2 *server* got the
  cursor+single-drain fix; its client/WS/gRPC twins didn't (I-NEW-3).
- **HTTP/2 server stream teardown was written per-site, and every copy
  forgot a different obligation.** Response sources never cancelled (SP-A),
  accepted upload flow-credit never returned (SP-C — the server eventually
  GOAWAYs the connection blaming an innocent peer), rejected DATA never
  credited (B11, five branches), DATA-on-closed-stream kills the whole
  connection where the client twin correctly RSTs one stream (B8-residual),
  failed flushes wedge the stream (SP-E), three resets are invisible to the
  fact surface (SP-F). One `remove_stream`/reset funnel fixes the family.
  The client has its own twin: abandoned streamed responses are never
  reaped, bricking a pooled connection after 64 abandons (CH-1).
- **HTTP client isolates leak state across requests.** The keepalive
  connection generation-stamps only `Deadline`; stale `Connected`/`Wrote`/
  `Read` continuations from a timed-out request leak into the next request
  on the same pooled isolate — worst case delivers request N's response to
  request N+1, panic case permanently zombifies the pool slot (E1; a slow
  SYN-ACK suffices, no tight race). The one-shot client has the same
  vectors plus a never-cancelled deadline (E3).
- **Protocol truth gaps remain on the edges.** The WebSocket *client*
  ignores FIN entirely — a conformant fragmenting server gets its partial
  fragment surfaced as a complete message, then the continuation frame
  forces a protocol close (A1). The gRPC streaming client synthesizes `Ok`
  when END_STREAM carries no `grpc-status`; its unary twin is honest (SP-D).
- **Trace/proof surfaces lie at shutdown and at the side doors.** The
  multishard shutdown drain panics on any cross-shard effect, destroying the
  shard's entire trace at exactly the moment it's evidence (CT-1 — an
  existing test quiesces first with a comment acknowledging the panic).
  The buffered observer has no flush/join yet the proof docs prescribe
  passing its drop count to `snapshot_complete` (CT-2); `trace_dropped` is
  hardcoded `None` so bounded retention has zero honest surfaces (CT-3);
  the capture *builder* path skips the G1 multishard fail-closed gate that
  the `RunCapture` path enforces (TG-1); `complete_trace()` launders a
  retention-truncated suffix as complete (TG-2); shrinkers never confirm
  the original case fails before re-pinning constants from a green run
  (TG-3).
- **Lifecycle bookkeeping leaks.** `child_records` has no removal site in
  the crate: stopped children are permanently un-GC-able, the GC scan
  latches on forever, entries leak unboundedly under spawn churn (C-3,
  compounding I-NEW-6). `ChildStop`/`SpawnCancel` envelopes are silently
  droppable under pair-queue pressure → permanently orphaned remote
  children (C-4). A registration bootstrap can be mis-paired with a
  harvested cross-shard call's context (C-2). Calls queued at callee-stop
  settle as `Timeout` instead of a Closed-class terminal (E5).
- **One build break.** `tina-sqlx-bridge` does not compile with
  `--features tracing` — missing `CommitAmbiguous` match arm, proven by
  `cargo check` (D-1). CI doesn't build that feature.

## Top 10 fixes (ranked by severity × confidence × blast radius)

| # | Sev | Finding | Location (`main` 0cd6a31) | Fix |
|---|-----|---------|---------------------------|-----|
| 1 | High ✓emp | C-1/I-NEW-2: multi-shard worker busy-spins whenever any work is in flight; `yield_now` loop + zero-timeout lane polls; no blocking wait reachable; comment claims otherwise. Probe: pending 30s sleep burned ~20% core (macOS, throttled; ≈full core expected on Linux) | `tina-runtime/src/threaded_multi_shard.rs:1100-1140`; proof `vendor-betelgeuse/io/darwin.rs:881`, `io/linux.rs:1037` | Block in the io_loop with a computed timeout like the single-shard `park_io` path (`dispatch.rs:1993`, `threaded.rs:1815`); fix the false comment |
| 2 | High ✓ | E1: keepalive isolate generation-stamps only `Deadline`; stale `Connected`/`Wrote`/`Read` cross into the next request → wrong response delivered or `expect` panic → permanent zombie pool slot (no supervisor, no health check, docs mandate "always release Reuse") | `tina-http/src/keepalive.rs:214-218,384-434,660-719,870-895` | Stamp every continuation with the request generation and drop stale ones; cancel/abandon pending transport ops in `fail_request` |
| 3 | High ✓ | A1: WS client ignores FIN, no reassembly — partial fragment surfaced as complete `Text`/`Binary`, following `0x0` continuation → protocol close. Conformant fragmenting peers corrupt data | `tina-http/src/websocket_client.rs:531-568` (correct server twin: `connection.rs:1829-1890`) | Port the server's fragmentation reassembly to the client consumer |
| 4 | High ✓ | CT-1: multishard shutdown drain uses plain `step()` whose remote-route closure `panic!`s on any `QueuedRemoteEnvelope` → worker dies pre-`ThreadedWorkerExit`, shard trace destroyed. Known: `multishard_fairness.rs:426-429` quiesces around it | `tina-runtime/src/threaded.rs:1838-1849` via `threaded_multi_shard.rs:1091`; panic at `dispatch.rs:225-263` | Drain with the worker loop's real `route_remote` closure instead of `step()` |
| 5 | High ✓ | C-5/I-NEW-1: every local send and isolate-call dispatch does `entries.iter().position()` O(N) per message; `entry_indexes` is provably substitutable at both sites (verified coherent across all mutations) | `tina-runtime/src/remote.rs:327-331,599-602` (callers `dispatch.rs:645,736,1305,1465,2914`) | Use `entry_indexes.get(&id)` at both sites (one line each); generation check unchanged |
| 6 | Med-High ✓ | C-3: `child_records` never shrink (zero removal sites in crate) → stopped children permanently un-GC-able, `has_stopped_entries` latches true → full GC scan every step forever + unbounded entry leak under spawn churn | `tina-runtime/src/dispatch.rs:3251-3253,2392-2469`; push sites `registration.rs:712,725,905,932` | Remove child records when child and parent are both stopped/GC'd; or index records by child and prune in GC |
| 7 | Med-High ✓ | SP-A + SP-C + B11 + B8-residual + SP-E + SP-F: the HTTP/2 server stream-teardown family — sources stranded, accepted upload credit kept (server self-GOAWAY ratchet), five reject branches leak peer window, DATA-on-closed kills connection, flush-failure wedges, silent resets | `tina-http/src/http2/server.rs:1004-1134,1257,1700-1730,1800-1930,2003-2045,2201-2208` | One `remove_stream`/reset funnel that cancels the source, returns summed `flow_credit`, credits rejected `flow_len`, RSTs instead of GOAWAYs, transitions state, emits the fact |
| 8 | Med ✓built | D-1: `tina-sqlx-bridge --features tracing` fails to compile (E0004: `emit_replied` missing `PgTransactionOutcome::CommitAmbiguous` arm) | `tina-sqlx-bridge/src/worker.rs:460` | Add the arm; build feature combos in CI |
| 9 | Med ✓ | SP-D: gRPC streaming client maps missing `grpc-status` at END_STREAM to `Ok` (`unwrap_or_else(Ok)`); unary twin honestly errors `MissingTrailers`. Trailer-stripped/half-closed streams read as clean success | `tina-http/src/grpc_client.rs:484-485` (honest twin `:325-335`) | Missing status = `Malformed(MissingTrailers)`, like unary |
| 10 | Med ✓ | C-2: registration bootstrap bypasses the positional `call_contexts` queue; a cross-shard call harvested behind it binds the call's context to the bootstrap → bootstrap dispatched as the call, real call settles `ReplyAbandoned`. Host-command bursts hold the window open | `tina-runtime/src/registration.rs:303-316,794-807,481-510`; harvest `remote.rs:632` | Enqueue the bootstrap through the same path with a `None` context |

## Full findings list (beyond the top 10)

Medium:

- **B11 ✓** `server.rs:1019-1121` (+5th site `:1073-1087`) — every reject
  branch in `handle_data` skips the connection credit; client twin documents
  the correct rule (§6.9.1). Part of fix #7.
- **B8-residual ✓** `server.rs:1016-1018` — closed-stream DATA → GOAWAY;
  `ActiveStream.reset` is dead code (never set). Part of fix #7.
- **E2 ✓ (High→Med)** `tina-mailbox-spsc/src/lib.rs:178-198` +
  `threaded.rs:1795-1820` — wake hook computes `was_empty` pre-publish; a
  racing consumer drain suppresses the wake while the worker parks with no
  timeout. Loom-style enumeration confirms; `park_needs_repoll` excludes
  mailboxes, so soundness rests entirely on the broken wake. Medium only
  because no in-tree code can construct the racy configuration today — but
  `tina/src/isolate.rs:159-168` *prescribes* the unsound recipe to custom
  mailbox authors. Fix the wake condition, the trait doc, and add the loom
  model. Becomes High the day spsc is wired as direct ingress.
- **E3 ✓** `tina-http/src/client.rs:64,143-188,258-259,296-319` — one-shot
  client: un-generation'd `Deadline` (never cancelled — request A's deadline
  deterministically times out request B back-to-back), same stale
  `Connected`/`Wrote` vectors as E1.
- **E5 ✓** `dispatch.rs:300-309,2450-2456` — calls queued at callee-stop are
  drained as `MessageAbandoned`, context dropped with no Drop impl → caller
  settles via full-budget `Timeout` instead of Closed-class. Cause
  conversion, the project's own named invariant.
- **C-4 ✓** `threaded_multi_shard.rs:1243-1247`, `multi_shard.rs:377-394`,
  `dispatch.rs:2651-2663,2710-2727` — `ChildStop`/`SpawnCancel` not in the
  preserved-terminal set; dropped silently on Full (one path has a
  second-chance that is itself droppable) → permanently orphaned remote
  children.
- **CH-1 ✓** `http2/client.rs` (no Drop/reaper; cap `:1202`) — abandoned
  streamed responses hold slot+buffer+window until connection close; 64
  abandonments brick the pooled connection (`admit_stream` → `Full`
  forever). Client twin of SP-A/SP-B.
- **SP-B ✓** — success-path streamed-response source has no terminator/owner;
  completed responses leak the source isolate by design-gap.
- **TG-1 ✓** `tina-sim/src/dst/replay_case.rs:734-881,850` — all three
  public capture entry points use the builder, which pins an order-sensitive
  hash with zero shard inspection; the G1 fail-closed gate guards only
  `RunCapture` (`live_replay.rs:395-398`). Loud-but-misattributed at replay.
- **TG-2 ✓** `threaded.rs:1291-1316`, `threaded_multi_shard.rs:592-613` —
  `complete_trace()`/`TraceSnapshot::complete` label the retention-truncated
  suffix complete; honest `trace_for_proof` has zero non-test callers.
  Latent (in-tree always runs `Full` retention) but one config line away.
- **CT-2 ✓** `tina-runtime/src/observer.rs:58-94` — `BufferedTraceObserver`
  has no flush/join (drain thread detached); `dropped_count()==0` ≠ drained,
  yet `live_replay.rs:23-27,144-147` documents passing exactly that to
  `snapshot_complete`. Both G1 gates pass on a prefix.
- **CT-3 ✓** `live_report.rs:268` — `trace_dropped` hardcoded `None`, no
  writer exists; the runtime's real drop counter (`lib.rs:782`) reaches no
  live surface. Bounded retention has zero honest reporting.
- **H11 ✓** `tina-macros/src/lib.rs:333-343` — split-mode authority rail:
  conditional early `return tina::noop()` typechecks (generated fn returns
  `Effect<Self>`), skipping the `RequestEffect` gate the fixture suite
  advertises as compile-time. Runtime backstop settles the call
  (`ReplyAbandoned`) — honesty gap, not a hang. No fixture covers the shape.
- **I-NEW-3 ✓ (corrected)** `http2/client.rs:1863,1877`, `websocket.rs:880`
  (driven by `connection.rs:1742-1753` and `websocket_client.rs:527`),
  `grpc.rs:644` — per-frame `drain()`; server twin has the cursor fix
  (`server.rs:700-705`). Magnitude corrected: worst ~14.5 MB memmove per
  16 KiB read (h2) / 300–900× CPU amplification on untrusted WS input.
  Medium for WS server; Low-Med for h2-client/gRPC.
- **I-NEW-4 ✓** `dispatch.rs:467-473` — `build_message_caller` linear-scans
  `pending_isolate_calls` per delivered local call; the I3 index
  (`pending_isolate_call_indexes`) sits unused at this site. O(P²)/wave.
- **I-NEW-5 ✓** `tina-runtime/src/deferred.rs:60-87`; `dispatch.rs:1045,1828`
  — `take_by_handle`/`take_by_local_call_id` O(P) on every deferred settle
  and every cancel/timeout, slot or no slot.
- **I-NEW-6 ✓** `dispatch.rs:1756-1791,3241-3278` — per-stop full partition +
  unconditional index rebuild of all pending calls (even when the stopper
  owns none); four-Vec rescan per stopped-blocked entry per step. C-3 makes
  the latter regime permanent.

Low-Med / Low (verified by their track agents; not independently
re-verified): SP-E `server.rs:1841-1858` (flush-failure wedge; part of
fix #7), SP-F (three silent reset sites), TG-3 `shrink.rs:172-215,348-406`,
`byte_replay.rs:230-265` (shrinkers re-pin constants from a possibly-green
run), TG-4 `byte_replay.rs:677` (UTF-8 boundary panic in loader), TG-5
`tina-tracing/src/timeline.rs:337,339,791` (timeline assumes global ids
post-G1), TG-6 `perf.rs:124-152` (perf JSON lacks `leak_checked`), D-2
(rpc-tokio shim starvation > deadline period misclassifies as `Timeout`),
D-3 `tina-tokio-bridge/src/lib.rs:485-492` (`Timeout→Closed` in public
`From`), D-4 `dispatch.rs:1669-1702` (CancelCall completions droppable,
contradicts docs/mailbox-capacity.md), D-5 (cancel double-counted in
metrics), D-6 `tina-aws-bridge/src/core.rs:31-58` (blocking 1 ms-sleep
drain; wedges if called on own shard), E4 (force-close keeps idle handles
until pool drop), B9/B10 (PRIORITY frame-size law; `:authority`/Host
mismatch), H9/H10/H12–H17 (raw-ident + hygiene residue: `r#deadline`
bypasses reserved-name check, `__tina_*` not actually non-collidable,
visitor false-positives, split-mode silently ignores user `handle_call`,
`mut`/`ref` patterns stripped, generic-trait/`#[cfg]` holes, `r#` leaking
into wire method names, decorative declared types), I-NEW-7/8/9
(supervision-path scans; unbounded default `TraceRetention::Full`
undocumented; `WaitList` per-key-limit double scan), F-A `process.rs:446`
(group-kill via fork/exec'd `kill` CLI can silently no-op under fd
exhaustion; use `libc::killpg`), F-B (TLS timeout leaves recv in flight;
self-healing), AWS-Q2-A (SNS/DynamoDB/Secrets have zero slot-lifecycle
tests; five copy-pasted workers), Q1-a (ProcessRun rustdoc never states
group-kill semantics), Q1-b (non-unix drain can't interrupt blocked read —
dead code today, real when a Windows backend lands).

## What was checked and found clean (do not re-file; proofs in track files)

The entire 2026-06-08 fix wave re-verified live at this HEAD by the track
agents in their areas: SP1/SP2/SP3/B1–B7/I10, G1–G5 front doors, D2 slot
conservation under every overflow ordering (including the flaky-test
judgment call — admission-`Full` carries no slot), D1/D3, A-F3 keepalive
over-send retire, F1–F6 (F6's drain budget cannot be double-spent), H7/H8
core, I1–I9 internal correctness, E1/E2-prior restart containment, pool
core (exactly-once handout, cancel-race recovery, ABA, force-close).

Fresh disproofs of note: HTTP/2 frame-size law fully enforced pre-buffer on
both sides with tests (`frame.rs:101`, `server.rs:714`, `client.rs:1853`);
client connection-window credit granted on receipt — abandoned streams
cannot starve siblings (`client.rs:2172-2175,1892-1897`); single-shard
readiness park lost-wakeup disproven (level-triggered doorbell protocol);
cross-lane harvest theft fixed and verified; clean-path multishard shutdown
trace collection sound (joins all signaled workers, honest `(shard,id)`
sort); `trace()`-vs-shutdown races fail visible via `missing_shards`;
install.rs double-install atomic; non-unix process supervision out of scope
three provable ways (CHANGELOG, CI matrix, betelgeuse backends); all five
AWS workers slot-exactly-once on eight terminal paths each, D2 fix applies
to them automatically via `enqueue_call_continuation`; HTTP terminal-cause
→ status mapping clean; RPC wire duplicate/malformed-id handling clean; WS
server outbound backpressure clean; keepalive `must_retire` vs pool
capacity clean; chunked decoder, HTTP/1 head parser, smuggling vectors all
clean.

## Invariants violated

- *Idle means idle* — a multi-shard runtime with only pending work burns
  cores (C-1); caps and indexes exist but the hot ingress doesn't use them
  (C-5, I-NEW-4/5/6).
- *A response belongs to the request that asked for it* — broken by stale
  keepalive continuations (E1, E3) and WS client fragmentation (A1).
- *Flow-control credit is conserved* — broken on the server's reject paths
  (B11), dead-stream upload credit (SP-C), and client abandoned streams
  (CH-1, stream-slot analogue).
- *Every resource has an owner on every path* — streamed-response sources
  (SP-A/SP-B), child records (C-3), abandoned client streams (CH-1).
- *Terminal causes are never silently converted* — callee-stop → `Timeout`
  (E5), dropped child control (C-4), missing gRPC status → `Ok` (SP-D),
  rpc-tokio starvation → `Timeout` (D-2), `Timeout→Closed` (D-3).
- *Trace and proof surfaces tell the truth* — shutdown drain destroys
  evidence (CT-1), unflushed observer + `complete`-labeled truncation +
  invisible drops (CT-2/TG-2/CT-3), ungated capture hash (TG-1), shrinkers
  re-pinning from green runs (TG-3).
- *Compile-time claims are compile-time* — the split-mode authority rail is
  quietly dynamic on the early-return path (H11).

## Areas needing deeper review

- HPACK dynamic-table behavior under adversarial table-size updates (track
  B covered law around it, not the table itself line-by-line).
- The `Mailbox::wake` trait contract across any future custom mailbox —
  the documented recipe is unsound (E2); fix doc + add a loom model before
  spsc becomes direct ingress.
- Linux empirical measurement of C-1 (macOS probe under-reads it; expect
  ≈full core).
- Sim-vs-live socket EOF semantics (second pass recorded an observation:
  sim's scripted empty-read = EOF is internally consistent but diverges
  from live partial-read semantics; not filed).
- Collapsing the five copy-pasted AWS workers into one generic worker
  (AWS-Q2-A is the standing hazard; D-1 is what it looks like when it
  fires).

## Suggested fuzz / property / integration tests

- 2-shard runtime with one armed listener and one pending 30s sleep: assert
  worker CPU time < 50 ms over 5 s (C-1 regression; the probe exists in the
  verifier transcript).
- WS client conformance: server sends fragmented text (2+3 fragments,
  interleaved ping) → client must deliver one reassembled message (A1).
- Keepalive generation property: inject delayed `Connected`/`Read`
  completions from request N during request N+1; assert no panic, no
  cross-delivery, slot still healthy (E1); same for one-shot deadline (E3).
- HTTP/2 server credit conservation: sum of WINDOW_UPDATE credit returned
  == sum of `flow_len` received, across accept/reject/reset/teardown
  permutations (B11 + SP-C in one property).
- DATA on completed stream while upload in flight → RST_STREAM, connection
  stays up (B8-residual).
- Count live isolates after client disconnect mid-streamed-response → zero
  stranded sources (SP-A/SP-B); after 65 abandoned client streams on one
  connection → new request still admitted (CH-1).
- gRPC: END_STREAM with no `grpc-status` on the streaming path → typed
  error, not `Ok` (SP-D).
- Multishard shutdown while a handler floods cross-shard sends → all shard
  traces present, no worker panic (CT-1).
- Loom: spsc wake-hook model — producer publish racing consumer
  drain-then-park; assert no parked-with-message state (E2).
- Spawn-churn property: N spawn/stop cycles of unsupervised children →
  `entries` length and per-step GC cost bounded (C-3).
- Registration bootstrap vs cross-shard call burst: bootstrap always
  dispatched as bootstrap, call always settles with its own context (C-2).
- CI: build all feature combinations (`--features tracing` would have
  caught D-1); port SQS/S3 slot tests to SNS/DynamoDB/Secrets (AWS-Q2-A).
- DST exactly-once-terminal over cross-shard fan-in — carried forward from
  06-08, still unimplemented (the agent assigned to it died in the network
  outage before writing code).

## Track coverage map

| Track | Scope | Findings (verified) |
|-------|-------|---------------------|
| A | HTTP/1, chunked, WS strictness | A1 (High) |
| B | HTTP/2 + gRPC law | B8-residual, B11 (Med); B9, B10 (Low) |
| C | Runtime calls, cross-shard, fairness | C-1 (High), C-3 (Med-High), C-2, C-4 (Med), C-5 (High, =I-NEW-1) |
| D | Bridges + external work | D-1 (Med, build break); D-2..D-6 (Low) |
| E | Resource ownership + drop | E1 (High), E2 (Med, adj.), E3, E5 (Med), E4 (Low) |
| F | Persistence, process, FS, signals, TLS | clean; F-A, F-B (Low) |
| G | Determinism, sim, proof harness | TG-1, TG-2 (Med), TG-3 (Low-Med), TG-4/5/6 (Low) |
| H | Macros + public API | H11 (Med); H9/H10/H12–H17 (Low) |
| I | Performance as correctness | I-NEW-1/2 (High, =C-5/C-1), I-NEW-3..6 (Med), I-NEW-7/8/9 (Low) |
| Second pass | Streaming-body seam | SP-A, SP-C (Med-High), SP-B, SP-D (Med), SP-E, SP-F (Low) |
| Carve-out: http2 | dropped-stream credit, frame law | CH-1 (Med); two disproofs |
| Carve-out: tracing | shutdown flush, install/live | CT-1 (High), CT-2, CT-3 (Med) |
| Carve-out: process+aws | non-unix, slot audit | clean; AWS-Q2-A, Q1-a/b (Low/Info) |
| Verification | refute every Med+ | 23/23 confirmed, 0 refuted; E2 High→Med; I-NEW-3 magnitude corrected |
