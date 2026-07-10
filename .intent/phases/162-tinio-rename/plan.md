# Tinio rename

Rename the public identity from Tina to Tinio before crates.io publication.
Decided 2026-07-09/10: `tinio` keeps the Tina lineage in the name and reads as
a Rust I/O runtime (tokio/monoio/glommio family). It is also the publish
unblock: crates.io already has an unrelated `tina` crate (v0.0.2, 2023), so
the flagship cannot publish under the old name at all.

This is the LAST pre-publish phase. It touches every crate, example, and doc.
**Run it solo on a frozen main**: every other session (codex included) must
land or pause first, or this becomes a repo-wide merge war. One executor, one
PR, reviewed as a whole.

## What we build

1. **Code identity rename.** Crate names, directory names, Cargo `[package]`
   names + all internal dep references, `use tina*::` imports, macro paths
   (`#[tina_runtime::isolate]` → `#[tinio_runtime::isolate]`), thread names,
   tracing targets, README/docs/examples. Mapping is mechanical:
   `tina` → `tinio`, `tina-runtime` → `tinio-runtime`, `tina-sim` →
   `tinio-sim`, and so on for all 18 non-vendor crates.
2. **Fold the Betelgeuse fork into `tinio-runtime`** as a self-contained
   module directory (decided 2026-07-10, recorded in
   `vendor-betelgeuse/VENDOR.md`). `LICENSE.md` (MIT OR Apache-2.0) and
   `VENDOR.md` move WITH the code; `tinio-runtime`'s crate docs + README
   credit the origin (a fork of Pekka Enberg's Betelgeuse). Keep the module
   boundary crisp — one directory, ledger inside — so diffing against
   upstream stays a one-command operation. No separate `tinio-betelgeuse`
   crate is published.
3. **Persisted-identifier compatibility.** The versioned format strings
   `"tina-replay-case-v1"` and `"tina-protocol-byte-replay-v1"` DO NOT
   change — they name on-disk artifact formats, and renaming them breaks
   every saved replay case. They go on a permanent allowlist. Any future
   format bump (v2) takes the `tinio-` prefix.
4. **Env vars.** `TINA_*` (6 today: DRIVER_RUNTIME_CONTRACT, DST_LONG,
   LONG_SOAK_SECONDS, PERF_GIT_SHA, PERF_IDLE_REPOLL_US,
   PROTOCOL_SOAK_ITERS) rename to `TINIO_*`. These are dev/test knobs, not
   user contracts — rename outright, no dual-read shim, update Makefile/docs.
5. **Temporary CI inventory guard.** A check that fails on NEW `tina`
   identifiers in source, with an explicit allowlist: the two format strings,
   lineage prose (README/docs may say "inspired by Peter Mbanugo's Tina"),
   `.intent/` (historical record — NEVER renamed), `CHANGELOG.md` history,
   and `tina.png` (rename the asset if a new logo exists; otherwise
   allowlist). Remove the guard once stable.
6. **Publish follow-through.** Update the `packaging` CI job + Makefile
   `verify-packaging` for the new names; re-verify the publication order from
   PR #283 under the fold-in (the Betelgeuse cut disappears — `tinio-runtime`
   becomes packageable); verify name availability on crates.io for every
   `tinio-*` crate (use a real User-Agent; the API rejects bare curl);
   update crate descriptions with the lineage note.

## Open decisions (settle with the human before executing)

- GitHub repo name: keep `tina-rs` or rename to `tinio` (GitHub redirects old
  URLs either way).
- The hero image / project logo.
- Whether the root facade crate is `tinio` (recommended: yes — it also
  reserves the name).

## What must NOT change

- **Behavior. Zero logic changes.** This phase is names only. Any diff hunk
  that changes control flow is scope creep — reject it.
- **Saved replay artifacts still load.** The two format strings unchanged,
  plus a test that loads a pre-rename saved artifact and replays it
  byte-identically post-rename.
- **Golden/DST traces.** Byte-identical is the target. If a trace event name
  or tracing target embeds a crate name and the rename unavoidably shifts a
  golden hash, that is an explicit, recorded rebless in this plan's log —
  never silent. Prefer keeping the embedded string stable where it is a
  format-like contract.
- **`.intent/` and `CHANGELOG.md` history** — never rewritten. They say
  "tina" forever; that is what history means.
- **Lineage honesty.** README keeps (and sharpens) the "independent Rust
  implementation inspired by Peter Mbanugo's Tina" section. The rename must
  not erase the credit.
- **trybuild compile-fail fixtures** (`*.stderr`) embed crate names — they
  must be REGENERATED (`TRYBUILD=overwrite`), then diffed by eye to confirm
  only names changed, not diagnostics.

## How we prove it

- Full `cargo test --workspace --locked` green under the new names, plus the
  three static gates (fmt, clippy `-D warnings`, doc `-D warnings`).
- Every standalone example builds and tests (the CI `systems-examples` job,
  `make SHELL=/bin/bash verify-examples`, and the sweep of all ~72 example
  manifests — the rename edits every one of their imports and path deps).
- `make SHELL=/bin/bash verify-packaging` green for the renamed crates;
  `cargo package --no-verify` for `tinio-runtime` now passes (fold-in
  removes the path-dep blocker).
- The pre-rename replay artifact loads and replays byte-identically.
- The CI inventory guard passes: zero `tina` identifiers outside the
  allowlist.
- The Betelgeuse module still diffs cleanly against upstream tip
  (`6d1f137`): residual == documented patch families, proving the fold-in
  didn't smuggle changes.

## Traps (greppable, wrong vs right)

- **Blind sed.** Wrong: repo-wide `s/tina/tinio/`. It corrupts lineage prose,
  history, the format strings, `.intent/`, and Pekka's URLs. Right: rename by
  category (Cargo names → dirs → imports → macros → env vars → docs), with
  the allowlist checked between passes.
- **Path deps in examples.** Every standalone example's Cargo.toml points at
  `../../tina-runtime` etc. — renamed directories break them all. The
  example sweep is the proof, not optional.
- **Doc intra-links.** `[`tina::Effect`]`-style links break under rename and
  fail the doc gate — the doc gate run is load-bearing, not a formality.
- **Macro re-export paths.** The `#[tinio_runtime::isolate]` attribute must
  resolve from examples that import via the facade too — check both import
  styles compile.
- **Half-rename.** A state where some crates are `tinio-*` and some `tina-*`
  does not build and cannot be left overnight. The rename lands as ONE PR.

## Register (explicitly out of scope here)

- The `tinio::prelude` curated facade / module reorganization (the external
  review's API-surface recommendation) — separate ergonomics phase, likely
  informed by the codex ergonomics pass. The rename must not wait for it.
- Publishing itself (`cargo publish` sequence) — its own phase after this
  one, with the release-candidate exercise from the external review.
- Outreach to Pekka Enberg — explicitly dropped per the human's decision
  (2026-07-10); attribution in code/docs is the chosen path.
