# Adversarial Review — 2026-06-08

Method: nine parallel deep-dive agents over playbook tracks A–I
(`.intent/review/adversarial-review-playbook.md`), a truth-gap second pass, and a
full verification pass that re-classified every Medium-or-higher finding against
the real project state. `✓` marks findings re-verified against source.

Full per-track findings live in `.intent/review/2026-06-08-tracks/track-{A..I}.md`
and `second-pass.md`.

## Read this first — the review target was stale

The review was kicked off against the working tree on branch
`codex/review-fix-wave-record-2026-05-21` (HEAD `49c3580`). That branch **diverged
from `main` at `1b17639` and is 125 commits behind `origin/main`** (only 2 ahead).
It is an append-only "fix-wave record" branch, not where current work lives.

Consequence: the first-pass agents flagged many bugs as "still live" that are in
fact **already fixed on `main`** — the prior fix waves (perf I1–I5, macros
H2/H3/H4, persistence F1/F2/F5, process-kill F3, TLS F4, restart-panic E2,
cross-shard terminal-drop C1, bridge D1/D3) all landed on `main` but are absent
from this branch. To answer the real question — *what is actually broken in the
project* — every finding was re-verified against a clean `origin/main` worktree
(HEAD `6c897af`). The findings below are classified by that verification.

So the headline finding is operational, not a code bug:

> **Do not base further work on `codex/review-fix-wave-record-2026-05-21`.** It is
> 125 commits behind and reintroduces fixed C10k-collapse perf bugs, the
> restart-factory shard crash, the journal-bricking torn `.idx`, and the macro
> data-corruption/escape-hatch bugs. Rebase on `main` or treat it as read-only
> history.

(Scope note: the request named both "the tina-rs project" and "tina". Both point
at this same workspace, so this is one review of the whole workspace.)

## Summary by risk boundary (status on `main`)

- **HTTP/2 / gRPC length & cap truth (NEW, the real cluster).** On `main`, the
  *buffered* response path checks `content-length` but the *streamed* path never
  does; the *encode* path enforces `max_messages` but the client-streaming
  *decode* path doesn't (unbounded `Vec<Req>`); the client advertises SETTINGS but
  the server sends an empty SETTINGS frame. Same shape every time: a cap/length is
  honored on one path and ignored on its symmetric twin. (SP1, SP2, SP3.)
- **Macro data corruption (NEW).** An RPC trait arg literally named `encoding` or
  `payload` is shadowed by a generated local, so the builder encodes the encoder
  *default* instead of the caller's value — silent wrong-wire-bytes. The reserved
  -name fix that landed for `deadline`/`reply_to`/etc. did not cover these two.
  (H7.)
- **Determinism / proof truth.** Live multishard event ids come from a shared
  `Arc<AtomicU64>` with `Relaxed` ordering, so `LiveTrace`'s "sort by id → stable
  hash" is false across threads; the documented `compare_live_shape` regression
  path flaps, and a DST invariant + a proof-harness test over-claim on that basis.
  Sim is single-threaded so it's deterministic — an exact sim/live divergence.
  (G1, G2.)
- **Bridge slot leak.** Every poll-loop bridge (sqlx, reqwest, aws, sqlite) drives
  its own continuation via `sleep().then(Poll)` delivered into its *own* bounded
  mailbox; the runtime drops that self-message on Full, permanently leaking the
  held admission slot. `tina-sqlite-bridge` (`max_in_flight = 1`) wedges entirely
  on one drop. (D2.)
- **Hot-path O(n)/O(n²) — residual.** The systemic flat-Vec-scan theme is mostly
  fixed on `main`, but three instances remain: stopped-entry GC (O(N²) burst),
  promoted-slot sweep (O(P²) drop wave), and HTTP/2 `find_stream` (O(S) per frame,
  bounded by the 64-stream cap). (I8, I9, I10.)
- **HTTP/1 keepalive body-length truth.** The keepalive *client* trusts the
  server's `Content-Length`: an over-sent body is silently truncated and the
  pooled connection is reused (`must_retire = false`) with no desync check. (A-F3.)
- **Lower tier.** Buffered HTTP/2 uploads in (64 KiB, `max_body_bytes`] deadlock
  for lack of a mid-upload WINDOW_UPDATE on the buffered path (B7); a failed
  process kill drops the stdout/stderr drain `JoinHandle`s and leaks threads+fds
  (F6); the rpc-tokio shim mailbox sized to the bridge cap can surface a dropped
  reply as `Timeout` instead of its true cause (D1-residual); the `call`-authority
  gate is a string scan with a string-literal false negative (H8).

## Top 10 fixes (live on `main`, ranked by severity × confidence × blast radius)

| # | Sev | Finding | Location (`main` 6c897af) | Fix |
|---|-----|---------|---------------------------|-----|
| 1 | High ✓ | H7: RPC builder generates `let encoding`/`let payload` that shadow a trait arg of the same name → caller's value replaced by encoder default (silent data corruption) | `tina-rpc-macros/src/lib.rs:593-596` | Rename generated locals to `__tina_encoding`/`__tina_payload`, or add `encoding`+`payload` to `RESERVED_REQUEST_PARAMS` (`:215`) and reject |
| 2 | High ✓ | SP2: client-streaming gRPC decode materializes unbounded `Vec<Req>`; `max_messages` enforced on encode but not decode | `tina-http/src/grpc.rs:1490-1500` (via `:838`, `:873`) | Count decoded messages, error past `max_messages` like the encode side at `:452` |
| 3 | High ✓ | SP1: streamed HTTP/2 response body length never checked against declared `content-length`; short/over-sent stream delivered as clean `End` | `tina-http/src/http2/client.rs:2075-2083`, `1542-1581` | Accumulate a per-stream byte counter, compare to `response_content_length` (`:2655`) at `End`, like the buffered path at `:2110-2113` |
| 4 | High ✓ | G1: shared `Arc<AtomicU64>` `Relaxed` event-id counter → live multishard `LiveTrace` hash nondeterministic; "sort by id → stable hash" is false | `lib.rs:347,360`; `threaded_multi_shard.rs:262`; `live_replay.rs:114` | Per-shard id namespacing (shard-id high bits) + stable `(shard,seq)` sort key; or fail-closed the multishard compare path |
| 5 | High ✓ | D2: poll-loop bridge `sleep().then(Poll)` self-continuation dropped on full own-mailbox → permanent admission-slot leak (sqlite `max_in_flight=1` wedges) | `dispatch.rs:2879-2925`; sqlx/reqwest/aws/sqlite `worker.rs` | Reserve self-continuation headroom, or make self-Poll non-droppable (retry/reschedule), or settle the call on drop |
| 6 | Med-High ✓ | I8: `gc_stopped_entries` O(N) rescan every step + O(N²) burst removal via `entries.remove(index)` | `dispatch.rs:3195,3200,3212` | Same HashMap-index + `swap_remove` pattern used for I1/I2 fixes; gc only flagged ids |
| 7 | Med ✓ | A-F3: keepalive client trusts server `Content-Length`; over-sent body truncated and connection reused → framing-desync window | `tina-http/src/keepalive.rs:905-911,825-826` | If `read_buf.len() > body_end` on a reusable conn, set `must_retire=true` (don't pool a desynced socket) |
| 8 | Med ✓ | SP3: server initial SETTINGS is empty — never advertises `MAX_CONCURRENT_STREAMS`/`INITIAL_WINDOW_SIZE`/`MAX_FRAME_SIZE`; cap enforced only reactively via RST | `tina-http/src/http2/frame.rs:146-153` | Build the initial SETTINGS from config like the client does (`client.rs:964-981`) |
| 9 | Med ✓ | B7: buffered HTTP/2 upload in (64 KiB, `max_body_bytes`] deadlocks — no mid-upload WINDOW_UPDATE on the buffered path (streaming path is credited) | `tina-http/src/http2/server.rs:1100-1102` | Send WINDOW_UPDATE as buffered DATA is consumed, mirroring the streaming credit at `:1277-1285` |
| 10 | Med ✓ | I9: `PromotedSlots::sweep_dropped` O(P) Arc-count scan every step, O(P²) on a drop wave | `tina-runtime/src/deferred.rs:105,110` | Track live count incrementally; `swap_remove` dropped slots |

## Full findings list (live on `main`)

Beyond the top 10:

- **G2 [Med] ✓** `tina-sim/src/dst/invariants.rs:131,145` — `events_are_monotonic` /
  `causes_point_backward` assume contiguous same-slice ids; spurious failures on
  live multishard traces. Same root cause as G1; scope the checks per shard.
- **I10 [Med] ✓** `tina-http/src/http2/server.rs:2192` (+ client mirror) —
  `find_stream` is an O(S) `iter().position()` run 2–3× per frame ⇒ O(S²)/round,
  bounded by the 64-stream cap. Index streams by id.
- **H8 [Med] ✓** `tina-macros/src/lib.rs:609-617` —
  `require_call_authority_mentioned` scans the body's token string, so a `call`
  appearing only inside a string literal passes the must-use-authority gate.
  Severity reduced now that H4 (the `noop()` escape hatch) is `unsafe`-gated on
  main. Walk the AST or require a real authority binding.
- **F6 [Low-Med] ✓** `tina-runtime/src/driver/process.rs:363,381` — both
  `KillUncertain` early returns in `kill_and_reap` drop the stdout/stderr drain
  `JoinHandle`s without `join_drain_bounded`; threads + pipe fds leak when a kill
  fails with the child still alive. Bound-join on every exit path.
- **D1-residual [Low] ✓** `tina-rpc-tokio/src/lib.rs:373-375` — the per-call
  deadline backstop now prevents the hang+leak, but the shim mailbox is still
  sized `bridge.max_in_flight * 2` against the *Client* cap, so a dropped real
  reply surfaces as `Timeout` rather than its true terminal cause. Size the shim
  to `client + bridge` in-flight, or carry the true cause.
- **G3 [Low] ✓** `tina-proof-harness/src/load.rs:92,244,255` — `leak_clean`
  defaults `true` when no leak check is supplied; a "leak-clean" report can mean
  "never checked." Default to `false`/`Unknown`.
- **G4 [Low, by-design] ✓** `dispatch.rs:3135-3146` — `TraceRetention::Bounded`/`Off`
  truncates the trace read via `runtime.trace()`; default `Full` mitigates.
- **G5 [Low] ✓** `tina-sim/src/dst/sweep.rs:113-128` — `sweep_seeds` calls the
  runner directly, skipping `observe_replay_case`'s full report-identity guard.

## What was disproven / already fixed on `main` (do not re-file)

Re-verified fixed on `main` (cite the fix): **C1** terminal-reply drop →
per-pair `terminal_overflow` + `route_remote_preserving_terminal`
(`threaded_multi_shard.rs:943,1158-1209`); **C3/I4** drain order/starvation →
alternating drain + rotating `next_start`; **I5** 1 ms sleep → readiness park;
**I1/I2/I3** flat-Vec scans → `HashMap`/`BTreeMap` indexes + `swap_remove`
(`296caa7`/`61519d6`/`25cef21`); **I6** WaitList park → free-slot stack; **I7**
pool waiter count/alloc → O(1); **B1** DATA padding flow-control →
`data_payload_view` flow_len; **B2** SETTINGS resume → `flush_pending_responses`;
**B3** `TE` → rejected unless `trailers`; **B4/B5/B6/B8** empty `:path`,
stream-window overrun, zero WINDOW_UPDATE, DATA-on-closed → all RST-the-stream now;
**E2** restart-factory panic → `catch_unwind` + `RestartChildSkipped`
(`dispatch.rs:3064`); **E1** shutdown joiner spawn-fail → re-takes handles; **F1**
torn `.idx` → `Ok(None)` replay fallback + atomic tmp+rename
(`persistence.rs:324-373`); **F2** macOS fsync → `F_FULLFSYNC`
(`darwin.rs:1488`); **F3** post-reap pgid kill → WNOWAIT peek + kill-before-wait
(Linux; non-Linux still has a documented narrow race); **F4** serial TLS worker →
sans-I/O on-shard rustls (slowloris HOL gone); **F5** journal parent-dir fsync →
now synced; **D1** rpc-tokio hang+leak → per-call deadline backstop; **D3** sqlite
slot-on-timeout → re-inserts slot; **H2** macro `deny(unused_variables)` →
removed; **H3** reserved builder params → rejected with span; **H4** `noop()`
escape hatch → `pub unsafe`. **A1** (WS unfragmented-frame-mid-fragmentation) was
already fixed even on the branch.

Disproven as bugs (proofs in track files): CONTINUATION flood (no header
buffering), duplicate pseudo-headers, response-body-length truth on the buffered
path, stream-id monotonicity, WINDOW_UPDATE overflow, host-side `let _ =
reply_tx.send`, cross-shard cancel no-op, double-settle, wall-clock/HashMap
leakage into the trace hash, poisoned-lock masking, the new pool code (committed
on main — exactly-once handout, no ABA, correct force-close/maintain/refill; all
37 pool tests pass).

## Invariants violated (on `main`)

- *Protocol headers and body lengths tell the truth* — broken on the streamed
  HTTP/2 response path (SP1) and the keepalive client (A-F3).
- *Bounded capacity means the real thing is bounded* — broken for
  client-streaming gRPC decode (SP2, unbounded message count).
- *Replay/trace is deterministic where used as proof* — broken for live
  multishard traces (G1, G2).
- *Every call settles exactly once with a typed terminal cause* — eroded by the
  bridge self-continuation drop (D2, slot never settles) and the rpc-tokio shim
  misclassification (D1-residual, true cause → `Timeout`).
- *The wire value is the caller's value* — broken by macro arg shadowing (H7).

## Areas needing deeper review

- `tina-http/src/http2/client.rs` flow-control on a never-pulled dropped streamed
  response (connection-window credit), and whether the server validates inbound
  frame size against the 16384 default it *implicitly* advertises (corollary of
  SP3).
- `tina-tracing` `install.rs`/`live.rs` and live multishard shutdown trace-flush
  completeness (not deeply covered).
- Windows process supervision: non-unix `killed_group` is hardcoded `false`, so
  timeout/cancel falls back to `child.kill()` only and does not reap descendants.
- Per-variant aws workers (sqs/sns/dynamodb/secrets) timeout slot re-insert —
  structurally share the correct sqlx model but not line-audited each.

## Suggested fuzz / property / integration tests

- gRPC client-streaming decode: feed `max_messages + 1` empty 5-byte frames,
  assert a typed error, not OOM (SP2).
- HTTP/2 streamed response: server declares `content-length: N`, sends `N-1` and
  `N+1` DATA bytes; assert the client surfaces an error, not clean `End` (SP1).
- Property test: RPC service trait with args named `encoding`/`payload`; assert
  the encoded wire bytes equal the caller's value, not the default (H7); add a
  trybuild fixture per reserved name.
- DST exactly-once-terminal property over cross-shard fan-in (would have caught
  C1 before it was fixed; keep as regression).
- Multishard `LiveTrace` determinism: run the same scenario twice across ≥2 OS
  threads, assert equal `stable_trace_hash` (G1).
- Bridge slot-conservation property: drive a bridge at `max_in_flight` with forced
  own-mailbox-Full, assert in-flight returns to 0 and no slot leaks (D2); sqlite
  `max_in_flight=1` is the sharp case.
- Keepalive over-send: server returns body longer than `Content-Length` on a
  reusable connection; assert the connection is retired, not pooled (A-F3).
- Loom test over pool cancel+release+maintain+refill interleavings (the new pool
  code is correct today; pin it).

## Track coverage map

| Track | Scope | Live-on-main findings |
|-------|-------|------------------------|
| A | HTTP/1, chunked, WS strictness | A-F3 (keepalive body-length truth) |
| B | HTTP/2 + gRPC law | B7 (buffered-upload deadlock); B1–B6/B8 all fixed on main |
| C | Runtime calls, cross-shard, fairness | none live (C1/C3 fixed) |
| D | Bridges + external work | D2 (self-continuation slot leak); D1-residual (misclass); D1/D3 fixed |
| E | Resource ownership + drop | none live (E1/E2 fixed; new pool code correct) |
| F | Persistence, process, FS, signals, TLS | F6 (drain-handle leak); F1–F5 fixed |
| G | Determinism, sim, proof harness | G1, G2 (+ G3/G4/G5 Low) |
| H | Macros + public API | H7 (arg shadowing), H8 (string-scan); H2/H3/H4 fixed |
| I | Performance as correctness | I8, I9, I10; I1–I7 fixed |
| Second pass | Truth gaps on main | SP1, SP2, SP3 |

## Resolution log — 2026-06-08 fix wave (append-only)

All live-on-`main` findings from this review were fixed, each test-first
(failing test written and confirmed failing before the fix), then verified
with `cargo test` for the touched crates and `cargo clippy`. Fixes landed on
seven focused branches off `main`, one PR per area. Branches were cut from
`main` at `9a57e05`; findings were re-verified against that state (line
numbers in the report above are from `6c897af` and had drifted — every bug
was relocated by symbol/semantics).

| Finding | Sev | Status | PR | Key test |
|---|---|---|---|---|
| H7 | High | fixed | #227 | reserved-arg compile-fail fixtures (`encoding`/`payload`) |
| H8 | Med | fixed | #227 | `split_request_call_in_string_literal` compile-fail |
| SP1 | High | fixed | #232 | `streamed_response_{short,over}_body_vs_content_length_is_protocol_error` |
| SP2 | High | fixed | #232 | `decode_streaming_body_rejects_too_many_messages` |
| SP3 | Med | fixed | #232 | `server_initial_settings_advertises_configured_limits` |
| B7 | Med | fixed | #232 | `http2_buffered_upload_larger_than_initial_window_completes` |
| I10 | Med | fixed | #232 | `stream_index_stays_consistent_across_swap_remove` |
| G1 | High | fixed (fallback c) | #231 | `live_multishard_proof_snapshot_fails_closed` + single-shard stability contrast |
| G2 | Med | fixed | #231 | per-shard monotonic/causality invariant tests |
| G3 | Low | fixed | #231 | `unchecked_run_reports_leak_as_unchecked_not_clean` |
| G4 | Low | fixed | #231 | `truncated_trace_is_detectable_and_refused_by_proof_accessor` |
| G5 | Low | fixed | #231 | `sweep_seeds_rejects_report_for_the_wrong_case` |
| D2 | High | fixed | #233 | `under_sized_mailbox_overflows_runtime_call_continuations_without_dropping`; sqlite `slot_is_conserved_under_mailbox_saturation` |
| D1-residual | Low | fixed | #233 | `dropped_shim_reply_times_out_instead_of_hanging`, `dropped_awaiter_holds_capacity_until_terminal_backstop` |
| I8 | Med-High | fixed | #230 | `stopped_entry_gc_compacts_a_burst_in_one_pass_and_keeps_indexes_consistent` |
| I9 | Med | fixed | #230 | `sweep_dropped_removes_exactly_dropped_and_keeps_live_resolvable` |
| A-F3 | Med | fixed | #228 | `content_length_over_send_retires_connection` |
| F6 | Low-Med | fixed | #229 | `kill_uncertain_spends_drain_budget` |

### Caveats and judgment calls (honest record)

- **H3** was already fixed on `main` (4-name `RESERVED_REQUEST_PARAMS` + its
  fixture); the H7 work extended that set with `encoding`/`payload` rather than
  re-fixing it. Doing both the generated-local rename (`__tina_*`) *and* the
  reserved-name rejection is belt-and-suspenders; the user-visible proof is the
  reserved-name compile-fail.
- **G1**: a *stable* multishard `LiveTrace` hash across runs is **not
  achievable** and this was proven — per-shard ids remove the id race, but a
  free-running threaded multishard runtime interleaves each shard's cross-shard
  deliveries with local work by wall-clock, so the per-shard event *sequence*
  itself differs run to run (identical event sets, first divergence mid-trace).
  No id scheme fixes that. The fix takes the report's explicit fallback (c):
  per-shard ids by construction **plus** fail-closed (`LiveTraceProofError::Multishard`)
  on the proof path, and corrects the over-claiming test name/docstring. The
  simulator's id scheme is deliberately unchanged (single-threaded, deterministic).
- **I9**: the filed defect (O(P²) drop wave + per-step cost when nothing dropped)
  is fixed via `swap_remove` + an empty-slots early return. A residual per-step
  O(P) `Arc::strong_count` scan over *live* promoted slots remains; eliminating
  it needs a drop-callback/marker the `Arc` does not cheaply expose. Flagged, not
  pursued.
- **SP2** added `max_messages` to the public `GrpcLimits` struct; in-tree callers
  and the grpc example were updated with `..Default::default()`, and the
  many-small-messages live test now passes `max_messages: 1024` to keep proving
  the high-count path flows when the cap permits.
- **D1-residual** changes the signature of `BridgeClient::new` (adds
  `client_max_in_flight`). This is a breaking API change; all in-tree callers are
  updated and `cargo clippy --workspace --all-targets` is clean.
- **D2 process note**: the agent working this cluster died mid-run during a
  disk-full (ENOSPC) + offline window, before committing. Its uncommitted work
  was reviewed, verified (compiles, all bridge tests pass), and finished by hand.
  In doing so a **flaky assertion** in the sqlite saturation test was found and
  fixed: under capacity-1 saturation a caller's call can bounce as a runtime-level
  `CallOutcome::Full` (distinct from the inner `Replied(Err(SqliteError::Full))`),
  which the test had not counted as settled (~20% flake). The match was widened to
  count admission-`Full`; `Timeout`/`Closed` still panic, so a genuine slot leak
  is still caught. Re-run 30× green.

### Disk incident

Midway through the wave the data volume hit 100% (ENOSPC on the shared temp
volume), caused by many sibling worktree `target/` dirs. This crashed one
background fixer (D2/D1, recovered as above) and the det agent reported the same
pressure. Resolved by clearing finished worktrees' `target/` dirs. No source or
review history was affected.
