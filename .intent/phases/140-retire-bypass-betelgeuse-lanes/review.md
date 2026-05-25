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
