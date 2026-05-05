# Plan Review 1

Verdict: right phase, good shape, not quite ready to hand to implementation.

Victor Marijnen is aimed at the right rock. It moves Tina from "has many local
I/O effects" to "can run a bounded local service and survive pressure." The plan
also keeps the right refusals: no flow syntax, no remoting, no clustering, no
hidden fallback queues.

Hostile grug still sees several pins missing. These are not objections to the
phase. They are places where implementation could wander and still claim the
plan was followed.

## Findings

1. **[P1] Cross-shard call transport is not pinned enough**

   Lines 161-178 require bounded live cross-shard call replies, but not the
   shape. This is the phase's biggest semantic rock. Pin that this is
   local-process, worker-thread-to-worker-thread only; request and reply both use
   bounded shard-pair paths; no remoting, registry, broker, service locator, or
   second call API. Also pin where timeout is owned and what happens if the
   requester times out before the reply reaches the source shard.

2. **[P1] Resource owner transfer rules are missing**

   Lines 125-143 say resources have owners, but server workloads need transfer:
   listener accepts a stream, child takes it, TLS wraps it, file/persistence work
   may be owned by a request isolate, and shutdown may move work into cleanup.
   Pin owner transfer as an explicit operation/result, especially
   listener-to-child and stream-to-TLS. Without this, cleanup can either leak or
   close another live owner's resource.

3. **[P1] Drain admission rules are still squishy**

   Lines 145-159 describe drain, but not the state machine. Pin states and
   admission: `Running -> Draining -> Stopped` or `Failed`; external ingress
   rejects in draining; listener accepts stop; internal cleanup effects are
   allowed only if owned by already-accepted work; cross-shard sends/calls during
   drain have named behavior. This is needed so tests know what "drain accepted
   work" means.

4. **[P1] Shard failure timing relative to pending work needs sharper contract**

   Lines 195-209 say quarantine/reject, but pending calls and driver completions
   can be in awkward places. Pin whether failure cancels pending local driver
   ops, tombstones them, or lets them complete into rejection; same for
   cross-shard in-flight request/reply messages. Healthy shards keep running, but
   failed-shard ownership cleanup must be exact.

5. **[P2] Health snapshot consistency model is unspecified**

   Lines 109-123 require health snapshots, but live multi-shard snapshots can be
   consistent, eventually consistent, or per-shard consistent. Pick one. Grug
   recommendation: per-shard consistent, whole-system best-effort with monotonic
   counters, never used for correctness decisions. Tests then compare health to
   trace/counters within that model.

6. **[P2] Config manifest needs a public-boundary decision**

   Lines 96-107 say add one typed manifest, but not where it lives or whether it
   replaces scattered builders. Pin names and relationship to existing config:
   either `LocalSystemConfig` is the preferred public shape with adapters from
   existing configs, or this is internal only. Since user-facing boundedness is
   the point, grug recommends public preferred shape.

7. **[P2] Inbound TLS can balloon unless scope is narrower**

   Lines 180-193 wisely reject big cert policy, but still leave room for ALPN,
   SNI, client auth, reload, certificate stores, and policy callbacks. Pin
   042's exact scope: static cert/key, no client auth, no ALPN/SNI routing, no
   reload, local deterministic cert fixture, timeout/cancel/full/closed tested.
   Everything else later.

8. **[P2] Acceptor/service pattern risks becoming ergonomics work**

   Lines 211-225 allow a helper or macro. This should come after raw semantics
   land and should be test-support/minimal public helper only. Pin that the
   e2e workload must first be expressible with normal Tina effects; helper only
   removes repeat ceremony and cannot introduce a new service framework.

9. **[P2] Call/reply type-safety target is directionally good but lacks expected
   implementation path**

   Lines 227-238 say reduce downcast panic risk. That can mean a tiny compile
   fail guard or a deep address/call redesign. Pin expected direction before
   work starts. Grug recommendation: typed-address common path must compile-fail
   wrong replies; low-level erased escape hatches stay but are renamed/loud; no
   deep rewrite unless cross-shard calls force it.

10. **[P2] DST bar says "catch one deliberate broken invariant" but not which
    invariants matter**

    Lines 254-266 are good but broad. Pin at least three deliberate invariant
    probes: owner cleanup leak, drain accepting forbidden ingress, and
    cross-shard reply delivered after timeout as success. These are the bugs DST
    should be scary-good at finding.

## Suggested Plan Edits

- Add a short "Pinned Architecture" section before Build Order:
  local-process only, bounded shard-pair request/reply transport, per-shard
  worker ownership, no broker/remoting.
- Add an "Owner Transfer" subsection under Rock 4.
- Add a "Drain State Machine" subsection under Rock 5.
- Add pending-work behavior to Rock 8.
- Add health consistency and config public-surface names to Rocks 2-3.
- Narrow inbound TLS scope explicitly.
- Move typed acceptor helper after raw e2e proof, or say helper is optional
  closeout polish only.
- Name the three DST broken-invariant probes.

After those edits, grug says ready to implement.

# Plan Review 2

Verdict: much stronger. Most Plan Review 1 cracks are sealed. One P1 and a few
P2 pins remain before implementation.

What improved:

- Local-process-only cross-shard architecture is now explicit.
- Timeout and late reply behavior are pinned.
- `LocalSystemConfig` is named as the teaching path.
- Health snapshot consistency is honest.
- Owner transfer, drain states, TLS scope, shard-failure pending work, acceptor
  helper discipline, call/reply direction, and DST invariant probes all landed.

## Remaining Findings

1. **[P1] Cross-shard call reply ownership still has one dangerous hole**

   Lines 46-56 and 196-216 pin bounded shard-pair request/reply paths, but do not
   say which shard owns the pending call record once the request crosses shards.
   This matters for timeout, requester stop, requester mailbox full, and reply
   path full. Pin the invariant: the requester shard owns the pending call state
   until terminal outcome; destination shard never owns caller liveness; reply
   transport carries only completion data; terminal outcome is emitted exactly
   once on the requester shard.

2. **[P2] `LocalSystemConfig` could accidentally become a partial manifest**

   Lines 108-122 name the config, but "if implemented" lets the plan skip
   capacities for live resources that already exist. For 042, the manifest
   should cover every bounded live queue/resource family that exists at phase
   start plus any new one added during the phase. If a family is intentionally
   not configurable, the plan should require it to appear in health/audit as
   fixed or unsupported.

3. **[P2] Health snapshot should include drain/shutdown terminal report shape**

   Lines 124-142 say health is observation and shutdown reports, and Rock 5 says
   terminal health/report data. But the report shape is not pinned. Add a small
   required terminal report: final state, deadline hit or clean, canceled counts,
   tombstoned counts, rejected-after-drain counts, failed shard ids, and
   remaining owned-resource counts.

4. **[P2] Resource cleanup should explicitly include listener accept loops**

   Lines 144-169 include TCP listeners and transfers, but accept loops are the
   classic service leak. Pin pending accept cancellation/tombstone on stop,
   drain, and shard failure; late accepted streams after cancellation must be
   closed/rejected visibly and not become unowned.

5. **[P2] Inbound TLS should test failed handshake cleanup**

   Lines 218-234 cover config/handshake/timeout/full/closed, but failed
   handshake cleanup is the sharp edge: raw stream must be closed or returned to
   one clear owner, pending op must terminate, and no `TlsStreamId` may leak.
   Add this to proof checklist.

6. **[P3] Build order may want type safety before cross-shard implementation**

   Lines 62-72 put call/reply type safety after cross-shard calls. That is okay
   if Rock 10 is small, but if typed-address changes affect call representation,
   doing it after Rock 6 risks rework. Add a note under Rock 6: if call/reply
   type representation must change, do the minimal Rock 10 piece first.

## Suggested Small Edits

- Add requester-shard pending-call ownership invariant under Pinned
  Architecture or Rock 6.
- Tighten `LocalSystemConfig` coverage rule.
- Add terminal shutdown report fields to Rock 5/Health.
- Add pending accept cleanup rule to Rock 4 or Rock 5.
- Add failed TLS handshake cleanup to Proof Checklist.
- Add the type-safety-before-cross-shard note under Rock 6.

After these, grug sees no glaring plan blockers.

# Implementation Audit

Current live service facts before Victor implementation:

- `LocalSystem` and `LocalMultiShardSystem` are the preferred app owners.
  `ThreadedRuntime` and `ThreadedMultiShardRuntime` are lower-level live
  runners.
- Live shards are worker-thread owned. User ingress enters through bounded
  `sync_channel` command queues.
- Explicit-step `MultiShardRuntime` has real bounded source/destination queues.
  Live multi-shard currently routes remote effects through target worker command
  queues and reports per-pair metrics.
- Same-shard isolate calls already exist with requester-owned pending state,
  mandatory timeout, `Full`/`Closed`/`Timeout` outcomes, and late-reply rejection.
- Cross-shard isolate calls were still documented/implemented as unavailable at
  audit start. Victor's first implementation slice adds bounded local
  request/reply envelopes while keeping pending-call truth on the requester
  shard.
- Runtime-owned resources are opaque ids: listener, stream, TLS stream, file,
  UDP socket, call id. Driver cancellation already tombstones many late
  completions.
- Stop/shutdown cancellation exists for driver calls owned by stopped
  requesters. Terminal report exists but mostly summarizes trace, not full
  resource-count health.
- Live health/topology reports exist for shard state, ingress pressure, remote
  pressure, storage lane capacity, trace retention, and dropped-trace count where
  available. They are observation, not correctness state.
- TLS client exists. Inbound/server-side TLS does not yet exist.
- Shard worker failure is visible as `Failed`; healthy shards can continue in
  tests. The post-failure contract still needs more direct pins and proof.
- Call/reply common path is typed by `Address<Message, Reply>`, but low-level
  erased paths can still panic on deliberate wrong-type misuse.

# Implementation Review 1

Verdict: strong Victor first half, not full Victor closeout yet.

What landed clean:

- `LocalSystemConfig` now exists as the public config manifest for live owners.
  It rejects zero capacities before start and feeds the lower-level worker
  config.
- Terminal reports now expose a shutdown report: final state, clean bit,
  canceled/tombstoned/rejected counts, failed shard ids, and owned-resource
  count placeholder.
- Live local cross-shard isolate calls now round-trip replies over bounded
  worker-to-worker paths. The requester shard owns pending call state and
  emits terminal outcomes.
- Cross-shard call destination-local `Full` and `Closed` now return typed
  `CallOutcome::Full` / `CallOutcome::Closed` to the requester when the reply
  path can carry them.
- The explicit-step runtime and `tina-sim` oracle both use the same remote
  envelope shape for send, call reply, and call failure outcomes.
- DST was extended with seeded random cross-shard call histories plus a shrinker
  for bounded transport full.

What DST found and fixed:

- `MultiShardSimulator::advance_to_next_timer` considered timers but not
  pending isolate-call deadlines. A cross-shard no-reply call could keep
  quiescence spinning. Fixed by including pending isolate-call deadlines.
- Remote call completion causality originally pointed at the send attempt, not
  the original call attempt. Fixed so `call_attempts_settle` remains meaningful
  for cross-shard calls.
- Destination-local remote call rejection first behaved like "trace rejection
  then timeout." Fixed so the requester receives typed `Full`/`Closed` outcomes
  when return transport succeeds.

Positive review:

- The core local multi-shard service story is much more real now: success,
  full, closed, timeout, and replay are all directly tested across live workers
  and the simulator oracle.
- The live and sim implementations moved together. That matters: DST is not a
  fake sidecar proof; it now exercises the same semantic rocks.
- Tests are user-shaped where it matters: `LocalSystem::multi_shard` starts live
  workers, sends real public messages, observes typed outcomes, and shuts down
  through public lifecycle APIs.

Blast-radius review:

- Existing same-shard call behavior stayed green.
- Existing cross-shard send behavior stayed green.
- Tokio comparison, TCP, persistence, storage, DNS, TLS-client, process, signal,
  and allocation tests stayed green under package test runs.
- The new remote envelope type is internal; public call syntax remains
  `call(address, msg, timeout).reply(...)`.

Hostile review:

- Full Victor is not closed yet. Inbound/server-side TLS is still not
  implemented.
- Graceful drain is still mostly the existing shutdown/drain wrapper plus
  terminal reporting. It is not yet a rich deadline-driven service mode with
  per-resource drain policy.
- Resource health counts are still partly summary-derived; owned-resource count
  is currently a placeholder, not a live driver inventory.
- Reply-path-full for destination-originated call failure return is not yet
  separately traced if the return queue is full while trying to report
  destination `Full`/`Closed`. The requester can still settle by timeout. This
  is bounded and visible, but not as sharp as success/full/closed happy
  return-path semantics.
- The live multi-shard implementation still uses bounded worker command queues
  as the physical remote transport. It reports per-pair metrics, but there is
  not yet a dedicated per-pair queue independent from target worker ingress.

Verification so far:

- `cargo +nightly check -p tina-runtime -p tina-sim`
- `cargo +nightly test -p tina-runtime`
- `cargo +nightly test -p tina-sim`
- `make verify`

Clippy finding fixed:

- Internal runtime/sim `dispatch_isolate_call` gained the routing closure needed
  for remote envelopes and tripped `too_many_arguments`. Kept the internal
  function shape and added a narrow allow on the dispatcher only.

# Implementation Review 2

Verdict: Victor's live cross-shard transport is now honest. Full Victor is
still not done because inbound/server-side TLS and deeper drain/resource
inventory remain open rocks.

Bug found and fixed:

- Live multi-shard remote transport reported per-pair capacity but physically
  rode the destination worker command queue. That mixed control ingress with
  remote transport and made `shard_pair_capacity` a soft claim. Fixed by adding
  real bounded source-shard -> destination-shard channels, wired from
  `ThreadedRuntimeConfig::shard_pair_capacity` and `LocalSystemConfig`.

Regression proof:

- `remote_queue_pressure_reports_capacity_and_full_counter` now sets roomy
  worker ingress and tiny shard-pair capacity, then parks the destination
  worker. It proves remote full comes from the real pair queue, not the command
  queue.
- `threaded_multishard_remote_queue_full_is_visible_at_source` now pins the
  same source-time full path on the lower-level threaded multi-shard runner.

Extra bug found and fixed:

- `dns_lane_resolves_with_injected_resolver` used a completion-observed side
  channel that fired before the worker necessarily enqueued the actual driver
  completion. Under load, the test could assert too early. Fixed by waiting on
  the real lane completion with a bounded deadline instead of a fixed small
  yield loop.

Positive review:

- Source/destination transport pressure is now physically separate from worker
  ingress pressure.
- Public topology reports now line up with the actual live queue being filled.
- Cross-shard call success/full/closed/timeout remains green after the transport
  split.
- The simulator/oracle and live runner both still agree on cross-shard call
  semantics under package tests.

Hostile review:

- Inbound/server-side TLS is still not implemented. Tina has outbound TLS
  client support and TLS simulation, but cannot yet host a native TLS listener
  inside the Tina resource model.
- Graceful drain is still first-slice: shutdown notification, bounded join, and
  terminal report exist, but drain policy is not yet a rich service state
  machine.
- `remaining_owned_resource_count` is still a placeholder in terminal reports.
  Trace-derived terminal accounting is real; live resource inventory is not yet
  complete.
- Destination-generated call failure return-path-full is still less sharp than
  ordinary reply-path-full. If the return transport itself is full while trying
  to report destination `Full`/`Closed`, the caller can still settle by timeout.

Verification:

- `cargo +nightly test -p tina-runtime --test local_system remote_queue_pressure_reports_capacity_and_full_counter -- --nocapture`
- `cargo +nightly test -p tina-runtime --test local_system live_cross_shard_isolate_call -- --nocapture`
- `cargo +nightly test -p tina-runtime --test betelgeuse_substrate threaded_multishard -- --nocapture`
- `cargo +nightly test -p tina-runtime -p tina-sim`
- `cargo fmt --all`
- `cargo +nightly clippy -p tina-runtime -p tina-sim --all-targets -- -D warnings`
- `make verify`

# Implementation Review 3

Verdict: the two remaining Victor review findings are fixed in the continuation
slice.

What landed:

- Native inbound TLS server rail: `TlsListenerId`, `tls_bind(...)`,
  `tls_accept(...)`, `tls_close_listener(...)`, plus existing
  `tls_read`/`tls_write`/`tls_close`.
- Live runtime TLS server e2e: bind with a static cert/key, accept a real rustls
  client, read `ping`, write `pong`, close stream, close listener, and assert
  the TLS call trace.
- Simulator TLS server oracle: bind/accept/close outcomes replay
  deterministically. The sim models TLS outcomes, not cryptography.
- Live resource inventory: terminal shutdown reports now sum live driver-owned
  resources instead of hardcoding zero.
- Resource inventory covers TCP listeners/streams, TLS listeners/streams, UDP
  sockets, files, and active pending driver calls. A held-file test pins live
  count and shutdown cleanup. The TLS server test pins live TLS listener count.

Important honesty:

- Victor's TLS server rail is a native TLS listener lane, not raw `StreamId` to
  `TlsStreamId` upgrade. Betelgeuse sockets do not expose the `Read`/`Write`
  stream rustls needs for that shape. Raw TCP-to-TLS owner transfer remains a
  later rock if the project wants it.
- The simulator TLS accept creates deterministic TLS stream outcomes. It is an
  oracle for Tina resource semantics, not a crypto test.

Bug found and fixed during review:

- Resource accounting initially summed TCP listeners but forgot TLS listeners.
  Fixed the aggregate and added a live TLS listener count assertion.

Verification:

- `cargo +nightly test -p tina-runtime --test local_system local_system_tls_server_accepts_reads_writes_and_closes -- --nocapture`
- `cargo +nightly test -p tina-runtime --test local_system local_system_reports_live_owned_resources_and_shutdown_cleanup -- --nocapture`
- `cargo +nightly test -p tina-runtime --test local_system -- --nocapture`
- `cargo +nightly test -p tina-sim --test io_simulation scripted_tls_server_bind_accept_close_and_replays -- --nocapture`
- `cargo +nightly test -p tina-sim --test io_simulation -- --nocapture`
- `cargo +nightly check --workspace`
- `cargo +nightly clippy -p tina-runtime -p tina-sim --all-targets -- -D warnings`
- `make verify`
