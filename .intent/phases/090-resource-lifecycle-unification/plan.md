# 090 Resource Lifecycle Unification

## Status

- Ready to implement.
- Run after 079/080/084/086 are merged.
- Can run beside WebSocket/AWS only if this stays narrow.

## Grug Truth

Resource starts.

Resource becomes ready, or fails.

Resource is used.

Resource admission is closed.

Resource handle is closed.

Pending work is cancelled, drained, or abandoned.

User sees what remains.

Same words should mean same thing everywhere.

Not every surface has every step.

Do not fake missing steps.

## Goal

Make one real lifecycle mismatch boring.

This is not the phase that rewrites every rail. This phase does:

- audit current lifecycle shapes;
- write down the common vocabulary;
- fix one concrete mismatch that users can hit;
- add tests proving the fix;
- leave larger semantic changes as named follow-ups.

One PR.

If the audit finds more than one real mismatch, pick the one with the
smallest safe code change and write the rest down. Do not start a framework.

## Non-Goals

- no broad runtime semantic rewrite;
- no trace vocabulary churn unless required by the chosen bug;
- no changing all closers to one trait;
- no global resource manager;
- no fake preemptive cancel for external systems;
- no WebSocket/AWS behavior in this phase;
- no bridge framework;
- no rename sweep.

## Words

Use these words consistently.

```text
open/start       = ask resource to begin
ready            = resource can be used, or startup failed visibly
use              = normal call/read/write/request work
close admission  = stop accepting new work
close resource   = close the owned OS/backend handle if possible
stop isolate     = stop the state machine
cancel           = stop waiting; stop work only if the owner can honestly do it
drain            = wait bounded time for already-accepted work to settle
terminal         = final report says what remains
pressure         = bounded capacity report says what filled
late result      = work completed after caller stopped waiting
```

If a crate uses another word, either map it in docs or fix the word.

## User Pathways

Users do not start from runtime internals. They ask:

```text
Can it still accept work?
Can accepted work still run?
Can I wait for it to settle?
Can I close the owned handle?
What remains if the deadline fires?
Where do late results show up?
```

Use these pathways when auditing.

### Full Service Path

This is the copied shutdown shape for real services:

```text
start
ready
use
close admission
drain accepted work
cancel leftovers
close resources
stop isolates
terminal report
```

This path makes sense for whole services, listeners, pools, and bridges.

### One-Shot Call Path

One-shot calls are different:

```text
dispatch
accepted / full / closed / rejected
replied / timeout / cancelled
late reply maybe
```

Do not force "ready" or "close resource" onto a one-shot call.

### Timer Path

Timers are:

```text
arm
fire / cancel / owner stopped
```

No open handle. No close resource. Do not pretend.

### Body Stream Path

Body streams are:

```text
start source
pull chunks
eof / error / cancel
drain metrics
terminal drained=true
```

The source may hold files, calls, or buffers. Cancel must give it a chance to
release them.

### External Bridge Path

External work is not Tina-owned work:

```text
install bridge
ready
use
close admission
drain accepted bridge work
shutdown worker/runtime
metrics + trace late results
```

Closing the bridge does not mean a remote query/request died unless the bridge
has an explicit, proven cancel mechanism.

### N/A Is Honest

Matrix cells may say:

- `not applicable`;
- `not Tina-owned`;
- `trace only`;
- `metrics only`;
- `future`.

That is better than inventing fake lifecycle symmetry.

## Rock 0: Audit Current Surfaces

Read:

- `docs/tina-user-guide/14-lifecycle-and-shutdown.md`;
- `docs/tina-user-guide/18-bridge-crates.md`;
- `tina-runtime/src/capabilities.rs`;
- TCP/TLS close/cancel paths in `tina-runtime`;
- `tina-runtime/src/pool.rs`;
- `tina-http/src/keepalive.rs`;
- `tina-http/src/connection.rs`;
- `tina-http/src/body_metrics.rs`;
- `tina-reqwest-bridge`;
- `tina-sqlite-bridge`;
- `tina-sqlx-bridge`.

Add a small lifecycle matrix to the user guide.

Columns:

- surface;
- start/open;
- ready/fail observation;
- close admission;
- close resource;
- cancel;
- drain;
- terminal report;
- pressure report;
- late-result truth;
- test that proves it.

Keep it short. One row per surface family, not one row per function.

Each row should answer the user questions above. If a step does not exist,
write `not applicable` or `not Tina-owned`; do not imply support.

## Rock 1: Pick One Mismatch

Default mismatch to fix unless the audit finds a worse one:

**HTTP keepalive pool shutdown is two-step folklore.**

There are two related problems.

First, `KeepaliveConnectionMsg::Stop` is documented as user-callable with a
`KeepaliveOutcome::Stopped` reply, but after 086 the call-shaped path rejects
`Stop` as `UnsupportedMessage`. That is a real lifecycle bug:

```text
documented stop reply != actual call behavior
```

Fix that first.

Required shape:

- `handle_call` accepts `KeepaliveConnectionMsg::Stop`;
- `call(conn, KeepaliveConnectionMsg::Stop, timeout)` replies
  `CallOutcome::Replied(KeepaliveOutcome::Stopped)`;
- `runtime.call_blocking(conn, KeepaliveConnectionMsg::Stop, timeout)` sees the
  same truth;
- the connection closes/drops its transport as honestly as the existing
  transport-close path allows;
- the isolate stops after the reply authority is consumed;
- late/stale request continuations after Stop are harmless and visible if the
  runtime already exposes them.

Also fix tests/docs that send `Stop` without checking the outcome. A lifecycle
test that ignores the terminal reply is not a lifecycle test.

Second, pool shutdown is still two-step folklore.

Today `build_keepalive_pool` returns:

```text
pool address
connection addresses
```

Closing admission on the `WorkerPool` does not stop the connection isolates.
The docs say to send `KeepaliveConnectionMsg::Stop` to every connection after
pool close. That is true, but easy to forget. A user can close the pool and
still leave connection isolates / transports running.

That is a lifecycle mismatch:

```text
pool close admission != connection resource close
```

Fix shape:

- keep the current explicit handles;
- add a tiny helper that makes close-admission-then-stop-connections the copied
  path;
- the helper must not hide `WorkerPool` close outcome;
- the helper must not fire-and-forget `Stop` if the API promises a reply;
- the helper must not claim a graceful transport close unless the connection path
  actually proves it;
- add a test that a copied shutdown path leaves no connection/resource running.

Possible API shapes:

```rust
// service-side helper over ordinary call effects
handles.stop_connections_effect::<I>(timeout, AppMsg::ConnStopped)

// or a small state helper if close-then-stop needs one continuation
KeepalivePoolShutdown::new(handles, CloseMode::Drain, timeout)
```

Choose the boring one after reading the code.

If the copied path needs multiple continuation turns, prefer a tiny explicit
state helper/report over a clever batch that hides partial shutdown truth.

The report should be dull data:

```text
pool close outcome
connections requested
connections stopped
connections timed out / rejected / already closed
```

No hidden retries.

If this helper gets weird, do not force it. Pick another concrete mismatch from
the audit and record why keepalive needs a later design.

Other likely mismatches to check:

- bridge `close()` sometimes means stop admitting, sometimes drain, sometimes
  just flip a flag;
- body metrics have `drained()`, pool/bridge reports may not have an equivalent;
- SQLite/SQLx/reqwest late-result counters do not all describe the same layer;
- startup readiness is typed for HTTPS but not every listener/bridge shape;
- shutdown reports name runtime resources but not every app-level resource.

## Rock 2: Lifecycle Vocabulary Doc

Update `docs/tina-user-guide/14-lifecycle-and-shutdown.md`.

Add a compact table:

```text
surface | close admission | close resource | cancel | drain | terminal proof
```

Use exact Tina names.

No theory wall. The page should help a user writing shutdown code.

## Rock 3: Tests

Required tests depend on the chosen mismatch.

For the default keepalive mismatch, prove:

- `call(conn, Stop, timeout)` returns `Replied(Stopped)`, not
  `Rejected(UnsupportedMessage)`;
- `runtime.call_blocking(conn, Stop, timeout)` returns the same;
- after `Stop`, the connection address rejects later request calls as `Closed`;
- closing the pool blocks new acquire;
- helper waits for or records every connection Stop outcome;
- helper report names any connection that did not stop before its deadline;
- repeated shutdown is harmless or returns typed closed;
- outstanding lease under `Drain`/`Force` keeps existing pool semantics;
- pressure/resource report after shutdown does not lie.

If a different mismatch is chosen, tests must prove:

- before fix, user could reasonably leave a resource running or misread close;
- after fix, the copied path reaches terminal truth;
- remaining work is visible, not swallowed.

## Rock 4: Specimen / Docs

Update one copied example if it uses the fixed surface.

Likely:

- `specimen_outbound_http` for keepalive pool shutdown;
- or bridge docs if the chosen mismatch is bridge lifecycle.

Do not retrofit every specimen.

## Parallel Rule

Safe beside WebSocket/AWS:

- audit matrix;
- docs vocabulary;
- one small helper/fix on an existing surface.

Not safe beside WebSocket/AWS:

- changing `RuntimeEventKind`;
- changing `CallOutcome`;
- renaming close/cancel types across crates;
- changing TCP/TLS/HTTP resource ownership rules broadly.

If the audit says a broad semantic change is needed, stop after the audit and
write the next phase. Do not sneak it into 090.

## Required Checks

- `cargo fmt --all --check`
- targeted tests for the touched crate;
- targeted clippy for the touched crate/tests;
- if docs changed: `RUSTDOCFLAGS=-D warnings cargo doc --workspace --no-deps`
- if runtime/resource semantics changed: `make verify`

## Done Means

- lifecycle matrix exists and is current;
- one real mismatch is fixed or explicitly rejected with reason;
- copied docs show the fixed path;
- tests prove no hidden resource remains for that path;
- roadmap/changelog name any follow-up instead of leaving folklore.
