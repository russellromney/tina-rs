#!/usr/bin/env bash
#
# Race-surface guard.
#
# Fails when the set of core-crate library files using a shared-memory
# synchronization primitive drifts from the reviewed allowlist. A new
# primitive in an unlisted file (or a listed file that no longer has one)
# forces a human to look, classify it, and add a loom/shuttle model for any
# genuine cross-thread structure before it lands.
#
# Surrogate proof: catches additions to the surface; does not prove the
# existing set race-free (the per-structure loom models do that). See
# `.intent/race-surface-allowlist.txt` and `.intent/SYSTEM.md`
# ("Shared-memory race surface").
#
set -euo pipefail

cd "$(dirname "$0")/.."

# Overridable for the self-test (tina-runtime/tests/race_surface_guard.rs),
# which points these at a temp fixture. Defaults are the real surface.
ALLOWLIST="${RACE_GUARD_ALLOWLIST:-.intent/race-surface-allowlist.txt}"

# Shared-nothing semantic core only. Bridges are a separate trusted-edge
# boundary; examples/tests are not shipped. (See the allowlist header.)
if [[ -n "${RACE_GUARD_SRC_DIRS:-}" ]]; then
    read -ra CORE_SRC <<< "$RACE_GUARD_SRC_DIRS"
else
    CORE_SRC=(
        tina/src
        tina-runtime/src
        tina-sim/src
        tina-mailbox-spsc/src
        tina-supervisor/src
    )
fi

# Shared-memory primitives that must be on the allowlist:
#   - UnsafeCell (manual interior mutability),
#   - unsafe impl Send|Sync (manual cross-thread claim),
#   - the std/loom atomic import path, and
#   - the atomic type names directly, so usage through a re-export that hides
#     the `sync::atomic` token is still caught.
PATTERN='UnsafeCell|unsafe impl (Send|Sync)|sync::atomic|Atomic(Bool|I8|I16|I32|I64|Isize|U8|U16|U32|U64|Usize|Ptr)'

if [[ ! -f "$ALLOWLIST" ]]; then
    echo "race-surface guard: missing allowlist $ALLOWLIST" >&2
    exit 1
fi

# Files in core src/ that contain a primitive — in real code, not just a
# comment, and not an in-src `tests.rs` module (test code is not shipped
# surface, like examples). A line is "real" if it matches the pattern and is
# not a `//`/`///` comment line.
found=""
while IFS= read -r file; do
    case "$(basename "$file")" in
        tests.rs) continue ;;
    esac
    if grep -E "$PATTERN" "$file" | grep -qvE '^[[:space:]]*//'; then
        found+="$file"$'\n'
    fi
done < <(grep -RlE "$PATTERN" --include='*.rs' "${CORE_SRC[@]}" 2>/dev/null || true)
found="$(printf '%s' "$found" | grep -v '^$' | sort -u || true)"

# Allowlisted paths: drop comments/blank lines, take the first field.
# `|| true`: an all-comment (or empty) allowlist is a valid "no surface" state,
# not a pipeline failure under `set -o pipefail`.
allowed="$(grep -vE '^[[:space:]]*(#|$)' "$ALLOWLIST" | awk '{print $1}' | sort -u || true)"

# New primitives off the allowlist.
new_offlist="$(comm -23 <(printf '%s\n' "$found" | grep -v '^$') <(printf '%s\n' "$allowed" | grep -v '^$') || true)"
# Allowlisted entries that no longer contain a primitive (rot).
stale="$(comm -13 <(printf '%s\n' "$found" | grep -v '^$') <(printf '%s\n' "$allowed" | grep -v '^$') || true)"

status=0

if [[ -n "$new_offlist" ]]; then
    status=1
    echo "race-surface guard: NEW shared-memory primitive in unlisted file(s):" >&2
    printf '  %s\n' $new_offlist >&2
    echo >&2
    echo "  A new UnsafeCell / unsafe impl Send|Sync / atomic appeared in core code." >&2
    echo "  Classify each file and add it to $ALLOWLIST. If it is a genuine" >&2
    echo "  cross-thread lock-free structure, add a loom/shuttle model first." >&2
    echo >&2
fi

if [[ -n "$stale" ]]; then
    status=1
    echo "race-surface guard: allowlist entr(ies) no longer use a primitive:" >&2
    printf '  %s\n' $stale >&2
    echo >&2
    echo "  Remove these from $ALLOWLIST so the inventory stays honest." >&2
    echo >&2
fi

if [[ "$status" -eq 0 ]]; then
    count="$(printf '%s\n' "$found" | grep -vc '^$' || true)"
    echo "race-surface guard: ok ($count core files match the allowlist)"
fi

exit "$status"
