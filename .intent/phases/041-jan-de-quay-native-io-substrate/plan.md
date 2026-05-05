# Phase 041: Jan de Quay Native I/O Substrate

## Goal

Finish Tina's local runtime I/O substrate story so a serious local service can
use Tina-owned effects for the common I/O families it would otherwise reach for
Tokio to handle.

Funkishus made the broad rail surface honest: live UDP and process exist,
storage is lane-backed, DNS and signals are simulator-first with typed live
unsupported, and TLS is adapter-only. Jan de Quay is the follow-through phase:
turn the remaining local I/O non-claims into native, bounded, traceable runtime
semantics where that can be done without weakening Tina.

At closeout, Tina should be able to say:

> A local Tina app can use runtime-owned TCP, UDP, DNS, TLS, files,
> persistence, process execution, timers, and shutdown signals through bounded
> driver rails with typed overload/failure/cancel outcomes, simulator parity
> where simulation is meaningful, and DST pressure over weird combinations.

This is production-core work. It is not a release/docs/demo phase.

## Why Now

The current roadmap says the same thing in several places: Tina is close on
local runtime shape, but still has missing local I/O rocks.

Current shipped shape:

- live time, TCP, file, persistence, UDP, and bounded process execution;
- simulator TCP, file, persistence, UDP, DNS, process, and signal scripts;
- live DNS and live OS signals report typed unsupported outcomes;
- native TLS is adapter-only, not a runtime-owned TLS rail;
- runtime capability reports are the public truth surface.

That is good grug. But if someone ports a real Tokio-ish local service, the next
holes are obvious:

- DNS resolution before outbound connections;
- TLS handshake and encrypted read/write;
- filesystem operations beyond simple read/write;
- live shutdown signal handling;
- mature cancellation/shutdown/reporting across those resource families.

## Non-Goals

These are important, but not Jan de Quay:

- no remoting between Tina runtimes;
- no clustering, membership, or placement;
- no durable mailbox;
- no database;
- no general-purpose async runtime;
- no claim that Tower/Axum middleware runs inside Tina isolates;
- no public release/Gemini story;
- no broad "faster than Tokio" claim;
- no flow macro implementation;
- no hidden mpsc fallback, hidden unbounded resolver queue, or silent adapter
  path.

## Phase Size

This is intentionally a big local-substrate phase.

The phase is one rock because DNS, TLS, richer file operations, and live
shutdown signals all touch the same contracts:

- `CallInput` / `CallOutput` / `CallError`;
- `RuntimeCapabilities`;
- driver ownership, cancellation, and shutdown;
- simulator scripts and replay;
- LocalSystem/bridge user-shaped e2e tests;
- DST resource histories.

If implementation discovers that native TLS or live signal handling requires a
separate unsafe/platform design, pause and split that part. Do not paper over
the split with `Unsupported` plus "done."

## Design Principles

1. **No fallback fog.**
   Unsupported means unsupported. Adapter-only means adapter-only. A runtime
   rail must either implement bounded Tina semantics or return a typed outcome
   that says exactly why it cannot.

2. **Every queue has a visible bound.**
   DNS resolver work, TLS handshakes, file operations, signal delivery, process
   execution, bridge ingress, and cross-shard transport must never hide pressure
   in an unbounded internal queue.

3. **Cancellation belongs to the requester.**
   If an isolate stops, its pending runtime-owned work is canceled or tombstoned.
   Canceling one call must not close another isolate's unrelated resource unless
   the ownership rule says the resource is exclusive and tests prove it.

4. **Started blocking work is honest.**
   If the selected live implementation cannot preempt already-started DNS,
   filesystem, or TLS work, that non-preemption is reported and tested. Queued
   work must still be cancelable before it starts.

5. **TLS is not raw TCP cosplay.**
   A TLS rail must perform real TLS semantics or stay adapter-only/unsupported.
   The capability report must not imply encrypted transport unless encrypted
   transport is actually used.

6. **Signals enter Tina as messages.**
   No global handler may mutate runtime state behind Tina's back. Live signal
   support must use a bounded runtime-owned delivery path and must be safe under
   tests. If a platform cannot support that safely, it reports unsupported.

7. **Simulator parity is semantic, not fake OS cosplay.**
   Simulator DNS/TLS/signal/file results should model Tina outcomes and ordering,
   not pretend to reproduce operating-system internals.

8. **DST is born with the feature.**
   Direct tests pin positive, negative, and edge cases. DST then combines rails
   in weird orders to find interactions direct tests did not think about.

## Resource Commitments

| Resource family | 041 target |
|---|---|
| DNS | Implement a live bounded resolver rail using a named bounded blocking lane unless a better substrate is already obvious. Started OS resolver calls may be non-preemptable, but queued work must be cancelable, timed-out callers must be tombstoned, and capability reports must say started DNS work is not preempted. Simulator already has scripts; extend as needed for live-vs-sim projection. No hidden unbounded resolver worker queue. |
| TLS | Implement the smallest real TLS rail using `rustls` unless audit finds a better maintained fit. Scope is a Tina-owned TLS stream resource handle, client/server loopback handshake, TLS read/write/close, typed cert/name/handshake/IO errors, bounded admission, timeout/cancel/shutdown, and simulator scripts. No ALPN, HTTP, cert store policy framework, async ecosystem integration, or middleware story. If this still wants a larger resource model, pause and split; do not close as "adapter-only done." |
| Filesystem | Add richer runtime-owned local filesystem operations needed by real services: metadata, exists/stat shape, rename/replace, unlink/remove, mkdir, read_dir/list, and parent-directory sync where supported. Platform support differences must be typed. |
| Signals/shutdown | Add runtime-owned shutdown notification as the minimum real rail. Add raw OS shutdown-signal capture only where safe and testable, and report it separately from runtime-injected shutdown. Expected raw OS scope is shutdown-oriented signals only (`SIGINT`/`SIGTERM` on Unix if implemented safely, Ctrl-C style support where portable). Simulator signal scripts remain the oracle for deterministic histories. No arbitrary process signal framework. |
| Driver/capabilities | Extend capability reports so every resource says native/lane-backed/adapter-only/simulated/unsupported, queue capacity, cancellation support, and started-work preemption support. |
| E2E/DST | Add one composed local app that uses DNS -> TLS/TCP or UDP -> persistence/file -> process/signal/shutdown semantics, with overload/cancel/failure tests and generated DST histories. |

## Pinned Resource Shapes

These shapes are plan-time contracts. Implementation can discover better names,
but not weaker semantics without stopping.

### DNS

Expected live DNS shape:

- bounded blocking resolver lane;
- configured queue capacity and started-worker capacity;
- queued work can be canceled before it starts;
- started resolver work may be non-preemptable;
- a timed-out started lookup tombstones the requester reply but keeps occupying
  a started-work slot until the resolver returns;
- capability/topology reports expose started DNS work and say it is not
  preemptable;
- shutdown cancels queued DNS work and tombstones started DNS completions
  without waiting forever for the OS resolver;
- tests use an injected/fake resolver lane to force success, failure, timeout,
  full, cancel, and never-completing started work.

### TLS

Expected live TLS shape:

- introduce a Tina-owned `TlsStream`/`TlsStreamId`-style handle instead of
  treating TLS as raw `TcpStreamId`;
- first implementation may create TCP plus TLS together through
  `tls_connect(...)`, or wrap an existing Tina TCP stream only if ownership
  stays clear;
- client-side TLS is mandatory; server-side TLS is required only for local
  loopback tests unless the audit proves it is cheap and clean to expose;
- at most one pending read and one pending write may exist per TLS stream only
  if the resource owner/lane model proves it safe; otherwise enforce one pending
  TLS operation per stream and test rejection;
- canceling one TLS operation must not close unrelated live work owned by
  another requester unless the TLS stream is explicitly exclusive to one owner;
- `tls_close`/close-notify has a typed outcome; half-close is unsupported unless
  implemented and tested deliberately;
- typed errors distinguish certificate/name failure, handshake failure, timeout,
  peer close/truncation, I/O failure, full, closed, and unsupported.

### Signals And Shutdown

Signal vocabulary is split:

- **Runtime shutdown notification:** deterministic Tina-owned shutdown event
  rail. This must land in 041.
- **Raw OS signal capture:** optional platform support. It must be reported
  separately as native or unsupported.

Tests must cover deterministic runtime-injected shutdown even if raw OS signal
capture remains unsupported on the live runtime.

### Filesystem Platform Support

Before adding richer filesystem ops, implementation must pin a support table:

- Unix existing-target rename replacement supported or tested;
- Windows existing-target replacement either uses a correct replacement
  primitive or reports unsupported;
- directory fsync support is platform-scoped;
- permission failures are best-effort tests, not portable proof requirements;
- every operation has exact `Unsupported`, `Uncertain`, or `Io` outcome rules.

## Expected User Shape

Names may change when code teaches grug better words, but the app shape should
stay ordinary Tina:

```rust
use tina::prelude::*;
use tina_runtime::prelude::*;

#[tina_runtime::isolate(message = FeedMsg, reply = FeedReply, shard = AppShard)]
impl LlamaFeed {
    fn handle(&mut self, msg: FeedMsg, ctx: &mut Context<Self>) -> Effect<Self> {
        match msg {
            FeedMsg::Start(host) => {
                dns_lookup(host, Duration::from_millis(200))
                    .reply(FeedMsg::Resolved)
            }
            FeedMsg::Resolved(Ok(addrs)) => {
                tls_connect(addrs[0], "hay.example", Duration::from_secs(1))
                    .reply(FeedMsg::Connected)
            }
            FeedMsg::Connected(Ok(tls)) => {
                tls_write(tls, b"GET /feed HTTP/1.1\r\n\r\n".to_vec())
                    .reply(FeedMsg::Written)
            }
            FeedMsg::Written(Ok(tls)) => {
                tls_read(tls, 16 * 1024).reply(FeedMsg::Read)
            }
            FeedMsg::Read(Ok(bytes)) => {
                journal_append(self.journal.clone(), self.next_index, bytes)
                    .reply(FeedMsg::Stored)
            }
            FeedMsg::ShutdownSignal(_) => stop(),
            FeedMsg::Resolved(Err(err))
            | FeedMsg::Connected(Err(err))
            | FeedMsg::Written(Err(err))
            | FeedMsg::Read(Err(err))
            | FeedMsg::Stored(Err(err)) => reply(FeedReply::Failed(err.into())),
        }
    }
}
```

This is still Tina. Suspension points are named messages. Failure is visible.
No `await` fog.

## Build Steps

### 1. Audit Current I/O Rail Contracts

Inventory current runtime and simulator rail behavior:

- call vocabulary and helper names;
- driver queues and lane capacities;
- cancellation behavior before start and after start;
- shutdown behavior with pending work;
- capability reports;
- LocalSystem and bridge exposure;
- DST resource histories.

Write the findings in `review.md`. No separate audit file.

Also pin before implementation:

- dependency choices and reasons (`rustls`, signal crates, cert generation, and
  any dev-only helpers);
- feature flags if a dependency should not be mandatory for all consumers;
- no broad workspace dependency blast unless the review explains why;
- public API blast-radius table for `CallInput`, `CallOutput`, `CallError`,
  helpers, traces, capabilities, simulator configs, LocalSystem, and bridge
  surfaces;
- one public helper vocabulary per rail.

Done means the phase starts from exact behavior, dependency choices, and public
symbol blast radius, not memory of old rocks.

### 2. Pin Capability Vocabulary

Extend or clarify public capability report vocabulary:

- `Native`;
- `LaneBacked`;
- `AdapterOnly`;
- `SimulatedOnly`;
- `Unsupported`;
- queue capacity;
- cancellation support;
- started-work preemption support;
- platform support notes.

Capability reports must be testable from the canonical `LocalSystem` path.

### 2.5. Enforce Implementation Order

Implement in this order, with a code-bug review after each resource family:

1. audit/capability vocabulary/dependency/public API blast radius;
2. DNS;
3. TLS;
4. richer filesystem;
5. runtime shutdown notification/raw OS signal capability;
6. composed e2e workloads;
7. DST and performance guardrails;
8. full positive/blast-radius/hostile phase review and fixes.

Do not half-land multiple resource families before reviewing the previous one.
This phase is big; order keeps grug from making soup.

### 3. Add Native DNS Rail

Add runtime-owned DNS lookup:

- typed helper, call input/output/error, trace events;
- bounded admission/capacity;
- timeout and cancellation;
- queued-work cancel before start;
- started-work tombstone if preemption is impossible;
- capability report that distinguishes queued cancellation from started
  non-preemption;
- live tests for success, failure, timeout, full, cancel, shutdown;
- simulator projection parity.

Expected first implementation: bounded blocking DNS lane over standard resolver
behavior. If that cannot be made honest, pause and split. Do not hide DNS behind
an unbounded resolver thread.

Test setup must not depend on flaky external DNS. Direct/e2e tests should use a
deterministic injected resolver or static local mapping. Real resolver smoke is
allowed only as a narrow optional test and must not carry semantic proof.

### 4. Add Native TLS Rail Or Split Explicitly

Design and implement the smallest real TLS rail:

- rustls-backed unless audit says otherwise;
- local test certificate generation for tests;
- runtime-owned TLS connect/handshake over a Tina TCP stream;
- TLS read/write/close helpers;
- typed certificate/DNS/handshake/IO errors;
- timeout/cancel/shutdown semantics;
- bounded handshake/read/write admission;
- simulator TLS scripts for success/failure/timeout/truncation;
- direct live loopback tests with local test certificates.

Keep TLS deliberately small: no ALPN, no HTTP, no middleware, no broad platform
certificate-store story, and no remote internet tests.

TLS tests must be local and deterministic:

- deterministic local certificate/key fixtures or generation;
- no system certificate store dependence;
- no external hostnames;
- explicit loopback binding with deterministic IPv4/IPv6 handling;
- deterministic resolver/static mapping for DNS plus TLS e2e.

Simulator TLS scripts model Tina outcomes, not cryptography:

- handshake success/failure/timeout;
- peer-name/certificate failure as scripted semantic outcome;
- read/write as logical bytes;
- close/truncation/EOF;
- replay identity and causal trace shape.

Pause gate: if TLS requires a larger resource model than this phase can do
well, split TLS into its own phase before touching remoting or release work.

### 5. Add Richer Filesystem Rail

Add runtime-owned file/directory operations:

- metadata/stat;
- exists/not-found shape;
- rename/replace;
- unlink/remove file;
- mkdir/create dir;
- read_dir/list;
- parent-directory fsync when supported;
- typed unsupported/uncertain platform outcomes.

Tests must include current-directory paths, relative paths, existing-target
rename, missing paths, permission-ish failures where portable, cancellation,
and shutdown with queued and started work.

Support-table tests must prove the capability report matches the platform:
rename replacement, directory fsync, parent-directory sync, and unsupported or
uncertain outcomes must be visible and deterministic.

### 6. Add Live Shutdown Signal Rail

Add runtime-owned shutdown signal handling:

- safe platform-supported shutdown signals where available;
- explicit runtime-injected shutdown source for deterministic tests;
- bounded signal delivery;
- unsubscribe/stop behavior;
- signal capability report;
- simulator script parity;
- direct tests that do not kill or perturb the test process.

If real OS signals are too process-global for reliable tests, implement a
runtime-owned shutdown notification rail now and leave raw OS signal capture as
explicit unsupported with the exact reason.

### 7. Compose Local User Workloads

Add at least two user-shaped e2e workloads:

1. DNS plus TLS/TCP plus persistence/file flow with timeout and cancellation.
2. Shutdown-signal plus process/file cleanup flow with overload and stop.

These must run through `LocalSystem` or the canonical live app surface, not
private driver calls.

Each workload needs:

- successful path;
- resource full path;
- timeout path;
- requester stopped path;
- shutdown with pending work;
- trace/capability assertions.

The bridge-facing proof must use DNS plus TLS if TLS lands in this phase. If
TLS splits, the bridge proof must use DNS plus file/persistence and record the
TLS split explicitly in `review.md`.

Each new resource family must have at least one user-shaped e2e or integration
proof for major negative outcomes:

- DNS full, timeout, failure, and cancel;
- TLS handshake failure, timeout, read/write failure, and cancel;
- filesystem unsupported/missing/uncertain and cancel;
- signal full, closed, unsubscribe, and unsupported/platform-supported shape;
- composed shutdown with multiple pending resource families.

### 8. Extend DST Resource Histories

Extend the DST harness to generate weird combined histories over:

- DNS success/failure/timeout/full/cancel;
- TLS handshake/read/write success/failure/timeout/full/cancel;
- file metadata/rename/unlink/mkdir/read_dir success/failure/unsupported;
- signal/shutdown delivery/drop/stop;
- process and UDP interactions from Funkishus;
- persistence recovery around file operations.

DST should assert:

- replay stability;
- no hidden pending work after quiescence/shutdown;
- accepted work has a terminal outcome;
- canceled work never mutates stopped requesters;
- capability unsupported paths are stable and typed;
- shrinking produces useful smaller histories.

### 9. Add Performance/Allocation Guardrails

Do not claim broad throughput. Do add narrow guardrails:

- per-resource queue capacity pressure tests;
- allocation/cost probes for DNS, TLS handshake, file metadata/listing, signal
  delivery, and composed workload;
- comparison to existing narrow cost model where useful;
- explicit notes for any unavoidable allocation.

Measurement mode:

- committed tests may assert bounded counters and no-regression shape, not
  noisy wall-clock performance;
- allocation probes follow the existing runtime allocation-probe pattern in
  debug profile unless the audit says the current pattern is unsuitable;
- wall-clock numbers, if collected, are `review.md` evidence only and not a
  correctness gate;
- exact hot paths: DNS lookup enqueue/complete, TLS handshake, TLS read/write,
  file metadata/read_dir, runtime shutdown notification delivery, and the
  composed e2e workload.

### 10. Hostile Review And Fix

Before closeout, do a three-part review:

- positive review: what user capability genuinely improved;
- blast-radius review: what public API, capability reports, traces, and tests
  changed;
- final public-symbol list for every new or changed public item;
- hostile review: bugs, halfwork, hidden queues, unsupported lies, cancellation
  holes, platform lies, DST gaps, and ergonomics weirdness.

Fix findings before closeout unless a finding requires a human decision.

## Required Tests

Direct tests:

- DNS success, NXDOMAIN-ish failure, timeout, full, cancel before start, stop
  while pending, shutdown while pending, and never-completing started work via
  injected resolver;
- TLS handshake success/failure, invalid cert/name, timeout, read/write close,
  cancel before start, stop while pending, shutdown while pending, stream
  ownership rejection, and close-notify/peer-close behavior;
- filesystem metadata, missing path, rename/replace, unlink, mkdir, read_dir,
  parent sync support, relative/current-directory paths, queued cancel, started
  tombstone;
- signal/shutdown delivery, unsubscribe/stop, full, closed, simulator replay,
  deterministic runtime-injected shutdown, live unsupported/platform-supported
  raw OS signal shape;
- capability reports for every resource family.

E2E tests:

- LocalSystem service using DNS plus TLS/TCP plus persistence/file;
- LocalSystem service using signal/shutdown plus process/file cleanup;
- bridge-facing call into a service that uses DNS plus TLS, or DNS plus
  file/persistence if TLS is explicitly split;
- multi-shard local service where one shard performs resource work and another
  observes typed outcomes.

DST tests:

- generated resource histories across DNS/TLS/file/signal/process/UDP;
- delete-shrunk failure cases stay reproducible;
- live-vs-sim semantic projection for supported rails;
- unsupported rails produce stable typed outcomes, not panics.

## Done Means

- Native DNS is implemented with bounded semantics, or the phase pauses and
  splits rather than pretending unsupported is completion.
- Native TLS is implemented with real TLS semantics, or the phase pauses and
  splits rather than pretending adapter-only is native TLS.
- Richer filesystem operations are runtime-owned, typed, capability-reported,
  and tested.
- Live shutdown signal/shutdown notification semantics are runtime-owned,
  bounded, and tested.
- Capability reports explain every I/O family honestly.
- E2E tests prove user-shaped workloads through the canonical app surface.
- DST combines the new rails with old rails and pressures weird histories.
- `make verify` passes.
- `SYSTEM.md`, `ROADMAP.md`, and `CHANGELOG.md` are updated with only the
  semantics that actually landed.

## Pause Gates

Pause for human input if:

- native TLS wants a large new resource model;
- live DNS wants an unbounded or externally hidden resolver queue;
- OS signal support wants unsafe/global behavior that tests cannot isolate;
- filesystem support differs by platform in a way that changes the public
  contract;
- cancellation of one call would close another live owner's resource;
- the phase starts becoming remoting, clustering, release engineering, or flow
  ergonomics.

## Parallel-Safe Work

These can run beside Jan de Quay if another grug/session wants useful work
without stepping on the main driver contract:

- CI matrix planning and dry-run workflow polishing, as long as it does not
  change runtime semantics;
- performance report formatting around already-measured probes;
- README language cleanup that does not add new claims;
- external review prompts for 040/041;
- research notes on future remoting, clustering, and runtime substrates;
- Barend flow syntax design notes only, no macro implementation until local I/O
  semantics are settled.

These should not run in parallel:

- changes to `CallInput`/`CallOutput`/`CallError`;
- driver queue/cancellation/shutdown semantics;
- `RuntimeCapabilities`;
- simulator resource semantics;
- DST resource-history core.

## Non-Claims After This Phase

Even if Jan de Quay succeeds:

- Tina is still not a distributed runtime;
- Tina still does not claim clustering/membership/placement;
- Tina still does not claim durable mailboxes or exactly-once delivery;
- Tina still does not claim broad performance superiority over Tokio;
- Tower/Axum middleware still lives outside Tina unless a later adapter phase
  changes that deliberately;
- Barend-style flow ergonomics remain optional syntax over already-tested Tina
  semantics.
