# 035 Jelle Zijlstra Plan Review 1

Verdict: much sharper and ready to hand to implementation after one explicit
first-step audit. The phase is big, but it is the right bigness: it attacks the
actual missing local I/O substrate instead of wandering into release docs or
random production theater.

## What Looks Strong

- The plan starts from code reality: Tina currently has time and server-side
  TCP, not a full local I/O story.
- It names the surprise missing rock: outbound TCP connect. A service that can
  accept connections but cannot connect outward is not a complete local service
  substrate.
- It uses existing substrate wood correctly: Betelgeuse already has native file
  primitives, while Tina has not exposed them as runtime-owned calls.
- It refuses DNS/TLS/UDP/process/signal as secret scope creep while still
  requiring a final support table with exact reasons.
- It keeps the Tina rule intact: new I/O enters through `RuntimeCall`, uses
  runtime-owned ids, and never leaks raw OS handles or Betelgeuse boxes to
  isolates.
- It requires both live runtime proof and simulator/DST proof for accepted I/O.
- It demands `LocalApp` and bridge-shaped tests, so this does not stop at unit
  plumbing.

## Main Risks

1. **Betelgeuse connect may be the hardest first rock.**
   The current socket trait has bind/accept/recv/send but no connect. Adding a
   completion slot and native backend operation touches the vendored substrate.
   That is correct, but implementation should do this first so the phase does
   not spend days on file polish before discovering connect is awkward.

2. **File simulation can accidentally become persistence.**
   Jelle only needs deterministic local file behavior for config/snapshot/log
   shaped tests. Durable journals, crash recovery, and mailbox persistence
   belong to Wim Kok.

3. **Path/security policy is not a core concurrency primitive.**
   File helpers should accept paths and surface typed outcomes, but they should
   not design sandboxing, permissions policy, or package layout in this phase.

4. **Support table must survive implementation pressure.**
   DNS/TLS/UDP/process/signal should not slide in because they sound
   production-ish. They land only if accepted workloads prove need.

5. **Mailbox fallback language is now fixed, but must stay fixed.**
   The roadmap no longer says `MPSC fallback`; it says mailbox producer model
   decision. That matters. A fallback path hides "main path did not work."
   Jelle must not add a second mailbox route while implementing I/O unless a
   named workload proves the current producer model cannot express the service.
   If multi-producer support ever lands, it must be a first-class bounded
   mailbox implementation with the same visible `Full`/`Closed` contract, not a
   secret escape hatch.

## Hostile Review After Mailbox Wording Fix

Verdict: plan still ready. The fallback smell is removed from both the roadmap
and Jelle rails.

What hostile grug tried to break:

- **"Fallback" hides failure.** Fixed. The roadmap now says one mailbox
  contract and only a possible future bounded multi-producer implementation.
- **Jelle might sneak in MPSC while building file/connect.** Fixed. The plan now
  has a design rule and pause gate: if I/O seems to require multi-producer
  mailbox support, pause and name the workload.
- **Too many mailbox concepts for users.** Still okay because this is not a new
  user surface. The current user promise remains bounded mailbox semantics with
  visible `Full`/`Closed`; implementation choices stay below that contract.
- **Could this block 035?** No. Outbound TCP connect and file I/O should not
  require MPSC. If they do, that is a discovery worth stopping for.

Remaining non-blocking risk:

- The crate layout still lists `tina-mailbox-mpsc` as possible future shape.
  That is acceptable now because it is no longer a fallback and is explicitly
  gated by a named workload.

## Launch Guidance

1. Start with a concrete code audit of `vendor-betelgeuse` socket/file traits,
   native backends, simulated backend, `tina-runtime/src/call.rs`,
   `tina-runtime/src/driver.rs`, and `tina-sim/src/lib.rs`.
2. Implement the smallest Betelgeuse connect proof before touching Tina's
   public call API.
3. After connect is real, thread it through Tina runtime and simulator with one
   user-shaped client workload.
4. Then do file I/O. Keep the simulator file model intentionally small and
   deterministic.
5. Close with the support table and `make verify`.

Grug says: good fire. Start with connect, then files. Do not summon TLS demon.

## Implementation Review 1

Verdict: Jelle now has the main local I/O breadth it set out to add. No
blocking findings after hostile pass.

What landed:

- Betelgeuse has native and simulated outbound TCP connect.
- Tina runtime has `TcpConnect` / `TcpConnected` plus `tcp_connect(addr)`.
- Tina runtime has runtime-owned file vocabulary: `FileId`,
  `FileOpenOptions`, `file_open`, `file_read_at`, `file_write_at`,
  `file_fsync`, `file_size`, `file_close`, and `mkdir`.
- Live driver owns file resources and file completion lanes without exposing
  raw OS handles.
- `tina-sim` has deterministic in-memory file behavior for local config /
  snapshot / log-shaped workloads.
- User-shaped live tests prove Tina can connect to a local server, write, read,
  and close.
- User-shaped live tests prove Tina can mkdir, open, write, fsync, size, read,
  and close a file.
- Simulator tests prove the same connect and file flows replay through the
  oracle shape.
- `LocalApp` hosts a service that uses runtime-owned file calls end to end and
  records the expected file call completions in the terminal trace.
- `tina-tokio-bridge` hosts a bridge-facing service that uses runtime-owned
  file calls before replying to Tokio.
- Tiny file helpers landed where they remove ceremony without hiding behavior:
  `FileOpenOptions::{read_only, write_only, read_write, read_write_create_truncate}`,
  `file_create`, `file_read`, and `file_write`. No `write_all` helper was added
  because full-write retry would be new control flow, not tiny sugar.

Support table after this slice:

| Family | Status | Reason |
|---|---|---|
| Time | supported | Existing runtime-owned `sleep(...)`. |
| TCP server | supported | Existing bind/accept/read/write/close. |
| TCP client connect | supported | Added native/simulated Betelgeuse connect and Tina `tcp_connect(...)`. |
| File / mkdir | supported | Added runtime-owned file ids and helpers over Betelgeuse file ops plus simulator oracle. |
| DNS | deferred | Needs resolver/cache semantics; Tina still accepts `SocketAddr`. |
| TLS | deferred | Needs handshake state-machine design over streams. |
| UDP | deferred | Packet semantics, multicast, and receive-buffer policy need their own phase. |
| Process | deferred | Needs child lifecycle, pipes, cancellation, and zombie proof. |
| Signal | deferred | Process-global and platform-specific; app edge can request shutdown for now. |

Hostile notes:

- Native `open` is still synchronous because Betelgeuse exposes `open` that
  way today. This phase did not invent a hidden blocking pool.
- Simulator file behavior is intentionally local and deterministic, not
  durable persistence or crash recovery.
- File write cancellation is not modeled as undo. Once a runtime-owned write
  has been submitted, cancellation removes completion pressure; it does not
  promise the substrate side effect can be reversed.
- The simulator now rejects read-only `create` / `truncate` file opens so its
  deterministic file oracle matches native Rust open-option constraints.

Verification:

- `cargo test --workspace` passed.
- `make verify` passed.
