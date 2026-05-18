# Phase 119: Production Resource And Data Maturity

## Status

- Future implementation plan for Wave A.
- Runs after Phase 116. HTTP/2/gRPC client resource maturity needs the real
  client connection and stream-slot shape first.
- Can run in parallel with later non-pool work if ownership stays mostly in
  pool resources, local persistence, and data specimens.

## Spike Facts

- Generic `WorkerPool<H>` cannot close an arbitrary `H`. It can mark/reject/
  report. The resource owner must do cleanup.
- `tina-http::keepalive` explicitly says there is no idle-connection timeout
  yet; long-idle stale sockets reconnect only on next request.
- SQLx owns much of its own pool truth. Tina can report bridge pressure and
  outer admission, but must not fake SQLx internals.
- Snapshot/journal rails already exist and prove basic append-before-apply,
  corrupt checksum, truncated tail, and recovery trace. User-shaped durable
  service helpers/specimens are still thin.
- The system backlog names persistent counter, Redis-ish keyspace, audit log,
  and durability-misorder attempts. Use those as proof, not abstract unit tests.

## Purpose

Make long-lived resources and local durable state less first-form.

The user story:

```text
my Tina service can run for a long time, pool resources sanely, and recover
local state after restart
```

## Includes

- `ResourceLifetime` policy: idle age, max age, retire reason
- `ResourceHealth` policy: healthy, suspect, retire
- resource-owner close/retire hooks for pools that own a close path
- pool shutdown/drain reports aligned with bridge/resource vocabulary
- DB, HTTP/1 keepalive, and HTTP/2/gRPC client resources using shared pressure
  language without pretending every resource is one lease per request
- snapshot/journal restore service pattern
- torn-write and corrupt-tail recovery specimens
- append-before-apply helper with type-state where it helps
- persistent keyspace system specimen
- durable audit-log system specimen

## Does Not Include

- no distributed consensus
- no durable mailbox
- no exactly-once claim
- no remote clustering
- no hiding database-specific pool truth
- no redesign of HTTP/2/gRPC client protocol state; Phase 116 owns that
- no generic `WorkerPool` magic close for handles it does not own
- no auto-reclaim of leased resources behind the user's back

## Implementation Shape

Names should be resource-owner words:

```text
ResourceLifetime
ResourceHealth
RetireReason
ResourcePolicyReport
JournaledState
PendingJournalAppend<T>
CommittedMutation<T>
RecoveryReport
```

Resource rules:

- Idle retirement applies only to idle resources.
- Max lifetime prevents handoff of stale idle resources. A leased resource is
  reported old, not stolen.
- Health checks happen at explicit points: before handoff, after release, or on
  scheduled maintenance. The report names which point found the bad resource.
- If the pool owns a close path, retirement closes and reports it.
- If the pool does not own a close path, retirement is a typed report and the
  owner must close.
- `ReleaseDisposition::Reuse` may be overridden to retire when the pool knows a
  resource is stale/closed. The caller sees the override.
- Shutdown reports use existing lifecycle words: drain, force, closed, timed
  out, leaked/leased count.

Data rules:

- `JournaledState` does not mutate in-memory state until append success creates
  a `CommittedMutation<T>`.
- Failed append returns the original mutation, so the caller can reject/retry
  without losing it.
- Recovery returns snapshot index, replayed count, truncated-tail warning, and
  corrupt-tail error separately.
- Snapshot commit keeps the existing temp-write/rename/fsync truth.
- No claim of exactly-once or durable mailbox.

## User Proof Specimens

- HTTP/1 keepalive pool with idle retirement, max lifetime, failed health check,
  and clean shutdown report.
- HTTP/2/gRPC client connection pool after Phase 116: stream slots are not pool
  leases; connection retirement does not strand active streams silently.
- SQLx/SQLite pressure reports share vocabulary but preserve their different
  DB truth.
- `system_redisish_keyspace`: restart after snapshot + journal; corrupt
  checksum stops recovery; truncated tail is warning.
- `system_audit_log`: append events, batch snapshot/fsync, recover from torn
  tail, reject corrupt checksum.
- Durability-misorder compile-fail/specimen: attempt to apply before committed
  append is rejected by the helper shape.

## Proof Shape

- idle resource retires and reports why
- max-lifetime retire does not hand stale resource to new caller
- health check retires bad resource
- shutdown drains or force-closes with report
- HTTP/2/gRPC client connection retire does not kill unrelated healthy
  connection state silently
- crash/restart restores expected state
- corrupt/torn journal tail is typed and recoverable
- trybuild tests prove the type-state helper cannot expose the mutation payload
  for apply until append success creates `CommittedMutation<T>`
- fill-retire-refill proves retired/stale resources do not consume admission
  forever
- live and sim persistence tests show the same recovery facts where supported

## Hostile Review Notes

- Do not make a generic pool pretend it can close resources it cannot close.
- Do not steal a leased resource because a timer fired.
- Do not hide DB-specific truth behind one fake pool abstraction.
- Do not mutate state before the durable append success message arrives.
- Do not call truncated tail "success" without a visible warning.
