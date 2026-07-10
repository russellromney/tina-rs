# Betelgeuse vendor provenance

`vendor-betelgeuse/` is a **vendored and tina-maintained fork** of Betelgeuse,
a completion-based I/O library (no runtime, no executor, no hidden tasks).

## Upstream

- Project: penberg/betelgeuse (Pekka Enberg)
- License: MIT OR Apache-2.0 (see `LICENSE.md`)
- Upstream has no tagged releases.
- **Vendored upstream commit: `6d1f13767afe0a331933a4abbd5afe3bbbe1ed5a`**
  ("Add tina-rs to README.md (#9)", upstream tip at the 2026-07-09 re-vendor).
- The original vendor point (never recorded at the time) was reconstructed as
  `97f4f40a768dc3c5a2f9d41b5e7fb6a9b80008a2` ("Add connect operation to
  IOSocket (#8)", 2026-05-02) by diffing candidate upstream trees against this
  directory until the residual equaled exactly the tina patch set below.
- Not vendored: upstream `.github/` and `.gitignore`. Everything else tracks
  upstream verbatim plus the patches below. `VENDOR.md` is tina-local.

## Why a fork, not a dependency

Upstream is pre-release with no semver and no waker/blocking-wait surface (that
is deliberate upstream — a TigerBeetle-style step loop). Tina needs a small set
of substrate features upstream does not provide, so we vendor and patch rather
than pin a moving git dependency.

## tina-local patch families

One re-apply commit per family in the 2026-07-09 re-vendor series; diff this
directory against the upstream commit above to regenerate any of them.

- **Simulated I/O backend** — `io::simulated` implements the substrate traits
  for TCP with no kernel calls (scriptable completion delay, partial writes).
  tina-runtime substrate tests and tina-sim parity suites run on it.
- **Connect raw-sockaddr rework** — `ConnectOp` carries a prepared
  `sockaddr_storage` + length (not a `SocketAddr`) so one op serves internet
  and Unix connects. Darwin drops upstream's EALREADY/getsockopt-EINTR retry
  states: EINPROGRESS/WouldBlock parks on kqueue, SO_ERROR decides.
- **Unix sockets on the substrate** — `bind_unix` / `connect_unix`, accepted
  streams as ordinary stream sockets (#207, retires bypass-Betelgeuse lanes).
  Backends own the socket-file lifecycle (stale-inode clear, unlink on close).
- **Native hot-path buffer reuse** — `recv_buf` / `send_owned` caller-owned
  buffer round-trips, buffer returned on error paths too (#217).
- **Completion release hooks** — `pending_completion_count` /
  `cancel_pending_completions` for runtime-driven shutdown without leaving
  backend-owned raw pointers (darwin watched-registry, linux AsyncCancel).
- **`F_FULLFSYNC`** on Darwin file sync (durability; ENOTSUP falls back to
  fsync).
- **Honest socket address introspection** — `local_addr` / `peer_addr`.
- **Lint/feature hygiene** — drop unused nightly features, satisfy the
  workspace's clippy -D warnings gate.
- **Owned positional writes + cursor sends (#285)** — `pwrite_owned` /
  `pwrite_owned_from` / `send_owned_from` and `PWriteOwnedCompletion`;
  `SendOp`/`PWriteOp` carry a `start` cursor so retries resend only the
  unwritten suffix while the completion returns the whole allocation on
  success and failure. Also from #285: file ranges validated against
  `off_t` before arming, io_uring lengths clamp to `u32::MAX` instead of
  wrapping, and darwin `cancel_pending_completions` keeps each watch
  recorded until its `EV_DELETE` succeeds (a failed delete no longer
  orphans ownership bookkeeping for the remaining watches). Landed after
  the 2026-07-09 re-vendor, so no re-apply commit exists for this family;
  diff against the upstream commit above to regenerate it.

Parallel substrate lanes (cloned `IOLoopHandle` sharing one loop) need no
patch: upstream's `IOLoopHandle` already derives `Clone`; tina merely uses it.

## Explicit-step I/O purity

An earlier experiment added a readiness-driven worker park to this fork; it
was removed and the fork is poll/step-pure: `IOLoop::step()` is the only
progress primitive, and it never sleeps. Keep it that way — no waker, park,
or readiness side-channel may return in a re-vendor or patch.

Linux socket ops always use `MSG_DONTWAIT` for `recv`, `recv_buf`, `send`,
and `send_owned`; send paths include `MSG_NOSIGNAL`. Threaded Tina workers
observe I/O completion by explicitly stepping and using their bounded idle
re-poll policy.

## Re-vendoring

Do not re-vendor to upstream tip unless a specific upstream change is required.
If one is, follow the 2026-07-09 series shape: one isolated commit bringing
this directory to verbatim upstream tip (it will not build — say so in the
message), then one commit per patch family re-applied, then update the
upstream commit hash above and prove the workspace green.

## Publish plan (decided 2026-07-10)

This fork never publishes to crates.io as its own crate. When the workspace
publishes (the Tinio rename/0.1.0), this directory folds into the runtime
crate as a module — vendored in the plain sense. `LICENSE.md` (MIT OR
Apache-2.0) and this file travel with the code, and the runtime crate's
docs/metadata credit the origin: a fork of Pekka Enberg's Betelgeuse. That
satisfies both licenses (retain license text + attribution). No upstream
contributions are planned as part of publishing; the per-family commits from
the 2026-07-09 re-vendor remain exportable if that ever changes.
