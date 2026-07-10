#!/usr/bin/env bash
#
# Packaging-readiness guard (crates.io 0.1.0 prep).
#
# Backstops the two gaps in the `cargo package --no-verify` calls the
# Makefile runs alongside this:
#   1. Missing description/license/repository is only a WARNING from
#      `cargo package` (exit 0), never a failure — so a crate can lose its
#      publish metadata and the packaging job stays green.
#   2. `cargo package` catches a versionless `{ path = "../x" }` dep ("all
#      dependencies must have a version requirement specified when
#      packaging"), but only for the three crates that can package today
#      (tina-macros/tina-rpc-macros/tina-codec). A versionless internal
#      path dep in any OTHER workspace crate (all of which transitively pull
#      the unpublished vendored betelgeuse and so cannot package at all)
#      goes unchecked.
# This guard asserts both, loudly, across every workspace crate.
#
# Enforced on every workspace crate except the vendored `betelgeuse` fork
# (living at ../vendor-betelgeuse) — the documented "open Betelgeuse
# question": it has no crates.io release, so it is deliberately versionless
# and unpublished. See tina-runtime/Cargo.toml and Makefile `verify-packaging`.
#
set -euo pipefail

cd "$(dirname "$0")/.."

# Overridable so the self-test can point at a fixture workspace.
MANIFEST="${PACKAGING_GUARD_MANIFEST:-Cargo.toml}"
# The one documented exception (vendored, unpublished, versionless).
EXCEPT="${PACKAGING_GUARD_EXCEPT:-betelgeuse}"

status=0

# --- 1. required publish metadata (description/license/repository) ----------
# `cargo metadata` resolves workspace inheritance (license.workspace = true),
# so this is the honest published-value check. A missing field is null/"".
meta_offenders="$(
  cargo metadata --format-version 1 --no-deps --manifest-path "$MANIFEST" 2>/dev/null | jq -r --arg except "$EXCEPT" '
    .packages[]
    | select(.name != $except)
    | . as $p | $p.name as $n
    | ( if ($p.description // "") == "" then "\($n): missing description" else empty end ),
      ( if ($p.license == null and $p.license_file == null) then "\($n): missing license" else empty end ),
      ( if ($p.repository // "") == "" then "\($n): missing repository" else empty end )
  '
)"

if [[ -n "$meta_offenders" ]]; then
    status=1
    echo "packaging guard: crate(s) missing required publish metadata:" >&2
    echo "$meta_offenders" | sed 's/^/  /' >&2
    echo >&2
    echo "  crates.io requires description + license + repository. cargo only" >&2
    echo "  WARNS on these, so add the missing field to the crate's Cargo.toml" >&2
    echo "  (or workspace-inherit it) before it can be published." >&2
    echo >&2
fi

# --- 2. versionless internal path deps --------------------------------------
# A `{ path = "../x" }` normal/build dep with no `version =` publishes a broken
# crate. `cargo package` rejects it, but only for crates that can package
# today, so read `cargo metadata` to cover every workspace member.
#
# Use metadata, NOT a raw manifest grep, so dev-dependencies are excluded:
# `cargo publish` strips a path-only dev-dep from the published manifest, so a
# versionless path DEV-dep is legitimate (cargo package exits 0 on it) and must
# NOT be flagged. In metadata a normal dep is kind=null, build dep kind="build",
# dev dep kind="dev"; a path dep has a non-null `path`; and no `version =` shows
# up as `req == "*"`. So the offenders are exactly the non-dev path deps with
# req "*" (minus the documented $EXCEPT betelgeuse path dep). This also covers
# target-specific `[target.'cfg(...)'.dependencies]`, which metadata flattens in.
dep_offenders="$(
  cargo metadata --format-version 1 --no-deps --manifest-path "$MANIFEST" 2>/dev/null | jq -r --arg except "$EXCEPT" '
    .packages[]
    | select(.name != $except)
    | .name as $pkg | .manifest_path as $mp
    | .dependencies[]
    | select(.kind != "dev")
    | select(.path != null)
    | select(.req == "*")
    | select(.name != $except)
    | "\($mp): dependency `\(.name)` has a path but no version requirement"
  '
)"

if [[ -n "$dep_offenders" ]]; then
    status=1
    echo "packaging guard: internal path dependenc(ies) with no version requirement:" >&2
    echo "$dep_offenders" | sed 's/^/  /' >&2
    echo >&2
    echo "  A path-only internal dep with no version publishes a broken" >&2
    echo "  crate. Add a matching version, e.g." >&2
    echo "  { path = \"../tina\", version = \"0.1.0\" }." >&2
    echo >&2
fi

if [[ "$status" -eq 0 ]]; then
    echo "packaging guard: ok (all publishable crates carry description/license/repository and versioned path deps)"
fi

exit "$status"
