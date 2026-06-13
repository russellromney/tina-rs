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

## Phase 157 explicit-step I/O purity

Phase 151 briefly added a readiness-driven worker park to this fork. Phase 157
removed that experiment and restored Betelgeuse's explicit completion loop:
`IOLoop::step()` is the only progress primitive, and it never sleeps.

Linux socket ops again always use `MSG_DONTWAIT` for `recv`, `recv_buf`, `send`,
and `send_owned`; send paths continue to include `MSG_NOSIGNAL`. Threaded Tina
workers observe I/O completion by explicitly stepping and using their bounded
idle re-poll policy. Do not reintroduce a wake callback, doorbell, blocking
`step`, or readiness park here unless the readiness observation is first modeled
as ordinary completion/event work in Tina and the simulator.

## Re-vendoring

Do not re-vendor to upstream tip unless a specific upstream change is required.
If one is, make the re-vendor its own isolated commit (prove the workspace
builds and tests pass) **before** layering tina-local patches back on, and
update the upstream commit reference above.
