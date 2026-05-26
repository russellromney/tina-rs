#!/usr/bin/env bash
#
# Runtime-owned rail bypass guard.
#
# Fails when the set of `tina-runtime/src/driver` files that use a bypass
# primitive — a worker thread, a blocking std stream socket, or blocking
# std::fs work — drifts from the reviewed inventory. A new bypass primitive in
# an unlisted file (or a listed file that no longer has one) forces a human to
# classify the rail and update the runtime capability report's `RailClass`
# before it lands.
#
# This is the substrate-honesty companion to the race-surface guard. It
# enforces the Phase 140 rule: every runtime-owned rail either rides the
# per-shard Betelgeuse completion substrate or is inventoried here with a
# written reason. See `.intent/runtime-rail-inventory.txt`.
#
set -euo pipefail

cd "$(dirname "$0")/.."

# Overridable for the self-test (tina-runtime/tests/rail_inventory_guard.rs),
# which points these at a temp fixture. Defaults are the real surface.
INVENTORY="${RAIL_INVENTORY:-.intent/runtime-rail-inventory.txt}"

if [[ -n "${RAIL_GUARD_SRC_DIRS:-}" ]]; then
    read -ra DRIVER_SRC <<< "$RAIL_GUARD_SRC_DIRS"
else
    DRIVER_SRC=(tina-runtime/src/driver)
fi

# Bypass primitives a runtime-owned rail must not add silently:
#   - thread::spawn / thread::Builder  (a worker thread; thread::sleep is fine),
#   - std::os::unix::net               (blocking std Unix-domain sockets),
#   - std::net::TcpListener/TcpStream  (blocking std TCP; the `net::` segment
#     keeps it from matching the `CallInput::Tcp*` variant names), and
#   - blocking std::fs work            (OpenOptions / rename / remove* /
#     read_dir / metadata / symlink_metadata / create_dir).
PATTERN='thread::spawn|thread::Builder|os::unix::net|net::TcpListener|net::TcpStream|fs::OpenOptions|fs::read_dir|fs::rename|fs::remove_file|fs::remove_dir|fs::metadata|fs::symlink_metadata|fs::create_dir'

if [[ ! -f "$INVENTORY" ]]; then
    echo "rail-inventory guard: missing inventory $INVENTORY" >&2
    exit 1
fi

# Files in the driver dir that contain a primitive in real code, not just a
# comment. A line is "real" if it matches the pattern and is not a line
# comment or block-comment body line.
found=""
while IFS= read -r file; do
    # In-`src` `tests.rs` modules are test code, not shipped rail surface.
    case "$(basename "$file")" in
        tests.rs) continue ;;
    esac
    if grep -E "$PATTERN" "$file" | grep -qvE '^[[:space:]]*(//|/\*|\*)'; then
        found+="$file"$'\n'
    fi
done < <(grep -RlE "$PATTERN" --include='*.rs' "${DRIVER_SRC[@]}" 2>/dev/null || true)
found="$(printf '%s' "$found" | grep -v '^$' | sort -u || true)"

# Inventoried paths: drop comments/blank lines, take the first field.
# `|| true`: an all-comment (or empty) inventory is a valid "all rails ride the
# substrate" state, not a pipeline failure under `set -o pipefail`.
allowed="$(grep -vE '^[[:space:]]*(#|$)' "$INVENTORY" | awk '{print $1}' | sort -u || true)"

new_offlist="$(comm -23 <(printf '%s\n' "$found" | grep -v '^$') <(printf '%s\n' "$allowed" | grep -v '^$') || true)"
stale="$(comm -13 <(printf '%s\n' "$found" | grep -v '^$') <(printf '%s\n' "$allowed" | grep -v '^$') || true)"

status=0

if [[ -n "$new_offlist" ]]; then
    status=1
    echo "rail-inventory guard: NEW bypass primitive in unlisted driver file(s):" >&2
    printf '  %s\n' $new_offlist >&2
    echo >&2
    echo "  A worker thread / blocking std socket / blocking std::fs call appeared" >&2
    echo "  in a runtime-owned rail that is not inventoried. Classify the rail" >&2
    echo "  (fallback-worker or justified-blocking-lane), add it to $INVENTORY," >&2
    echo "  and update its RailClass in tina-runtime/src/capabilities.rs. If the" >&2
    echo "  rail can ride the Betelgeuse substrate instead, do that." >&2
    echo >&2
fi

if [[ -n "$stale" ]]; then
    status=1
    echo "rail-inventory guard: inventory entr(ies) no longer use a bypass primitive:" >&2
    printf '  %s\n' $stale >&2
    echo >&2
    echo "  These rails now ride the substrate (or moved). Remove them from" >&2
    echo "  $INVENTORY so the inventory stays honest." >&2
    echo >&2
fi

if [[ "$status" -eq 0 ]]; then
    count="$(printf '%s\n' "$found" | grep -vc '^$' || true)"
    echo "rail-inventory guard: ok ($count driver files match the inventory)"
fi

exit "$status"
