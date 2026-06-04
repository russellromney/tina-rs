# Phase 150: Scheduler, Turn, and Tail Performance

## Status

- Planned after Phase 149.
- Phase 149 made HTTP/1 p50 respectable in some warmed rows.
- Phase 149 did not fix tails, worker-turn wake gaps, host-call overhead, or
  Linux trust.

## Grug Truth

Tina is no longer silly slow.

But tails still bad.

Small work still crosses too many turns.

This phase attacks scheduler and turn cost, not one more tiny `Vec` reserve.

Fast path must stay Tina:

- bounded;
- fair;
- typed pressure;
- trace true;
- replay honest.

## Current Truth

From local macOS/aarch64 release rows:

- `host_request_reply`: about `200us` p50, still about 10x a Tokio-ish row.
- `service_request_reply_chain`: about `392us` p50, still about 7x a
  Tokio-ish row.
- HTTP/1 close request: about `840us` p50 vs Axum about `562us`.
- HTTP/1 fixed-body close: about `804us` p50 vs Axum about `724us`.
- HTTP/1 warmed keepalive steady-state:
  - small: Tina about `363us` p50, Axum about `386us`;
  - fixed: Tina about `370us` p50, Axum about `419us`;
  - Tina p90/p99 are still around milliseconds and too noisy.
- HTTP process allocations improved, but Tina still allocates more often than
  Axum in common rows.

Known code shape:

- single-shard worker drains `runtime.step()` until no handler ran, then parks
  on the command queue for `idle_wait`;
- multi-shard worker drains remote inbound, steps once, then either parks or
  yields while in-flight work exists;
- `Runtime::step_with_remote` advances driver, harvests timeouts, snapshots one
  message per isolate, then runs at most one handler per isolate per round;
- `call_blocking` uses persistent dispatcher isolates, but still crosses host
  thread -> shard worker -> target isolate -> dispatcher -> host;
- HTTP hotpath rows still contain many worker turns and noisy tail gaps.

## Goal

Make tail and turn performance meaningfully better.

Done means:

- `hotpath_call_blocking` gets lower p50 and p90/p99, or the review explains
  the exact irreducible cross-thread cost with stage evidence;
- HTTP/1 close/fixed/keepalive hotpath rows have lower tail or fewer turns;
- worker loop does not burn CPU while idle or while waiting on timers/I/O;
- fairness/load tests still prove hot actors cannot starve cold actors;
- Linux/x86 rows exist or the review names the exact external blocker;
- all new speed paths preserve trace, pressure, cancel, close, and replay truth.

## Non-Goals

- no production performance claim;
- no public API churn unless a measured row needs it;
- no broad HTTP parser/header public redesign;
- no hidden user-code callbacks;
- no arbitrary "continue with another effect" fast path;
- no dropping trace facts to go faster;
- no unbounded batch drain;
- no changing HTTP wire bytes;
- no weakening body pressure accounting;
- no changing call/reply terminal cause vocabulary.

## Rock 1: Tail-Aware Perf Rows

Build:

- extend hotpath reports with:
  - p90 and p99 stage totals, not only p50 total;
  - slowest stage names for p90/p99 samples;
  - worker park/yield counts if observable;
  - `scheduler_gap_count` for gaps above a chosen threshold;
  - `max_scheduler_gap_ns`;
- keep existing p50 fields for compatibility;
- update `perf_record.sh` and `perf_check.sh` parsing for the new fields;
- add rows:
  - `hotpath_call_blocking_tail`;
  - `hotpath_service_request_reply_chain_tail`;
  - `hotpath_http1_close_request_tail`;
  - `hotpath_http1_keepalive_steady_state_tail`;
  - `hotpath_http1_fixed_body_close_tail`.

Threshold:

- use `100us` as the first scheduler-gap threshold on local release rows;
- make the threshold a report field so Linux can tune it later if needed;
- do not fail CI on p99 timing, only record and compare locally.

Proof:

- tests prove JSON/text rows contain p50/p90/p99, slowest stage, gap count, and
  max gap;
- sample parser accepts the new rows;
- old Phase 149 rows still parse;
- review names which stages dominate p90/p99 before and after code changes.

## Rock 2: Bounded Worker Drain Budget

The worker currently drains while every step makes progress. That killed the
1ms sleep bug. It can still be too simple:

- too little drain means tail gaps;
- too much drain can starve commands, remote inbound, or cold isolates.

Build:

- add `ThreadedRuntimeConfig` fields:
  - `hot_drain_max_rounds`;
  - `hot_drain_max_elapsed`;
  - `idle_repoll_interval`;
- defaults:
  - enough rounds to finish a tiny local call without parking;
  - small elapsed budget so one hot shard cannot monopolize forever;
  - preserve current `idle_wait` as a compatibility upper bound;
- single-shard loop:
  - after a command or delivered step, run bounded hot-drain rounds before
    parking;
  - after the budget expires, poll command queue before continuing;
  - if no work is deliverable, park;
- multi-shard loop:
  - apply the same bounded hot-drain idea;
  - keep remote inbound drain budget;
  - poll commands between local/remote batches;
  - do not yield forever while in-flight work exists unless stage evidence
    proves it is better than bounded parking.

Rules:

- no unbounded drain;
- no command starvation;
- no remote-inbound starvation;
- no hot isolate can consume all turns forever;
- no busy spin when only a timer is pending;
- config validation rejects zero budgets unless the value explicitly means
  "use default".

Proof:

- unit/integration tests:
  - idle worker parks and does not spin;
  - pending timer does not burn a core-ish loop;
  - hot local call completes without unnecessary park gaps;
  - command submitted during hot drain is observed within the configured budget;
  - shutdown during hot drain is observed within the configured budget;
  - multi-shard remote flood does not starve command/shutdown;
  - multi-shard local hot actor does not starve remote inbound;
- existing fairness/load tests pass;
- new perf rows show fewer/lower scheduler gaps.

## Rock 3: Backend Completion Batch Drain

Runtime driver completions may be ready in groups. Handling one completion per
turn creates avoidable wake gaps.

Build:

- add a bounded completion drain path in the runtime driver advance layer;
- keep per-completion trace facts;
- enqueue at most `driver_completion_drain_budget` completions per step;
- make budget configurable through runtime config with a safe default;
- preserve deterministic ordering in simulator/oracle paths;
- do not batch user handler execution beyond existing one-message-per-isolate
  fairness unless Rock 4 changes it with proof.

Proof:

- test many ready timers/TCP completions drain in bounded batches;
- test budget cap leaves later completions for later rounds, not dropped;
- test completion order remains deterministic;
- test failure completions still record `CallFailed` and terminal cause;
- DST replay hash only changes for append-only/faster-truth reasons, not lost
  facts;
- hotpath rows show reduced completion-to-mailbox gaps or fewer worker parks.

## Rock 4: Ready Isolate Scheduling Without Starvation

`step_with_remote` scans every isolate and takes one message per live isolate.
That is simple and deterministic. It is also the wrong long-term shape for a
large service with many quiet isolates.

Build the ready scheduler now.

Internal shape:

- add a runtime-owned ready queue of entry indexes;
- add an `entry.ready` bit so an isolate appears in the queue at most once;
- when `enqueue_entry_message` succeeds, mark the target ready;
- bootstrap, send, call continuation, deferred reply, observed send, remote
  inbound, child lifecycle, and terminal fallback message paths all go through
  the same mark-ready path;
- when `recv_entry_message(index)` returns a message:
  - clear `ready` if the mailbox is now empty;
  - keep/requeue `ready` if more messages remain;
- when an isolate stops or is garbage-collected, clear/drop its ready state;
- preserve one-message-per-isolate-per-round fairness by draining at most one
  message for each ready entry seen in the current scheduler round;
- keep a slow debug/test fallback that can scan all entries and assert the ready
  queue is not missing a non-empty mailbox.

Rules:

- no nondeterministic hash/set iteration in scheduler order;
- ready queue order is deterministic FIFO by first-ready transition;
- self-send still means ordinary later turn, not recursive handler execution;
- closed/full mailbox outcomes are unchanged;
- stopped isolate messages still become `MessageAbandoned` truth;
- cross-shard and local paths use the same ready marking rule.

Proof:

- new `tina-runtime/tests/ready_scheduler.rs`;
- deterministic replay tests pass;
- equal-priority isolates still alternate under pressure;
- cold isolate receives service under a hot isolate flood;
- quiet-isolate scan cost drops in a test with many registered quiet isolates
  and one hot isolate;
- stopped isolate is not polled forever;
- bootstrap messages are delivered;
- self-send messages are later-turn, not same-handler recursion;
- mailbox-full and closed-address outcomes unchanged;
- debug assertion/fallback test catches a deliberately unmarked non-empty
  mailbox through a test-only seam or helper;
- trace order is stable or review names the exact trace-hash rebase reason.

## Rock 5: Host Call Fast Lane

`call_blocking` is much better but still expensive.

Build:

- reduce host-call dispatcher cost without removing bounded admission:
  - reuse host-call task boxes where safe, or replace with a typed small command
    enum for common call shapes;
  - avoid avoidable allocations in dispatcher messages;
  - keep dispatcher pool capacity explicit;
  - keep reply channel pool;
  - keep typed `CallOutcome`;
  - keep `call_blocking_request` and typed handle helpers on the same path;
- add hotpath counters for:
  - host command accepted;
  - dispatcher handler started;
  - target handler started;
  - reply sent to host;
  - dispatcher stopped/cleared tail.

Rules:

- no host thread directly mutates isolate state;
- no bypass of mailbox capacity;
- no hidden unbounded host-call queue;
- no per-call isolate registration coming back;
- no losing timeout/closed/rejected truth;
- no special case that only works for `SingleShard`.

Proof:

- `call_blocking`, `call_blocking_typed`, and `call_blocking_request` tests pass;
- timeouts still time out;
- target stopped returns typed closed/rejected truth;
- dispatcher mailbox full is visible;
- multi-threaded host burst stays bounded;
- process allocations for `hotpath_call_blocking` do not regress and should
  improve if this rock changes storage.

## Rock 6: HTTP Protocol Turn Cleanup

Use the scheduler work first. Then remove remaining HTTP turns only where no
service policy boundary is crossed.

Targets:

- terminal close-after-write path from Phase 149;
- keepalive next-read rearm path;
- response write completion path where all accounting is already done;
- stale timeout/tail events after clean terminal close.

Build:

- add narrow protocol-local terminal/follow-up actions only if current
  `RuntimeCallCompletion` is not enough;
- never run user service code from a hidden callback;
- body metrics and pressure release must happen before any terminal bypass;
- partial write/failure must still go through ordinary message path;
- stale tail facts must stay visible but not dominate measured windows.

Proof:

- HTTP/1 server smoke, keepalive, body lifecycle, streaming response, chunked,
  WebSocket upgrade, and pressure tests pass;
- hotpath close/fixed/keepalive rows show fewer turns or smaller tail gaps;
- wire bytes match before/after tests;
- close failure and partial write tests prove no swallowed failure;
- body high-water/current returns to zero;
- idle timeout/slowloris behavior unchanged.

## Rock 7: Linux/x86 Evidence

Build:

- run Linux/x86 release perf rows at least twice after code changes;
- record rows in Phase 150 `perf_history.jsonl`;
- if the session cannot drive GitHub/manual Linux, review must name:
  - exact command;
  - exact missing permission/blocker;
  - which rows remain mac-only.

Rows:

- `hotpath_call_blocking_tail`;
- `hotpath_service_request_reply_chain_tail`;
- `http1_close_request`;
- `http1_fixed_body_close`;
- `http1_keepalive_steady_state_small`;
- `http1_keepalive_steady_state_fixed`;
- mini-service health hot row if still present.

Proof:

- macOS rows and Linux rows are keyed by platform/arch/profile;
- `perf_check` never compares macOS against Linux;
- review states whether Linux shows the same bottleneck or a different one.

## Rock 8: Soak And CPU Sanity

Fast path must not leak or spin.

Build:

- add a short perf soak:
  - warmed keepalive;
  - bounded concurrent clients;
  - service-owned concurrency cap;
  - final resource current zero;
  - stable-ish RSS;
  - no pressure creep;
  - no transport timeout under normal load;
- add an idle CPU sanity probe:
  - worker with no work;
  - worker with one pending timer;
  - worker with one pending TCP read on an open socket whose peer sends nothing;
  - count loop wakeups or trace/observer ticks over a fixed window.

Proof:

- `make proof-soak` still passes;
- opt-in long soak command remains documented;
- idle/pending probes have bounded wake counts;
- if a repeated timeout appears, fix it as a bug before merge.

## Rock 9: Docs, Claims, And Next Bottleneck

Update:

- Phase 150 `perf_history.jsonl`;
- Phase 150 `perf_sample.txt`;
- Phase 150 `review.md`;
- `examples/systems/perf_native/README.md`;
- `ROADMAP.md`;
- `CHANGELOG.md`.

Required wording:

- local evidence;
- not production performance claim;
- p50 improved vs tails improved;
- what cost remains;
- Linux status;
- next bottleneck after the phase.

## Required Verification

Run at least:

- `cargo fmt --all --check`;
- `git diff --check`;
- `cargo test -p tina-runtime --test scheduler_turn_tail -- --nocapture`;
- `cargo test -p tina-runtime --test ready_scheduler -- --nocapture`;
- `cargo test -p tina-runtime --test threaded_call_blocking -- --nocapture`;
- `cargo test -p tina-runtime --test host_burst -- --nocapture`;
- `cargo test -p tina-runtime --test multishard_fairness -- --nocapture`;
- `cargo test -p tina-runtime --test runtime_terminal_completion_action -- --nocapture`;
- `cargo test -p tina-sim --test timer_semantics -- --nocapture`;
- `cargo test -p tina-http --test server_smoke --test server_keepalive --test body_lifecycle --test streaming_response -- --nocapture`;
- `cargo test --release --manifest-path examples/systems/perf_native/Cargo.toml --test hotpath -- --nocapture`;
- `cargo test --release --manifest-path examples/systems/perf_native/Cargo.toml --test perf -- --nocapture`;
- `make perf-check`;
- `make proof-fast`;
- `make proof-soak` unless a real external blocker is named.

If a command is skipped, `review.md` must say why.

## Done

This phase is done only when:

- at least one host/service call row improves;
- at least one HTTP hotpath tail row improves;
- no fairness/load regression;
- no idle spin;
- no hidden terminal truth loss;
- Linux evidence exists or the blocker is explicit;
- review names the next bottleneck honestly.
