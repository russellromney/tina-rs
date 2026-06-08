# Track D — Bridges and external work (2026-06-08)

Scope: `tina-rpc-tokio`, `tina-sqlx-bridge`, `tina-sqlite-bridge`,
`tina-aws-bridge`, `tina-reqwest-bridge`, `tina-tokio-bridge`,
`tina-tower-bridge`. Working tree, HEAD 49c3580.

Concept separation used throughout: caller authority (timeout/cancel)
vs admission capacity (permits/in-flight cap) vs external physical work
(the real query/request) vs late results vs retries vs terminal
classification.

---

## D1 — [Critical/High] rpc-tokio shim mailbox sized against the wrong cap → live reply dropped → awaiter hangs forever + slot leak

- File: `tina-rpc-tokio/src/lib.rs:351-353` (shim mailbox = `max_in_flight * 2`),
  with the consuming awaiter at `tina-rpc-tokio/src/lib.rs:502-509` (`rx.await`)
  and the forgotten permit at `:416`.
- Root mechanics in `tina-rpc/src/client.rs:525-532` (`begin_close` fails
  every in-flight at once) and `:694-703` (one `ClientResultMsg` per reply),
  delivered via fire-and-forget `send` (`:494`) that is silently dropped on a
  full target mailbox (`tina-runtime/src/dispatch.rs:549-556` SendRejected →
  no delivery, no retry).

Invariant violated: "bounded means the real thing is bounded" and "every
call settles exactly once with a typed terminal cause." The bridge sizes
its reply-demux mailbox against `bridge.max_in_flight`, but the actual
number of `ClientResultMsg` the shim can receive in one burst is bounded by
`Client.max_in_flight` (default 64) — a *different, independent* cap.

Concrete bug: the shim mailbox holds `2 * bridge.max_in_flight` slots. The
`tina_rpc::Client` can flush up to `Client.max_in_flight` `ClientResultMsg`
in a single effect batch — most cleanly on connection close, where
`begin_close` drains the entire in-flight map and emits one `notify` per
entry at once. When that burst exceeds `2 * bridge.max_in_flight`, the
bounded shim mailbox overflows and the runtime drops the excess
`ClientResultMsg` silently (`SendRejected::Full`). If any dropped reply
belongs to a *live* (non-cancelled) correlator, that call's awaiter blocks
on `rx.await` forever and its forgotten admission permit is never returned
→ permanent slot leak that shrinks effective capacity toward zero.

Why it happens in real use:
- `BridgeClient::new` lets the caller choose `bridge.max_in_flight` with no
  visibility into the underlying `Client.max_in_flight`. The doc claims
  `*2` "cannot overflow it"; that reasoning only holds if shim traffic is
  bounded by `bridge.max_in_flight`, which it is not.
- The shim is shared by every `BridgeClient` clone, and the same `Client`
  may also be driven by non-bridge `tina_rpc` callers; its in-flight set is
  not bounded by any single bridge's cap.
- A cancel/re-admit churn cycle keeps cancelled-but-still-in-flight
  requests occupying `Client` slots; their eventual replies arrive at the
  shim on top of live replies.

Trigger recipe (deterministic): `bridge.max_in_flight = 4` (shim = 8),
`Client.max_in_flight = 64`. Admit several calls, cancel some (they remain
in-flight at the Client), admit more so > 8 requests are outstanding at the
Client, then drop the connection. `begin_close` emits > 8 `ClientResultMsg`
into the 8-slot shim mailbox in one turn; the surplus is dropped; a live
awaiter among them hangs.

Repro/test idea: integration test against the *real* `tina_rpc::Client`
(current tests use a `ClientStub` and never exercise `begin_close` fan-out).
Set `bridge.max_in_flight = 2`, `Client.max_in_flight = 8`; open the client,
admit 8 live calls, force a connection close, assert all 8 settle with a
typed terminal (`ConnectionClosed`) within a deadline and
`available_slots() == max_in_flight` afterward. Today some calls never
settle and `available_slots()` ends below `max_in_flight`.

Fix (smallest correct): size the shim mailbox against the *Client's*
in-flight cap, not the bridge's. Accept `client_max_in_flight` in
`BridgeClient::new` and set `shim_mailbox = client_max_in_flight + slack`,
or have `tina_rpc::Client` expose its `max_in_flight` so the bridge can
read it. Belt-and-braces: make the shim deliver via an *observed* send so a
dropped reply is detected and the matching awaiter is settled with a typed
terminal instead of hanging (today the Client's `notify` is fire-and-forget
with no failure path).

LLM-pattern: yes. Plausible-looking "× 2 absorbs the burst" rationale that
silently picks the wrong cap to multiply, with tests that prove the helper
internals (CancelGuard/observer slot accounting) but never the user-visible
"every call settles" behavior against the real peer.

---

## D2 — [High/High] Poll-continuation self-send is dropped on a full own-mailbox → permanent slot leak (all poll-loop bridges)

- Files: `tina-sqlx-bridge/src/worker.rs:328,358,384,388` (`sleep(...).then(PgMsg::Poll(id))`);
  `tina-reqwest-bridge/src/worker.rs:420,468,557,613`;
  `tina-aws-bridge/src/worker.rs:354,390,394` (and the sqs/sns/dynamodb/secrets
  workers, same shape);
  `tina-sqlite-bridge/src/worker.rs:394,434`.
- Runtime root cause: `tina-runtime/src/dispatch.rs:596-637` — a call
  continuation is delivered into the same isolate's *bounded* mailbox via
  `dispatch_local_send` (`tina-runtime/src/remote.rs:245-249`); on
  `Full` it records `SendRejected` and **drops the continuation** with no
  retry/reschedule.

Invariant violated: "shutdown/poll eventually settles" and "bounded
capacity caps the real thing." The poll-loop bridges assume the
`Poll(id)` self-wakeup is delivered reliably. It is not.

Concrete bug: each bridge worker keeps a per-request slot alive only as
long as its `Poll(id)` continuation keeps firing. The continuation is a
self-`send` into the worker's own mailbox, which is shared with incoming
`Send` requests (mailbox_capacity > max_in_flight by default). If the
mailbox is saturated with `Send`s at the moment a `Poll(id)` continuation
is dispatched, the continuation is dropped. That request's slot is then
never polled again: the entry stays in `in_flight` forever, the spawned
result is never collected, and `max_in_flight` is permanently reduced by
one. Repeated occurrences walk the bridge down to `Full` for everything.
The caller's runtime `call` eventually times out, but the *bridge-side*
capacity leak is permanent and silent.

Why it happens in real use: defaults already allow it — sqlx
`mailbox_capacity = 64`, `max_in_flight = 8`; reqwest `64 / 16`. A burst of
callers fills the mailbox with `Send`s (the surplus get `Full` *in the
handler* but still occupy mailbox slots until handled), and the in-flight
Poll continuations lose the race for a free slot.

Worst instance: `tina-sqlite-bridge` runs `max_in_flight = 1`. A single
dropped `Poll` wedges the entire bridge — every later call returns `Full`
forever.

Repro/test idea: register a sqlx/reqwest worker with `mailbox_capacity` ==
`max_in_flight` (or small), admit one slow request, then flood the mailbox
to capacity with `Send`s on the same turn the `Poll` continuation is due;
assert the slow request still settles and `in_flight` returns to 0. Today
the slot can leak.

Fix: do not rely on best-effort self-continuation for liveness of a held
slot. Options: (a) deliver poll-loop continuations through a path that
cannot be dropped (a dedicated unbounded/priority self-timer queue, or
`send_observed_until`-style retry), or (b) reserve mailbox headroom for
continuations (size the mailbox `>= mailbox_capacity_for_sends +
max_in_flight` and refuse to admit `Send`s into the reserved band). The
runtime could also surface dropped self-continuations to the isolate
rather than only the trace.

LLM-pattern: yes. The poll loop "obviously" reschedules itself; the failure
is one layer down (bounded self-mailbox drop) and invisible at the bridge
source.

---

## D3 — [High/Med] sqlite bridge frees the admission slot on timeout while the blocking worker thread still holds the connection → max_in_flight violated; Full misclassified as Internal

- File: `tina-sqlite-bridge/src/worker.rs:397-435` (timeout branch does
  `take()` at `:399` and never re-inserts), admit at `:329` (`in_flight`
  is now `None` so a new request is admitted), `cmd_tx` is
  `sync_channel(1)` (`:260`) with the explicit comment "bridge never sends
  a second Run before the first completes" (`:258`).

Invariant violated: "timeout settles caller authority but does not lie
about external work" and "bounded in-flight caps physical work." Other
bridges (sqlx `worker.rs:360-385`, aws `worker.rs:371-392`) keep the slot
held until the spawned task reaches terminal, marking `abandoned` — exactly
to keep `max_in_flight` bounding physical work. The sqlite bridge does the
opposite.

Concrete bug: on bridge timeout the sqlite worker sets `abandoned`, sets
`in_flight = None`, replies `Timeout`, and stops rescheduling Poll. The
single blocking worker thread is still executing the timed-out query (sqlite
is sync; `abandoned` is advisory and does not interrupt it). The freed slot
lets `admit` accept a new request and `cmd_tx.try_send` a second `Run`. The
`sync_channel(1)` buffer is empty (the worker already *received* the first
Run and is running it), so the second `try_send` succeeds and the
"never a second Run before completion" invariant is broken: two requests are
now outstanding against the one-connection worker, serialized behind the
still-running timed-out query. A *third* request then finds the channel full
and gets `SqliteError::Internal("worker thread unavailable")` — a terminal
*misclassification*: this is admission backpressure (`Full`), not an
internal fault.

Why it happens in real use: any query that exceeds `default_timeout` on the
sqlite bridge under continued traffic. Long `VACUUM`, a big write, or a slow
`QueryRows` will time out at the bridge while the connection thread keeps
running, and follow-up calls oversubscribe / mislabel.

Repro/test idea: config with tiny `default_timeout` and a query the worker
thread holds past it (e.g. a busy loop via a custom SQL function, or a
genuinely slow statement); fire one call, let it time out, fire two more
back-to-back; assert the second does not over-admit (it should be `Full`
until the worker is actually idle) and the third is `Full`, never
`Internal`.

Fix: mirror sqlx/aws — keep the `InFlight` slot occupied after a bridge
timeout (reply `Timeout` to the caller, set `abandoned`, but re-insert the
slot and keep polling until the worker thread sends its terminal on
`response_rx`). Only then free the slot. This restores the
`sync_channel(1)` invariant and keeps `max_in_flight = 1` honest.

LLM-pattern: yes. Looks like the sqlx timeout path but silently drops the
"keep the slot leased" half, and the explicit channel-capacity comment is
now false.

---

## D4 — [Low/Med] reqwest retry-on-timeout / retry-on-io re-sends a request that may have already executed (exactly-once hazard); documented but easy to misconfigure

- File: `tina-reqwest-bridge/src/worker.rs:505-526` (timeout → abort →
  `schedule_retry`) and `:633-656` (io error → retry); policy
  `tina-reqwest-bridge/src/types.rs:182-204`.

Invariant: "retries must not violate exactly-once for non-idempotent work."
On a per-attempt *timeout*, the bridge aborts the local future and re-sends
— but a timeout is caller-side: the original request may already be at the
server and committed. Same for a connection-reset-after-send mapped to
`ReqwestError::Reqwest` with `on_reqwest_io = true`. Retrying a POST in
either case double-executes.

This is **documented** (types.rs:168-174: "Configuring retry is the user's
promise that the configured requests are safe to repeat"), so it is a
correct-by-design hazard rather than a latent bug. Recorded for
completeness and to flag that the bridge does not — and cannot — inspect
method/idempotency. No fix required; consider a doc cross-link on the
`on_timeout` / `on_reqwest_io` fields warning that both can re-send work
that already reached the server.

LLM-pattern: n/a (deliberate, documented).

---

## Disproven / checked-and-clean

- **rpc-tokio observer double-release / CancelGuard double-release.** The
  "only release if I actually removed the pending entry" rule
  (`lib.rs:228-235`, `:586-595`) is correct and covered by
  `observer_err_with_stale_entry_does_not_release_slot` and
  `cancel_guard_drop_releases_only_when_it_removed_pending_entry`. The
  observer fires exactly once (`tina-runtime/src/threaded.rs:1042-1056`).
  No double-release. Clean.

- **rpc-tokio terminal classification.** `map_client_result`
  (`lib.rs:130-141`) maps every `ClientResult` variant to a distinct
  `BridgeError`; `fire_observer_err` maps the three observed-error kinds
  correctly (Full / ConnectionClosed / IoError). No Full↔Closed↔Timeout
  conflation. Clean (covered by `map_client_result_round_trips_each_variant`).

- **sqlx/aws timeout = late-result accounting.** Both keep the slot leased
  after timeout, set `abandoned`, keep polling, and tally
  `late_results` / `note_late_external_terminal` when the spawned task
  finishes (sqlx `worker.rs:291-302,356-366`; aws `worker.rs:316-325,
  363-369`). External work stays bounded by `max_in_flight`. Correct — and
  the model D3 should match.

- **aws plain-send (`reply_plain`) never times out.** Intentional and
  correct: a `send` (not `call`) has no caller authority to settle, so the
  slot is held until external terminal (`worker.rs:371-372`). Clean.

- **tina-tokio-bridge cancel-on-drop.** `BridgeCancellation::drop` sets the
  `cancelled` flag (`lib.rs:358-364`); `BridgeGuard::handle` noops a
  cancelled request (`:418-420`). Timeout returns before disarm so the flag
  is set; an un-handled queued request is noop'd, an already-handled one
  just drops its response (counted `dropped_responses`). No slot leak (the
  only "slot" is the bounded mailbox, which drains the message either way).
  Clean.

- **tina-tokio-bridge `call_with_retry` on Full.** Each retry is a fresh
  request/oneshot; `Full` means the message never reached the handler, so
  no double-execution (`lib.rs:989-1008`). Clean.

- **tina-tower-bridge.** Thin shell over `BridgeHandle`; `poll_ready` never
  `Pending` (documented), admission surfaces `Full` on the call future. No
  extra queue. Clean.

- **aws `await_drain` uses `std::thread::sleep`** (`core.rs:56`). It is a
  user-facing closer helper, not run on a shard or required to be async.
  Minor: blocks the calling thread if invoked inside a Tokio task; the
  tokio-bridge offers an async drain variant but the aws closer does not.
  Noted, not a bug.

---

## Areas needing deeper review

- The Poll-continuation drop (D2) is a *runtime* primitive weakness with
  bridge-visible consequences. Track C/E should decide whether self-
  continuations that keep a held resource alive may ever be dropped on a
  full own-mailbox. Every poll-loop bridge depends on this.
- rpc-tokio integration coverage uses a stub Client; it should be run
  against the real `tina_rpc::Client` to exercise `begin_close` fan-out and
  server reply bursts (D1).
- sqs/sns/dynamodb/secrets aws workers were structurally confirmed to share
  the sqlx/aws slot model but not line-audited individually for timeout
  re-insert; spot-check each `poll` timeout branch keeps the slot.
