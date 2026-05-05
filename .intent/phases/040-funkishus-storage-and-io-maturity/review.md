# Phase 040 Plan Review: Funkishus Storage And I/O Maturity

## Verdict

Right next phase. Not yet ready to hand to implementation.

The plan aims at the correct production gap: Tina's local core is much closer
now, but the I/O/storage story is not yet broad or boring enough for a serious
Tokio-shaped service port. Funkishus should own that.

Hostile grug likes:

- It refuses remoting, clustering, durable mailboxes, release docs, hidden
  Tokio fallback, and broad performance claims.
- It keeps the Tina rule set central: visible overload, traceable failure,
  replayable races.
- It correctly treats adapter policy and capability reporting as core runtime
  work, not docs polish.
- It says new I/O rails must be born with direct tests and DST.
- It asks for one composed user-shaped service instead of many toy snippets.

But several load-bearing decisions are still too squishy. If implementation
starts from this plan as-is, a future grug can either build too much swamp or
close too little rock.

## Findings

### 1. [P1] Per-rail minimums are not pinned enough

Funkishus lists DNS, UDP, TLS, process, and signal, but several rails can close
as "typed unsupported" or "adapter only" while Done Means still says Tina
"covers the common resource families honestly." Honest unsupported is good,
but if most rails close that way, the phase did not materially move Tina toward
real local service readiness.

The plan should pin a minimum expected direction for each rail:

- storage: implement lane-backed live contract, no new full storage reactor;
- UDP: implement live loopback plus simulator;
- process: implement a narrow bounded `run_command` style rail;
- signal: implement simulator injection plus platform-aware live support or
  explicit unsupported where platform lacks it;
- TLS: likely adapter/unsupported only in 040, not native TLS;
- DNS: either bounded lane-backed resolver with honest already-started
  non-preemption, or explicitly punt with a clear reason.

Without this, "big I/O maturity phase" can become "capability table says no."

### 2. [P1] Storage reactor decision is left too open

The plan says "true nonblocking storage reactor" or "lane-backed blocking
storage" and asks implementation to decide. That repeats an old pattern: a
phase about completing a substrate story begins by re-deciding what substrate
story means.

Recommendation: pin 040 to **lane-backed blocking storage as the production
shape for now**, with no shard-worker disk blocking on canonical live paths.
Full platform async storage should be a later phase only if evidence proves the
lane-backed model is not good enough. This keeps 040 large but buildable.

### 3. [P1] Capability vocabulary mixes orthogonal concepts

`Supported`, `Unsupported`, `SimulatedOnly`, `Native`, `Blocking`,
`LaneBacked`, `Cancelable`, `TombstonedOnCancel`, `ShutdownDrained`, and
`CommitUncertainPossible` are not one enum. They are different axes.

This should become a structured report, roughly:

- support: `Supported | Unsupported | SimulatedOnly | AdapterOnly`;
- execution: `Inline | LaneBackedBlocking | CompletionBacked | ExternalAdapter`;
- cancellation: `CancelableBeforeStart | TombstonedAfterStart |
  ResourceCloseOnly | NotCancelable`;
- shutdown: `Drained | Canceled | Tombstoned | Unsupported`;
- durability: platform support fields for fsync/rename/commit-uncertain.

Otherwise review cannot tell whether "supported but blocking but tombstoned"
is representable without inventing weird enum combinations.

### 4. [P1] DNS cancellation semantics are risky

System DNS lookup is often blocking and not meaningfully cancelable once
started. The plan says live implementation may use system resolver if
bounded/cancel semantics are honest, but does not define honest enough.

Pin DNS to one of:

- bounded lane-backed blocking resolver where cancellation only prevents queued
  work and tombstones started work; or
- typed unsupported in live runtime, with simulator DNS support for semantics.

Do not let 040 claim "cancelable DNS" unless the implementation can actually
cancel the underlying operation.

### 5. [P1] Native TLS is a likely swamp

TLS pulls in certificates, async handshakes, stream wrapping, error mapping,
platform roots, and security posture. The plan's "native minimal handshake if
reasonable" is too inviting.

Recommendation: 040 should explicitly choose **TLS adapter rail or typed
unsupported**, not native TLS. Native TLS can be a later phase once TCP/UDP/DNS
and driver policy are boring. This keeps Tina honest and avoids a half-secure
TLS implementation.

### 6. [P2] Process rail needs stricter first-slice boundaries

Process support can explode: stdin piping, stdout streaming, stderr streaming,
environment, working directory, kill trees, Windows semantics, signals,
zombie reaping, output backpressure.

The plan should pin first slice:

- `run_command` / `spawn_wait` with bounded captured output;
- no interactive stdin streaming unless explicitly supported;
- output cap produces typed truncation/full outcome;
- timeout kill policy named;
- platform unsupported behavior tested.

Without that, process rail becomes tiny shell toy or too-large runtime inside
runtime.

### 7. [P2] Signal rail needs singleton/global-handler discipline

Signals are global process state. A sloppy implementation can install global
handlers that bypass Tina traces or conflict between tests.

The plan should require:

- one runtime-owned signal registry per app/runtime;
- bounded event delivery;
- unsubscribe on isolate stop;
- no raw global handler calling isolate code;
- simulator injection as the proof oracle;
- live support only where platform semantics are clear.

### 8. [P2] Composed e2e workload is ambitious but underspecified

The composed service lists TCP, DNS/UDP, state, storage, timer, cancellation,
overload, shutdown, topology. Good. But it does not say what user-visible state
proves success.

Pin expected assertions:

- accepted request mutates state only after durable append succeeds;
- rejected/timeout/canceled request does not mutate state;
- UDP/DNS failure is visible in reply and trace;
- overload returns typed `Full`/`StorageFull`;
- shutdown leaves no hidden pending work;
- restart/recovery rebuilds expected state from snapshot/journal.

### 9. [P2] DST requirements need minimum sweep/shrink bars

"Add generated histories" is good, but too easy to satisfy with tiny random
smoke tests.

Add minimum bars:

- at least one new resource model has deletion shrinking;
- at least one composed I/O history has fixed regression seeds plus randomized
  seed sweep;
- generated histories must force at least one success, one timeout/cancel, one
  full/closed, and one unsupported/failure path;
- every new rail has both direct negative tests and at least one DST model or
  explicit reason why DST does not apply.

### 10. [P2] Public boundary blast radius needs a named rule

The plan says pause if a public `tina` trait change is larger than runtime call
vocabulary, but Funkishus will probably add many `RuntimeCall` variants and
outcome types in `tina-runtime`.

Add a rule:

- `tina` trait crate should not grow resource-specific helpers;
- resource-specific helpers live in `tina-runtime` prelude;
- existing runtime-owned call shape remains the extension point;
- simulator mirrors runtime event/outcome vocabulary instead of inventing a
  second model.

This protects the crate-boundary rule in `.intent/SYSTEM.md`.

## Recommended Plan Changes

Before implementation, update `plan.md` to:

1. Pin 040's minimum resource outcomes:
   storage lane-backed live maturity, UDP live+sim, process narrow live+sim,
   signal sim+platform-aware live/unsupported, TLS adapter/unsupported, DNS
   bounded lane-backed or explicit unsupported.
2. Replace the flat capability vocabulary with structured capability axes.
3. Pin storage to lane-backed blocking as the 040 target, not full new storage
   reactor.
4. Make DNS cancellation semantics honest about queued-vs-started work.
5. Choose TLS adapter/unsupported for this phase.
6. Narrow process first slice.
7. Add global signal-handler discipline.
8. Add exact e2e assertions.
9. Add DST minimum bars.
10. Add public-boundary rule for `tina` versus `tina-runtime`.

## Positive Read

This is the right big branch. It moves Tina toward "Claude can port a serious
local Tokio-ish service and not lose the Tina guarantees" much more than
ergonomics work would.

The important instinct is correct: do not build remoting yet; do not publish
yet; do not polish examples yet. Make local runtime-owned I/O broad, bounded,
observable, and replay-pressured first.

## Hostile Read

The plan currently risks becoming a giant support matrix that says "unsupported"
in many places while still feeling like a win. That is honest but not enough.
Funkishus should force at least a few real rocks into existence: storage
maturity, UDP, process, and a composed app proof.

The other risk is semantic mush. If capability words are not structured, the
implementation can claim cancellation, support, backend kind, and shutdown
behavior in prose without making those dimensions testable.

Fix those two problems and grug says plan ready.

## Review Response

Plan updated after this review.

What changed:

- Added pinned minimum resource commitments.
- Pinned 040 storage to `LaneBackedBlockingStorage`, not a vague storage
  reactor decision.
- Replaced flat capability vocabulary with structured support/execution/
  cancellation/shutdown/durability axes.
- Made DNS queued-cancel versus started-tombstone semantics explicit.
- Removed native TLS from 040; TLS is adapter-only or typed unsupported.
- Narrowed process to bounded `run_command` / `spawn_wait`.
- Added signal registry/global-handler discipline.
- Added exact composed e2e assertions.
- Added DST minimum bars for shrinking, fixed seeds, randomized sweep, and
  forced negative paths.
- Added crate-boundary rule: resource helpers stay in `tina-runtime`, not
  `tina`.

Updated verdict: ready for implementation review after the usual initial audit.

## Second Hostile Review

### Verdict

Much better. The big escape hatches from the first review are closed.

Still not perfect. The remaining issues are smaller but real: they are the kind
that can turn "bounded I/O substrate" into "bounded except for the one worker
thread/pipe/process we forgot to think about."

### New Findings

#### 11. [P1] "No hidden background tasks" conflicts with lane-backed work

The plan correctly pins storage, DNS, and process toward bounded lane-backed
blocking work. But the design principles still say drivers may not smuggle
"hidden background tasks." A storage lane or process lane is, practically, a
background worker. The distinction is that it is named, bounded, capability-
reported, and shutdown/cancel-tested.

Plan should explicitly allow **named bounded driver lanes**:

- queue capacity is configured/reported;
- running work counts against capacity;
- lane lifecycle is tied to runtime lifecycle;
- shutdown cancels queued work and tombstones or drains started work according
  to the resource matrix;
- no lane may recursively enqueue into unbounded internal queues.

Without this, implementation can either violate the principle or avoid the
right lane-backed design.

#### 12. [P1] Process cancellation/output cap needs sharper safety contract

`run_command` with bounded stdout/stderr is deceptively hard. If Tina stops
reading after cap, the child can block on a full pipe. If Tina only tombstones
timeout instead of killing the child, the process can continue mutating the
outside world after Tina says timeout/cancel.

Plan should pin first-slice process semantics:

- timeout/cancel for a started child attempts kill and waits/reaps within a
  bounded policy;
- if kill/reap cannot be proven, outcome is `KillUncertain`/similar and is
  traced;
- stdout/stderr capture either drains and truncates, or explicitly rejects
  capture mode before spawn;
- no shell-by-default helper; command plus args is the safe path.

This is grug important. Process is where hidden side effects live.

#### 13. [P2] Composed e2e should force UDP and process, not only DNS/UDP

The composed service currently says "DNS or UDP" and does not require process.
Since DNS may honestly close as live unsupported, the main e2e can still avoid
the two concrete new rails that most prove 040 moved real rock.

Plan should require the composed service to use:

- TCP ingress;
- UDP live/sim rail;
- persistence/storage;
- timer/cancellation;
- one process call, likely a tiny bounded command;
- topology/capability report.

DNS and signal can have separate direct/DST tests if live support is
unsupported or platform-specific.

#### 14. [P2] Signal live support should default to simulator-first

The plan says platform-aware live support where honest. Good, but signal tests
can become flaky or global-state-hostile. Pin default shape:

- simulator signal injection is mandatory;
- live signal capability may be unsupported unless there is a deterministic,
  isolated test path;
- live tests must not send process-wide signals that can kill or affect the
  test runner;
- if using safe test-only signal like explicit injected runtime signal, name it
  as not OS-signal proof.

#### 15. [P2] Capability report access point should be pinned

"From the canonical app path" is close, but implementation could bury this on a
runtime type while `LocalSystem` users still cannot see it.

Pin required access:

- `LocalSystem::capabilities()` or equivalent;
- `LocalMultiShardSystem::capabilities()` or equivalent;
- terminal report preserves final capability/topology context if relevant;
- bridge exposes only bridge capability projection, not a second full runtime
  capability model.

### Second Review Recommendation

Update the plan for findings 11-15 before coding. None need human decision;
the obvious choices are:

- named bounded driver lanes are allowed and required where used;
- process cancel means kill/reap attempt, not only tombstone;
- composed e2e must include UDP and process;
- signal is simulator-first, live only if deterministic and safe;
- capability access is pinned on `LocalSystem`/`LocalMultiShardSystem`.

After those edits, grug says implementation can start.

## Second Review Response

Plan updated after the second hostile review.

What changed:

- Named bounded driver lanes are now explicitly allowed, with capacity,
  lifecycle, shutdown, and no-hidden-queue rules.
- Storage and any other lane-backed resource must count running accepted work
  against capacity and report capacity.
- Process first slice now uses command-plus-args, no shell-by-default,
  bounded output, no interactive stdin, and timeout/cancel kill-and-reap with
  a typed uncertain outcome if kill/reap cannot be proven.
- Signal is simulator-first. Live signal support only lands when deterministic
  and safe to test; process-wide dangerous signal tests are forbidden.
- Composed e2e must include UDP and one bounded process call, plus storage,
  timer/cancellation, overload, shutdown, and topology.
- Capability access is pinned on the canonical app path:
  `LocalSystem::capabilities()` / `LocalMultiShardSystem::capabilities()` or
  equivalent.

Updated verdict: ready to execute after the initial implementation audit.

## Implementation Audit 1

Funkishus starts from these exact live semantics after the naming polish commit
`84b805f`:

- timers are runtime-owned and inline in the driver; cancellation removes the
  pending timer by `CallId`;
- TCP bind/close are inline-safe; TCP accept/connect/read/write are
  Betelgeuse completion-backed with caller-owned completion slots;
- TCP read/write may be full-duplex, but duplicate work on the same lane
  returns `ResourceBusy`;
- local file open/close are inline-safe; file read/write/fsync/size and mkdir
  are Betelgeuse completion-backed;
- snapshot/journal work is already off the shard worker on live paths through
  a bounded storage lane;
- storage lane admission is bounded and returns `StorageFull`/`StorageClosed`;
- storage cancellation tombstones accepted work, skips canceled queued work,
  and swallows late completions;
- shutdown cancels timers, cancels/tombstones storage, calls Betelgeuse
  completion cancellation, closes TCP/file resources, and refuses clean
  shutdown if backend completion slots remain owned;
- `LocalSystem`/`LocalMultiShardSystem` expose topology, but before this
  implementation slice did not expose a structured resource capability table;
- DNS, UDP, TLS, process, and signal had no runtime call vocabulary yet.

First implementation decision:

- Add a structured capability table before adding new rails. This gives tests
  and users an honest answer for supported, unsupported, adapter-only,
  lane-backed, completion-backed, and tombstoned shapes. It also prevents later
  rails from closing on vague prose.

First slice implemented:

- `ResourceSupport`, `ResourceExecutionShape`, `CancellationSupport`,
  `ShutdownSupport`, `ResourceCapability`, `DurabilityCapability`, and
  `RuntimeCapabilities` landed in `tina-runtime`.
- `ThreadedRuntime`, `ThreadedMultiShardRuntime`, `LocalSystem`, and
  `LocalMultiShardSystem` now expose `capabilities()`.
- Current live capabilities say: timers supported inline; TCP and local files
  supported completion-backed; local persistence/storage supported as
  lane-backed blocking with visible capacity; DNS/UDP/process/signal
  unsupported for now; TLS adapter-only.

## Implementation Audit 2

UDP live rail landed in `tina-runtime`.

What is true now:

- `UdpBind`, `UdpSendTo`, `UdpRecvFrom`, and `UdpSocketClose` are runtime-owned
  calls with typed helper constructors in `tina-runtime`.
- Live UDP uses nonblocking `std::net::UdpSocket` behind the driver boundary.
- Duplicate pending recv on one socket returns `ResourceBusy`.
- Close while recv is pending returns `ResourceBusy`.
- Requester stop cancels/tombstones pending recv work through existing call
  ownership.
- Loopback e2e proves send/recv/truncation/close through `LocalSystem`.
- Capability reports now mark UDP supported on the local live runtime.

Review note:

- UDP protocol loss remains UDP protocol loss. Tina makes runtime-owned lane
  pressure and truncation visible; it does not pretend the OS can report remote
  UDP delivery.

## Implementation Audit 3

Scripted simulator UDP landed after live UDP.

What is true now:

- `SimulatorConfig` now has scripted UDP sockets and scripted inbound datagrams.
- Simulator `UdpBind`, `UdpSendTo`, `UdpRecvFrom`, and `UdpSocketClose` mirror
  the live call/output/trace vocabulary.
- Scripted UDP loopback is deterministic and replayed byte-for-byte.
- Delayed inbound datagrams wake pending recvs through explicit simulator
  steps.
- Duplicate recv, close-while-pending, requester-stop cancellation, and
  receive-queue-full are direct negative tests.
- UDP and TCP completion capacity accounting is lane-specific even though the
  simulator still harvests completions through one ordered internal queue.

Review note:

- The internal queue name still says `pending_tcp_completions`; that name is now
  historically stale because it also harvests UDP and file completions. It is a
  naming cleanup opportunity, not a semantic bug after the lane-specific
  capacity fix.

## Implementation Audit 4

DNS rail landed as call vocabulary plus simulator semantics, with live support
intentionally unsupported.

What is true now:

- `DnsLookup` and `dns_lookup(host, port, timeout)` exist in `tina-runtime`.
- Live `ThreadedRuntime` returns typed `Unsupported` for DNS instead of using a
  hidden system resolver worker that cannot be honestly canceled once started.
- `LocalSystem::capabilities()` continues to report live DNS as unsupported.
- `tina-sim` can script DNS success, failure, timeout, and lane-full pressure.
- Simulator DNS replays byte-for-byte and uses the same `CallKind::DnsLookup`,
  `CallOutput::DnsResolved`, and `CallError` vocabulary as the live rail.

Review note:

- This is a deliberate Tina-honesty choice. A future live DNS phase can add a
  bounded resolver adapter only if shutdown/cancel can be named without hiding
  an unbounded or unjoinable worker behind Tina.

## Implementation Audit 5

Narrow process rail landed with live and simulator coverage.

What is true now:

- `ProcessRun`, `ProcessStatus`, `ProcessRunResult`, and
  `process_run(command, args, timeout, stdout_limit, stderr_limit)` exist in
  `tina-runtime`.
- The live process rail uses a named bounded lane, not a shell helper and not
  an unbounded runtime fallback.
- Live process execution uses command-plus-args, null stdin, bounded
  stdout/stderr capture, drain-and-truncate pipe handling, timeout kill/reap,
  and requester-stop tombstoning through call ownership.
- `LocalSystem::capabilities()` now reports process support as
  `LaneBackedBlocking` with bounded capacity.
- `tina-sim` can script process exit, timeout, I/O failure, kill-uncertain, and
  lane-full pressure with the same call/output/error vocabulary.

Review note:

- This is intentionally not interactive process I/O. No stdin streaming, no
  shell-by-default helper, no process tree claim, and no broad cross-platform
  signal/kill story beyond the typed `KillUncertain` escape hatch.

## Implementation Audit 6

Composed local I/O proof landed.

What is true now:

- A single live `LocalSystem` service performs UDP loopback, bounded process
  execution, and journal append in one user-shaped state machine.
- The service mutates durable state only after UDP receive and process output
  both succeed.
- The test replays the resulting journal from disk and asserts the exact
  committed bytes.
- Trace assertions prove `UdpBind`, `UdpSendTo`, `UdpRecvFrom`, `ProcessRun`,
  and `JournalAppend` all completed as runtime-owned Tina calls.

Review note:

- This does not yet include TCP ingress in the same single service; separate
  live tests already cover TCP-plus-journal and cross-shard TCP persistence.
  The remaining full-composition gap is to combine TCP ingress with the new
  UDP/process rails if 040 needs one maximal app-shaped proof.
- Direct integration test pins supported and unsupported resource families
  through the canonical `LocalSystem` path.

Targeted proof:

- `cargo +nightly check -p tina-runtime`
- `cargo +nightly test -p tina-runtime --test local_system local_system_capabilities_name_supported_and_unsupported_resource_families`

## Implementation Audit 2

Betelgeuse does not expose UDP, DNS, process, or signal primitives. Its README
and `lib.rs` confirm the current backend scope is completion-shaped TCP/files,
with no runtime and no hidden tasks. That means Funkishus must not pretend UDP
is "Betelgeuse-backed" today.

UDP first slice implemented with a Tina-owned nonblocking driver rail:

- `UdpSocketId` is runtime-owned, like `ListenerId` and `StreamId`;
- `udp_bind`, `udp_send_to`, `udp_recv_from`, and `udp_close_socket` are typed
  helpers in `tina-runtime`;
- live UDP sockets are `std::net::UdpSocket` resources owned inside the
  driver, set nonblocking, and polled only from driver `advance`;
- `UdpRecvFrom` occupies a per-socket pending receive lane and duplicate
  receives return `ResourceBusy`;
- requester stop/cancel removes pending UDP receive responsibility without
  closing unrelated resources;
- close while a live recv lane is active returns `ResourceBusy`;
- datagram truncation is visible as a boolean in `CallOutput::UdpReceived`;
- `RuntimeCapabilities` now reports UDP as `Supported` with
  `PollBacked` execution, not Betelgeuse completion-backed;
- `tina-sim` currently returns typed `Unsupported` for UDP until the simulator
  UDP rail lands later in this phase.

Targeted proof:

- `cargo +nightly check -p tina-runtime -p tina-sim -p tina-tokio-bridge`
- `cargo +nightly test -p tina-runtime --test local_system local_system_udp_loopback_surfaces_send_recv_truncation_and_close`
- `cargo +nightly test -p tina-runtime driver::tests::udp_recv_lane_rejects_duplicate_and_close_until_cancelled`

Open next rock:

- add simulator UDP packet scripting so live-vs-sim does not diverge on the
  final Funkishus contract;
- add UDP requester-stop-before-packet proof from the user-facing path.
