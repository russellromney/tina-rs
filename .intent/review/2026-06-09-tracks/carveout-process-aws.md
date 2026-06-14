# Carve-out — non-unix process supervision + per-variant AWS worker slot audit (2026-06-09)

HEAD reviewed: `0cd6a31` (= origin/main). Source treated read-only.
Sibling tracks read first: track-F.md (process; F6 drain-join verified on unix,
kill-CLI fork/exec filed as F-2026-06-09-A) and track-D.md (bridges; AWS
blocking-drain filed as D-6). Nothing here duplicates those.

Status: COMPLETE.

## Verdict (condensed)

- **Q1 (non-unix process supervision): not a defect.** Windows is provably out
  of scope (docs, CI, and no betelgeuse backend — the cfg(not(unix)) code is
  unbuildable-dead on every supported target). Three residuals recorded:
  Q1-a (Info: API doc gap on group-kill semantics), Q1-b (Low, latent:
  F6's no-detached-drain property is unix-only), Q1-c (accepted-Low:
  documented macOS pid-reuse corner; optional `waitid(WNOWAIT)` widening).
- **Q2 (AWS per-variant slot audit): code clean, tests uneven.** All five
  workers (S3/SQS/SNS/DynamoDB/Secrets) are byte-identical state machines;
  slot conservation holds on every terminal path in each; the D2 fix covers
  them automatically (their wakeups are `sleep().then(Poll)` runtime-call
  continuations, non-droppable since PR #233 — delivery path verified).
  One Low finding: SNS/DynamoDB/Secrets have no Full-path, no
  timeout/slot-held/late-result, and only empty-drain test coverage, and no
  variant has a mailbox-saturation slot-conservation test (AWS-Q2-A).

---

## Q1 — Non-unix process supervision (tina-runtime/src/driver/process.rs)

### Code state on HEAD (note: brief's premise is stale)

The 2026-06-08 review text said "non-unix `killed_group` is hardcoded `false`".
That variable no longer exists; `kill_and_reap` (process.rs:345) was restructured.
Current shape:

- Leader kill: `child.kill()` unconditionally (process.rs:358) — works on all
  std platforms (SIGKILL / TerminateProcess).
- Group kill: `kill_process_group` is `#[cfg(unix)]` only, called at
  process.rs:363 (kill path) and process.rs:482 (truncation path inside
  `process_exited`). On non-unix there is **no descendant kill at all** — same
  semantic gap the old `killed_group: false` encoded, now expressed by cfg.
- `spawn_process` (process.rs:436-441): `process_group(0)` is `#[cfg(unix)]`
  only; non-unix children share the parent's group/job. No Windows Job Object.

### Verdict: NOT a filed defect — Windows is explicitly out of scope, provably

Disproof of "real defect" (the platform is unsupported, three independent proofs):

1. **Docs claim Linux+macOS only.** CHANGELOG.md:1090-1091 (Phase 117 heading):
   "Supported targets are Linux and macOS; Windows waits on a `betelgeuse`
   Windows backend."
2. **CI never builds non-unix.** `.github/workflows/verify.yml:16-18` matrix is
   `ubuntu-latest` + `macos-latest` only; perf.yml and weekly.yml are
   ubuntu-only.
3. **The runtime cannot build/run on Windows anyway.** `tina-runtime` depends
   unconditionally on `vendor-betelgeuse` (tina-runtime/Cargo.toml:26), and
   betelgeuse's io backend has only `#[cfg(target_os = "macos")]` darwin and
   `#[cfg(target_os = "linux")]` linux modules (vendor-betelgeuse/io/mod.rs:1-4).
   There is no Windows completion backend, so the `cfg(not(unix))` branches in
   process.rs are dead code on every buildable target. A "defect" in
   unreachable code on an unsupported platform is not a defect.

So: timeout/cancel using `child.kill()` only, descendants unreaped on non-unix —
**acceptable**, consistent with the project's stated platform claim. The prior
review's own fix table agrees: docs/adversarial-review.md L10 "Fixed **on Unix**
by process-group cleanup and bounded drain joins" and A2 "kills the **Unix**
process group" — the unix-only scope is stated in the shipped review doc.

### Residual gaps recorded (not filed as defects; latent for a future Windows port)

**Q1-a [Info / High confidence] API surface says nothing about descendant/group
semantics — on any platform.**
`CallInput::ProcessRun` rustdoc (tina-runtime/src/call/io.rs:360-372) documents
only "Maximum time the process may run before Tina attempts kill/reap". The
process-group ownership claim ("process_run owns the whole process group") lives
only in internal comments (driver/process.rs:474) and docs/adversarial-review.md.
A user cannot learn from the public API that (a) descendants are killed on unix,
(b) that guarantee is unix-only. One doc sentence on the `ProcessRun` variant or
`process_run` helper closes it. Severity Info because the only divergent
platform cannot build (proof above).

**Q1-b [Low / High confidence, latent-only] F6 drain-join fix does NOT fully
hold on the non-unix path — bounded, but leaks the drain thread.**
- Invariant (from F6/PR #229): drain threads are cancelled and *joined*, never
  detached while possibly blocked forever.
- Mechanism: on unix, `spawn_drain_limited` (process.rs:513) sets `O_NONBLOCK`,
  so the drain loop polls and observes the `cancel` flag within ~1ms; the
  two-stage `join_drain_bounded` (process.rs:582) then joins promptly. On
  non-unix, `spawn_drain_limited` (process.rs:523-529) does **not** set the fd
  nonblocking (no fcntl), so the drain thread sits in a **blocking**
  `reader.read()` (process.rs:553) and checks `thread_cancel` only between
  reads. The `cancel.store(true)` at process.rs:600 cannot interrupt a blocked
  read; after `PROCESS_DRAIN_CANCEL_JOIN_TIMEOUT` the `DrainHandle` is dropped
  (process.rs:612) — a detached thread blocked in `read()` plus a leaked pipe
  handle, for as long as any unreaped descendant keeps the pipe open (and
  descendants are never group-killed on non-unix, see above — the two gaps
  compound).
- What *does* hold on non-unix: the bound itself. `join_drain_bounded` returns
  by deadline on every path, so the process worker thread is never pinned and
  every terminal still settles typed. The F6 *liveness* fix holds; the F6
  *no-detached-thread* property is unix-only.
- Real-use scenario: none today (unbuildable platform). Becomes real the day a
  Windows backend lands and a `cmd /c start`-style child outlives the timeout.
- Failing-test idea (future): Windows-only test spawning a child whose
  grandchild holds stderr open past timeout; assert no thread-count growth
  after N timed-out runs.
- Fix sketch: on Windows use overlapped/peek-style bounded reads or
  `CancelSynchronousIo` on the drain thread; or kill descendants first via a
  Job Object so EOF arrives, making cancel-join moot. Gate `process_run` with
  a `compile_error!`/typed `Unsupported` on non-unix until then (cheapest
  honest option).

**Q1-c [Low / Medium confidence] macOS (non-Linux unix) pid-reuse race on the
truncation group-kill — documented in-code, accepted; track-F carved it to this
report.**
- File/line: driver/process.rs:300-305 (`child_has_exited` non-Linux uses
  `try_wait`, which **reaps** the leader) vs process.rs:469-483
  (`process_exited` then signals `kill_process_group(process_group)` on
  truncation, after the bounded drains spend up to 200ms).
- Invariant: never signal a pgid the kernel may have recycled.
- Mechanism: on Linux, `waitid(WNOWAIT)` (process.rs:277-298) keeps the leader
  a zombie, so pid==pgid stays reserved across the group kill. On macOS the
  leader is already reaped at group-kill time. POSIX keeps a pid unreusable
  while it is still the pgid of a group **with live members** — and the
  truncation path usually fires precisely because live descendants hold the
  pipes, which protects the pgid. The exposed window is the corner where
  truncation came from the byte-limit (not live descendants), all group
  members are already dead, and the OS cycles the entire pid space within the
  ~≤200ms drain-join gap to hand pid as a *new group leader*. Implausible in
  practice (macOS pid space ~99998, sequential allocation), and the in-code
  comment at process.rs:480-481 states the residual race honestly.
- Disproof of "filed defect": probability-bounded, documented, and the blast
  radius is one stray SIGKILL to a *new process group leader* that won the
  exact pid in a sub-200ms window. Recorded as accepted-Low, no action
  required. Optional hardening: macOS also has `waitid(..., WNOWAIT)`; the
  `#[cfg(target_os = "linux")]` peek could plausibly widen to
  `#[cfg(unix)]`, deleting the race instead of documenting it (needs a macOS
  waitid semantics check — `si_pid` population differs subtly across BSDs).

---

## Q2 — Per-variant AWS workers (tina-aws-bridge)

Worker census (complete — checked lib.rs exports, bridge_adapter.rs, helpers.rs
for any other admission/slot logic; there is none): exactly five workers.

| variant | file | admit | poll | identical to S3 model? |
|---|---|---|---|---|
| S3 | worker.rs | 268-360 | 362-415 | (reference) |
| SQS | sqs_worker.rs | 258-352 | 354-407 | yes, line-for-line |
| SNS | sns_worker.rs | 257-348 | 350-403 | yes, line-for-line |
| DynamoDB | dynamodb_worker.rs | 271-364 | 366-421 | yes, line-for-line |
| Secrets | secrets_worker.rs | 255-347 | 349-404 | yes, line-for-line |

Slot = entry in `self.in_flight: HashMap<u64, InFlight>`, capacity-gated by
`in_flight.len() >= config.max_in_flight` before any SDK spawn. Self-wakeup =
`sleep(poll_interval).then(|_| Msg::Poll(id))`.

### (a) Admission slot returned exactly once on every terminal path — HOLDS, all five

Path-by-path (line refs are S3/worker.rs; the other four are the same shape at
the offsets in the census table):

1. **Admission rejection** (Closed :274 / Invalid :286 / Full :299): caller
   settled with a typed error before any slot insert or SDK spawn. No slot
   taken, none to return.
2. **Poll → `Ok(result)`** (:363-375): `remove(&id)` frees the slot exactly
   once, `note_terminal` re-derives `in_flight_current` from the *post-remove*
   map len, caller settled (or `note_late_external_terminal` if previously
   abandoned). No re-arm — chain ends.
3. **Poll → `Empty`, not timed out** (:398-399): slot re-inserted, exactly one
   new sleep continuation armed. Conserved.
4. **Poll → `Empty`, caller-deadline hit** (:377-397, caller-waiting only):
   caller settled `Timeout`, `abandoned` set, **slot kept** (re-inserted) and
   the chain re-armed — the honest H2/D3 contract: external capacity stays
   occupied until the SDK terminal. `in_flight_current` correctly NOT
   decremented (sqs_bridge.rs:877 pins this). The late terminal then frees the
   slot via path 2/5 exactly once.
5. **Poll → `Closed`** (SDK task died without sending — panic, or
   `Handle::spawn` on a shut-down tokio runtime dropping the task) (:401-413):
   slot freed once, typed `Internal` terminal. Self-healing.
6. **Caller cancel / reply-drop**: the worker's slot lifecycle never depends on
   the requester. `reply_to_request` to a gone/full requester is the
   documented best-effort delivery (track-D disproof list); the slot is freed
   by the poll chain regardless.
7. **Forged/stale `Poll(id)`** (the only way to run `poll` out of band — the
   variant is `#[doc(hidden)]` but constructible): unknown id → `remove` =
   `None` → `noop()` (:363-365). A duplicate chain (forged Poll on a live id
   while not terminal) re-inserts and re-arms, creating a second chain — but
   `remove(&id)` is exclusive, so only the first chain to observe the terminal
   completes/frees; the loser noops and its chain ends. No double-free, no
   double-complete, no leak. (`handle_call` rejects Poll with
   `UnsupportedMessage`, so the call path can't even do this.)
8. **id reuse**: `next_id` is a wrapping u64; collision needs 2^64 admissions.

The per-poll invariant "each consumed Poll message arms at most one new sleep
continuation, and exactly one iff the slot survives" holds in every branch of
all five `poll` bodies — read individually, not assumed from the template.

### (b) D2 self-continuation fix — APPLIES automatically, verified, nothing missed

PR #233 (cb5afa3) is a **runtime-side** fix: it touched dispatch.rs /
registration.rs only (no bridge crate except sqlite's tests). Any driver-call
completion delivered as a `.then(...)` message routes through
`deliver_backend_completion_action` → `enqueue_call_continuation`
(tina-runtime/src/dispatch.rs:2202-2230): bounded mailbox first, parks in the
per-entry `continuation_overflow` on Full, never dropped; only a gone requester
(`Closed`) is terminal (registration.rs:533-555). All five AWS workers' wakeups
are exactly this class (`sleep().then(Poll)`), so a full own-mailbox parks the
Poll instead of dropping it — the pre-#233 leak (Poll dropped → slot held
forever → bridge wedges at `Full`) is closed for AWS without any AWS-side code.

Also checked the failure flavor: a *failed* sleep completion still delivers the
Poll message — `deliver_backend_completion_action` matches on the completion
shape regardless of `failure_reason` (dispatch.rs:2210-2240; the reason only
selects CallCompleted vs CallFailed events). So a degraded timer lane degrades
the poll cadence, never the slot accounting.

### (c) Slot-conservation test coverage per variant — UNEVEN (finding below)

| variant | Full rejection | timeout + slot-held + late-result | non-empty drain | mailbox-saturation slot conservation |
|---|---|---|---|---|
| S3 (tests/bridge.rs) | yes (:526) | yes (:665) | yes (:620) | **no** |
| SQS (tests/sqs_bridge.rs) | yes (:757) | yes (:854; asserts `in_flight_current == 1` after caller timeout at :877 — the sharpest slot-held proof in the crate) | yes (:757) | **no** |
| SNS (tests/sns_bridge.rs) | **no** | **no** | no — drain test (:439) only covers the trivially-empty case | **no** |
| DynamoDB (tests/dynamodb_bridge.rs) | **no** | **no** | no — empty-drain only (:490) | **no** |
| Secrets (tests/secrets_bridge.rs) | **no** | **no** | no — empty-drain only (:502) | **no** |

### Finding AWS-Q2-A [Low / High confidence] SNS, DynamoDB, and Secrets workers have no slot-lifecycle test coverage; no variant has a saturation slot-conservation test

- **File/line:** tina-aws-bridge/tests/sns_bridge.rs:439,
  tests/dynamodb_bridge.rs:490, tests/secrets_bridge.rs:502 (the only
  slot-adjacent tests — all assert a drain of an *already-empty* bridge);
  contrast tests/bridge.rs:526/620/665 and tests/sqs_bridge.rs:757/854.
- **Invariant:** every admitted operation's slot is returned exactly once on
  success/error/timeout/cancel — and that invariant should be *pinned per
  variant*, because the five workers are five **copies** of a ~450-line state
  machine, not one shared implementation.
- **Concrete gap (not a code bug):** the code is identical today, verified
  line-by-line above. But the crate's own history shows what copies do:
  track-D's D-1 is exactly a "five twins, edited four" defect class. A future
  edit to one variant's `poll` (say, DynamoDB grows a retry arm) that breaks
  slot re-insert would pass that variant's current test suite untouched —
  nothing exercises Full, timeout-keeps-slot, late-result, or a non-empty
  drain on SNS/DDB/Secrets. And the D2 protection the workers now lean on
  (Poll parks on full own-mailbox) is tested only via the runtime
  (capacity_truth) and sqlite (`slot_is_conserved_under_mailbox_saturation`,
  tina-sqlite-bridge/tests/bridge.rs:1011) — zero AWS-side proof.
- **Real-use scenario:** ops sizes a DynamoDB bridge at `max_in_flight = N`,
  a regression in a later per-variant edit leaks one slot per caller timeout,
  bridge degrades to permanent `Full` under a Dynamo brownout — the exact
  D3-class wedge this audit was asked to rule out, invisible until production.
- **Failing-test idea:** per variant, the two cheap ports: (1) the SQS
  timeout test (caller timeout → `in_flight_current` stays 1 → late terminal
  → drains to 0, admit succeeds again); (2) an S3-style
  `max_in_flight_rejects_full` + non-empty `close_and_drain`. Plus one
  crate-level sqlite-style saturation test: capacity-1 mailbox bridge, flood
  Sends so Poll continuations overflow, assert `in_flight_current` returns to
  0 and the slot re-admits every round.
- **Fix sketch:** lift the FakeS3/FakeSqs slow-response harness into a shared
  test helper and stamp the three missing variants; or — better long-term —
  collapse the five copied state machines into one generic
  `AwsWorker<Op: AwsOperation>` so there is one body to test and the twin-edit
  hazard disappears (the per-service code is only the request enum, validator,
  classifier, and SDK call).
- **LLM-pattern:** yes — copy-stamped variants with the *first two* copies
  tested and later copies shipped on structural faith.

### Disproven suspicions (Q2, with proof)

- **Timeout double-settle:** the timeout arm `take()`s the `request_context`
  before re-inserting the slot, so the later terminal sees `None` +
  `reply_plain == false` → `noop()` (complete_terminal's third arm). One
  caller settle, exactly once, in all five variants.
- **Drain counter divergence:** `in_flight_current` is recomputed from map len
  after every remove (`note_terminal`) and after every insert (admit), and the
  timeout path deliberately leaves it untouched — so `close_and_drain`
  honestly waits for abandoned-but-running SDK work. Pinned by
  sqs_bridge.rs:877.
- **Spawn-before-capacity-check:** no — capacity gate precedes
  `runtime.spawn` in all five admits; a rejected request never starts SDK work.
- **Tokio-runtime shutdown wedging slots:** `Handle::spawn` on a shut-down
  runtime drops the task immediately → `tx` drops → `rx` reads `Closed` →
  path 5 frees the slot with a typed `Internal`. No wedge.

---

## Coverage statement

Q1: process.rs read end-to-end (690 lines) including tests; platform claims
checked in CHANGELOG, README, ROADMAP, CI workflows, Cargo.tomls,
vendor-betelgeuse io backends, and the public call-surface rustdoc
(call/io.rs, call/process.rs). Q2: all five worker admit/poll/complete bodies
read individually; runtime continuation-delivery path read (dispatch.rs
2202-2240, registration.rs 528-560); all six test files surveyed; lib.rs /
bridge_adapter.rs / helpers.rs checked for additional workers (none). No cargo
run was needed; no source file modified.
