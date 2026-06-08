# Track F — Persistence, process, filesystem, signals, TLS

HEAD 49c3580 (branch `codex/review-fix-wave-record-2026-05-21`), working tree.
READ-ONLY review. Scope: `tina-runtime/src/persistence.rs`,
`tina-runtime/src/driver/{storage,process,tls,signals}.rs`,
`tina-runtime/src/driver/mod.rs`, `vendor-betelgeuse/io/darwin.rs`,
`tina-supervisor/src/lib.rs`, `tina-http/src/listener_tls.rs`.

Prior findings F1–F4 were re-verified against current source. A note in an
earlier decision log claimed "F3 RESOLVED (fixed, Linux) `child_has_exited`
peeks the leader". **That fix is not present on this HEAD** — there is no
`child_has_exited`, and `process.rs` still kills the group after reap. F3 is
live again (reverted or never landed here).

---

## Ranked findings

### F1 — Torn / short `.idx` sidecar permanently bricks all future journal appends
- Severity: **High** · Confidence: **High** · LLM-style: **Yes**
- `tina-runtime/src/persistence.rs:213-218` (read path),
  `:232-247` (`store_journal_last_index`, non-atomic writer).
- Invariant: a corrupt *derived cache* (the `.idx` is a rebuildable hint) must
  degrade to the source of truth (replay the journal), never wedge the store.
- Bug: `load_journal_last_index` does
  `file.read_exact(&mut bytes).map_err(|_| CallError::CorruptRecord)?` and
  `if &bytes[..8] != JOURNAL_INDEX_MAGIC { return Err(CorruptRecord) }`. Any
  short read (torn write) or bad magic returns `CorruptRecord`. That error
  propagates through `validate_next_journal_index` →
  `append_journal_record`, so **every future append fails** even though the
  journal file itself is intact and replayable. The `.idx` is rewritten on
  every append with `OpenOptions::truncate(true).write(true)` (no tmp+rename),
  so it is the single most-frequently-torn file in the store, and a crash
  mid-rewrite bricks the journal.
- Real-use trigger: power loss / OOM-kill during the 24-byte idx rewrite, or a
  partially-flushed idx on an unclean shutdown.
- Repro idea: `append_journal_record(p, 1, b"a")`; truncate `<p>.idx` to 4
  bytes; `append_journal_record(p, 2, b"b")` → currently `Err(CorruptRecord)`.
  Expected: succeeds via replay fallback. (Test name:
  `torn_idx_falls_back_to_replay`.)
- Fix: in `load_journal_last_index`, map short read / bad magic / decode
  failure to `Ok(None)` (fall back to journal replay) instead of
  `CorruptRecord`; and write the idx crash-atomically (tmp file + fsync +
  rename), matching `commit_snapshot`.

### F3 — Post-reap process-group kill can signal a recycled pid
- Severity: **High** (Medium per prior; raised — it can SIGKILL an unrelated
  process group) · Confidence: **High** · LLM-style: **Yes**
- `tina-runtime/src/driver/process.rs:279,289-296,356-373` (esp. `:371`).
- Invariant: never send a signal to a pid/pgid after the owning child has been
  reaped — the kernel may have recycled it.
- Bug: child is spawned in its own group (`process_group(0)`, so pgid == pid).
  In the normal-exit path, `child.try_wait()` at `:290` returning
  `Ok(Some(status))` **reaps** the leader zombie, freeing its pid for reuse.
  `process_exited` is then called with the *stale* `process_group = child.id()`
  captured at `:279`. When stdout/stderr were truncated, `:371` runs
  `kill -KILL -<pgid>`. Between reap and that kill, the OS can recycle the
  pgid; the SIGKILL then hits an unrelated process group. The
  `kill_and_reap` path is safe (it kills *before* `child.wait()` at `:306`),
  but the `process_exited` truncation branch is not.
- Real-use trigger: a spawned tool that forks a backgrounded grandchild
  holding stdout open, exits itself, output exceeds the limit (truncated), on
  a busy host churning pids.
- Repro idea: hard to force pid reuse deterministically; an interposed
  `kill_process_group` spy can assert the kill is issued *after* the leader is
  reaped (test name: `truncated_output_kills_group_before_reap`). The fix is
  to reorder, not to add a flaky pid-reuse test.
- Fix: kill the group *before* the reaping `wait`. Capture child output drain
  results, and if truncation is detected (or always, for the group), issue
  `kill_process_group` while the leader is still un-reaped, then `wait`. I.e.
  do the descendant cleanup on the same side of the reap as `kill_and_reap`
  does. Alternatively keep the leader as a `Child` and only reap after the
  group kill.

### F2 — macOS `file_fsync` rail uses `fsync(2)`, not `F_FULLFSYNC` (no stable-media durability)
- Severity: **Medium** · Confidence: **High** · LLM-style: **Yes**
- `vendor-betelgeuse/io/darwin.rs:867-884` (`libc::fsync(op.fd)`); user rail
  `tina-runtime/src/call/files.rs:58-60` (`file_fsync`).
- Invariant: a durability primitive named `file_fsync` must actually push data
  to stable media on the platform.
- Bug: on macOS, `fsync(2)` only flushes to the drive's volatile write cache;
  `fcntl(fd, F_FULLFSYNC)` is required to force a stable-media barrier. The
  Betelgeuse darwin IO loop's `Fsync` op calls plain `libc::fsync`, so the
  user-facing `file_fsync` rail returns success without durability across a
  power loss. (Tina's own snapshot/journal path uses `std::fs::File::sync_all`,
  which on macOS *does* issue `F_FULLFSYNC`, so persistence.rs is unaffected —
  only the user IO rail lies.)
- Real-use trigger: a Tina app that builds its own durable file format on the
  `file_*` rail and calls `file_fsync` before reporting a commit; a power loss
  then loses acknowledged data.
- Fix: in the darwin `Fsync` arm, try `libc::fcntl(op.fd, libc::F_FULLFSYNC)`
  first; on `ENOTSUP`/`EINVAL` (non-local FS) fall back to `libc::fsync`.

### F4 — Single serial TLS worker head-of-line-blocks the whole shard's TLS work (slowloris)
- Severity: **Medium-High** · Confidence: **High** · LLM-style: **Yes**
- `tina-runtime/src/driver/tls.rs:4` (doc: "one worker drains the lane
  serially today"), `:323` (single `thread::spawn`), `:1041-1055`
  (handshake loop), `:1070-1133` (`read_tls`/`write_tls` run to completion on
  the worker), `tina-http/src/listener_tls.rs:40-73`.
- Invariant: one slow/malicious peer must not be able to stall TLS progress for
  every other TLS stream on the shard.
- Bug: `TlsLane` is a single worker thread that executes each `accept`/
  `connect`/`read`/`write`/`close` synchronously to completion. A slowloris
  peer that completes the TCP accept and then dribbles (or stalls) the TLS
  handshake/read holds the worker for the full configured timeout
  (`tls_io_timeout`, default **30s** in dev). During that window *no other TLS
  stream on the shard makes progress* — every queued TLS op waits behind the
  stalled one. `tls_lane_capacity` is queue depth, not concurrency, so it does
  not help. The HTTP listener mitigates *accept* with a short re-poll
  (250ms/100ms) but read/write still block on the full I/O timeout.
- Real-use trigger: any HTTPS listener facing the internet; a handful of slow
  clients can deny TLS service to all others on the shard.
- Repro idea: bind a TLS listener, open two TLS streams, have one peer send a
  partial record and stall; assert the second stream's `tls_read` does not
  complete within a bound while the first is stalled (test name:
  `slow_tls_peer_does_not_block_other_streams`).
- Fix: make the worker non-blocking with a short internal poll grain (mirror
  the Unix lane), or run a small bounded pool of TLS workers so one stalled
  stream cannot occupy the only worker. At minimum yield between streams with a
  per-op poll budget rather than blocking for the whole timeout.

### F5 (new) — Journal append does not fsync the parent directory on first create
- Severity: **Medium** · Confidence: **High** · LLM-style: **Yes**
- `tina-runtime/src/persistence.rs:136-158` (`append_journal_record`); compare
  `commit_snapshot_with_parent_sync:117` which *does* fsync the parent.
- Invariant: a newly created file is only durable after its parent directory's
  dirent is fsynced; `file.sync_all()` syncs the inode + data, not the dirent
  that names it.
- Bug: `append_journal_record` does `create_dir_all`, opens the journal with
  `create(true).append(true)`, `write_all`, then `file.sync_all()` — but never
  calls `sync_parent_directory`. On the **first** append (file creation), a
  crash after `sync_all` can still lose the journal file entirely because the
  directory entry creating it was never made durable. The snapshot commit path
  syncs the parent; the journal append path silently does not, so the project's
  own `directory_fsync_after_rename: Supported` durability claim does not hold
  for journal creation.
- Real-use trigger: first journal write of a freshly-provisioned store,
  followed by power loss before the filesystem's periodic dirent flush.
- Repro idea: hard to test without a crash-injecting FS; a structural test can
  assert `append_journal_record` calls the parent-sync hook on first create
  (refactor to take a `sync_parent` fn like `commit_snapshot_with_parent_sync`).
- Fix: after the first successful create+sync, fsync the parent directory once
  (only needs to happen when the file did not previously exist). Reuse
  `sync_parent_directory`.

### F6 (new) — `kill_and_reap` leaks drain threads + pipe fds when `child.kill()` fails
- Severity: **Low-Medium** · Confidence: **Medium** · LLM-style: **Yes**
- `tina-runtime/src/driver/process.rs:310-315`.
- Invariant: kill/reap is bounded and releases the stdout/stderr drain threads
  and pipe fds even on the failure path.
- Bug: when the group kill failed and `child.kill()` also fails and
  `try_wait()` returns `Ok(None)`/`Err`, the function returns
  `KillUncertain` at `:313` while the `stdout`/`stderr` `Option<JoinHandle>`
  arguments are simply dropped. Dropping a `JoinHandle` *detaches* the drain
  thread; the child is still alive (kill failed) so its write ends stay open,
  and each drain thread blocks in `reader.read()` indefinitely, leaking a
  thread and the pipe read fd. Other paths route through
  `join_drain_bounded`/`process_exited`, which bound the join; only this
  branch leaks.
- Real-use trigger: rare — requires both `kill_process_group` and
  `child.kill()` to fail (e.g. EPERM after a privilege drop) with the child
  still running. Bounded by how often that occurs, but it is an unbounded
  leak per occurrence.
- Fix: before returning on this branch, drop the pipe fds / `join_drain_bounded`
  with the bounded budget so the drain threads observe EOF or time out instead
  of being detached forever. (Note the threads can still block if the child
  holds the pipe open; closing the child's stdout/stderr handles or relying on
  the bounded join is needed — at minimum call `join_drain_bounded` so the
  budget is spent rather than leaking silently.)

---

## Disproven / verified-safe

- **TLS certificate verification is NOT weakened.** `connect_tls`
  (`tls.rs:918-970`) builds `ClientConfig::builder().with_root_certificates`
  with real verification — no `dangerous()`, no custom no-verify verifier. An
  empty `root_certificates` yields an empty `RootCertStore`, so verification
  fails closed. Hostname is validated via `ServerName::try_from`. ALPN
  mismatch fails closed (`TlsAlpnMismatch`, `:966-968`). Proof: no
  `dangerous`/`ServerCertVerifier`/`set_certificate_verifier` anywhere in
  `tls.rs`.

- **`submit_connect` throwaway `cancelled` Arc is not a cancel-leak.**
  `tls.rs:453` constructs a fresh `Arc::new(AtomicBool::new(false))` in the
  `TlsCommand::Connect`, but `submit_command` overwrites it via
  `command.set_cancelled(Arc::clone(&cancelled))` (`:601`, all variants
  handled `:1186-1207`) with the lane-registered flag. The throwaway is dead.
  Redundant, not a bug — connect cancel/timeout still wires correctly.

- **Snapshot commit ordering is correct.** `commit_snapshot_with_parent_sync`
  does temp-write → `sync_all` → `fs::rename` → parent dir fsync; rename
  failure removes the temp file; a parent-sync failure after a successful
  rename returns `CommitUncertain` (the snapshot is installed but not provably
  durable). Covered by `parent_sync_failure_after_rename_is_commit_uncertain`
  and `snapshot_rename_failure_removes_temp_file`.

- **Journal replay tail/checksum handling is correct.** `replay_journal_bytes`
  treats a short header or a payload that runs past EOF as a `TruncatedTail`
  warning (not corruption), enforces strictly increasing indices, and rejects
  bad checksums and bad magic mid-stream as `CorruptRecord`. Decode bounds use
  `checked_add`.

- **Signal handler state is per-driver and released on drop.**
  `OsSignalDispatcher` (`signals.rs`) registers `signal-hook` flag handlers per
  `BetelgeuseDriver`, fans a single OS signal out to every driver via separate
  `Arc<AtomicBool>` flags, chains pre-existing handlers, and unregisters its
  tokens on `Drop`. No global mutable handler state race; no custom unsafe
  handler. Install failure panics loudly rather than claiming false support.

- **`tina-supervisor` does not spawn/kill processes.** It is config-only
  (`SupervisorConfig` over `RestartPolicy`/`RestartBudget`); no process groups,
  job objects, child kill/reap, or inherited pipes. No findings here.

- **Storage fsync does not block the shard in production.** The production
  threaded runtime uses `BetelgeuseDriver::with_io_loop_and_capacities` →
  `StorageLane::new` (off-shard worker thread, `driver/mod.rs:275`,
  `threaded.rs:1408`). `StorageLane::inline()` (which would run fsync on the
  shard turn) is only the default for the bare `BetelgeuseDriver::new`
  (`mod.rs:250`), used in single/test contexts. Not a production hot-path bug.

---

## Coverage note

Covered: persistence framing/replay/idx-sidecar, snapshot commit atomicity,
journal append durability (incl. the new dir-fsync gap), storage lane
inline-vs-worker placement, process spawn / process-group kill / reap ordering
/ inherited-pipe drain bounding, TLS lane serialization + verification + ALPN,
OS signal handler lifecycle, supervisor surface, macOS fsync rail.

Not deeply exercised (needs follow-up): crash-injection / fault-injection FS
tests for F1 / F5 (would prove the bricking and the lost-dirent under real
power-loss semantics); a deterministic pid-reuse harness for F3 (recommend
proving kill-before-reap ordering instead); a slowloris integration test for
F4 against the real HTTP TLS listener; Windows job-object behavior (no Windows
process-group path exists — `killed_group` is hardcoded `false` on non-unix,
so timeout/cancel falls back to `child.kill()` only, which does not reap
descendants — worth a Windows-specific track if Windows is a target).
