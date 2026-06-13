# Phase 159: CI Dependency Hygiene

Status: planned.

## Goal

Make CI dependency resolution reproducible on pull requests, while still
learning about upstream dependency drift on purpose.

The transient `aws-smithy-types` / `time` failure exposed a policy gap: the
workspace currently verifies without a committed root `Cargo.lock`, so a PR can
fail because crates.io changed underneath it rather than because the branch
changed Tina. That is bad signal for correctness work. It is still useful to
know when fresh dependency resolution breaks, but that should be a named
scheduled check, not surprise noise in every PR gate.

## Decision

Per-PR and `main` CI should be locked.

The repository should commit a root workspace `Cargo.lock`, and the normal
workspace cargo commands should run with `--locked`. A separate scheduled
fresh-resolution job should intentionally ignore or regenerate the lockfile and
report ecosystem breakage.

## Non-Negotiables

- Do not add `--locked` to CI before committing the lockfile it needs.
- Do not hide fresh-resolution breakage forever. Keep one scheduled or manual
  job that proves the dependency graph can still resolve from current registry
  state.
- Do not mix "reproducible PR gate" and "fresh dependency canary" in one job.
  Their failures mean different things.
- Do not accidentally lock independent example workspaces without deciding
  whether their lockfiles are committed too.
- Do not update dependencies opportunistically in unrelated feature PRs.

## Build

### 1. Inventory workspaces and lockfile ownership

Record:

- root workspace crates covered by the root `Cargo.lock`;
- example crates that are independent workspaces;
- extension smoke crates under `examples/extensions`;
- whether any example already carries its own `Cargo.lock`;
- which CI and Makefile targets exercise each workspace.

### 2. Commit the root lockfile

Remove the root `Cargo.lock` ignore rule from `.gitignore`, then run
`cargo generate-lockfile` at the repository root and commit the resulting
`Cargo.lock` in its own dependency-hygiene PR.

Review expectations:

- no source changes in the lockfile commit;
- no broad dependency upgrades unless `cargo generate-lockfile` requires them;
- summarize any surprising transitive versions.

### 3. Lock the normal verification path

Update the root verification commands after the lockfile exists:

- `cargo check --workspace --locked`;
- `cargo test --workspace --locked`;
- `cargo clippy --workspace --all-targets --locked -- -D warnings`;
- `cargo doc --workspace --no-deps --locked`;
- Linux smoke tests in `.github/workflows/verify.yml` also use `--locked`.

Keep `cargo fmt` unchanged.

### 4. Decide example lock policy

For each independent example workspace, choose one:

- commit its own `Cargo.lock` and run example verification with `--locked`;
- leave it intentionally unlocked but move it to a scheduled/manual
  fresh-resolution lane;
- convert it into the root workspace if that is the real ownership model.

The decision should be documented in the Makefile comment near
`verify-examples`.

### 5. Add a fresh-resolution canary

Add a scheduled/manual workflow that runs from current registry state and makes
the signal explicit.

Good options:

- copy the tree to a temporary directory, remove `Cargo.lock`, then run
  `cargo update` / `cargo check --workspace`;
- or run `cargo update --locked` first to prove the committed lockfile is
  respected, then a separate unlocked resolve/check as the canary.

The job name should say "fresh dependency resolution" so failures are triaged
as ecosystem drift, not ordinary PR breakage.

## Acceptance

- Root `Cargo.lock` is committed.
- `.gitignore` no longer ignores the root `Cargo.lock`.
- Pull-request CI uses `--locked` for root workspace check/test/clippy/doc and
  Linux smoke tests.
- There is a scheduled or manual fresh-resolution job.
- Example workspace lock policy is written down and reflected in
  `verify-examples`.
- A dependency bump PR has a clear recipe: run `cargo update -p ...`, review the
  lockfile diff, and let locked CI prove the result.
