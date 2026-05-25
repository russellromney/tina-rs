# Phase 140: Retire Bypass-Betelgeuse Lanes

## Status

- Planned pointer (2026-05-25). **Do not implement before Phase 136 and Phase
  138 land.**
- This phase exists so we do not lose the pattern: TLS and storage are two
  concrete bypass-lane fixes. Once both are proven, sweep the remaining runtime
  rails and either move them onto Betelgeuse or write down why they cannot move.
- This is an implementation phase after its prerequisites. The rail list is
  already known; when launched, each step below lands code/proof.
- Current known bypasses on main: `driver/unix.rs` owns a blocking worker over
  `std::os::unix::net`; DNS owns a blocking resolver worker; process owns spawn
  / wait workers; storage metadata fallback remains after Phase 138.

## Prerequisites

- Phase 136 landed: TLS runs sans-I/O over the runtime TCP rail, with no
  `tina-tls-*` worker threads and unchanged security posture.
- Phase 138 landed: live durability reads/writes/fsync/mkdir/size ride
  Betelgeuse, with only the thin metadata fallback worker left.

## Purpose

Make the thread-per-core substrate story boring:

```text
every Tina-owned rail either rides the per-shard Betelgeuse substrate, or has a
short written reason why it must stay a bounded blocking lane
```

## Includes

- **Unix-domain sockets onto substrate.** Replace the private `driver/unix.rs`
  blocking worker with a completion-backed Unix-domain rail. If vendored
  Betelgeuse lacks the exact Unix-socket operations, add the narrow backend
  support needed for bind/accept/connect/read/write/close rather than keeping a
  hidden worker. Unix sockets are sockets; they should follow the same substrate
  rule as TCP/TLS.
- **Keep DNS as a bounded blocking lane with justification.** Platform
  `getaddrinfo` / resolver behavior has no portable Betelgeuse opcode. The lane
  stays, but docs/capabilities must say why.
- **Keep process spawn/wait as a bounded blocking lane with justification.**
  `fork`/`exec`/wait/reap are OS process lifecycle, not reactor I/O. The lane
  stays, but cancellation/drain/report truth must be explicit.
- **Storage fallback stays narrow.** The only storage worker left after Phase 138
  is rename/remove/readdir/metadata until Betelgeuse grows those ops. Name this
  as fallback, not a general storage lane.
- **Capability reports updated.** Runtime capabilities must distinguish:
  completion-backed, fallback-worker, blocking-lane-with-justification,
  unsupported, and simulator-scripted.
- **Guard the old pattern.** Add a test/static check that new runtime-owned
  rails cannot add `thread::spawn`/blocking std socket/file work without touching
  the capability report and this justification list.

## Does Not Include

- DNS implementation rewrite.
- Process implementation rewrite.
- Adding new Betelgeuse opcodes unless Unix/storage fallback work proves it is
  the smallest honest move.
- Performance claims. This phase is substrate honesty + cleanup.

## How We Prove The New Behavior

- A runtime rail inventory test lists every Tina-owned rail and classifies it:
  Betelgeuse-backed, fallback-worker, justified blocking lane, unsupported, or
  simulator-only.
- Unix-domain sockets have a Betelgeuse-backed live proof: bind, accept,
  connect, read, write, close, slow peer, close while pending, and
  `Full`/`Closed` pressure. No private Unix worker thread remains.
- DNS and process capability docs explain why they remain blocking lanes, and
  their capacity/cancel/shutdown reports are still covered by existing rail tests.
- The static guard fails if a new runtime rail adds `std::net`, `std::fs`, or
  `thread::spawn` bypass work without updating the inventory.

## How We Prove We Did Not Break Old Intent

- Existing TCP/TLS/storage/DNS/process/Unix/signal rail tests pass.
- `LocalSystem` capability reports still name every bounded lane capacity.
- Simulator/DST tests remain unchanged except for capability wording; no
  simulator behavior changes.

## IDD Next Step

Keep this plan on main as a dependency-gated pointer. Launch only after Phase 136
and Phase 138 have merged and their implementation reviews prove the pattern.
