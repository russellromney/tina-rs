# 019 Review

Session:

- B (review)

## Plan Review 1

Artifact reviewed:

- `.intent/phases/019-voyager-single-shard-io-simulation/plan.md`

Reviewed against `.intent/SYSTEM.md`, the closeout state of 018, the live
runtime's TCP call surface
(`tina-runtime-current/src/call.rs`,
`tina-runtime-current/src/io_backend.rs`), and the current state of
`tina-sim` (which today routes every non-`Sleep` `CallRequest` through
`CallResult::Failed(CallFailureReason::Unsupported)`, so 019's scope is
real, not cosmetic).

### What looks strong

- The slice's bar is named honestly: Mariner echo parity, not "TCP
  primitives in the simulator." That is the right bar — primitive calls
  passing in isolation can hide every interesting bug.
- The plan refuses to invent simulator-only network verbs and pins to
  the already-shipped `CallRequest::TcpBind/TcpAccept/TcpRead/TcpWrite/
  TcpListenerClose/TcpStreamClose` surface. This protects SYSTEM.md's
  rule that the simulator must use the live runtime's meaning model.
- Direct, surrogate, and blast-radius proof are kept distinct. The
  surrogate-proof list is sharp:
  - bookkeeping without an echo workload
  - replay-without-TCP-workloads claiming TCP support
  - bytes-only assertions without the event record
  - one-client happy path posing as Mariner parity
  - primitive calls without a lifecycle workload
  Those are the actual failure modes for this kind of slice.
- Pause gates are honest: if faithful TCP needs new public verbs, or
  packet-level realism, or much broader fault/checker architecture,
  pause. Those are the right escape valves.
- "What Will Not Change" explicitly fences off filesystem, subprocess,
  UDP, TLS, DNS, packet-level TCP. Good fences.

### What is weak or missing

1. **Slice size and possible split point.**
   This package adds:
   - bind/accept/read/write/close completion semantics
   - virtual listener/stream resources
   - completion ordering discipline
   - partial reads + partial writes
   - listener re-arm + graceful close + multi-client + bounded-overlap
   - invalid-resource and pending-op rejection
   - seeded TCP perturbation
   - a checker-backed replay of a TCP-lifecycle/order failure
   - four user-shaped echo workloads
   That is materially larger than 016, 017, and 018 individually, and
   plausibly larger than 016+017+018 combined. The plan should either
   name a sub-ordering with an honest split point (e.g., "if perturbation
   plus checker plus four echo workloads exceeds the slice, defer the
   bounded-overlap variant and the fault-sensitive workload to 020") or
   say plainly that the slice is intentionally large and accept the
   review bar that goes with that.

2. **Connection-as-restartable-child interaction with 018 is undecided.**
   SYSTEM.md says the live `tcp_echo` listener spawns each accepted
   connection as a `RestartableSpawnSpec` child via
   `with_bootstrap`, and that this is what lets the connection's first
   read fire through the normal handler pipeline. If "Mariner echo
   parity" is to mean what it says, then the simulated echo workload
   must compose 018's spawn/restart machinery with 019's TCP machinery
   in the same workload. The plan does not pin this. State plainly:
   "the simulated echo workload uses the same connection-as-restartable-
   child shape as the live `tcp_echo` example, exercising 018's
   `RestartableSpawnSpec` + bootstrap surface alongside 019's TCP
   surface." Otherwise the proof can quietly degrade to one-shot
   spawning and "echo parity" weakens.

3. **Address introspection contract is unspoken.**
   The live runtime ships `CallResult::TcpBound { local_addr }` and
   `CallResult::TcpAccepted { peer_addr }`, and SYSTEM.md is explicit
   that the listener captures its own typed address from the bound
   `local_addr`. The plan does not say whether the simulator preserves
   this contract. If it does not, every workload that reads `local_addr`
   to advertise itself diverges from live behavior under simulation.
   Pin it: simulated bind returns a deterministic in-simulator
   `SocketAddr`, accepted streams report a deterministic `peer_addr`,
   and the listener/connection isolate code in the workload uses the
   same field reads as in the live `tcp_echo` example.

4. **Deterministic completion ordering rule is unstated.**
   "Deterministic completion ordering" is the load-bearing claim, but
   the plan does not name the rule. Options the plan should pick from:
   - per-resource FIFO with global tie-break by request-order ordinal
   - per-step harvest in registration order (matching the live runtime's
     timer harvest discipline)
   - some other rule
   Without pinning, the simulator can be deterministic in one direction
   and wrong in another, and the proof tests cannot meaningfully assert
   ordering. Pin a single rule and cite the runtime's existing rule
   that motivates it.

5. **Sync-vs-async dispatch split is unspoken.**
   SYSTEM.md says synchronous Betelgeuse ops (`bind`, `close`) complete
   inline during dispatch in the live runtime, while async ops
   (`accept`, `recv`, `send`) stay pending. This is observable through
   the trace (CallCompleted in the same step vs. a later step). The
   plan does not say whether the simulator preserves this split. Future
   invariants will hang on it. Pin it.

6. **Partial-write deterministic source is unstated.**
   "Partial writes remain possible and must be modeled" — but caused by
   what, deterministically? Peer-side buffer capacity? Seeded
   perturbation? A configurable per-stream cap? Without naming the
   cause, "partial writes are modeled" can degrade to "we have the
   capability but tests happen to never exercise it because writes fit."
   Pin the deterministic source so partial-write workloads honestly fire.

7. **Partial/short-read deterministic source is unstated.**
   Same problem at the read end. SYSTEM.md mentions the connection
   isolate's pending-buffer + drain pattern (driven by partial *writes*),
   but a partial *read* needs its own cause — typically a peer that
   wrote less than `max_len`. Pin it: short reads happen when the
   peer-side buffer holds fewer bytes than `max_len`, plus optional
   seeded perturbation. Otherwise the partial-read proof can quietly
   pass on full reads.

8. **Replay-from-config-alone honesty is missing.**
   017 was honest about "the seed is semantically real, not just
   recorded." 018 should have been more explicit about closures (Plan
   Review for 018 finding 7 / Implementation Review finding 7). 019
   has the same exposure, sharper: workloads supply byte payloads and
   isolate code as Rust, and `CallResult::TcpRead { bytes: Vec<u8> }`
   /`TcpWrote { count }` are not serialized. State plainly: "replay
   reproduces the same TCP event record when rerun against the same
   workload binary and simulator version; payloads and isolate code are
   not serialized into the artifact." Otherwise an outside reader will
   read "saved artifact reproduces the same simulated TCP run exactly"
   as packet-level reproducibility from the artifact alone.

9. **TCP perturbation surface is vague — `and/or` is a tell.**
   "seeded perturbation over TCP completion ordering and/or completion
   visibility." Pick one (or both, named). Compare to 017, which named
   `LocalSendFaultMode::DelayByRounds` and `FaultMode::DelayBy`
   explicitly. The current wording lets the implementation pick whichever
   is easier and call it done.

10. **Bounded-mailbox / backpressure on the TCP path is not in the proof
    list.**
    Bounded mailboxes are a load-bearing tina rule. The simulator's
    TCP path can surface `MailboxFull` when the connection isolate's
    inbox is full. The plan mentions backpressure as an option in
    "fault-sensitive workload proving a replayable TCP ordering,
    lifecycle, or *backpressure* issue," but there is no direct proof
    requirement that a TCP completion can hit a full mailbox and
    surface `CallCompletionRejected { reason: MailboxFull }`. Without
    that, the bounded-mailbox discipline is unproved on the TCP path.

11. **Listener-close-while-accept-pending is not pinned as a direct
    proof.**
    "Pending operation rejection on requester stop or resource close"
    is in the build-step list but not in the direct-proof list as a
    *combination* test. The race-shaped failures in TCP simulation
    almost always live exactly here:
    - listener close while accept is pending
    - stream close while read is pending
    - stream close while write is pending
    - requester isolate stops while any of the above
    Pin at least the first three as direct proofs, naming the expected
    `CallCompletionRejected { reason: ... }` shape from the live trace
    vocabulary.

12. **Same connection-isolate code, not a stand-in.**
    SYSTEM.md says the live connection isolate has a pending-buffer +
    drain pattern, exercised by a separate unit test. The plan does not
    say whether the simulator runs *that same isolate code* under
    simulation, or substitutes a simulator-only stand-in for the
    connection. Mariner echo parity demands the same isolate code. Say
    so out loud, the way 018 said it would execute the real public
    spawn payloads.

13. **Composition with 017 faults and 018 spawn/restart is undecided.**
    018 said honestly: "spawn/restart paths compose additively with
    017's seeded fault surfaces, but 018 does not add new spawn/restart-
    specific perturbation." 019 needs the same statement. Defaulting to
    "TCP perturbation is its own surface; 017 and 018 surfaces stay
    scoped to their existing paths" would be honest. The reader cannot
    tell today whether enabling `LocalSendFaultMode::DelayByRounds`
    while running an echo workload is supported, undefined, or expected
    to compose.

14. **Read-result payload and write-count shapes need a one-line pin.**
    The live runtime ships `CallResult::TcpRead { bytes: Vec<u8> }` and
    `CallResult::TcpWrote { count: usize }`. The plan does not say
    whether the simulator returns the same shapes. If it does not, the
    workload code diverges. If it does, say so.

15. **Resource-id ownership is underspecified.**
    SYSTEM.md says raw sockets never escape into isolate state and
    resources are runtime-owned opaque ids. The plan implicitly inherits
    that, but does not say whether the simulator adds isolate-local
    ownership rules on top of the live runtime. Pin it honestly:
    listener and stream ids are runtime-owned handles that may be passed
    between isolates, and stale/closed/unknown ids surface through
    `CallFailed(InvalidResource)`.

16. **DST-value vs. parity-catch-up framing.**
    016 made the simulator real. 017 made the seed real and added
    checkers. 018 made spawn/supervision real. 019 is dominantly a
    parity-catch-up slice on TCP, with one DST-value addition (a
    checker-backed replay of a TCP failure). That is fine, but the
    "Voyager" framing tilts toward DST value, and the slice's center of
    gravity is on parity. Either acknowledge that 019 is a parity slice
    that adds one DST-value test, or explicitly grow the DST-value
    side to match 017/018's depth (e.g., two distinct fault-sensitive
    TCP workloads, not one).

17. **Listener re-arm — say it comes for free.**
    The live listener re-arms via `Effect::Batch(Spawn, Send-self)`.
    Both `Batch` and `Spawn` work in the simulator as of 018. State that
    explicitly: re-arm is exercised through the existing batch + spawn
    surface, no simulator-only re-arm helper is added.

18. **Single-shard scoping of stream resources.**
    The slice is correctly fenced as single-shard. Add a one-liner
    saying simulated TCP resources belong to one simulator instance and
    are not transferable across simulator instances or shards. Forestall
    a future cross-shard interpretation.

### How this could still be broken while the listed tests pass

- "Bind/accept/read/write/close" pass through the public path, but the
  listener test isolate reads a fake `local_addr` the simulator made
  up, while the live listener reads the real bound address. Workload
  code that compiles into both paths runs different bytes in each.
- Partial-write tests pass because the simulator is *capable* of short
  writes, but the deterministic source for shortness is never tripped
  by the chosen byte payloads.
- Partial-read tests pass because reads return `bytes.len() ==
  max_len`, never the short case.
- Multi-client echo passes because clients are sequential and never
  expose ordering bugs that only show up under bounded-overlap.
- Bounded-overlap echo passes because the simulator's completion-order
  rule happens to align with the order the workload expects, but a
  later refactor breaks that by accident because the rule was never
  named.
- Listener-close test passes because no accepts were pending; the
  close-while-accept-pending case is silently uncovered.
- Backpressure path is "modeled" but no test ever hits a full mailbox
  on the TCP path, so a regression that disables `MailboxFull` rejection
  on TCP completions still leaves every test green.
- The fault-sensitive TCP workload uses ordering perturbation; checker
  halts and replays. Visibility perturbation (or vice versa) is silently
  unproved because `and/or` let the implementation pick one and call it
  done.
- "Replay reproduces" passes in-process during the same `cargo test`
  run, but artifacts do not capture byte payloads, and an outside reader
  who tries to reproduce from the saved artifact alone — as 017's
  honesty line implies they can — is surprised.
- Connection isolate is a one-shot in the simulated workload, while
  the live `tcp_echo` example uses a `RestartableSpawnSpec`. Echo bytes
  flow correctly because no panic ever fires; "Mariner echo parity"
  passes the assertion list while quietly missing the supervision half
  of the parity claim.

### What old behavior is still at risk

- 016 timer determinism, 017 seeded faults, 018 spawn/supervision: all
  green at this point. Risk is structural — 019 adds substantial new
  state to `Simulator<S>` (virtual listener resources, stream resources,
  pending-completion queue, ordering discipline, perturbation source).
  The step loop must preserve invariants:
  - timer due-time harvest still happens at the start of each step
  - panic restart still composes with any new TCP-driven sends in the
    same step
  - 017 fault paths still apply only to local-send and timer-wake
  Edits to the step loop during 019 implementation must preserve all of
  those.
- The blast-radius claim "existing `tina-sim` timer/replay/fault/
  checker/spawn/supervision tests still pass" must hold without
  migrating any existing test. Today's `Spawn = Infallible` workloads
  must remain compile-clean and behavior-clean. Say so explicitly so
  the implementation review has a sharp gate to check.

### What needs a human decision

- Whether the slice should be split (split point: bounded-overlap
  workload + fault-sensitive workload + checker into a 020 follow-up,
  with 019 closing on bind/accept/read/write/close + one-client echo +
  sequential multi-client + listener re-arm + close + invalid-resource
  + close-races-pending). My read: split, unless there is a specific
  reason to keep this large.
- Whether the simulated echo workload uses 018's
  `RestartableSpawnSpec`-shaped connection child. My read: yes; that
  is the only shape that earns "Mariner echo parity."
- Whether `local_addr`/`peer_addr` are honest in the simulator (i.e.,
  the listener can read its own bound address). My read: yes; without
  that, listener self-addressing diverges from live behavior.
- Whether the deterministic completion-ordering rule and the sync-vs-
  async dispatch-time split are pinned in the plan or left to the
  implementation. My read: pin them in the plan; both are observable
  through the trace and future invariants will hang on them.

### Recommendation

Plan is structurally on-shape (no new `tina` boundary, public TCP call
surface only, single-shard fenced, distinguishes direct/surrogate/blast-
radius proof, traps name the right things). Not ready to hand off to
implementation as written — the slice is large and the proof list has
too many `and/or` and unspecified-mechanism gaps for a slice this load-
bearing.

Amend the plan before implementation begins to:

1. Name an honest split point or accept the larger-slice review bar.
2. Pin connection-as-restartable-child as part of the simulated echo
   workload (composes 018 with 019).
3. Pin `local_addr`/`peer_addr` honesty in the simulator.
4. Pin the deterministic completion-ordering rule, citing the runtime's
   existing rule.
5. Pin the sync-vs-async dispatch-time split (bind/close inline; accept/
   recv/send pending).
6. Pin the deterministic source of partial writes.
7. Pin the deterministic source of partial/short reads.
8. Carry forward 017/018's replay-honesty sentence: artifacts reproduce
   under the same workload binary and simulator version, not from
   serialized payloads.
9. Replace `and/or` on the TCP perturbation surface with a chosen,
   named perturbation (or two named ones, both required).
10. Add a backpressure direct proof: a TCP completion hits a full
    mailbox and surfaces `CallCompletionRejected { reason: MailboxFull }`.
11. Add the close-while-pending direct proofs:
    - listener close + pending accept
    - stream close + pending read
    - stream close + pending write
12. State that the simulated echo runs the same connection isolate code
    as the live `tcp_echo` example, including the pending-buffer +
    drain pattern.
13. State explicitly that 019's TCP perturbation is its own surface;
    017 fault paths and 018 spawn/restart paths stay scoped to their
    existing surfaces.
14. Pin `CallResult::TcpRead { bytes }` and `TcpWrote { count }` as the
    same shapes the live runtime returns.
15. Pin resource-id ownership honestly: runtime-owned handles may be passed
    between isolates; stale/closed/unknown ids fail as
    `CallFailed(InvalidResource)`.
16. Acknowledge 019 is a parity-dominant slice with one DST-value
    addition, or grow the DST-value side to match 017/018 depth.
17. State that listener re-arm comes through 018's batch + spawn
    surface; no simulator-only re-arm helper is added.
18. State that simulated TCP resources are scoped to one simulator
    instance and not transferable across instances or shards.

None of these require a new `tina` boundary. They are tightening the
proof list against the parity bar the plan already names. After those,
the slice is reviewable on its own terms.

## Session A Response To Plan Review 1

Accepted the review's framing: 019 remains a deliberately large package, and
the plan now accepts the larger review bar rather than splitting the phase.
The purpose is still one coherent story: single-shard TCP/DST parity for the
Mariner echo workload.

Plan changes made:

- Added an explicit large-slice statement: 019 should not close on primitive
  TCP calls or a one-client happy path.
- Pinned connection-as-restartable-child as part of Mariner echo parity:
  listener spawns each connection via `RestartableSpawnSpec` with bootstrap,
  and listener re-arm uses the existing `Effect::Batch(Spawn, Send-self)`
  workflow.
- Pinned `local_addr` / `peer_addr` behavior:
  - `TcpBound { local_addr }`
  - `TcpAccepted { peer_addr }`
- Pinned live `CallResult` payload shapes:
  - `TcpRead { bytes }`
  - `TcpWrote { count }`
- Pinned deterministic completion ordering:
  - per-resource FIFO
  - global tie-break by call request order for completions visible in the same
    step
- Pinned observable sync-vs-async behavior:
  - bind/close complete inline during dispatch
  - accept/read/write remain pending and complete on later steps
- Pinned deterministic causes for partial I/O:
  - partial writes from explicit per-stream write caps
  - short reads from available peer-buffer bytes and/or configured read chunk
    sizes
- Replaced TCP perturbation `and/or` with two named required surfaces:
  - delayed TCP completions by deterministic simulator steps
  - ready-completion reordering for tied completions while preserving
    per-resource FIFO
- Added direct proof requirements for:
  - TCP completion into a full requester mailbox =>
    `CallCompletionRejected { reason: MailboxFull }`
  - listener close while accept pending
  - stream close while read pending
  - stream close while write pending
  - requester stop while accept/read/write pending
  - invalid resource failures
  - stale/closed/unknown resource use as `CallFailed(InvalidResource)`
  - no simulator-only isolate-local ownership rule over runtime-issued
    listener/stream handles
- Added replay honesty language: replay is against the same workload binary and
  simulator version, not serialized isolate code, TCP payload scripts, spawn
  factories, or bootstrap closures.
- Added explicit composition scoping:
  - 019 TCP perturbation is its own surface
  - 017 local-send/timer fault surfaces and 018 spawn/restart behavior keep
    their existing meanings and must compose additively
- Added single-simulator/single-shard resource scope language.
- Added traps forbidding simulator-only listener re-arm and simulator-only
  connection stand-ins.

No `tina` boundary change is implied by these amendments.

## Implementation Review 1 — Session B

Reviewed the landed implementation against the amended plan and against the
behavior the live runtime already ships, by reading `tina-sim/src/lib.rs`,
the new `tina-sim/tests/io_simulation.rs` (1786 lines, 17 tests), the
extended `tina-sim/tests/supervision_simulation.rs` (now 11 tests, up from
7 — 018 implementation-review follow-ups also landed), the new
`tina-sim/tests/consumer_api.rs`, the live-runtime references in
`tina-runtime-current/src/call.rs` and `io_backend.rs`, and the updated
SYSTEM/CHANGELOG/ROADMAP. Reran the test surface and `make verify` end-to-
end.

### What I verified directly

- Boundary promise held. `git status` shows zero changes under `tina/`,
  `tina-supervisor/`, `tina-runtime-current/`. The simulator imports
  `ListenerId`, `StreamId`, `CallFailureReason`,
  `CallCompletionRejectedReason` from `tina-runtime-current`, but adds
  nothing to those crates.
- `cargo +nightly test -p tina-sim` is green:
  - `io_simulation` 17/17 (new in 019)
  - `supervision_simulation` 11/11 (4 new since 018 closeout)
  - `consumer_api` 2/2 (new)
  - `faulted_replay` 4/4
  - `retry_backoff` 4/4
  - `timer_semantics` 7/7
- `make verify` exits 0 (clippy `-D warnings`, doc, loom, full workspace
  test).
- Existing `Spawn = Infallible` workloads in `retry_backoff.rs`,
  `timer_semantics.rs`, `faulted_replay.rs`, and supervision tests still
  compile and run unchanged. Additivity preserved.
- 019 does not introduce a second meaning model. New events all flow
  through the live `RuntimeEventKind` vocabulary
  (`CallDispatchAttempted`, `CallCompleted`, `CallFailed`,
  `CallCompletionRejected`, `Spawned`, etc.) using
  `tina-runtime-current`'s `CallKind`, `CallFailureReason`,
  `CallCompletionRejectedReason` enums. SYSTEM.md's "use the real runtime
  event model" rule holds.

### Plan-review amendments: status against the implementation

1. Slice size acknowledged as large — **closed**.
   Plan was amended to say so out loud; the larger review bar is what is
   in front of me.
2. Connection-as-restartable-child composes 018 with 019 — **closed**.
   `Listener` (io_simulation.rs:139) uses
   `type Spawn = RestartableSpawnSpec<Connection>` and spawns each
   accepted connection via `RestartableSpawnSpec::new(...)
   .with_bootstrap(|| ConnectionMsg::Start)` (lines 193-203). Listener
   re-arm goes through `Effect::Batch(spawn, Send-self)` (lines 210-213),
   not a simulator helper. The listener is supervised
   (`OneForOne` / `RestartBudget::new(4)`, line 696-699). 018's
   spawn/restart machinery and 019's TCP machinery are exercised in the
   same workload, by construction.
3. `local_addr` / `peer_addr` honesty — **closed**.
   `handle_tcp_bind` returns `CallResult::TcpBound { listener,
   local_addr }` from the scripted `local_addr` (lib.rs:1665-1668);
   `handle_tcp_accept` returns
   `CallResult::TcpAccepted { stream, peer_addr }` from the scripted
   peer (lib.rs:1691-1696). The `Listener` isolate reads
   `local_addr` from the result and stores it via `bound_addr_slot`,
   and `scripted_tcp_echo_round_trips_one_client_payload` asserts
   `run.bound_addr == Some(local_addr(41000))` — proving the workload
   actually consumes the address rather than the test inspecting
   simulator state.
4. Per-resource FIFO with global tie-break by request order — **closed in
   code, partially closed in proof**.
   `schedule_tcp_completion` clamps `ready_at_step` to
   `max(ready_at_step, previous_ready_for_same_resource)` (lib.rs:1817-
   1826), and `order_ready_tcp_completions` tie-breaks ties by
   `insertion_order`. The CHANGELOG even records that the seeded delay
   path was *fixed* to preserve per-resource FIFO during implementation
   ("seeded TCP delay perturbation now preserves per-resource FIFO by
   never allowing later completions on the same listener/stream to
   overtake earlier ones") — strong signal that the proof surface
   actually exercised the rule. However, no direct test pins the
   per-resource FIFO property by sequencing two reads on the same
   stream and asserting completion order. Acceptable — the rule is
   exercised indirectly by every echo workload — but worth a one-line
   note as surrogate-supported rather than directly proved.
5. Sync (bind/close) inline vs async (accept/read/write) pending —
   **closed**.
   `handle_tcp_bind` calls `deliver_completion(..)`
   (lib.rs:1663-1669); `handle_tcp_listener_close` and
   `handle_tcp_stream_close` likewise call `deliver_completion(..)`
   (lib.rs:1779, 1801). `handle_tcp_accept`, `handle_tcp_read`,
   `handle_tcp_write` all call `schedule_tcp_completion` with
   `ready_at_step = step_ordinal + 1 + delay` (lib.rs:1817). The trace
   distinction is observable in event ordering. Not pinned by a direct
   trace-shape test, but visible enough that any regression would
   immediately show in echo-workload trace counts.
6. Partial-write deterministic source — **closed**.
   `ScriptedPeerConfig.write_cap` is the explicit per-stream cap; bind
   panics if `write_cap == 0` (lib.rs:1642-1644).
   `accept_reports_peer_addr_and_read_write_use_live_result_shapes`
   asserts `WriteProbeMsg::Wrote(3)` for a 7-byte write under
   `write_cap = 3` — direct proof that the cap fires.
7. Partial-read deterministic source — **closed**.
   `ScriptedPeerConfig.inbound_chunks` + `read_chunk_cap` are the
   explicit causes; `stream_read_result` honors both (lib.rs:1881-1909).
   The overlap-and-partial-io test uses `read_chunk_cap = Some(2)` and
   asserts the echo round-trips 6-byte payloads through 2-byte short
   reads (>= 6 reads expected per direction).
8. Replay honesty — **closed**.
   SYSTEM.md now carries the sentence: "Replay artifacts still reproduce
   against the same workload binary and simulator version; they do not
   serialize arbitrary isolate values, TCP payload scripts, spawn
   factories, or bootstrap closures." The 017 honesty pattern is now
   uniform across 017/018/019.
9. Two named TCP perturbation surfaces — **closed**.
   `TcpCompletionFaultMode::DelayBySteps { one_in, steps }` and
   `ReorderReady { one_in }` (lib.rs:160-181). Both are directly
   proved divergent (`different_seeds_diverge_under_tcp_delay_faults`,
   `different_seeds_diverge_under_tcp_ready_reordering`). The
   `ReorderReady` path is also wired into the checker-replay test
   (`tcp_checker_failure_replays_under_ready_reordering`).
10. Bounded-mailbox / `MailboxFull` on TCP path — **closed**.
    `tcp_completion_rejected_when_requester_mailbox_is_full` uses
    `register_with_mailbox_capacity(.., 1)` to fill the waiter's mailbox
    via a follow-up `Fill` message, then asserts
    `CallCompletionRejected { call_kind: TcpAccept, reason:
    MailboxFull, .. }` against the trace. Direct event-record proof.
11. Close-while-pending direct proofs — **closed for all three**.
    - `listener_close_fails_pending_accept`
    - `stream_close_fails_pending_read`
    - `stream_close_fails_pending_write`
    All three exist and run. They drive the `fail_pending_accepts`
    /`fail_pending_tcp_completions` paths (lib.rs:1923-1951) which use
    the live `CallFailureReason::InvalidResource` shape.
12. Same connection-isolate code as the live echo (pending-buffer +
    drain) — **closed**.
    `Connection::handle` (io_simulation.rs:54-77) carries the same
    pending-buffer pattern as the Mariner echo connection: on
    `WriteCompleted { count }`, drain the consumed prefix and re-issue
    the write if more remains; otherwise re-issue the read. Not the
    *exact* same source file, but the same shape, which is the
    proof-bar standard the live `tcp_echo` example uses.
13. Composition scoping with 017/018 — **closed**.
    SYSTEM.md states it; `consumer_api.rs` and a new
    `restart_workload_composes_with_seeded_local_send_delay` (in
    supervision_simulation.rs) test the additive-but-scoped claim.
14. `TcpRead { bytes }` and `TcpWrote { count }` shapes — **closed**.
    `accept_reports_peer_addr_and_read_write_use_live_result_shapes`
    pattern-matches on the live result shapes directly.
15. Cross-isolate / opaque resource discipline — **closed for invalid
    handles, no isolate-locality rule**.
    `invalid_tcp_resources_surface_failures` proves a fabricated
    `ListenerId::new(999)` / `StreamId::new(999)` produce
    `CallFailed(InvalidResource)`. The simulator does *not* impose any
    isolate-local capability rule on resource ids, which is exactly the
    plan's amended language ("listener and stream ids remain runtime-
    owned handles that may be passed between isolates; the simulator
    must not add isolate-local capability rules"). Faithful to the
    runtime.
16. DST-value framing — **closed honestly**.
    Two named perturbations + one checker-replay test. Plan
    acknowledges this is a parity-dominant slice.
17. Listener re-arm via 018 batch+spawn — **closed**.
    Visible in code; no simulator-only re-arm helper exists.
18. Single-simulator resource scope — **closed**.
    SYSTEM.md says so; lib.rs creates resources with monotonically
    increasing ids inside one `Simulator<S>`.

### What I verified beyond the amendment list

- The implementation flushed out two real driver bugs during 019
  (recorded in CHANGELOG):
  - `run_until_quiescent()` and the checked variant now keep stepping
    while pending TCP completions remain in flight; previously they
    stopped early when no timers or visible messages existed yet
  - seeded TCP delay perturbation now preserves per-resource FIFO
  Both are honest signs that the new proof surface exercised real
  behavior. The current `run_until_quiescent` loop (lib.rs:1004-1023)
  now checks `has_in_flight_calls()` before declaring quiescence.
- 018 implementation-review residual gaps quietly closed in this slice.
  `supervision_simulation.rs` grew from 7 to 11 tests:
  - `restart_sensitive_checker_failure_is_replayable` — closes the
    "checker can *halt* on a restart-sensitive invariant" gap I flagged
    in the 018 implementation review (item 9, partially closed →
    closed)
  - `restartable_bootstrap_factory_runs_fresh_for_each_replacement` —
    closes the "bootstrap *value* can differ per restart" gap I flagged
    in the same review
  - `same_step_spawns_from_different_parents_follow_registration_order`
    — extends 018's deterministic-ordering proof beyond the batch case
  - `restart_workload_composes_with_seeded_local_send_delay` — proves
    the additive-composition claim with a real test, not just a
    SYSTEM.md sentence
  These are not 019-required but are net positive carryover.
- The `Listener` workload reads `local_addr` and `peer_addr` through
  the same field reads the live `tcp_echo` example uses (matching on
  `CallResult::TcpBound { local_addr, .. }` and
  `CallResult::TcpAccepted { peer_addr, .. }`). Not just type-shape
  parity — call-site parity.
- `pending_completion_capacity` is enforceable: `schedule_tcp_completion`
  returns `CallFailed(Io)` if the queue is full (lib.rs:1810-1813).
  Plan explicitly listed bounded-pending-completion as part of the
  bounded-buffer story.

### How this could still be broken while the listed tests pass

- Per-resource FIFO is enforced in the implementation and indirectly
  exercised by every echo workload, but no direct test sequences two
  reads/writes on the same stream and asserts strict completion order.
  A regression that breaks the `previous_ready` clamp under specific
  ordinal patterns might still pass every echo workload because the
  echo path issues read → write → read serially, never two pending
  reads in flight. Surrogate-supported, not directly proved.
- "Different seeds diverge" tests use `assert_ne!` on full event
  records. Any divergence at all (including event-id renumbering)
  triggers inequality, so the property is weaker than it reads. A
  bug that perturbs only event ids without changing real ordering
  would pass. Acceptable for this slice but worth a future tightening
  to assert specific reorderings.
- The checker-replay test uses `ReorderReady { one_in: 1 }` —
  *maximal* reordering. A bug where reordering only fires on the
  first batch but not later batches would still trip the checker on
  the first batch and pass. A `one_in: 2` or `one_in: 3` test would
  be more revealing. Surrogate-acceptable for this slice.
- `pending_completion_capacity` exhaustion produces
  `CallFailed(Io)` (lib.rs:1810-1813), but no direct test exercises
  it. Reachable code with no proof. Plan didn't strictly require it,
  but SYSTEM.md now claims "explicitly bounded ... rather than hidden
  unbounded buffering" — that claim is one test away from proved.
- `handle_tcp_bind` returns `Failed(Io)` for unknown bind addresses
  and for re-binding an open listener (lib.rs:1610, 1622). Reachable
  but not directly tested.
- The bounded-overlap test name says "overlap and partial I/O" but
  uses `accept_after_step: 1` for both peers, so both are immediately
  ready — bounded-overlap parity with the live runtime is plausibly
  weaker than the test name suggests. Honest enough for closeout
  given the listener accepts both before processing, but worth flagging
  if a future Voyager slice needs sharper overlap semantics.
- The "graceful listener close" path is exercised by every echo
  workload (the listener closes itself after `target_accepts`), but
  no test directly asserts the trace shape "all peers handled, then
  `TcpListenerClosed`, then `IsolateStopped`" as a single sequenced
  invariant. Indirect proof through workload completion.

### What old behavior is still at risk

- 016 timer determinism, 017 seeded faults, 018 spawn/supervision: all
  green. Risk is structural — 019 added substantial state to
  `Simulator<S>` (`listeners`, `streams`, `pending_accepts`,
  `pending_tcp_completions`, `next_listener_id`, `next_stream_id`,
  `next_tcp_completion_ordinal`). The `step()` loop now harvests
  timers *and* TCP at the start of each step (lib.rs:905-906), in
  that order. Future edits must preserve:
  - timer harvest before TCP harvest (so a timer that fires and
    schedules a TCP call is processed in the same step the timer
    fires)
  - TCP harvest happens before the `round_messages` snapshot, so a
    completion that arrives this step is visible this step
  - `run_until_quiescent` continues while either timers, in-flight
    calls, or pending messages remain — the current ordering checks
    are at lib.rs:1009-1020.
- The `run_until_quiescent` early-stop bug is exactly the kind of
  regression a future refactor could reintroduce. No automated
  invariant pins the loop's must-continue conditions, just the
  workload-completion assertions.

### What needs a human decision

- Whether the few surrogate-supported gaps above (per-resource FIFO
  direct test, capacity-exhaustion test, divergence-shape tightening,
  graceful-close trace-shape assertion) close in this phase or as a
  one-line follow-up note in `commits.txt` / closeout. My read:
  closeout is reasonable as-is; surrogate proof through the echo
  workloads is honest enough at this maturity. Worth a short
  follow-up note acknowledging it.
- Whether 019 should also pin a trap against future runtime tests that
  smuggle real `Vec<u8>` payloads through scripted peers in ways that
  diverge from live wire bytes. Probably not — the loopback model is
  the agreed-upon scope.

### Recommendation

- All 18 plan-review-amendment items are closed in code. Direct proofs
  cover bind/accept/read/write/close, both perturbation surfaces,
  close-while-pending for all three pending-op variants, requester-stop
  rejection, mailbox-full rejection, invalid resources, partial reads,
  partial writes, listener-as-supervisor with restartable connection
  children, replay-from-config, and checker-halt-and-replay under
  reordering. The slice as landed clears the "full Mariner echo story
  under simulation" bar the plan set.
- Implementation flushed out two real simulator driver bugs
  (`run_until_quiescent` early stop; per-resource FIFO under seeded
  delay) — strong signal that the new proof surface is exercising real
  behavior rather than just adding test surface area.
- 018 residual gaps from the prior implementation review (restart-
  sensitive checker halt, bootstrap factory freshness, same-step
  cross-parent ordering, fault composition) are also closed in this
  slice as carryover.
- Boundary promise held; existing 016/017/018 workloads run unchanged;
  `make verify` is green.

019 is closeable on its own terms. Suggested closeout notes (not
blocking):

1. Acknowledge that per-resource FIFO is surrogate-supported through
   echo workloads rather than directly proved by a same-stream
   sequenced-completion test.
2. Acknowledge that "different seeds diverge" tests assert
   `assert_ne!` on full event records and could be tightened to assert
   specific reorderings.
3. Acknowledge that `pending_completion_capacity` exhaustion is
   reachable but not directly tested; SYSTEM.md's "explicitly bounded"
   claim leans on it.
4. Acknowledge that the bounded-overlap test name is slightly stronger
   than the test exercises (both peers are immediately ready); future
   sharper-overlap work belongs in a later slice.

After those notes, 019 closes single-shard I/O simulation honestly: no
new `tina` boundary, public TCP call surface only, single-shard,
parity with the live echo story including connection-as-restartable-
child, two named TCP perturbations, replay-from-config, and checker-
backed replay of a reordered run.

## Closeout Review 1

Session:

- A

Final closeout verdict:

- 019 is closeable and ready to ship on its own terms.

What was tightened after the earlier review:

- Per-resource FIFO is now directly proved by a same-stream read-order test
  under `DelayBySteps`, not just surrogate-supported through the echo
  workloads.
- `pending_completion_capacity` exhaustion is now directly proved and surfaces
  visible `CallFailed(Io)` rather than relying on reachable-but-untested code.
- "Different seeds diverge" is now proved by specific public-path ordering
  assertions on competing accept completions, not merely `assert_ne!` on full
  event records.
- Overlap proof is now sharper: in addition to the original bounded-overlap
  echo case, a tangled two-connection workload drains one byte at a time under
  forced ready-completion reordering and still preserves correct echoed output.

What is directly proved at closeout:

- public-path bind/accept/read/write/listener-close/stream-close through
  `tina-sim`
- deterministic `local_addr` and `peer_addr` surfaced through the live
  `CallResult` vocabulary
- live `TcpRead { bytes }` and `TcpWrote { count }` result shapes
- invalid, stale, and closed resource handling through
  `CallFailed(InvalidResource)`
- listener-close-while-accept-pending
- stream-close-while-read-pending
- stream-close-while-write-pending
- requester-stop rejection for pending accept/read/write completions
- mailbox-full rejection on the TCP completion path
- explicit pending-completion-capacity exhaustion
- per-resource FIFO under delayed TCP completions
- partial-write drain behavior
- partial/short-read behavior
- one-client echo
- sequential multi-client echo
- bounded-overlap echo
- tangled overlap with single-byte drain under forced reordering
- listener-as-supervisor spawning restartable connection children with
  bootstrap, matching the live `tcp_echo` shape
- checker halt and replay over a reordered TCP run
- downstream consumer use of the public `tina-sim` API, including replay from
  saved config

What remains intentionally out of scope:

- packet-level TCP realism
- multi-shard I/O simulation
- filesystem, subprocess, UDP, TLS, DNS, or broader kernel simulation
- serialization of arbitrary Rust isolate code, closures, factories, or byte
  payload scripts into replay artifacts; replay remains "same workload binary +
  same simulator version"

Verification at closeout:

- `cargo fmt --all`
- `cargo test -p tina-sim`
- `cargo check --workspace`
- `make verify`

No 019 human-escalation items remain.
