# Betelgeuse vendor provenance

`vendor-betelgeuse/` is a **vendored and tina-maintained fork** of Betelgeuse,
a completion-based I/O library (no runtime, no executor, no hidden tasks).

## Upstream

- Project: penberg/betelgeuse (Pekka Enberg)
- License: MIT OR Apache-2.0 (see `LICENSE.md`)
- Upstream has no tagged releases. Our copy tracks upstream tip around
  **May 2026** (it carries the recent `connect` op and the
  `ConnectionStep::Keep -> Continue` rename).
- Exact upstream commit hash for the vendor point is **not recorded** in this
  tree (historical vendor predates this marker). Treat the upstream URL above as
  the canonical source; diff against it before any re-vendor.

## Why a fork, not a dependency

Upstream is pre-release with no semver and no waker/blocking-wait surface (that
is deliberate upstream — a TigerBeetle-style step loop). Tina needs a small set
of substrate features upstream does not provide, so we vendor and patch rather
than pin a moving git dependency.

## tina-local patch families (already present before Phase 151)

- **Unix sockets on the substrate** — `bind_unix` / `connect_unix`, accepted
  streams as ordinary stream sockets (#207, retires bypass-Betelgeuse lanes).
- **Native hot-path buffer reuse** — `recv_buf` / `send_owned` caller-owned
  buffer round-trips (#217).
- **Completion release hooks** — `pending_completion_count` /
  `cancel_pending_completions` for runtime-driven shutdown without leaving
  backend-owned raw pointers.
- **Parallel substrate support** — cloned `IOLoopHandle` lanes (TCP/TLS/Unix
  share one loop).
- **`F_FULLFSYNC`** on Darwin file sync (durability).
- **Honest socket address introspection** — `local_addr` / `peer_addr`.

## Phase 151 patch family (this change)

- **`IOLoop::step_blocking(timeout)`** — additive blocking drain. `step()` and
  the op/completion machinery are unchanged; `step_blocking` submits the same
  queued work, then the backend may sleep up to `timeout` for a completion or a
  doorbell wake (`None` blocks until a real event, safe because a doorbell is
  always armed).
- **`IOLoop::waker() -> IOWaker`** — a `Send + Sync + Clone` doorbell handle that
  is a **separate OS handle** (Linux `eventfd`, macOS `EVFILT_USER`, simulated
  condvar), not a clone of the `Rc<dyn IOLoop>`. A host thread may wake the loop
  but never touches backend state.
- **Linux reads/writes use io_uring's default fast-poll** — dropped
  `MSG_DONTWAIT` from the `recv`/`recv_buf`/`send`/`send_owned` ops. This is a
  workload choice, not a defect fix. Upstream sets `MSG_DONTWAIT` and retries on
  `EAGAIN` — a busy-poll loop that is optimal for an *always-busy* server (the
  parakernel/TigerBeetle target), where you never idle and never want to pay a
  wakeup. tina is a general runtime that *idles* between requests, so it wants
  io_uring's default async mode: omit `MSG_DONTWAIT` and let fast-poll arm the
  poll. Crucially the two are **identical under load** — when data is ready the
  recv completes inline either way; they only differ when there is no data, i.e.
  at idle, where fast-poll lets the worker block on the kernel instead of
  spinning. So this changes nothing in the regime upstream's design operates in;
  it only defines the idle gap that upstream's always-busy target never enters.
  (macOS/kqueue already arms a real `EVFILT` watch, so it is unaffected.)
- Doorbell coalescing truth is an `AtomicBool` that only `step_blocking` clears;
  the kernel interrupt (eventfd / `EVFILT_USER`) only unblocks a waiting kernel
  call. `step()` may observe and skip the doorbell event but never clears the
  coalescing flag, so a wake that arrives during a non-blocking drain still wakes
  the next `step_blocking` (no lost wakeup).

No broad re-vendor was performed for Phase 151: the blocking wait and waker are
additive to the existing fork.

## Re-vendoring

Do not re-vendor to upstream tip unless a specific upstream change is required.
If one is, make the re-vendor its own isolated commit (prove the workspace
builds and tests pass) **before** layering tina-local patches back on, and
update the upstream commit reference above.
