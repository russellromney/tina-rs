# 073 — Pool Consumers

## Status

- Done: plan drafted from 067 worker-pool findings; Rock 1 HTTP/1.1
  keepalive pool landed (`tina_http::KeepaliveConnection` plus
  `build_keepalive_pool` returning `KeepalivePoolHandles`). 18
  integration tests cover reuse, cross-origin isolation, stale
  retirement (always-Reuse self-heal + explicit-Retire
  capacity-drop), capacity / waiter reclaim, deadline propagation,
  request_timeout wall-clock correlation, drain (incl. with parked
  waiters) & force close + Stop-after-Force, HTTPS origin identity
  and real HTTPS keepalive smoke, multi-slot pool, pressure report,
  DST replay determinism.
- Done: server-side keepalive landed in 076, and the keepalive pool is
  now proven end-to-end against the native listener. `specimen_outbound_http`
  uses the pooled keepalive shape.
- Closed: HTTP keepalive consumer landed and dependent follow-ups moved to
  their own completed phases (077, 078, 079).
- Moved out: bridge convention audit lives in 081.
- Deferred: multi-shard pool, keyed cluster pool, automatic lease
  reclaim.

## Goal

Make the 067 pool vocabulary earn its keep in real Tina consumers.

Do not build a bigger pool because pools are exciting. Build the next
consumer that needs one, then let the repeated shape harden.

Grug truth:

> One good local pool first. Then consumers. Then maybe many pools.

## Non-Goals

- No multi-shard pool implementation.
- No global pool registry.
- No hidden retry.
- No hidden queue.
- No timer that steals a leaked lease back behind the user.
- No abstraction that makes `Full`, `Closed`, timeout, stale lease, or
  resource health less visible.

## Prerequisite

Phase 072 must land first:

- deadline / remaining-budget helper;
- bounded `PendingCallSet`;
- explicit cleanup on completion / timeout / cancel / owner stop;
- fill-cancel-refill proof.
- one bridge/external-work proof showing accepted work can complete late
  after Tina stops waiting.

Pool consumers amplify timeout confusion. Do not start this phase while
deadline ownership is still fuzzy.

## Rock 1 — HTTP Keepalive Pool

Replace or supersede the current `tina_http::HttpConnectionPool`
capacity-1 gate with a real local pool consumer.

First form:

- keyed by origin: scheme + host/authority + port + TLS trust identity;
- bounded max idle connections;
- bounded max leased connections;
- FIFO acquire waiters;
- explicit acquire timeout;
- `ReleaseDisposition::{Reuse, Retire}`;
- pool may override `Reuse` to `Retire` when it knows the connection is
  stale or closed;
- close modes: drain and force;
- pressure report: capacity, available, leased, waiters, full, timeout,
  cancelled, closed, retired.

No HTTP/2 multiplexing here. No proxy, cookie, redirect, or system-root
story. This is HTTP/1 keepalive resource reuse.

Proof:

- sequential requests reuse one connection;
- stale server close retires the connection;
- acquire full is visible;
- acquire timeout frees waiter capacity;
- cancelled acquire frees waiter capacity;
- drain stops new acquire and lets leased resources return;
- force closes waiters and marks late releases stale;
- DST or trace proof names the pressure facts.

## Rock 2 — SQLite / DB Pool Check

Do not build a pooled SQLite bridge unless the serial bridge now hurts
in a specimen.

Smallest useful check:

- write one specimen or test that wants more than one DB operation in
  flight;
- decide if SQLite needs N independent connections or stays serial;
- prove busy/constraint/timeout stay typed;
- prove pool `Full` is different from SQLite `Busy`.

If this does not teach anything new, record that and stop.

## Rock 3 — Bridge Consumer Audit

Look at existing bridge crates after HTTP keepalive:

- `tina-reqwest-bridge`;
- `tina-sqlite-bridge`;
- `tina-tokio-bridge`;
- `tina-tower-bridge`;
- `tina-rpc-tokio`.

Ask only:

- does this crate already have a pool-shaped resource?
- does it need acquire/release/close vocabulary?
- does it need health/retire vocabulary?
- does it already have its own version of the same report?

Two crates can be coincidence. Three repeated shapes is evidence.

Do not extract a shared pool crate unless the audit finds three real
copies with the same nouns.

## Rock 4 — Specimens

Update existing specimens before adding new ones.

Candidates:

- outbound HTTP / HTTPS client specimen using keepalive;
- graceful shutdown specimen showing pool drain;
- overload specimen showing acquire full vs request timeout.

The README must say what got shorter and what stayed explicit.

## Rock 5 — Multi-Shard Memo, Not Code

After at least one real pool consumer lands, write the memo:

- one global pool serving callers from many shards;
- shard-local pools with placement;
- local-first then remote fallback;
- release goes to origin shard or resource shard;
- deadline ownership across shard hops;
- stale lease identity across generations.

Do not implement until a consumer needs one of those shapes.

## Done Means

- One production-shaped consumer uses the 067 pool vocabulary.
- Pool pressure appears in tests, not just docs.
- Lease health and close mode semantics are visible.
- Timeout/cancel cleanup reclaims waiter capacity.
- The old capacity-1 HTTP pool is either retired or documented as
  legacy first form.
- The roadmap says multi-shard pool waits for evidence.
