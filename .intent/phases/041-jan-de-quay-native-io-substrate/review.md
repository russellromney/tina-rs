# Jan de Quay Hostile Plan Review

## Verdict

Good direction, not yet ready to execute.

The phase names the right next production-core rock: local I/O substrate
completion before Barend/Gemini/remoting. But several parts still let
implementation close on vibes instead of contracts. The dangerous areas are
native TLS shape, DNS timeout/cancel honesty, signal scope, filesystem
platform semantics, and test determinism.

## P1 Findings

### 1. Native TLS has no pinned resource model

The plan says "runtime-owned TLS connect/handshake over a Tina TCP stream" and
"TLS read/write/close helpers", but it does not pin the actual handle/resource
shape. This matters because TLS is not just a call result; it is a stateful
protocol object with handshake state, encrypted read/write buffers, close-notify
state, peer identity, and ownership rules.

Before implementation, pin expected shape:

- likely a `TlsStream`/`TlsStreamId` resource handle, not raw `TcpStreamId`;
- whether TLS wraps an existing Tina TCP stream or creates TCP+TLS together;
- whether read and write may be pending concurrently on one TLS stream;
- what happens if requester A cancels a TLS read while requester B has a TLS
  write pending;
- how close-notify and half-close are represented;
- whether server-side TLS is in scope or only client-side TLS loopback.

Without this, TLS can quietly become a pile of special cases in `driver.rs`.

### 2. DNS timeout/cancel semantics can still leak stuck lane workers

The plan correctly says standard resolver calls may be non-preemptable and
timed-out callers should be tombstoned. But if a started DNS call never returns,
the bounded DNS lane can lose a worker forever. That is still visible overload,
but the plan does not require a terminal report, shutdown behavior, or capacity
accounting for wedged started work.

Pin the rule:

- queued DNS can be canceled;
- started DNS may not be preempted;
- a timed-out started DNS keeps occupying a started-work slot until it returns;
- capability/topology reports expose `started_nonpreemptable`/busy slots;
- shutdown does not wait forever for non-preemptable resolver work unless the
  API explicitly says it does;
- tests use an injected/fake resolver lane to force a never-completing DNS call
  without relying on real network weirdness.

Otherwise "timeout" can look safer than it is.

### 3. Live OS signal handling is still a slippery closeout path

The plan says live shutdown signals if safe, else runtime-owned shutdown
notification plus raw OS signal unsupported. That is honest, but it lets the
phase close while still sounding like "live OS signal support" landed.

Pin expected direction:

- 041 must at least ship runtime-owned shutdown notification as a real rail;
- raw OS signal capture is either implemented with a named dependency and
  bounded delivery contract, or explicitly remains unsupported;
- the capability report must distinguish `RuntimeShutdownNotification` from
  `OsSignalCapture`;
- tests must cover both the deterministic injected shutdown path and the
  unsupported/platform-supported OS signal capability path.

No process-global handler haze.

## P2 Findings

### 4. TLS test plan risks flaky internet/cert/DNS behavior

The plan says no remote internet tests, good. But DNS plus TLS e2e can still
accidentally depend on host resolver behavior, `localhost` quirks, IPv4/IPv6
ordering, or certificate-name mismatch.

Pin the test setup:

- use an injected deterministic resolver or static local mapping for e2e;
- generate local certs deterministically in test code or fixtures;
- bind local loopback explicitly and handle IPv4/IPv6 deterministically;
- no dependence on system certificate store;
- no external hostnames.

Grug want tests that fail because Tina wrong, not because laptop DNS feels
spicy.

### 5. Filesystem platform contract is too broad

"rename/replace", "parent-directory fsync", and "permission-ish failures" are
all platform-shaped. Previous phases already found commit-uncertain and
rename-replacement traps. 041 should not reopen that fog.

Pin a support table before adding ops:

- Unix replace rename supported or tested;
- Windows existing-target replacement either uses a correct replacement
  primitive or reports unsupported;
- directory fsync support is platform-scoped;
- permission failures are best-effort tests, not required portable proof;
- every op has exact `Unsupported`, `Uncertain`, or `Io` outcome rules.

### 6. TLS simulator parity is underspecified

Simulator TLS should not pretend to do cryptography, but the plan only says
"simulator TLS scripts". It needs to say what is simulated:

- handshake success/failure/timeout;
- peer-name/cert failure as scripted semantic outcome;
- encrypted read/write as logical bytes, not crypto;
- close/truncation/EOF;
- replay identity and causal trace shape.

This keeps sim honest without turning it into fake rustls.

### 7. E2E workload requirement should require negative paths per resource, not only per workload

The e2e section says each workload needs full/timeout/stopped/shutdown paths.
Good, but a workload with DNS full and TLS happy could still leave TLS full
untested through user shape.

Require at least one user-shaped e2e or integration proof for each new resource
family's major negative outcomes:

- DNS full/timeout/failure/cancel;
- TLS handshake failure/timeout/read-write failure/cancel;
- filesystem unsupported/missing/uncertain/cancel;
- signal full/closed/unsubscribe/unsupported;
- composed shutdown with multiple pending resource families.

### 8. Bridge-facing proof may become too small

"Bridge-facing call into a service that uses at least one new runtime rail" is
easy to satisfy with the least scary rail. For production-shaped usefulness,
pick the target now.

Recommendation: bridge-facing proof should use TLS or DNS plus persistence/file,
because that is the shape a Tokio/Tower caller actually cares about. If TLS
splits, bridge proof should use DNS plus file/persistence and explicitly record
TLS split.

### 9. Performance guardrails need exact measurement mode

The plan says allocation/cost probes, but not profile/tooling. Past phases used
narrow allocation probes and cost models; keep same discipline.

Pin:

- debug or release profile;
- wall-clock or allocation count or both;
- no performance claim from noisy CI;
- exact hot paths to measure;
- whether results land in review only or committed tests.

## What Is Strong

- Correct next phase. This is the right thing before flow syntax or public
  launch story.
- Explicit no-fallback posture is good.
- The plan refuses remoting/clustering/release creep.
- Direct tests plus DST is the right proof shape.
- Capability reports as public truth surface is exactly Tina-ish.

## Required Plan Fixes Before Execution

1. Pin the TLS resource model and minimum client/server scope.
2. Pin DNS non-preemptable started-work reporting and shutdown behavior.
3. Split signal vocabulary into runtime shutdown notification vs raw OS signal
   capture.
4. Pin deterministic local DNS/TLS test setup.
5. Add filesystem platform support table requirements.
6. Define simulator TLS semantics explicitly.
7. Strengthen e2e negative-path requirements per resource family.
8. Pick the bridge-facing proof target.
9. Pin performance probe method.

No human product decision seems needed. These are engineering-shape fixes.

## Review Response

Plan updated.

- TLS now has a pinned `TlsStream`/`TlsStreamId`-style resource model, ownership
  rules, close-notify stance, client/server loopback scope, and typed error
  vocabulary.
- DNS now explicitly models queued cancellation, started non-preemption,
  tombstoned timeout, busy-slot reporting, non-waiting shutdown, and injected
  never-completing resolver tests.
- Signal support is split into required runtime shutdown notification and
  optional raw OS signal capture with separate capability reporting.
- DNS/TLS tests are pinned to deterministic local resolver/cert/loopback setup.
- Filesystem support-table requirements are pinned before richer ops.
- Simulator TLS semantics are logical outcome scripts, not fake cryptography.
- E2E negative-path coverage is required per resource family.
- Bridge proof target is DNS plus TLS, or DNS plus file/persistence if TLS is
  explicitly split.
- Performance probes are scoped to bounded counters/allocation shape in tests;
  wall-clock stays review evidence only.

## Second Hostile Pass

Verdict: close to ready, but three smaller rocks still need plan pins before
execution starts.

1. **Scope order inside the big phase is not strict enough.**
   DNS, TLS, filesystem, signal, e2e, DST, and perf are all large. The plan
   should require implementation order with review after each resource family:
   audit/capabilities, DNS, TLS, filesystem, shutdown signals, e2e, DST/perf.
   Otherwise a session can half-land five rails and only discover bad ownership
   near closeout.

2. **Dependency policy is not pinned.**
   TLS likely adds `rustls`; signal may add a signal crate; cert generation may
   add dev dependencies. The plan should require dependency choices in the
   audit section with reasons, feature flags if needed, and no broad workspace
   dependency blast unless justified.

3. **Public API churn control is missing.**
   041 touches `CallInput`, `CallOutput`, `CallError`, helpers, traces,
   capabilities, simulator configs, and bridge surfaces. The plan should require
   a blast-radius table before implementation begins and a final public-symbol
   list at closeout. Grug wants one vocabulary, not three helper names per rail.

These are not human product decisions. They are rails to keep the big phase from
becoming soup.

## Second Review Response

Plan updated again.

- Build step 1 now requires dependency choices/reasons, feature-flag stance,
  and a public API blast-radius table before implementation.
- Build step 2.5 now pins strict implementation order with code-bug review after
  each resource family.
- Closeout review now requires a final public-symbol list.

## Implementation Audit 1: Current Rail Contract

Starting point observed in code before 041 implementation:

- `CallInput` already has DNS and signal request vocabulary:
  `DnsLookup { host, port, timeout }` and `SignalWait { name, timeout }`.
- Live `BetelgeuseDriver` currently returns `Unsupported` for DNS and signal.
- `tina-sim` already scripts DNS and signal with success/failure/timeout and
  bounded lane-full behavior.
- Live UDP and process rails exist. Process uses a bounded worker lane.
- File live rail currently supports open/read/write/fsync/size/close plus
  `Mkdir`; richer metadata/rename/unlink/read_dir are missing.
- Capability table currently reports DNS and signal unsupported, TLS
  adapter-only, UDP/process/storage supported.
- Public API blast radius for 041:
  - `tina-runtime/src/call.rs`: `CallInput`, `CallOutput`, `CallError`,
    typed helper functions, resource ids.
  - `tina-runtime/src/trace.rs`: `CallKind` and trace names.
  - `tina-runtime/src/lib.rs`: exports, `RuntimeCapabilities`.
  - `tina-runtime/src/driver.rs`: live driver lanes and cancellation.
  - `tina-sim/src/lib.rs`: simulator config/scripts/resource histories.
  - `tina-tokio-bridge`: only proof/tests unless bridge surface truly needs
    new public API.
- Dependency stance:
  - DNS first uses `std::net::ToSocketAddrs` behind a bounded lane. No new
    dependency.
  - TLS may add `rustls` only after the DNS slice and TLS resource audit.
  - Signal raw OS capture may add a dependency only after runtime shutdown
    notification lands and raw capture is still justified.
  - Test cert generation should prefer dev-only dependency or small fixture;
    no broad dependency blast before TLS audit.

## Implementation Closeout Review

Verdict: Jan de Quay lands the intended local I/O substrate slice. The phase is
not a release claim and not a distributed-runtime claim, but the local Tina app
story is materially stronger.

### Positive Review

Users can now write Tina-owned local services that use:

- bounded live DNS through `dns_lookup`;
- native rustls-backed TLS through `tls_connect`, `tls_read`, `tls_write`, and
  `tls_close`;
- richer runtime-owned path operations through `path_metadata`,
  `rename_replace`, `remove_file`, `read_dir`, and `sync_parent`;
- runtime shutdown notification through `signal_wait("shutdown", ...)`;
- previous TCP, UDP, process, timer, file, and persistence rails in the same
  `LocalSystem` shape.

This closes the most obvious local I/O holes for a Tokio-shaped service port:
name resolution, encrypted client connection, file/path maintenance, and
graceful shutdown notification are now Tina effects with traceable outcomes.

### Blast-Radius Review

Public/new vocabulary added or changed:

- `TlsStreamId`;
- `PathKind`, `PathMetadata`;
- `CallInput::{TlsConnect,TlsRead,TlsWrite,TlsClose,PathMetadata,RenameReplace,
  RemoveFile,ReadDir,SyncParent}`;
- `CallOutput::{TlsConnected,TlsRead,TlsWrote,TlsClosed,PathMetadata,
  PathRenamed,FileRemoved,DirectoryRead,ParentSynced}`;
- `CallError::{DnsFull,DnsClosed,TlsFull,TlsClosed,TlsCertificate,TlsName,
  TlsHandshake,NotFound}`;
- helpers `tls_connect`, `tls_read`, `tls_write`, `tls_close`,
  `path_metadata`, `rename_replace`, `remove_file`, `read_dir`,
  `sync_parent`;
- `CallKind::{TlsConnect,TlsRead,TlsWrite,TlsClose,DnsLookup,SignalWait,
  PathMetadata,RenameReplace,RemoveFile,ReadDir,SyncParent}`;
- simulator config/script types for TLS and expanded resource histories;
- `RuntimeCapabilities::threaded` now reports live DNS and TLS supported and
  signal as runtime-shutdown-notification supported.

Dependency blast radius:

- `rustls` is a runtime dependency for native TLS.
- `rcgen` is a dev dependency for deterministic local certificate tests.
- No signal crate was added; raw OS signal capture remains a non-claim.

### Hostile Review

No open P1/P2 bugs remain from this pass.

Findings caught and fixed during closeout:

1. **Clippy large enum variant in TLS completion.** The TLS completion result
   carried the connected rustls stream inline, making the enum huge. Fixed by
   boxing only the connected-stream result.
2. **TLS queued timeout only applied after worker start.** TLS pending entries
   initially lacked a runtime deadline. A call queued behind other TLS work
   could wait longer than its timeout before the worker even started it. Fixed
   by adding `deadline` to `TlsPending`, checking it in `TlsWorkerLane::advance`,
   tombstoning/canceling timed-out work, and adding
   `tls_lane_deadline_tombstones_queued_work_until_late_completion`.
3. **Committed system notes were stale.** `SYSTEM.md` and roadmap/changelog
   wording still described live DNS as unsupported and TLS as adapter-only.
   Updated the contract language to match the landed rail semantics and keep
   raw OS signal capture as the explicit non-claim.

Residual risks / honest non-claims:

- DNS uses a bounded blocking lane over the standard resolver. Queued work can
  be canceled; started resolver work is tombstoned, not preempted.
- TLS uses a bounded blocking lane over rustls/TcpStream. It is real TLS, but
  not a full async TLS reactor, not ALPN/HTTP policy, and not platform cert
  store policy.
- Runtime shutdown notification is real Tina-owned signal delivery. Raw OS
  signal capture is still not implemented.
- Rich path operations are runtime-owned and typed, but platform filesystem
  semantics still matter. Tina reports support/unsupported/uncertain/I/O
  instead of claiming one universal filesystem.
- Broad throughput and production-server superiority remain unclaimed.

### Proof

`make verify` passes after the closeout fixes.

High-signal proof coverage:

- `tina-runtime --lib`: DNS and TLS lane tests, including TLS queued timeout
  tombstone/late-completion behavior.
- `tina-runtime --test local_system`: 20 user-shaped live tests, including DNS,
  TLS, file/path operations, shutdown notification, storage pressure, UDP,
  process, persistence, and composed service flows.
- `tina-sim --test io_simulation`: 39 simulator/DST tests, including scripted
  DNS/TLS/signal/process/UDP/file/TCP and ResourceRail replay/delete-shrink.
- Workspace doctests, docs, loom mailbox tests, and clippy with `-D warnings`
  all pass through `make verify`.
