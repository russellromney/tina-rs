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
# A `{ path = "../x" }` dep with no `version =` publishes a broken crate.
# `cargo package` rejects it, but only for crates that can package today, so
# grep the raw manifests to cover every workspace crate. Every internal path
# dep in this repo is a one-line inline table, so a same-line check is
# sufficient. Scope to WORKSPACE-member manifests only: the `examples/**`
# crates are separate, unpublished workspaces that legitimately path-dep
# without a version.
# `while read` into an array (not `mapfile`): macOS ships bash 3.2, which has
# no `mapfile`.
member_manifests=()
while IFS= read -r line; do
    [[ -n "$line" ]] && member_manifests+=("$line")
done < <(
  cargo metadata --format-version 1 --no-deps --manifest-path "$MANIFEST" 2>/dev/null | jq -r --arg except "$EXCEPT" '
    .packages[] | select(.name != $except) | .manifest_path
  '
)
dep_offenders="$(
  { [[ ${#member_manifests[@]} -gt 0 ]] && grep -Hn 'path = "\.\./' "${member_manifests[@]}" || true; } \
    | grep -v 'version =' \
    | grep -v "vendor-$EXCEPT" \
    || true
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
