# Rock 11 Audit Note — Bridge Setup Convention

## Status

Audit + convention. **No shared bridge crate.** Two direct
shapes, two layered. Plan rule "three repeats is evidence" does
not yet fire.

## What's In Tree

| Crate                | Top-level entry                                   | Layered on |
| -------------------- | ------------------------------------------------- | ---------- |
| `tina-tokio-bridge`  | `BridgeHost::register_bridge(...)`                | direct     |
| `tina-reqwest-bridge`| `ReqwestWorker::install(runtime, config)`         | direct     |
| `tina-tower-bridge`  | `TowerService::new(handle)` over a `BridgeHandle` | tokio      |
| `tina-rpc-tokio`     | `Client::new(handle, ...)` over a `BridgeHandle`  | tokio      |
| `tina-sqlite-bridge` | (proposed)                                        | direct     |

The upcoming sqlite bridge would make a third direct shape.
Lock the convention now so sqlite does not invent a sixth
dialect.

## Convention For Direct Bridges

```rust
WorkerType::install(runtime, config) -> Result<Installed<...>, InstallError>
```

`config: Config` is one typed value. Numeric fields
(`mailbox_capacity`, per-attempt timeout, retry policy, …) live
there. `Config::validate()` returns
`ConfigError::{ZeroX, AboveCap, …}`. `install` calls `validate`
first. Invalid config never registers.

`Installed<...>` carries:

- `address: Address<Msg, Reply>` — the worker's typed inbox;
- `closer: Closer` — cloneable handle that flips closed.
  New sends reply `Closed`. In-flight work runs to natural
  completion;
- `metrics: MetricsHandle` — read-side handle for admitted /
  rejected / completed / errored counters.

`InstallError` separates config validation errors from runtime
registration errors.

## Convention For Layered Bridges

Take an already-installed `BridgeHandle`. Adapt to the layered
ecosystem (Tower trait, RPC client). Do not re-validate config.
Trust the underlying bridge. Entry: `Layer::new(handle, ...)`.

## Vocabulary

Same word, same meaning across crates.

| Word               | Meaning                                                                |
| ------------------ | ---------------------------------------------------------------------- |
| `install`          | "validate config, register worker, return address+closer+metrics"     |
| `close`            | flip closed state; new sends reply `Closed`                            |
| `drain`            | wait for in-flight work to complete (per-attempt timeout bounded)     |
| `metrics`          | a snapshot or read-side handle, never a write/reset surface            |
| `mailbox_capacity` | caller-visible bound on admitted-but-not-yet-handled message count     |
| `late reply`       | a worker reply that arrives after the requester closed; logged + dropped |
| `dropped caller`   | a requester whose `IsolateCall` closed before reply arrived            |

## Late-Reply / Dropped-Caller Story

Direct bridges all behave the same way:

1. Requester `IsolateCall` closes (timeout or stop). Bridge
   reply still arrives later.
2. Reply lands at `runtime.deliver_reply` against a closed
   slot. Runtime emits `CallReplyRejected { RequesterClosed }`.
3. Bridge does not see the rejection. Bridge sees its own
   attempt as completed.

Bridge metric "completed" counts the bridge's view. Runtime
trace records the rejection. Different events, not a
disagreement.

## Supplied-Client Ownership

`tina-reqwest-bridge` introduced `with_supplied_client(client,
config)`. The convention:

- **External client owns:** connection pool, TLS config, DNS,
  redirect policy, base URL, default headers — anything that
  is "client behavior".
- **Tina config owns:** `mailbox_capacity`, per-attempt
  timeout, retry policy, connect timeout if applied at the
  per-attempt boundary — "bridge admission / scheduling" knobs.

Future bridges with a supplied client (sqlite pool, postgres
pool, kafka producer) follow the same split.

## Setup Return Shape

Every direct bridge's `install` returns a small struct
`Installed<Bridge>` (or `InstalledFooBridge`). Required fields
in order:

1. `address` — worker's `Address<...>`;
2. `closer` — closer handle;
3. `metrics` — metrics handle;
4. (optional) host runtime / driver handles.

Caller pattern:

```rust
let bridge = ReqwestWorker::install(&runtime, config)?;
let addr = bridge.address;
let closer = bridge.closer.clone();
let metrics = bridge.metrics.clone();
```

## What This Audit Does Not Do

- Does not define a `tina-bridge-common` crate. Two direct
  shapes is one fewer than the rule requires.
- Does not rename `tina-tokio-bridge::register_bridge` to
  `install`. That bridge has a different shape (host-led,
  handle-based, multi-bridge); renaming breaks callers without
  improving truth.
- Does not prescribe a unified error type. Each direct bridge
  keeps its own vocabulary (`ReqwestError::*`,
  `SqliteError::*`).

## Recommendation For The Sqlite Bridge

When `tina-sqlite-bridge` lands:

- `SqliteWorker::install(runtime, config)` returning
  `InstalledSqliteBridge { address, closer, metrics, … }`;
- `SqliteConfig::validate()` returning typed config errors;
- no supplied-client / external-pool constructor in the first
  form. First form owns one blocking `rusqlite::Connection`.
  If a later pool-backed SQLite/SQLx bridge accepts a supplied
  client or pool, supplied-client ownership follows the split
  above;
- typed `SqliteError::{Closed, Busy, IoError, Decode,
  Constraint, Internal}`;
- `SqliteMetricsHandle` shape mirroring `ReqwestMetricsHandle`;
- documentation cluster: install, close, drain, metrics,
  config validation, bounded admission, late reply,
  dropped caller, supplied-client ownership. Same words.

That brings the third direct bridge in line and earns "three
shapes is evidence" a pass for the next audit.
