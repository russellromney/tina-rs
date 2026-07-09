# Bridge Author Kit — the copied path

This page is for someone about to write or read a Tina bridge crate
for the first time. It leads with the **bridge author's job**, then
maps each step to the existing trait/type names. For the deeper
vocabulary (close vs drain vs shutdown, late-result truth, pressure
surface, classifier) see [18-bridge-crates.md](18-bridge-crates.md).

The rule still holds:

> Tokio may speak ecosystem. Tina owns state. Bridge shows pressure.
> Bridge may adapt. Bridge may not lie.

## What you are doing

You have an SDK or Tokio-shaped library. You want Tina services to
call it without losing pressure, cancellation, or trace truth. The
copied path has eight steps. Each step has one helper name. Each step
keeps one truth visible.

```text
1. config validates caps                 → typed InstallError
2. install starts worker, returns handle → BridgeInstall
3. closer stops admission                → BridgeCloser::close()
4. drain waits or reports timeout        → close_and_drain(deadline)
5. metrics count worker-terminal facts   → XxxMetricsHandle::snapshot()
6. pressure reports caps + current + HW  → BridgePressure
7. classifier names retry / fatal / ok   → BridgeOutcomeClass
8. late-result truth is documented       → late_results counter
```

## Step 1 — Config validates caps

Your config carries `mailbox_capacity`, `max_in_flight`,
`per_request_timeout`, and any per-SDK size caps. `validate()` returns
a typed `InstallError` for any cap that is zero, larger than the
mailbox, or otherwise impossible. No panics from a bad cap.

```rust
let config = XxxConfig::new(...)
    .mailbox_capacity(64)
    .max_in_flight(8)
    .per_request_timeout(Duration::from_secs(10));
config.validate().map_err(InstallError::Config)?;
```

## Step 2 — Install starts the worker

`install_xxx(runtime, config)` returns `Result<InstalledXxxBridge,
InstallError>`. Failure is typed; the caller never has to "check if
the handle is real." The install handle implements
`tina_runtime::bridge::BridgeInstall`:

```rust
pub trait BridgeInstall {
    type Closer: BridgeCloser;
    type Metrics;
    fn closer(&self) -> &Self::Closer;
    fn metrics(&self) -> &Self::Metrics;
}
```

Callers send typed requests to the install's public `address` field —
never to a raw worker. The address *type* stays bridge-specific because
address shapes differ per bridge, while the shared `BridgeInstall` trait
standardizes the closer and metrics handles.

The bridge never registers itself on the user's runtime implicitly;
the user passes the runtime explicitly, exactly once.

## Step 3 — Closer stops admission

`BridgeCloser::close()` flips the closed flag. Already-admitted SDK
work continues; new admissions return `Closed`. The closer is
cloneable and `Send`. Close is idempotent — calling it twice is fine.

```rust
let closer = bridge.closer().clone();
// ...later, from a shutdown handler:
closer.close();
```

`close()` does **not** wait for in-flight work. It is cheap; use it
when you want to stop taking new requests immediately and let in-flight
work finish on its own.

## Step 4 — Drain waits or reports timeout

`closer.close_and_drain(deadline)` closes admission, waits for
in-flight count to reach zero (or for `deadline` to fire), and returns
a typed `XxxDrainReport`:

- on success: every in-flight request observed a worker-terminal
  outcome before the deadline;
- on timeout: the report names which kinds of operations are still in
  flight so the caller can decide to give them more time or move on.

```rust
let report = closer.close_and_drain(Duration::from_secs(30));
if report.drained {
    // clean stop: every in-flight request settled before the deadline
} else {
    // still in flight; `report.in_flight_remaining` and
    // `report.in_flight_kinds` name what's left — log and proceed
}
```

Drop the install handle to release the bridge's Tokio runtime (if it
owns one); supplied-runtime installs never shut down the caller's
Tokio runtime.

## Step 5 — Metrics count worker-terminal facts

`bridge.metrics().snapshot()` returns the typed counter snapshot:
in-flight, accepted, rejected (full/closed), worker-terminal kinds,
late results. The metrics handle is cheap to clone; pass it wherever
you need to read the snapshot.

Two truths to keep visible:

- **Worker-terminal**: the SDK round-trip finished, success or
  classified failure. Counts go in worker-terminal metrics.
- **Caller-observed**: the Tina reply slot received the outcome.

They coincide when the caller is still listening. They diverge when
the caller has given up (deadline, cancel, bridge timeout); see step 8.

## Step 6 — Pressure reports caps + current + high water

`bridge.metrics().pressure_report()` returns the per-bridge richer
pressure shape. To expose it on a `ServicePressureReport`, convert to
the shared `BridgePressure`:

```rust
use tina_runtime::bridge::BridgePressure;
let pressure: BridgePressure = bridge.metrics().pressure_report().into();
service_pressure.add_measured("xxx-bridge",
    pressure.capacity_surface(CapacityMode::Fixed));
```

`BridgePressure` fields are private. Construction is `measured(...)`,
`unavailable(name, reason)`, or a per-bridge `From` impl. A forged
literal would let a buggy adapter rename or under-report a surface;
the closed type prevents that.

## Step 7 — Classifier names retry / fatal / success

The AWS and SQLite bridges ship `XxxOutcomeExt::classify(&outcome)`
returning
[`tina_runtime::bridge::BridgeOutcomeClass`](../../tina-runtime/src/bridge.rs);
the reqwest bridge exposes its own richer `ReqwestOutcomeClass`. The
shared `BridgeOutcomeClass` shape is:

- `Succeeded`;
- `Retryable(BridgeRetryable)` — caller decides under their own
  idempotency rules;
- `Unavailable(BridgeUnavailable)` — bridge or resource is closed; a
  new handle is required;
- `Fatal(BridgeFatal)` — input/permission/code change required.

Two anti-fog rules:

1. `Closed` is **`Unavailable`**, not `Retryable`. Retrying on the
   closed handle reproduces the failure.
2. A generic `Sdk(_)` wrapper is `Fatal(SdkUnknown)`. Only typed
   SDK throttle/retry metadata earns `Retryable(SdkRetryable)`.

## Step 8 — Late-result truth

When a Tina caller's deadline fires while SDK work continues:

- the reply slot gets `CallOutcome::Timeout` (or
  `Replied(Err(BridgeTimeout))` if the bridge's deadline fired);
- the SDK future runs to completion on the bridge runtime. Worker
  terminal increments. `late_results` increments. The slot leaves the
  in-flight set.
- if the SDK call mutated remote state, the mutation may have already
  happened. The bridge **cannot** prove otherwise. Idempotency belongs
  to the caller.

If the bridge cannot observe late terminal completion (rare,
fire-and-forget shapes), the docs must say so and
`late_result_count == 0` must be documented as "not observed," not
"none happened."

## Copy-paste checklist

When you write the next bridge, prove these in one hermetic test file:

- happy-path: typed request in, SDK called, typed response out;
- `Full` / `Closed`: admission flips and callers see the right typed
  outcome;
- caller-timeout: a `late_results` count of exactly one (no
  double-tally);
- drain mid-flight: the drain report names remaining in-flight kinds;
- classifier coverage of every typed error;
- pressure report exposes the installed capacity (cannot be faked by
  passing a fresh config to the metrics handle);
- late-result count visible when observable; documented `0` when not.

## Worked example

The reqwest bridge is the smallest end-to-end copy of this kit. Read
[`tina-reqwest-bridge/README.md`](../../tina-reqwest-bridge/README.md)
for the non-AWS shape, then [`tina-aws-bridge`](../../tina-aws-bridge/)
for the multi-service AWS shape.

## What is not a bridge author's job

- Inventing retry budgets — caller-owned.
- Adding hidden queues — `Full` is good overload.
- Surfacing late results as success — increment `late_results` and
  attach `BridgeCallerWarning::ExternalWorkMayContinue` if helpful.
- Holding a Tina runtime open through process teardown — drop the
  install handle and let the runtime own teardown.
