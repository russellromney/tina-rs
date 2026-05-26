# Phase 140 Review (append-only)

## Plan Review 1 — second-reviewer seed (2026-05-25)

Verdict: keep this as a dependency-gated phase, not active work. The pattern is
real, but implementing the sweep before 136 and 138 land would be speculative.

### Required shape

- Do not chase a "zero helper threads" headline. DNS and process are allowed to
  remain bounded blocking lanes when the reason is written down and reported.
- Do not let storage fallback become a generic storage lane again. After 138, it
  is only rename/remove/readdir/metadata unless Betelgeuse grows those ops.
- Do not re-open TLS once 136 lands except through the inventory/guard.
- The main deliverable is a runtime rail inventory plus a guard against future
  bypass lanes.

### Proof bar

- A test must fail when a new runtime rail adds blocking std socket/file/thread
  work without updating the inventory.
- Capability reports must stay user-facing. The answer should be "this rail is
  completion-backed" or "this rail is a bounded blocking lane because X," not
  "read the source."

## Plan Review 2 — grug pass (2026-05-25)

Verdict: the pointer was useful, but Unix-domain sockets were still phrased as
"maybe move if cheap." That is planning language.

### Finding 1 — Unix sockets should be a real deliverable

Main already has `driver/unix.rs` as a private blocking worker over
`std::os::unix::net`. TCP/TLS are moving to the substrate, so Unix-domain sockets
should not keep a hidden worker. Fixed in plan v1: Phase 140 must move Unix
bind/accept/connect/read/write/close onto a completion-backed rail, adding narrow
Betelgeuse backend support if needed.

## Implementation 1 — shipped (2026-05-25)

All six core items landed; the proof bar above is met.

### Unix-domain sockets onto the substrate

- `driver/unix.rs` is now `BetelgeuseUnix`: a completion-backed lane sharing the
  per-shard Betelgeuse loop with TCP/TLS. The `std::os::unix::net` worker thread
  is gone. Lane discipline matches TCP (accept/read/write lanes, `ResourceBusy`,
  close-wins, tombstoned shutdown, no Drop walking the shared backend).
- Added the missing Unix *addressing* to vendored Betelgeuse:
  `IOSocket::bind_unix` / `connect_unix` (darwin + linux real, simulated
  `Unsupported`), with the socket-file lifecycle owned in the substrate
  (stale-unlink before bind; unlink-on-close, socket inodes only). Accept/recv/
  send/close were already family-agnostic at the fd layer.

### DNS / process / storage held as written-down lanes

- DNS and process stay justified blocking lanes; the storage fallback stays the
  narrow rename/remove/readdir/metadata worker. Each carries a justification
  string in the capability report.

### Capability classification + guard

- `RailClass` (completion-backed / inline / poll-backed / fallback-worker /
  justified-blocking-lane / simulator-scripted / unsupported) + per-rail
  justification on the capability report. Unix is now completion-backed.
- `scripts/rail_inventory_guard.sh` + `.intent/runtime-rail-inventory.txt` fail
  the build (via `make verify`) if a runtime rail adds a worker thread / blocking
  std socket / blocking std::fs without being inventoried.

### Proof

- Substrate: `vendor-betelgeuse/tests/io.rs` unix round-trip + EOF (both backends).
- Runtime live: `local_system.rs::unix_live_echo` round-trip + socket-file safety.
- Pressure / lifecycle / shutdown: `driver::tests::unix_lane::*`.
- Classification: `capabilities` unit tests + `tests/rail_capability_inventory.rs`.
- Guard fires: `tests/rail_inventory_guard.rs`.
- `cargo fmt --all --check`, clippy on betelgeuse + tina-runtime, and the full
  `tina-runtime` + `tina-sim` suites pass.

### Deliberately retained blocking lanes

DNS resolver, process spawn/wait, and the rename/remove/readdir/metadata storage
fallback. Each is bounded, inventoried, and classified with a written reason; no
hidden Unix worker remains.
