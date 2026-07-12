#!/usr/bin/env bash
# Prevent public examples from drifting back to panic-on-startup constructors.
set -euo pipefail

cd "$(dirname "$0")/.."

readonly runtime_pattern='Threaded(MultiShard)?Runtime::(new|with_config|with_config_and_trace_observer)\b'
runtime_hits="$({
    rg -n "$runtime_pattern" examples \
        --glob '*.rs' \
        --glob '!**/tests/**' \
        --glob '!**/benches/**' || true
})"

local_system_hits="$({
    find examples -type f -name '*.rs' \
        ! -path '*/tests/*' ! -path '*/benches/*' -print0 \
        | xargs -0 perl -0777 -ne '
            while (/LocalSystem::.{0,500}?\.build\s*\(/sg) {
                my $prefix = substr($_, 0, $-[0]);
                my $line = 1 + ($prefix =~ tr/\n//);
                print "$ARGV:$line: infallible LocalSystem::...build()\n";
            }
        ' || true
})"

if [[ -n "$runtime_hits" || -n "$local_system_hits" ]]; then
    echo "examples startup API guard: infallible production startup found" >&2
    [[ -z "$runtime_hits" ]] || printf '%s\n' "$runtime_hits" >&2
    [[ -z "$local_system_hits" ]] || printf '%s\n' "$local_system_hits" >&2
    echo "Use Threaded*Runtime::try_* or LocalSystem::...try_build() and propagate StartupError." >&2
    exit 1
fi

echo "examples startup API guard: ok"
