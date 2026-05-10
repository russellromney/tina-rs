# 067 Bounded Pool Vocabulary

## Status

- Done: design drafted from specimen worker-pool, sharded, reqwest, SQLite,
  and DB bridge pressure.
- In progress: blocked on 066 final merge/rebase, not on more API design.
- Open: implement one bounded pool primitive after 066 `CallHandle` /
  `cancel_call` lands.
- Deferred: DB connection pool, HTTP keep-alive pool extraction, AWS pool,
  generic job framework, hidden submit queue.

## Goal

Real services use pools everywhere.

Tina needs a small pool vocabulary that can serve worker pools, DB pools, HTTP
pools, and future AWS clients without becoming a hidden queue.

Core rule:

```text
A pool owns scarce resources.
A pool is not an unbounded waiting room.
```

This phase comes after 066.

Pool waiters need cancellation truth. They should use the 066 first-form
primitive: `call_with_handle(...).reply(...)` plus `cancel_call(handle)`.

Do not wait for `Deadline` or `PendingCallSet`; those are deferred 066 rocks.
Use existing per-call `Duration` timeouts and explicit pool waiter cleanup.

Compiler rule:

```text
If compiler can know wrong, make wrong not compile.
If only runtime can know wrong, make typed outcome plus trace fact.
```

Grug pool rules:

```text
borrow thing.
use thing.
return thing, or retire thing.
if no thing, say Full.
if pool closed, say Closed.
if wait too long, say Timeout.
never pretend thing came back by magic.
```

## Non-Goals

- No hidden queue.
- No hidden retry.
- No generic `Submit(job)` framework as the primitive.
- No DB-specific pool in first form.
- No HTTP keep-alive pool in first form.
- No automatic resource replacement unless explicitly named.
- No "best effort" release that silently accepts stale/double leases.
- No lease timeout / max lease age / leak detector in first form. A lease
  returns capacity only through explicit `Release` / `Retire`.

## Rock 0: API Home

Decide module/crate home before coding.

Likely split:

- generic vocabulary (`PoolConfig`, `PoolLease`, `AcquireOutcome`,
  `ReleaseOutcome`, `ReleaseDisposition`) goes where users can name it without
  dragging in runtime machinery: probably `tina`, or a tiny future `tina-pool`;
- first concrete `WorkerPool` goes where the implementation honestly belongs:
  probably `tina-runtime`, unless it proves runtime-agnostic;
- DB/HTTP/AWS-specific pools stay out of this phase.

Rules:

- do not scatter pool helpers across examples;
- keep lease internals private;
- keep concrete worker-pool machinery private unless it is intended API;
- if the home is uncertain, land a design note and keep implementation narrow.

Proof:

- docs say where users import pool vocabulary from;
- examples do not define their own shadow `PoolLease`/`AcquireOutcome` types.

## Rock 1: First-Form Contract

Pool config must name both resource capacity and waiter capacity.

```rust
pub struct PoolConfig {
    pub capacity: usize,
    pub max_waiters: usize,
    pub acquire_timeout: Duration,
}
```

Do not include `idle_timeout` in first-form config.

Idle retirement is later. It needs timers and trace truth. Do not smuggle it
into first form.

Outcomes stay typed:

```rust
enum AcquireOutcome<H> {
    Acquired(H),
    Full,
    Closed,
    Timeout,
}
```

Release also has typed truth:

```rust
enum ReleaseOutcome {
    Released,
    Retired,
    StaleLease,
    DoubleRelease,
    PoolClosed,
}
```

Exact names may change, but every outcome must remain visible.

`PoolLease`, `AcquireOutcome`, `ReleaseOutcome`, and other pool truth values
should be `#[must_use]`.

Tiny pressure report is first-form, not observability platform:

```rust
struct PoolPressureReport {
    capacity: usize,
    available: usize,
    leased: usize,
    waiters: usize,
    full_count: u64,
    timeout_count: u64,
    cancel_count: u64,
    retired_count: u64,
    closed: bool,
}
```

Exact fields may change. The report must answer simple questions:

- is pool full?
- how many things are borrowed?
- how many callers are waiting?
- did callers timeout/cancel?
- did resources retire?
- is pool closed?

## Rock 2: Pool Lease Identity

A lease is the borrowed thing token.

It must be move-owned and identity-checked.

Candidate:

```rust
pub struct PoolLease<H> {
    pool_id: PoolId,
    resource_id: ResourceId,
    generation: u64,
    handle: H,
}
```

Rules:

- lease is not `Copy`;
- avoid `Clone` unless clone cannot cause double release;
- lease fields are private; users cannot forge leases;
- release consumes the lease;
- lease is not a call-cancel handle and cannot cancel runtime calls;
- lease holds a resource handle/view, not necessarily the owned resource. For
  worker pools this may be an `Address`; for DB/HTTP pools later it may be a
  narrower handle;
- stale generation is rejected visibly;
- double release is rejected visibly;
- release after pool close is rejected visibly;
- lease exposes only the resource handle needed by user code.

Proof:

- valid release works;
- double release rejected;
- stale lease rejected;
- release after close rejected;
- wrong-pool release rejected;
- compile-fail proof where practical: cannot clone/copy lease, cannot use lease
  after release, cannot pass lease to `cancel_call`, cannot pass `CallHandle` to
  release.

## Rock 3: Acquire / Release Primitive

Ship acquire/release first.

Do not ship submit first. Submit is a job framework. Acquire/release is the
primitive.

Candidate:

```rust
call_with_handle(pool, PoolMsg::Acquire, acquire_timeout)
    .reply(AppMsg::Acquired)

call(pool, PoolMsg::Release {
    lease,
    disposition: ReleaseDisposition::Reuse,
}, timeout).reply(AppMsg::Released)
```

Prefer an explicit disposition over a boolean if examples flinch:

```rust
enum ReleaseDisposition {
    Reuse,
    Retire,
}

enum CloseMode {
    Drain,
    Force,
}
```

Rules:

- `capacity == 0` rejects config;
- `max_waiters == 0` means shed immediately when all resources are busy;
- waiter table uses fixed-capacity storage: slab/ring/table or equivalent, not
  a growing `HashMap`;
- stored waiters are deferred reply slots owned by the pool, not magic runtime
  observers;
- waiter order is FIFO in first form;
- cancelled/timed-out waiters are removed without disturbing the order of
  remaining waiters;
- caller timeout closes the deferred slot; the pool must prune the closed
  waiter on the next release/close/sweep path and free waiter capacity;
- explicit `cancel_call(handle)` closes the deferred slot; the pool must prune
  it the same way;
- do not claim "timeout removes waiter" unless the table actually drops the
  entry and `PoolPressureReport.waiters` decreases;
- drain close / `CloseMode::Drain` stops new acquire, closes waiters as
  `Closed`, and lets outstanding leases return/retire;
- force close / `CloseMode::Force` stops new acquire, closes waiters as
  `Closed`, and marks outstanding leases stale/retired by policy;
- release has an acknowledgement path. Use `call(...Release...)` when outcome
  matters, or a clearly named fire-and-observe helper with visible rejection;
- ordinary best-effort `send` is not the blessed release path;
- release consumes the lease and returns/records a `ReleaseOutcome`;
- caller-owned disposition is the first truth: `Reuse` means caller believes
  resource is good, `Retire` means caller does not;
- pool may override `Reuse` to retire/reject when it knows the resource is
  stale, closed, wrong generation, or otherwise unhealthy, but this override is
  a typed `ReleaseOutcome`;
- no hidden retry.

Proof:

- immediate acquire when resource idle;
- queued waiter when resource busy and waiter cap available;
- `Full` when resources busy and waiter cap full/zero;
- timeout closes the caller wait and pool pruning removes the waiter;
- explicit cancel closes the caller wait and pool pruning removes the waiter;
- fill waiter table, let callers timeout/cancel, then prove new waiters can be
  admitted after the pool prunes closed slots;
- close settles waiters as `Closed`;
- release hands resource to next waiter in deterministic order;
- FIFO waiter order is proved, including after cancelling/timing out middle
  waiters;
- drain close and force close have separate tests for waiters and late releases;
- close all waiters, then admit new waiters without stale `Full`;
- release acknowledgement reports stale/double/wrong-pool/closed outcomes;
- pressure report counts full/timeout/cancel/retire/closed truth.

## Rock 4: Lease Release Ergonomics

Acquire/release is honest. It is also easy to forget release on error branches.

Add small helpers that make release explicit without owning the workflow.

Candidate:

```rust
lease.release_effect_for::<I>(pool, ReleaseDisposition::Reuse)
```

or:

```rust
PoolLease::release_effect::<I>(lease, pool, ReleaseDisposition::Retire)
```

Rules:

- method name says release;
- pool address and `ReleaseDisposition` are visible;
- no automatic release on drop in first form;
- no hidden retry if release mailbox is full;
- helper returns an ordinary `Effect<I>`.

Proof:

- examples use helper in success and error branches;
- release rejection is still observable.

## Rock 5: Pool Outcome Ergonomics

Do not flatten by default.

Small map/classify helpers are okay. Hiding `Full`/`Closed`/`Timeout` is not.

Possible helpers:

```rust
impl<H> AcquireOutcome<H> {
    pub fn map_acquired<U>(self, f: impl FnOnce(H) -> U) -> AcquireOutcome<U>;
    pub fn pressure_reason(&self) -> Option<PoolPressureReason>;
}
```

Rules:

- `Full`, `Closed`, and `Timeout` remain distinct;
- no "turn all errors into io error";
- classification is opt-in.

Proof:

- examples get less match boilerplate only where clarity improves;
- raw match stays documented.

## Rock 6: First Concrete Pool: Worker Pool

Prove the words with a pure Tina worker pool before DB/HTTP pools.

Shape:

- N worker isolates/resources;
- pool owns worker addresses as resources;
- acquire returns a lease containing one worker address;
- caller does work with the worker;
- caller releases or retires the lease.

Rules:

- no worker restart policy in first form unless already supplied by the parent;
- worker panic/restart story is explicit non-goal or delegated to supervisor;
- first-form failure rule: if leased worker returns `Closed`, or caller knows
  worker stopped, caller releases with `ReleaseDisposition::Retire`, not
  `Reuse`;
- pool may reject `Reuse` for a known-dead/stale worker, but it must report that
  as a typed release outcome;
- resource health rule: caller-owned disposition comes first, pool-owned known
  health may override only visibly. No silent reuse of known-bad worker;
- no job submission abstraction yet;
- no hidden queue beyond `max_waiters`.

Proof:

- acquire all workers;
- full when exhausted with no waiters;
- waiter receives worker after release;
- timeout/cancel waiter frees capacity;
- close closes waiters and rejects release;
- worker failure path retires lease or produces a typed release rejection;
- pressure report shows leased/waiter/retired/full counts.

## Rock 7: Specimen Migration

Update existing pool-shaped specimens first. Do not create a new specimen unless
the existing ones cannot honestly show the new pool model.

Primary targets:

- `eiffel_dynamic_worker_pool` / renamed `specimen_dynamic_worker_pool`;
- `eiffel_graceful_pool_shutdown` / renamed
  `specimen_graceful_pool_shutdown`.

Only add a new bounded-worker-pool specimen if both existing examples are too
pedagogical or too specialized to carry the lesson.

README must compare:

- Tokio channel/oneshot pool shape;
- Tina acquire/release shape;
- where `Full`/`Closed`/`Timeout` appear;
- why explicit release is more verbose but safer;
- how caller cancellation removes waiters.

## Rock 8: Future Pool Notes

Write down how this maps later, without implementing:

- SQLite/Postgres connection pool;
- HTTP keep-alive pool;
- AWS client/request pool;
- sharded worker pool;
- rate-limited pool.

Especially name where truth differs:

- DB transaction ownership;
- HTTP connection reuse and broken sockets;
- AWS idempotency/retry policy;
- sharded placement;
- resource health checking;
- max lease age / leaked lease detection;
- idle retirement.

## Dependency On 066

067 must not invent its own cancellation semantics.

It consumes from 066:

- caller-owned call handles;
- explicit cancel vs timeout distinction;
- capacity reclamation rules;
- late-reply trace vocabulary.

It does **not** consume deferred 066 rocks:

- no `Deadline` helper;
- no `PendingCallSet`.

If 066 does not land cleanly, do not start 067 implementation. Pools are too
easy to get subtly wrong without the cancellation primitive.

## Done Means

- one bounded acquire/release pool exists;
- API home for generic vocabulary and concrete worker pool is decided;
- waiter capacity is bounded and tested;
- timeout/cancel/close reclaim waiter capacity after explicit pool-side
  pruning;
- release has visible acknowledgement;
- stale/double release is rejected visibly;
- close modes and resource-health policy are documented and tested;
- pressure report exists;
- one specimen/example uses the pool;
- DB/HTTP/AWS pool follow-ups can reuse the vocabulary instead of inventing
  their own.
