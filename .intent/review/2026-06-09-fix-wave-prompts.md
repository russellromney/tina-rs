# 2026-06-09 fix wave — agent launch prompts

Companion to `adversarial-review-2026-06-09.md`. Five area agents, one per
prompt below, one branch/PR per area (the #227–#233 convention). Worktrees
already exist:

| Worktree | Branch | Prompt |
|---|---|---|
| `~/Documents/Github/tina-rs-fw-runtime` | `codex/fix-wave-2026-06-09-runtime` | §1 |
| `~/Documents/Github/tina-rs-fw-h2server` | `codex/fix-wave-2026-06-09-h2-server` | §2 |
| `~/Documents/Github/tina-rs-fw-clients` | `codex/fix-wave-2026-06-09-http-clients` | §3 |
| `~/Documents/Github/tina-rs-fw-proofs` | `codex/fix-wave-2026-06-09-proof-truth` | §4 |
| `~/Documents/Github/tina-rs-fw-bridges` | `codex/fix-wave-2026-06-09-bridges-macros` | §5 |

All branches are off `origin/main` `0cd6a31` (the reviewed HEAD), clean as of
2026-06-09 ~23:00. Before launching: `git -C <worktree> status` — if a prior
interrupted run left edits, reset or review them first. If `main` has moved,
rebasing the branches first is optional; findings relocate by symbol anyway.

Orchestrator notes:
- Launch all five as parallel background agents, prompts verbatim.
- Shared build cache: agents are told to set
  `CARGO_TARGET_DIR=/Users/russellromney/Documents/Github/tina-rs-adv/target`.
  Builds serialize on cargo's lock; that is expected. Watch disk (`df -h`);
  the wave died once before on ENOSPC.
- §1 (runtime) is the long pole; §4 must not touch §1's files (constraint is
  in the prompt).
- After agents finish: review each diff, run touched-crate tests + clippy,
  push each branch, open one PR per area citing the finding ids, and append
  a resolution log to `adversarial-review-2026-06-09.md` (append-only,
  the #234 pattern: finding → status → PR → key test, plus honest caveats).
- Two deliberate deferrals, do not assign: loom pool test (resource-maturity
  pool work in flight in the main checkout), five-AWS-worker dedup refactor
  (flag as follow-up only).

---

## §1 — runtime core

You are a fix agent in the 2026-06-09 tina-rs adversarial-review fix wave. Your worktree: /Users/russellromney/Documents/Github/tina-rs-fw-runtime (branch codex/fix-wave-2026-06-09-runtime, off origin/main 0cd6a31). Do all work there. Do NOT push. Do not create new worktrees.

Build discipline: ALWAYS run cargo as `CARGO_TARGET_DIR=/Users/russellromney/Documents/Github/tina-rs-adv/target cargo ...` (warm shared cache; sibling agents share it — cargo's lock serializes builds, just wait). Targeted tests only (`cargo test -p <crate> <filter>`); no `--all-targets` workspace builds; no release builds. On ENOSPC: stop and report, don't thrash.

Source of truth for findings (read first; NOT in your worktree — read these absolute paths): /Users/russellromney/Documents/Github/tina-rs-adv/.intent/review/adversarial-review-2026-06-09.md plus track detail in /Users/russellromney/Documents/Github/tina-rs-adv/.intent/review/2026-06-09-tracks/track-C.md, track-I.md, track-E.md (E5), track-D.md (D-4), carveout-tracing.md (CT-1). All findings were adversarially verified; line numbers may have drifted — relocate by symbol.

Your findings, in priority order (all in tina-runtime/src/: threaded_multi_shard.rs, threaded.rs, dispatch.rs, remote.rs, deferred.rs, registration.rs):
1. **C-1 (High)** — multi-shard worker busy-spins (yield_now over zero-timeout lane polls) whenever any work is in flight; no blocking wait reachable; also the no-yield `continue` when terminal_overflow non-empty. Fix: block in the io_loop with a computed timeout, mirroring the single-shard park (`park_io`, dispatch.rs ~:1993; worker use threaded.rs ~:1815). Must preserve terminal_overflow retry correctness and cross-shard delivery latency (a peer can wake it — check what wake mechanism multi-shard has; if none exists for cross-shard delivery you need a doorbell like the single-shard one, or a bounded park timeout as a correct-but-cruder first step — choose the strongest design you can verify, and be honest in the commit about residual latency tradeoffs). Fix the false "blocks inside the betelgeuse io_loop" comment. Regression test: 2-shard runtime, one pending 30s sleep + idle listener; assert worker thread CPU time stays tiny over a few seconds (libc rusage/thread time; mark `#[ignore]`-heavy variants if needed but keep one cheap CI-viable assertion, e.g. a park/step counter exposed via cfg(test) or metrics).
2. **C-5 (High)** — `entries.iter().position()` O(N) per message at remote.rs ~:327 and ~:599; replace with `entry_indexes.get(&id)` (verified substitutable: every entries mutation keeps the map coherent; generation check unchanged). Add/extend a test that exercises send + call delivery through both sites.
3. **CT-1 (High)** — multishard shutdown drain (threaded.rs `deliver_shutdown_signal_and_drain` ~:1838) loops plain `step()` whose remote-route closure panics on any QueuedRemoteEnvelope → shard trace destroyed. Fix: drain with the worker's real route_remote closure. Test: shutdown while a handler floods cross-shard sends → no panic, all shard traces present (un-quiesce the pattern that tina-runtime/tests/multishard_fairness.rs:426-429 tiptoes around; update that comment/test accordingly).
4. **C-3 (Med-High)** — `child_records` never shrink → stopped entries permanently un-GC-able, GC scan latches forever, unbounded leak. Fix: prune records when both ends are stopped/GC-able (or index by child + prune in GC). Property test: N spawn/stop cycles of unsupervised children → entries len and GC cost bounded; stopped entries actually GC.
5. **C-2 (Med)** — registration bootstrap bypasses positional call_contexts queue (registration.rs ~:303-316, ~:794-807) → misbind with harvested cross-shard call. Fix: enqueue bootstrap with a None context through the same path. Test: bootstrap + concurrent cross-shard call burst → bootstrap dispatched as bootstrap, call settles with own context.
6. **C-4 (Med)** — ChildStop/SpawnCancel droppable (not in preserved-terminal set at threaded_multi_shard.rs ~:964-970/multi_shard.rs ~:457; `let _ = route_remote` harvest path; trace-only-no-retry at dispatch.rs ~:2651,~:2710). Fix: add to preserved set / make retryable. Test: child stop under pair-queue Full pressure → child eventually stops, no orphan.
7. **E5 (Med)** — calls queued at callee-stop drained as MessageAbandoned with context dropped → caller settles Timeout instead of Closed-class (dispatch.rs ~:300-309, ~:2450-2456). Fix: reject_call_context with a typed terminal on both abandonment sites. Test: call queued behind a poison message that stops the callee → caller gets Closed-class terminal promptly, not timeout.
8. **D-4 (Low-Med)** — CancelCall completions delivered via droppable enqueue_entry_message (dispatch.rs ~:1669-1702), contradicting docs/mailbox-capacity.md's "runtime-call continuations never drop". Route via enqueue_call_continuation. Test: cancel completion under requester mailbox Full → delivered.
9. Optional if time and risk allow: **I-NEW-4** (build_message_caller use pending_isolate_call_indexes, dispatch.rs ~:467), **I-NEW-5** (PromotedSlots O(P) lookups → indexed or early-exit-if-empty on cancel/timeout path), **I-NEW-6** (skip partition+rebuild when stopper owns no calls; cheaper can_gc scans). Each tiny + tested.

Method per finding: write the failing test FIRST, confirm it fails for the right reason, fix, green. One commit per finding (terse message naming the id, e.g. "runtime: park multi-shard worker instead of yield-spinning (C-1)"); end every commit message with:
Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>
Code comments: terse and dense, match surrounding idiom, never reference finding ids or the review in code.
Finish: full `cargo test -p tina-runtime` (+ tina if touched) and `cargo clippy -p tina-runtime` clean.

Final message (to orchestrator): per finding — fixed? test name(s), commit sha, any honest caveats (e.g. C-1 residual latency tradeoff); anything not fixed and why.

---

## §2 — HTTP/2 server teardown funnel

You are a fix agent in the 2026-06-09 tina-rs adversarial-review fix wave. Worktree: /Users/russellromney/Documents/Github/tina-rs-fw-h2server (branch codex/fix-wave-2026-06-09-h2-server, off origin/main 0cd6a31). Work only there. Do NOT push. No new worktrees.

Build discipline: ALWAYS `CARGO_TARGET_DIR=/Users/russellromney/Documents/Github/tina-rs-adv/target cargo ...` (shared warm cache; sibling builds serialize on the lock — wait). Targeted tests (`cargo test -p tina-http <filter>`); no workspace --all-targets; no release. On ENOSPC: stop and report.

Findings source (read first; absolute paths, not in your worktree): /Users/russellromney/Documents/Github/tina-rs-adv/.intent/review/adversarial-review-2026-06-09.md, .../2026-06-09-tracks/track-B.md and second-pass.md (SP-A/SP-C/SP-E/SP-F full details + verifier evidence). All verified; line numbers may drift — relocate by symbol.

Your area: the HTTP/2 server stream-teardown family in tina-http/src/http2/server.rs. The verifier's structural recommendation: ONE `remove_stream`/reset funnel through which every teardown path goes, so each obligation is met once. Findings:
1. **SP-A (Med-High)** — no teardown/reset path cancels streaming response sources: `cancel_response_source` (~:2021) called from only 2 of ~6+ sites; the four handle_stream_chunk reset paths (~:1800,1828,1873,1924) discard the ActiveStream; no connection-teardown path (close_now, GOAWAY, rapid-reset, Closed→stop) iterates streams. Cancel the source everywhere a stream dies, including connection death.
2. **SP-C (Med-High)** — accepted upload DATA's `flow_credit` queued in request_chunks is returned only on consume; every remove_stream caller drops queued chunks without crediting → server's own recv_window ratchets to FlowControl GOAWAY blaming the peer. Sum + credit dropped chunks in the funnel. Also fix the zero-live-streams flush blind spot (`flush_deferred_request_window_credit` ~:2201 filters by live streams, so connection-level pending credit never flushes with none).
3. **B11 (Med)** — five reject branches in handle_data (~:1019-1121, incl. content-length overrun ~:1073) return before the debit at ~:1122 without crediting connection flow for the already-debited-by-peer DATA → peer window leaks to 0. Credit flow_len on every reject path (client twin at client.rs ~:2169-2175 documents the §6.9.1 rule).
4. **B8-residual (Med)** — DATA on removed stream → Err → connection GOAWAY (~:1016-1018) where client twin RSTs the stream (client.rs ~:2176-2182). Fix: RST_STREAM(STREAM_CLOSED) + keep connection (and the B11 credit applies). Remove or use the dead `ActiveStream.reset` flag (declared ~:303, never set).
5. **SP-E (Low-Med)** — flush-failure path (~:1841-1858) enqueues RST with `let _` (which fails for the same queue-cap reason) and leaves the stream wedged with pending data; only peer events retry. Make the funnel transition the stream and retry/flush deterministically (e.g. on handle_wrote drain).
6. **SP-F (Low)** — three server-initiated resets emit no protocol fact/trace event; emit from the funnel.
7. Optional: **B9 (Low)** — wrong-length PRIORITY → connection GOAWAY; RFC wants stream-level FRAME_SIZE_ERROR.

Tests (test-first per finding; tina-http/tests/http2_live.rs and unit tests in server.rs both exist as homes — follow existing style):
- Credit conservation property: across accept/reject/reset/teardown permutations, sum of WINDOW_UPDATE credit returned == sum of flow_len received (covers B11+SP-C together).
- DATA on completed stream while upload still in flight → RST_STREAM, connection stays usable (B8-residual).
- Client disconnect mid-streamed-response → zero stranded source isolates (count live isolates; SP-A); same for mid-stream reset paths.
- Upload abort (client RST mid-upload with unconsumed chunks) → connection recv window fully restored; subsequent large upload on a new stream succeeds (SP-C).
- Flush-failure then peer WINDOW_UPDATE/wrote → response completes or stream cleanly reset, never wedged (SP-E).

Method: failing test first (confirm it fails right), fix, green. One commit per finding or tight pair, terse message naming the id; end every commit message with:
Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>
Comments terse/dense, match idiom, no finding ids in code. Finish: `cargo test -p tina-http` + `cargo clippy -p tina-http` clean.

Final message: per finding — fixed? test names, commit sha, honest caveats; anything unfixed and why.

---

## §3 — HTTP clients (keepalive, one-shot, WS client, h2 client, gRPC client)

You are a fix agent in the 2026-06-09 tina-rs adversarial-review fix wave. Worktree: /Users/russellromney/Documents/Github/tina-rs-fw-clients (branch codex/fix-wave-2026-06-09-http-clients, off origin/main 0cd6a31). Work only there. Do NOT push. No new worktrees.

Build discipline: ALWAYS `CARGO_TARGET_DIR=/Users/russellromney/Documents/Github/tina-rs-adv/target cargo ...` (shared warm cache; sibling builds serialize — wait). Targeted tests; no workspace --all-targets; no release. On ENOSPC: stop and report.

Findings source (read first; absolute paths): /Users/russellromney/Documents/Github/tina-rs-adv/.intent/review/adversarial-review-2026-06-09.md, .../2026-06-09-tracks/track-A.md (A1), track-E.md (E1/E3), second-pass.md (SP-D), track-I.md (I-NEW-3), carveout-http2.md (CH-1). All adversarially verified; line numbers may drift — relocate by symbol.

Your findings (tina-http client-side; files disjoint from the sibling agent working on http2/server.rs — do not touch server.rs):
1. **E1 (High)** — tina-http/src/keepalive.rs: only `Deadline` carries a request generation; stale `Connected`/`TlsConnected`/`Wrote`/`Read` continuations from a timed-out request leak into the next request on the pooled isolate (guards are just `in_flight.is_some()`); `fail_request` (~:870) cancels nothing, so a slow connect's late `Connected(Ok)` installs a wrong transport, then the new connect's Connected panics on `pending_connect_bytes.take().expect`; stale Read can deliver request N's response to N+1. Fix: stamp EVERY continuation with the generation and drop stale ones at the handler top; have fail_request invalidate/cancel pending transport ops (close the half-connected transport on arrival if stale). Tests: inject delayed Connected/Read completions from request N during N+1 → no panic, no cross-delivery, slot stays healthy and reusable. NOTE: the user has uncommitted keepalive.rs changes in a different checkout (resource-maturity work) — fix against main as-is; a later merge conflict is expected and accepted.
2. **E3 (Med)** — tina-http/src/client.rs one-shot client: same stale vectors plus `Deadline(Result<(),CallError>)` carries no generation and the sleep is never cancelled → request A's deadline deterministically times out request B back-to-back. Same generation fix; test: two back-to-back requests where A's deadline fires during B → B unaffected.
3. **A1 (High)** — tina-http/src/websocket_client.rs ~:531-568: client ignores `frame.fin`, no continuation reassembly; partial fragment surfaced as complete message, following 0x0 frame → protocol close. Port the server's reassembly (connection.rs ~:1829-1890) to the client consumer (respect max-message caps; interleaved control frames allowed mid-fragmentation per RFC 6455). Test: server sends fragmented text (2-3 fragments + interleaved ping) → client delivers ONE reassembled message; oversized reassembly → typed error.
4. **CH-1 (Med)** — tina-http/src/http2/client.rs: abandoned streamed responses are never reaped (no Drop guard, bare u32 stream id) — slot+buffer+per-stream window held until connection close; 64 abandons brick the pooled connection (`admit_stream` Full forever, cap ~:1202). Fix: the report sketches (a) Drop-guard handle that enqueues Cancel, or (b) per-stream pull-idle deadline (plumbing reserved ~:171-177). Pick the strongest fit with existing API shape. Test: abandon >max_concurrent_streams streamed responses → new request still admitted; RST_STREAM sent / slot freed.
5. **SP-D (Med)** — tina-http/src/grpc_client.rs ~:484-485: streaming client maps missing grpc-status at END_STREAM to Ok via unwrap_or_else; unary twin (~:325-335) honestly errors MissingTrailers. Fix to match unary. Test: END_STREAM with no grpc-status on streaming path → typed Malformed error, not Ok.
6. **I-NEW-3 (Med/Low-Med)** — per-frame `drain()` O(frames×bytes): http2/client.rs ~:1863,1877; websocket.rs ~:880 (parser used by WS server loop connection.rs ~:1742 and WS client ~:527); grpc.rs ~:644. Port the server's cursor+single-post-loop-drain pattern (http2/server.rs ~:700-705) to all three. Tests: existing suites must stay green; add a many-tiny-frames decode test asserting correctness (perf assert optional).

Method: failing test FIRST (confirm fails right), fix, green. One commit per finding, terse message naming the id; end every commit message with:
Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>
Comments terse/dense, match idiom, no finding ids in code. Finish: `cargo test -p tina-http` + `cargo clippy -p tina-http` clean (coordinate-free: if a sibling's server.rs changes aren't in your branch that's fine — your branch is independent).

Final message: per finding — fixed? test names, commit sha, honest caveats; anything unfixed and why.

---

## §4 — proof/trace truth, SPSC wake, DST exactly-once test

You are a fix agent in the 2026-06-09 tina-rs adversarial-review fix wave. Worktree: /Users/russellromney/Documents/Github/tina-rs-fw-proofs (branch codex/fix-wave-2026-06-09-proof-truth, off origin/main 0cd6a31). Work only there. Do NOT push. No new worktrees.

Build discipline: ALWAYS `CARGO_TARGET_DIR=/Users/russellromney/Documents/Github/tina-rs-adv/target cargo ...` (shared warm cache; sibling builds serialize — wait). Targeted tests; no workspace --all-targets; no release. On ENOSPC: stop and report.

Findings source (read first; absolute paths): /Users/russellromney/Documents/Github/tina-rs-adv/.intent/review/adversarial-review-2026-06-09.md, .../2026-06-09-tracks/track-G.md (TG-1..6), carveout-tracing.md (CT-2, CT-3), track-E.md (E2), plus the verifier evidence embedded in the report. All verified; line numbers drift — relocate by symbol.

CONSTRAINT: a sibling agent owns tina-runtime/src/{threaded_multi_shard.rs worker loop, dispatch.rs, remote.rs, registration.rs, deferred.rs} — do not modify those. Your tina-runtime touches are limited to observer.rs, live_report.rs, threaded.rs accessors (complete_trace surface only), lib.rs counter plumbing, trace.rs if needed.

Your findings:
1. **E2 (Med, was High)** — tina-mailbox-spsc/src/lib.rs ~:178-198: wake hook computes was_empty from pre-publish head load; racing consumer drain suppresses the wake → worker parks forever with the message queued. Fix the wake condition (e.g. wake unconditionally while a hook is installed, or a consumer-visible emptiness protocol); ALSO fix the trait doc at tina/src/isolate.rs ~:159-168 which prescribes the unsound recipe to custom mailbox authors; ADD a loom model in tina-mailbox-spsc/tests/ covering producer-publish racing consumer drain-then-park: assert no parked-with-message state.
2. **CT-2 (Med)** — tina-runtime/src/observer.rs ~:58-94: BufferedTraceObserver has no flush/join (drain thread detached, handle discarded). Add flush()/shutdown-join so dropped_count + drained state are truthful; fix the proof-doc recipe in tina-proof-harness/src/live_replay.rs ~:23-27,144-147 to require flush before snapshot_complete. Test: events pushed then flush → all delivered before snapshot; without flush the API now makes the unsound read impossible (or returns a typed not-drained state).
3. **CT-3 (Med)** — live_report.rs ~:268: trace_dropped hardcoded None; runtime's real drop counter (lib.rs ~:782) reaches no surface. Plumb it into LiveShardMetrics/topology + tina-tracing/src/live.rs's emitted field; add a threaded-runtime accessor. Test: bounded retention + overflow → trace_dropped reports a non-zero count on the live surfaces.
4. **TG-1 (Med)** — tina-sim/src/dst/replay_case.rs ~:734-881: LiveReplayCaptureBuilder::finish pins an order-sensitive hash with no multishard/lossy gate (gate exists only on RunCapture, live_replay.rs ~:395-398). Apply the same fail-closed gate (multishard → typed error; lossy/undrained observer → typed error) to the builder path used by capture_run/capture_live_run/capture_overload_run. Test mirrors the RunCapture gate test.
5. **TG-2 (Med)** — tina-runtime/src/threaded.rs ~:1291-1316 (+ multi-shard mirror :592-613): complete_trace()/TraceSnapshot::complete label the retention-truncated suffix complete. Make the accessor honest: refuse (typed error) or mark partial when retention != Full / drops occurred; route the existing honest trace_for_proof into the public path. Test: bounded retention + overflow → complete_trace refuses or reports partial.
6. **TG-3 (Low-Med)** — tina-sim/src/dst/shrink.rs ~:172-215,~:348-406 and tina-proof-harness/src/byte_replay.rs ~:230-265: shrinkers never run still_fails on the ORIGINAL case; non-reproducing capture exits as a "shrunk bug" with constants re-pinned from a green run. Fix: verify original fails first; typed NotReproducing error otherwise. Test per helper.
7. **TG-4 (Low)** — byte_replay.rs ~:677: non-char-boundary slice panic on multi-byte UTF-8 in a chunk= line → return typed Decode error. Test with a multi-byte payload.
8. **TG-5 (Low)** — tina-tracing/src/timeline.rs ~:337,339,791: timeline export still assumes global ids post-G1 (id-only sort, ambiguous cause_id, deferred spans keyed by bare slot_id mispair across shards). Sort/key by (shard,id). Test: two-shard synthetic trace → spans pair correctly.
9. **TG-6 (Low)** — tina-proof-harness/src/perf.rs ~:124-152: perf JSON emits leak_clean without leak_checked → add the field (schema bump per existing convention). Test asserts both fields.
10. **DST exactly-once test (carried from 06-08, still missing)** — add a DST property: cross-shard fan-in under mailbox pressure → every call settles exactly once with a typed terminal (regression for the old C1 terminal-drop class). Home: tina-sim dst tests / tina-runtime multishard tests, following existing DST style. Check first it truly doesn't exist (search for an equivalent before writing).

Method: failing test FIRST per finding, fix, green. One commit per finding, terse message naming the id; end every commit message with:
Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>
Comments terse/dense, match idiom, no finding ids in code. Finish: `cargo test -p tina-sim -p tina-proof-harness -p tina-tracing -p tina-mailbox-spsc` + targeted tina-runtime tests + clippy on touched crates, clean.

Final message: per finding — fixed? test names, commit sha, honest caveats; anything unfixed and why.

---

## §5 — bridges + macros + process

You are a fix agent in the 2026-06-09 tina-rs adversarial-review fix wave. Worktree: /Users/russellromney/Documents/Github/tina-rs-fw-bridges (branch codex/fix-wave-2026-06-09-bridges-macros, off origin/main 0cd6a31). Work only there. Do NOT push. No new worktrees.

Build discipline: ALWAYS `CARGO_TARGET_DIR=/Users/russellromney/Documents/Github/tina-rs-adv/target cargo ...` (shared warm cache; sibling builds serialize — wait). Targeted tests; no workspace --all-targets; no release. On ENOSPC: stop and report.

Findings source (read first; absolute paths): /Users/russellromney/Documents/Github/tina-rs-adv/.intent/review/adversarial-review-2026-06-09.md, .../2026-06-09-tracks/track-D.md, track-H.md, carveout-process-aws.md, track-F.md (F-A). All verified; lines drift — relocate by symbol.

Your findings:
1. **D-1 (Med, build break)** — tina-sqlx-bridge/src/worker.rs ~:460: `emit_replied` missing `PgTransactionOutcome::CommitAmbiguous` arm → `--features tracing` fails to compile (E0004). Add the arm (emit the honest ambiguous outcome). Then add feature-combo builds to CI (.github/workflows/verify.yml): at minimum `cargo check -p tina-sqlx-bridge --features tracing` — keep CI cost sane.
2. **AWS-Q2-A (Low, coverage)** — tina-aws-bridge: SNS/DynamoDB/Secrets workers have zero slot-lifecycle tests. Port the SQS timeout test (sqs_bridge.rs ~:854, pins in_flight_current==1 after caller timeout) and the S3 Full/drain tests to all three bare variants. (Do NOT attempt the five-worker dedup refactor — flag it as follow-up.)
3. **D-3 (Low)** — tina-tokio-bridge/src/lib.rs ~:485-492: public `From<BridgeError> for SendRejectedReason` maps Timeout→Closed. Fix the mapping honestly (Timeout→a timeout-shaped reason if the enum has one; if it doesn't, add the variant or document+test the deliberate choice — no silent terminal-word conversion).
4. **D-5 (Low)** — tina-tokio-bridge ~:1040-1061: caller-cancel preflight maps to MailboxClosed → cancel double-counts as timeout+closed in metrics. Classify honestly; test the metric counts.
5. **D-6 (Low)** — tina-aws-bridge/src/core.rs ~:31-58: close_and_drain/drain_and_shutdown is a blocking 1ms thread::sleep poll loop; called from the bridge's own shard it stalls forever. Minimum: doc the host-thread-only contract loudly + debug_assert not-on-shard-thread; better if cheap: make drain cooperative. Test/doc accordingly.
6. **H11 (Med)** — tina-macros/src/lib.rs ~:333-343: split-mode authority rail bypassed by conditional early `return tina::noop()` in the spliced body (generated fn returns Effect<Self>). Fix: splice the user body as an immediately-invoked closure (so `return` exits the closure typed RequestEffect<Self>, restoring the compile-time rail) — verify no borrow/lifetime regressions across existing fixtures. Add a trybuild compile-fail fixture for the early-return shape (tina-runtime/tests/safety_rails_diagnostics.rs style).
7. **H9 (Low)** — tina-rpc-macros/src/lib.rs ~:321: RESERVED_REQUEST_PARAMS compare misses raw idents (`r#deadline`) → use IdentExt::unraw. Fixture: r#deadline rejected.
8. **H10 (Low)** — tina-rpc-macros ~:534-535: `__tina_encoding`/`__tina_payload` are call-site idents (collidable). Use Span::mixed_site like tina-macros does. Test: arg named __tina_encoding either rejected or wire-correct.
9. **H16 (Low)** — tina-rpc-macros ~:126,264: raw-ident method/trait names leak `r#` into wire strings (`fn r#move` → "r#move"). Unraw for wire names; test pins the wire string.
10. Optional cheap lows: **H12** (authority visitor false-positive on r#call — unraw in the visitor), **H14** (isolate macro strips mut/ref/@ from handler bindings — preserve patterns), **F-A** — tina-runtime/src/driver/process.rs ~:446: group-kill via fork/exec'd `kill` CLI silently no-ops under fd/pid exhaustion → use libc::killpg directly (unix-gated; test existing process suite stays green).

Method: failing test FIRST per finding (for macro fixes: trybuild fixture or wire-bytes assertion first), fix, green. One commit per finding, terse message naming the id; end every commit message with:
Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>
Comments terse/dense, match idiom, no finding ids in code. Finish: `cargo test` for each touched crate (sqlx bridge: also `--features tracing`), clippy clean on touched crates.

Final message: per finding — fixed? test names, commit sha, honest caveats; anything unfixed and why.
