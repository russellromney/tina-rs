# 077 DB Pool Consumers

## Status

- Done: plan created after 067 pools, 072 deadlines, 073 HTTP pool,
  075 SQLx bridge, and 076 server keepalive landed.
- In progress: none.
- Open: build DB pool consumers.
- Deferred: generic multi-shard pools, ORM/schema tooling, connection
  leak timers, and automatic retry.

## Goal

Real services hit databases.

Tina now has:

- bounded bridge workers (`tina-sqlite-bridge`, `tina-sqlx-bridge`);
- bounded pool vocabulary (`WorkerPool`, lease/release/close modes);
- deadlines and `PendingCallSet`;
- capacity reports.

This phase joins them.

Grug truth:

```text
borrow db lane.
do db work.
return lane, or retire lane.
if no lane, say Full.
if wait too long, say Timeout.
if db says Busy, say Busy.
do not confuse pool pressure with database pressure.
```

## Non-Goals

- No ORM.
- No migrations.
- No hidden retry.
- No unbounded query queue.
- No multi-shard pool.
- No fake "cancel means query died" guarantee.

## Rock 0 — Audit Current DB Bridges

Read:

- `tina-sqlite-bridge`;
- `tina-sqlx-bridge`;
- `specimen_sqlite_counter`;
- `specimen_postgres_counter`;
- `tina_http::build_keepalive_pool` as the existing pool consumer.

Name what is already bounded:

- bridge mailbox;
- bridge `max_in_flight`;
- SQLx pool connections;
- worker thread / blocking connection;
- caller waiters;
- response row caps.

Do not build before this audit.

## Rock 1 — Pick First Consumer Shape

Prefer Postgres first if SQLx pool semantics make the win obvious:

- SQLx already owns N DB connections;
- Tina should bound admitted query work above that pool;
- pool `Full` / `Timeout` must remain distinct from
  `PgError::PoolAcquireTimeout`, `PgError::Timeout`, and SQL errors.

SQLite first form may stay serial:

- one `rusqlite::Connection` is honest and simple;
- N SQLite connections can create WAL/busy semantics that distract from
  the pool lesson;
- only pool SQLite if a specimen proves serial hurts.

Decision must be written in this plan before code.

## Rock 2 — Postgres Pool Consumer

Build the smallest useful shape.

Candidate:

```text
PgLane = one bridge worker address or one bounded DB lane.
Pool leases PgLane handles.
Caller acquires lane, sends PgRequest through lane, releases Reuse.
Release Retire only when lane is dead/suspect.
```

Acceptable alternative:

```text
one PgWorker with max_in_flight=N already acts like the pool;
then do not wrap it. Instead ship docs/tests proving the bridge is the pool.
```

Do the honest thing. Do not add a pool just because the phase says pool.

Proof:

- acquire `Full`;
- acquire timeout;
- cancel/timeout frees waiter capacity;
- DB timeout distinct from pool acquire timeout;
- SQL error does not retire the lane by default;
- closed bridge/lane forces typed close/retire;
- pressure report says capacity/current/high-water/full.

## Rock 3 — SQLite Decision

Either:

- keep SQLite serial and document why `max_in_flight=1` is the pool; or
- build a small N-connection SQLite pool with explicit WAL/busy notes.

If building N SQLite connections, prove:

- `Busy` remains a SQLite outcome, not pool `Full`;
- constraint errors do not retire a lane;
- close/drain/force semantics are typed;
- response caps still apply per query.

## Rock 4 — Specimens

Update or add one specimen that reads like a real service:

- HTTP-ish request handler calling DB through bounded pool; or
- host script comparing bounded DB concurrency against Tokio SQLx.

README must say:

- what got shorter;
- what got more explicit;
- where pool pressure differs from database pressure;
- why cancellation may only stop waiting unless DB-side cancel exists.

## Done Means

- At least one DB consumer uses or explicitly rejects the pool vocabulary
  with proof.
- No hidden query queue.
- `Full`, `Timeout`, DB `Busy`, SQLx pool timeout, and SQL errors remain
  separate facts.
- One specimen shows the copied shape.
- Roadmap/changelog updated.
