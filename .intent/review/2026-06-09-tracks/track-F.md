# Track F — persistence, process, filesystem, signals, TLS

HEAD reviewed: `0cd6a31` (= origin/main). Source treated read-only.

Carve-out honored: the non-unix `killed_group` / descendant-reaping assessment is
left to the sibling agent. Everything else in Track F is covered here.

## Verdict

**Clean.** No fresh Critical/High/Medium bug found. The fresh surface — the TLS
sans-I/O on-shard lane (Phase 136, PR #206) and the storage durability path moved
onto the per-shard Betelgeuse rail (PR #205) — is carefully built and matches its
inline oracle on the durability ordering that matters. The prior F1–F6 fixes
re-verify on current code, and the F6 drain-join fix holds on every exit path with
no double-spend. Two genuinely-Low observations and several disproven suspicions
are recorded below so they are not silently dropped.

---

## Re-verified fixed on current main (do not re-file)

- **F1 torn `.idx` / append durability.** `persistence.rs` snapshot
  (`commit_snapshot_with_parent_sync`, 85), atomic file replace
  (`commit_file_atomic_with_parent_sync`, 134), and journal index sidecar
  (`store_journal_last_index`, 347) all use temp-write → `sync_all` → rename →
  parent-dir fsync, with temp cleanup on every failure branch. The reactor mirror
  (`SnapshotCommitJob`/`StoreIndexJob`, storage.rs 1299/1886) reproduces the same
  ordering over Betelgeuse ops. Journal append fsyncs the **data** before the
  sidecar pointer is written, so a crash in the gap is recovered by length-mismatch
  → replay → checksum-validated truncate (`ValidateIndex`, storage.rs 1532).
- **F2 macOS `F_FULLFSYNC`.** `vendor-betelgeuse/io/darwin.rs:1488` `full_fsync`
  issues `fcntl(F_FULLFSYNC)` and only falls back to plain `fsync` on `ENOTSUP`.
  The reactor `Fsync` opcode routes through it (darwin.rs:1248). The inline path's
  `File::sync_all` is also F_FULLFSYNC-backed by std on macOS.
- **F3 post-reap pgid kill.** `child_has_exited` uses `waitid(WNOWAIT)` on Linux to
  peek exit without consuming the zombie (process.rs:277), keeping the leader pid
  (== pgid) reserved across the group kill. Non-Linux pid-reuse race is documented
  in-code (process.rs:480) and is the sibling's carve-out.
- **F4 serial TLS worker → sans-I/O on-shard rustls.** `tls.rs` drives one rustls
  `Connection` per stream on the shard thread; at most one Betelgeuse recv *or*
  send per stream (`SocketOp`, `flush_or_fill`). No worker thread, no HOL slowloris.
- **F5 journal parent-dir fsync.** Present on every commit path (`sync_parent`).
- **F6 bounded drain join on KillUncertain (PR #229).** Re-scrutinized per the
  brief — see below.

## F6 re-scrutiny (the assigned deep-look)

`kill_and_reap` (process.rs:345) consumes `stdout`/`stderr: Option<DrainHandle>`
by value and routes **every** exit through a bounded join:

| exit path | join site |
|---|---|
| direct kill failed, child already reaped (`Ok(Some)`) | `process_exited` → `join_drain_bounded` x2 |
| direct kill failed, child still alive (`Ok(None)`) | `kill_uncertain` → `drain_and_discard` |
| direct kill failed, `try_wait` errored (`Err`) | `kill_uncertain_with(fallback)` → `drain_and_discard` |
| killed, reap deadline exceeded (`Ok(None)` loop) | `kill_uncertain` → `drain_and_discard` |
| killed, reaped or vanished | `drain_and_discard` (line 400) |

And the caller (`execute_process_command`, 307) reaches `kill_and_reap` on cancel,
timeout, and `child_has_exited` error; the only other terminal is `process_exited`,
which also joins both handles. So **every** terminal joins the drains.

- *Does the bounded join hold on every exit path?* Yes — table above.
- *Can the drain budget be spent twice?* No. Each `Option<DrainHandle>` is moved
  into exactly one terminal function per call; after the move the local is consumed,
  so no path can re-join the same handle. The budget is `2 × 100ms` per terminal,
  spent once.

The two `kill_uncertain_*` proofs (process.rs:633, 656) pin the cancel-and-join
behavior with idle (never-EOF) pipes. Fix is solid.

## Findings

### F-2026-06-09-A [Low / Medium confidence] best-effort group kill silently no-ops under fd/PID exhaustion

`kill_process_group` (process.rs:446) shells out — `Command::new("kill").arg("-KILL").arg("-{pid}").status()` — i.e. it `fork`/`exec`s a `kill` binary to signal the group, rather than calling `libc::killpg`/`libc::kill`. The result is `let _ =`-ignored at both call sites (kill path line 363, truncation path line 482).

- **Invariant touched:** "process_run owns the whole process group; do not let a background grandchild escape the runtime rail."
- **Concrete bug:** when the runtime is under the exact pressure that motivates a kill (fd exhaustion, PID-table pressure, or `kill` not on `PATH`), the `fork`/`exec` of the helper fails, the group signal is silently dropped, and inherited-pipe grandchildren can survive. The leader itself is still killed directly via `child.kill()` (libc), so the leak is limited to descendants, and only in that rare failure window.
- **Why real:** a host runner under load is precisely when descendant cleanup matters; spawning a process to send a signal adds a failure mode that a direct syscall does not have.
- **Fix:** replace the helper-process call with a direct `libc::kill(-(pid as i32), libc::SIGKILL)` (or `killpg`). Same best-effort semantics, no fork/exec/PATH dependency.
- **LLM-pattern?** Mild — reaching for the `kill` CLI instead of the syscall is a plausible-looking shortcut.

### F-2026-06-09-B [Low / Low confidence] timed-out TLS read leaves the stream's recv in flight until the peer acts

When a `tls_read`/`tls_write` whole-op deadline fires (tls.rs:781), the pending op is cancelled and tombstoned, but the in-flight Betelgeuse recv/send it armed lives in the **stream's** `TlsIo` (`streams`), not the op, so `holds_live_backend_ref()` is `false` and the tombstone is reaped at once. The orphaned socket op is correctly tracked by `has_live_socket_work` (so shutdown drains it) and is harvested by the *next* op on the stream — no double-free, no slot leak. But if the caller times out a read and then never issues another op or `tls_close` on that stream, the lane keeps stepping the shared io_loop every `advance` until the peer finally sends or closes.

- **Severity rationale:** this is a self-healing, single-socket condition that is visible in `resource_report().tls_streams`, and it is fundamentally a caller resource-management choice (timed out a read but kept the stream). Recording it as an awareness item, not an actionable defect.
- **Fix (optional):** none required; if desired, document that a timed-out TLS read leaves the stream usable but with a recv possibly in flight, and that callers should `tls_close` streams they abandon.

## Disproven suspicions (proof recorded)

- **Directory `F_FULLFSYNC` divergence between reactor and inline.** Suspected: the
  reactor parent-dir fsync (`SyncParentJob`, storage.rs:987) routes a *directory* fd
  through `full_fsync` → `fcntl(F_FULLFSYNC)`, which on some macOS versions rejects
  directory fds with `EINVAL` (not `ENOTSUP`, so the plain-`fsync` fallback would not
  trigger) → every snapshot commit / journal append falsely returns
  `CommitUncertain`. **Disproven:** the on-real-rail reactor tests
  `journal_append_then_replay_round_trip_on_real_rail` (storage.rs:2201) and
  `reactor_snapshot_commit_then_load_round_trips` (2762) assert `JournalAppended` /
  `SnapshotCommitted`, both of which are only emitted *after* the parent-dir fsync
  succeeds. These are committed-green on the macOS dev platform, so directory
  `F_FULLFSYNC` resolves (works, or returns `ENOTSUP` and falls back) — no divergence.
- **Reactor vs. inline durability-ordering divergence.** Verified the reactor
  `WriteNewFile` fsyncs the temp file before rename, journal append fsyncs data
  before the sidecar, and `truncate_file` (storage.rs:570) is un-fsynced **in both
  paths** (`repair_journal_tail`, persistence.rs:375, is also un-fsynced) and is
  always followed by an append+fsync — so the two paths agree.
- **TLS verification weakened.** `build_client_connection` (tls.rs:1373) uses
  `with_root_certificates(explicit DER roots).with_no_client_auth()` and a validated
  `ServerName`; no custom/insecure verifier, no system-root widening. Expiry is still
  enforced — proven by `lane_connect_rejects_expired_server_cert` (tls.rs:1976).
  ALPN mismatch when offered is a typed failure (`lane_connect_alpn_offered_but_server_declines_is_mismatch`).
- **TLS close-wins double terminal.** `submit_close`/`submit_listener_close` set
  `cancelled` *and* push to `cancelled_by_close`; `advance` emits one synthetic
  `TargetClosed` (guarded by an existence check) and the timeout loop skips
  already-cancelled entries — exactly one terminal per preempted op.
- **TLS close stealing a stream with an in-flight socket op.** `pump_close`
  (tls.rs:922) harvests the inherited socket op before queueing `close_notify` and
  waits (under the whole-op deadline) for the backend to release the inherited
  completion box, so no referenced box is dropped.
- **Read truncation vs. clean close confusion.** `ingest` signals rustls EOF exactly
  once (`eof_signaled`, tls.rs:254); a clean `close_notify` reads as `Ok(0)` (empty
  `TlsRead`) while a truncated connection surfaces `UnexpectedEof` → `Io`.
- **TLS lane `cancel_pending` use-after-free on shared io_loop.** Shutdown-only;
  clears `pending`/`streams` only when `!has_live_socket_work()`, else returns
  `BackendStillOwnsCompletions` and leaves boxes alive. No `Drop` impl by design
  (tls.rs:1291) so a bare drop never walks the shared backend watch-list.
- **Per-driver signal handler state.** `OsSignalDispatcher` (signals.rs) registers
  per-driver `signal-hook` flag handlers and unregisters its tokens on `Drop`;
  signal-hook chains, so one driver dropping does not silence another's flag.

## Areas wanting a deeper look (not bugs found here)

- A power-failure / `kill -9`-the-process crash matrix on the **reactor** durability
  path specifically (the inline oracle has fence/uncertain tests; the Betelgeuse-rail
  path would benefit from the same crash-injection coverage, e.g. crash between
  journal-data fsync and sidecar rename, and between rename and parent-dir fsync).
- A directory-`F_FULLFSYNC` unit test in `darwin.rs` (only a regular-file test
  exists at line 1532) to pin the behavior the disproven suspicion above relies on.
- TLS handshake fuzz: split/oversized/zero-length records across recv chunks, and
  shutdown initiated mid-handshake, against the on-shard pump.
