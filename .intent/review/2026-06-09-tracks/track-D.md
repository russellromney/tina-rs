# Track D — Bridges and External Work (2026-06-09, HEAD 0cd6a31)

Scope: tina-sqlx-bridge, tina-reqwest-bridge, tina-aws-bridge, tina-sqlite-bridge,
tina-rpc-tokio, tina-tokio-bridge, tina-tower-bridge, and the bridge-facing
completion-delivery surface in `tina-runtime/src/dispatch.rs`.
Carve-out honored: per-variant AWS slot-conservation line audit and the
cross-worker D2 self-continuation pattern check are a sibling agent's.

Method: read every bridge worker's admit/poll/timeout/close path end to end;
scrutinized the D2/D1-residual fix commit (`cb5afa3`, PR #233) itself; attempted
to disprove every candidate before filing. One targeted `cargo check` run to
confirm a compile-level finding (essential).

## Verdict

The slot-leak class from the prior review is genuinely fixed and the fix itself
holds up under adversarial reading. The bridge crates' capacity/timeout/late-result
semantics are consistent and honest. One real, proven regression found: the
`tracing` feature of tina-sqlx-bridge does not compile on HEAD. The rest is
low-tier: a residual misclassification window in the rpc-tokio shim, a public
conversion that teaches `Timeout == Closed`, and a doc/code gap on cancel-call
completions.

---

## Findings

### D-1 — tina-sqlx-bridge does not compile with `--features tracing` (E0004)

1. **Severity:** Medium (High for any user enabling the feature — hard build break)
2. **Confidence:** High (proven by compiler)
3. **File/line:** `tina-sqlx-bridge/src/worker.rs:460` (`emit_replied`), variant at
   `tina-sqlx-bridge/src/types.rs:796`
4. **Violated invariant:** every advertised cargo feature must build; cfg-gated
   code is still code.
5. **Bug:** `emit_replied`'s `match outcome` covers `Committed` and `RolledBack`
   only. Commit `c4a3b17` ("Surface ambiguous sqlx transaction commits") added
   `PgTransactionOutcome::CommitAmbiguous` and updated `tally_worker_terminal`
   (non-gated) but not the `#[cfg(feature = "tracing")] emit_replied` function.
   ```
   error[E0004]: non-exhaustive patterns: `&PgTransactionOutcome::CommitAmbiguous { .. }` not covered
     --> tina-sqlx-bridge/src/worker.rs:460:55
   ```
6. **Why real:** the feature is public and documented ("Optional tracing
   emission", Cargo.toml:45). Any downstream `tina-sqlx-bridge = { features =
   ["tracing"] }` fails to build. It shipped because no in-tree consumer enables
   the feature and CI/clippy runs default features only — the same blind spot
   that will eat the next cfg-gated edit.
7. **Repro:** `cargo check -p tina-sqlx-bridge --features tracing` (fails on HEAD).
8. **Fix:** add the missing arm (e.g. `CommitAmbiguous { completed, error }` →
   `event!(WARN, outcome = "tx_commit_ambiguous", step_count, detail = %error)`).
   Then add a feature-matrix build to CI (at minimum `--features tracing` per
   bridge crate, or `--all-features` workspace check).
9. **LLM-pattern:** yes — classic "updated the visible function, missed the
   cfg-gated twin"; tests can't catch what never compiles in CI.

### D-2 — rpc-tokio shim can still drop a live reply under shim starvation (residual of D1-residual)

1. **Severity:** Low
2. **Confidence:** Medium (mechanism verified in code; window requires an
   unusual stall)
3. **File/line:** `tina-rpc-tokio/src/lib.rs:385-394` (shim sizing),
   `:487-499` (deadline backstop frees slots on wall clock)
4. **Violated invariant:** a terminal error should not become a `Timeout`.
5. **Bug:** the shim mailbox is sized `client_max_in_flight + max_in_flight`,
   which provably absorbs one close burst plus the bridge's own admitted
   replies. But shim ingress is ordinary droppable send, and the bridge's
   admission slots are freed by the *wall-clock* deadline backstop, not by shim
   drain. If the shim isolate is not scheduled for longer than one per-call
   deadline period (shard wedged by a long handler turn), the backstop frees M
   slots per period, M new requests are admitted and settled by the Client, and
   the shim mailbox accumulates beyond `C + M` after ~⌈(C+M)/M⌉ periods. The
   overflowing replies are dropped; their awaiters settle as backstop
   `BridgeError::Timeout` instead of their true cause. No capacity leaks (the
   backstop releases the slot), so this is misclassification only.
6. **Why real (barely):** needs a shard stall exceeding the configured per-call
   deadline, sustained across periods. Plausible with aggressive deadlines
   (1-10ms) plus a heavy co-resident isolate; implausible at default deadlines.
7. **Failing test idea:** shard with a deliberately slow second isolate
   (handler sleeps > deadline), bridge with deadline ≈ 1ms, loop calls; assert
   no awaiter's true `ConnectionClosed`/`Ok` is reported as `Timeout` while the
   Client's send of that result was dropped (observable via the dropped-send
   trace/metrics).
8. **Fix options:** carry the true cause into the backstop (record last-known
   client-side state), or make shim ingress non-droppable for this shim (it is
   runtime-owned plumbing, same liveness argument as the D2 continuation
   overflow), or document the deadline-floor assumption.
9. **LLM-pattern:** no — this is an honest residual of a deliberate sizing fix;
   the fix's own doc comment claims slightly more than the mechanism guarantees.

### D-3 — public `From<BridgeError> for SendRejectedReason` converts `Timeout` into `Closed`

1. **Severity:** Low
2. **Confidence:** High (conversion exists and is public; no in-tree caller)
3. **File/line:** `tina-tokio-bridge/src/lib.rs:485-492`
4. **Violated invariant:** "`Full`, `Closed`, `Rejected`, timeout, and
   cancellation are never silently converted into each other."
5. **Bug:** `BridgeError::Timeout => SendRejectedReason::Closed`. Any
   downstream code that funnels bridge outcomes into the runtime's send-reject
   vocabulary through this blessed `From` silently reclassifies a timeout as a
   closed target — the exact word-blur Track D hunts. The sibling impl
   (`From<BridgeError> for CallError`) maps `Timeout => Timeout` correctly, so
   the two conversions teach different truths for the same input.
6. **Why real:** it is a public API on a public crate; `?`/`into()` call sites
   will pick it up invisibly. Unused in-tree today (verified by grep), which is
   why it has never been caught.
7. **Repro:** `assert_eq!(SendRejectedReason::from(BridgeError::Timeout), SendRejectedReason::Closed)` — passes today, reads like a bug.
8. **Fix:** delete the impl (no in-tree users), or make it a named method
   `fn as_send_rejected_lossy(...)` with a doc stating the lossy mapping.
9. **LLM-pattern:** yes — plausible-looking exhaustive `From` written to satisfy
   the type system, wrong at the semantic boundary.

### D-4 — CancelCall completions are droppable on a full mailbox, contradicting the new "runtime-call continuations never drop" rule

1. **Severity:** Low-Medium
2. **Confidence:** Medium (code behavior certain; impact depends on user code
   waiting on a CancelOutcome)
3. **File/line:** `tina-runtime/src/dispatch.rs:1669-1702` (`dispatch_cancel_call`
   delivery via droppable `enqueue_entry_message`); contrast
   `docs/mailbox-capacity.md` § "Runtime-call continuations never drop" and the
   non-droppable path at `dispatch.rs:2217` (`enqueue_call_continuation`).
4. **Violated invariant:** doc-stated "the runtime never drops a runtime-call
   continuation" — `CallKind::CancelCall` is a runtime call whose completion is
   delivered through the droppable path.
5. **Bug:** after PR #233, driver-call completions (`deliver_backend_completion_action`)
   overflow instead of dropping, but a cancel call's `CancelOutcome` message is
   still `enqueue_entry_message` → `CallCompletionRejected { MailboxFull }` and
   gone. The cancel itself takes effect (pending entry removed, handle set
   `Cancelled`), so nothing leaks runtime-side — but the requester that issued
   `cancel(...).then(...)` never observes the outcome, and any user-held
   resource keyed to "cancel confirmed" wedges. Same delivery shape applies to
   `ObservedSend` outcomes (`dispatch.rs:1394-1424`), but those are explicitly
   documented as best-effort in `docs/mailbox-capacity.md`; CancelCall is not
   listed there at all, and the doc's blanket "runtime calls" sentence reads as
   covering it.
6. **Why real:** cancel is precisely the operation issued under saturation —
   the moment the requester's mailbox is most likely to be full.
7. **Failing test idea:** isolate with capacity-1 mailbox issues a call, then a
   cancel with `.then(CancelDone)`, while the harness keeps its mailbox full;
   assert the `CancelDone` continuation eventually arrives (fails today —
   trace shows `CallCompletionRejected { CancelCall, MailboxFull }`).
8. **Fix:** route CancelCall (and arguably ObservedSend) completions through
   `enqueue_call_continuation`; or amend `docs/mailbox-capacity.md` to list
   CancelCall/ObservedSend as the droppable exceptions, by name.
9. **LLM-pattern:** partial — the D2 fix correctly generalized one site and the
   doc over-generalized the claim; the remaining sites were not re-audited
   against the new sentence.

### D-5 — tokio-bridge metrics misattribute caller-cancel as `closed` (and double-count with `timeout`)

1. **Severity:** Low (metrics only)
2. **Confidence:** High
3. **File/line:** `tina-tokio-bridge/src/lib.rs:1040-1044` (preflight maps a
   cancelled queued request to `ThreadedSendObservedError::MailboxClosed`),
   `:1058-1061` (observer records it via `record_error_on` → `closed += 1`).
4. **Violated invariant:** cancellation is not `Closed`.
5. **Bug:** when a caller times out before the worker dequeues its request, the
   timeout site records `timeout += 1`; the worker-side preflight then reports
   the stale request as `MailboxClosed`, so the observer records `closed += 1`
   for the same logical call. `BridgeMetricsSnapshot.closed` is documented as
   "bridge, worker, mailbox, or responder was closed" — a caller-side timeout
   is none of those. Dashboards see phantom closes exactly during overload.
6. **Why real:** any timeout that beats worker dequeue (the common overload
   ordering) takes this path.
7. **Failing test idea:** one slow isolate, capacity-1 mailbox, two calls with
   short timeout; assert `closed == 0` after both time out (fails today).
8. **Fix:** add a distinct preflight outcome (e.g. skip the observer error and
   count a dedicated `cancelled_before_admission` counter), or at minimum stop
   feeding preflight-cancel into `record_error_on`.
9. **LLM-pattern:** mild — reusing the nearest existing error variant instead
   of modeling the real state.

### D-6 — blocking drain loops are safe only from host threads, and nothing says so

1. **Severity:** Low (footgun, not a defect on the intended path)
2. **Confidence:** High (mechanism), Low (anyone actually misusing it in-tree)
3. **File/line:** `tina-aws-bridge/src/core.rs:31-58` (`await_drain`,
   1ms `thread::sleep` poll loop), used by every `XxxCloser::close_and_drain`
   (e.g. `tina-aws-bridge/src/worker.rs:101`); same shape in
   `tina-tokio-bridge/src/lib.rs:739-766` (`drain_and_shutdown`, which at least
   ships an async twin).
4. **Violated invariant:** shutdown must eventually settle / no blocking waits
   on the thread that produces the awaited progress.
5. **Bug:** the AWS in-flight counter only decrements when the bridge isolate's
   poll runs on its shard thread. `close_and_drain` called from any isolate
   handler on that shard blocks the shard for the full `timeout`, guarantees
   `drained == false`, and stalls every co-resident isolate meanwhile. No doc
   on the method or crate root restricts it to host threads.
6. **Why real:** "drain the bridge before stopping" is a natural thing to do
   from a supervisor isolate.
7. **Failing test idea:** call `close_and_drain(500ms)` from an isolate handler
   on the bridge's shard with one in-flight op that would finish in 10ms;
   assert it reports drained (fails today: shard blocked, never drains).
8. **Fix:** document "host thread only" on every `close_and_drain`, or have it
   detect/assert it is not on a shard thread, or provide a poll-shaped
   non-blocking variant.
9. **LLM-pattern:** no — deliberate simple design, just under-documented.

---

## Disproven suspicions (with proof)

- **D2 fix re-audit (continuation overflow, PR #233): no new gap found.**
  Checked every overflow ordering: (a) `recv_entry_message` drains overflow
  before the mailbox and carries `call_context` inline, so the parallel
  `call_contexts` queue cannot desync (`tina-runtime/src/registration.rs:494-505,
  531-555`); (b) the skip-empty scan uses `entry_has_pending_message`, which
  checks both lanes, so an overflow-only entry is never skipped
  (`dispatch.rs:276-289`); (c) both entry-construction sites initialize
  `continuation_overflow` (`registration.rs:387, 644`), so restart gets a fresh
  empty lane and the old lane drops with the old incarnation's state — same
  lifecycle as the mailbox; (d) only `Closed` (requester gone) remains terminal;
  (e) overflow growth is bounded by the isolate's own outstanding runtime
  calls — self-inflicted, not amplifiable by peers.
- **The "flaky test" judgment call (counting admission `CallOutcome::Full` as
  settled) is sound.** Runtime-level `Full` means the bridge isolate's bounded
  mailbox bounced the request at delivery; the bridge's slot is only taken
  inside `admit()` after the message is handled
  (`tina-sqlite-bridge/src/worker.rs:295-395`), so an admission-`Full` caller
  never held a slot. More importantly the test's slot-conservation proof does
  not rest on per-caller outcomes at all: it asserts `admitted >= baseline +
  rounds` (a wedged capacity-1 bridge fails this) and in-flight drains to 0
  (`tina-sqlite-bridge/tests/bridge.rs:1011-1041`). `Timeout`/`Closed` still
  panic.
- **Isolate-call replies dropped on requester `MailboxFull`**
  (`dispatch.rs:1925-1947`, after `complete_isolate_call` removed the pending
  entry, so no later backstop): looks alarming, but it is the documented
  best-effort design — `docs/mailbox-capacity.md` § "Diagnosing under-capacity"
  names exactly this event, and sizing rules put outstanding continuations in
  the capacity formula. Not re-filed. (Cancel-call completions are *not*
  covered by that doc — that residual is finding D-4.)
- **rpc-tokio `fire_observer_err` mapping `IngressFull|WorkerStopped →
  IoError`** (`tina-rpc-tokio/src/lib.rs:215-221`): suspected capacity→IO
  misclassification; disproven as unreachable. The observer passed to
  `try_send_and_observe_with` can only ever receive `MailboxFull`/`MailboxClosed`
  (`tina-runtime/src/threaded.rs:1257-1271`); ingress errors return
  synchronously as `ThreadedTrySendError` and take the `ClientUnavailable`
  branch (`lib.rs:542-548`). The arms are dead defensive code. If the worker
  stops after accepting the command, the observer closure is dropped unfired
  and the per-call deadline backstop settles the awaiter (as `Timeout` — folded
  into D-2's residual-misclassification note).
- **Correlator collision when two `BridgeClient`s share one `tina_rpc::Client`:**
  disproven. The Client keys `in_flight` by its own wire `request_id`
  (`tina-rpc/src/client.rs:316, 657-664`) and routes each result to the
  per-request `reply_to` (each bridge's own shim), so per-bridge correlator
  spaces never meet.
- **SQLite budget manifest claiming configurable capacity the worker can't
  deliver:** disproven — `SqliteConfig::validate` pins `max_in_flight`,
  `pending_reply_capacity`, and `external_pool_size` to exactly 1
  (`tina-sqlite-bridge/src/types.rs:399-442`), so the budget rows
  (`budget.rs:21-46`) cannot over-claim.
- **`settle_pending` poisoned-mutex hang in rpc-tokio** (`lib.rs:233`,
  `lock().ok()` no-ops on poison while the awaiter has no rx-side timeout):
  effectively unreachable — the only code that runs while holding the lock is
  `HashMap::insert`/`remove` (no panic path short of OOM). Noted: the insert
  path `expect`s on poison while settle paths silently no-op; if a panic site
  is ever added under the lock, the silent arm becomes a hang. Worth a
  one-line consistency cleanup, not a finding.
- **Reqwest per-request timeout longer than `config.default_timeout` silently
  capped by the client-level timeout:** disproven — `build_reqwest_request`
  sets `RequestBuilder::timeout` (`tina-reqwest-bridge/src/worker.rs:896-898`),
  which overrides the client default per reqwest semantics, and the bridge's
  per-attempt `tokio::time::timeout` uses the same value.
- **Reqwest timeout-abort racing a just-completed success:** the success is
  discarded and a retry runs; only retry-safe (idempotent-by-RFC) methods reach
  that branch (`worker.rs:514-537`, `method_is_retry_safe_by_default`), and the
  caller gets exactly one outcome. Standard, acceptable.
- **AWS classifier marking timeouts `Retryable` without an idempotency gate**
  (`tina-aws-bridge/src/classifier.rs`): disproven as a violation — the shared
  vocabulary explicitly defines `Retryable` as "if the caller's idempotency
  rules permit" and `BridgeCallerWarning::from_outcome` flags
  `CallerTimeout`/`BridgeTimeout` as work-may-still-be-running
  (`tina-runtime/src/bridge.rs:55-71, 518-527`).
- **Slot conservation in sqlx / reqwest / sqlite workers (non-AWS):** verified
  by path-tracing. sqlx: timeout marks `abandoned`, keeps receiver+slot until
  worker terminal; exactly one poll chain per admission; `PendingRetry` slots
  (reqwest) count against `max_in_flight`, so retry waits cannot over-admit;
  sqlite mirrors this with `in_flight: Option<_>` and `sync_channel(1)`
  invariants that hold (command is removed from the channel before any response
  can exist, so `try_send` cannot spuriously fail while idle).
- **Stale comment, not a bug:** `install_with_pool` says "DB-side cancel is
  opt-in only on the config-built path" (`tina-sqlx-bridge/src/worker.rs:597-599`),
  but the cancel sidecar is a documented Phase-123 no-op on *every* path
  (`lib.rs:209-213`; `db_cancels_sent` always 0). Crate-root docs are honest;
  the method comment should be synced when D-1 is fixed.

## Word-consistency map (Full / Closed / Timeout across bridges)

Checked the same words across all seven crates:
- `Full` = admission capacity (bridge cap) in sqlx/sqlite/reqwest/aws and
  rpc-tokio (bridge semaphore); in tina-tokio-bridge `Full` = bounded ingress
  or target mailbox. Both are pre-acceptance capacity refusals with no work
  started — consistent in the sense that matters (safe to retry, nothing ran).
- `Closed` = admission-closed everywhere; never used for timeout — except the
  two deviations filed as D-3 (public From impl) and D-5 (metrics attribution).
- `Timeout` = caller-authority settled while external work may continue, with
  slot held until worker terminal (sqlx/sqlite/aws) or task abort (reqwest,
  documented). Honest and uniform; late results are counted, never delivered.
- Two crates export a type named `BridgeError` (tina-tokio-bridge: 3 variants;
  tina-rpc-tokio: 10 variants) with different `Full` semantics as above.
  Tolerable, but worth a doc cross-link if it ever confuses.

## Coverage map

| Area | Result |
|---|---|
| D2/D1-residual fix commit (cb5afa3) re-audit | holds; no new gap (see disproven) |
| dispatch.rs completion-delivery surface | D-4 (CancelCall droppable); ObservedSend documented |
| tina-sqlx-bridge | D-1 (tracing build break); slots verified |
| tina-sqlite-bridge | clean; judgment call verified sound |
| tina-reqwest-bridge | clean; retry gating verified |
| tina-aws-bridge (minus carve-out) | D-6 (drain footgun); classifier clean |
| tina-rpc-tokio | D-2 residual; dead arms; cancel-safety verified |
| tina-tokio-bridge / tina-tower-bridge | D-3, D-5; tower layer thin and clean |
| tina-rpc client outbound-connect surface (phase 131) | spot-checked; queue/deadline/close paths consistent |

## Suggested tests

1. CI feature-matrix: `cargo check -p <each bridge> --features tracing`
   (catches D-1 class permanently).
2. Cancel-under-saturation: capacity-1 requester issues cancel `.then(...)`
   with a full mailbox; assert the CancelOutcome continuation arrives (D-4).
3. tokio-bridge metric attribution: two timed-out calls against a wedged
   isolate ⇒ `closed == 0`, `timeout == 2` (D-5).
4. rpc-tokio cause-truth under stall: short deadlines + slow co-resident
   isolate; assert dropped-reply count is zero or causes stay true (D-2).
