#!/usr/bin/env bash
# Prevent public examples from drifting back to panic-on-startup constructors.
set -euo pipefail

cd "$(dirname "$0")/.."

readonly runtime_pattern='(Threaded(MultiShard)?Runtime::(new|with_config[[:alnum:]_]*)|BridgeHost::new)\b'
readonly local_system_pattern='LocalSystem::(?:(?!;).)*?\.build\s*\('

scan_runtime() {
    rg -n "$runtime_pattern" "$@" \
        --glob '*.rs' \
        --glob '!**/tests/**' \
        --glob '!**/benches/**' || true
}

scan_local_system() {
    find "$@" -type f -name '*.rs' \
        ! -path '*/target/*' ! -path '*/tests/*' ! -path '*/benches/*' -print0 \
        | xargs -0 perl -0777 -ne '
            while (/'"$local_system_pattern"'/sg) {
                my $prefix = substr($_, 0, $-[0]);
                my $line = 1 + ($prefix =~ tr/\n//);
                print "$ARGV:$line: infallible LocalSystem::...build()\n";
            }
        ' || true
}

scan_runtime_aliases() {
    find "$@" -type f -name '*.rs' \
        ! -path '*/target/*' ! -path '*/tests/*' ! -path '*/benches/*' -print0 \
        | xargs -0 perl -0777 -ne '
            my @aliases = /\btype\s+([A-Za-z_][A-Za-z0-9_]*)\s*=\s*Threaded(?:MultiShard)?Runtime\b/sg;
            for my $alias (@aliases) {
                pos($_) = 0;
                while (/\b\Q$alias\E::(?:new|with_config[[:alnum:]_]*)\b/sg) {
                    my $prefix = substr($_, 0, $-[0]);
                    my $line = 1 + ($prefix =~ tr/\n//);
                    print "$ARGV:$line: infallible runtime alias constructor\n";
                }
            }
        ' || true
}

if [[ "${1:-}" == "--self-test" ]]; then
    fixtures="$(mktemp -d)"
    trap 'rm -rf "$fixtures"' EXIT
    mkdir -p "$fixtures/src" "$fixtures/tests"
    printf '%s\n' 'fn bad() { ThreadedRuntime::new(S, F); }' >"$fixtures/src/threaded.rs"
    printf '%s\n' 'fn bad() { BridgeHost::new(S, F, C); }' >"$fixtures/src/bridge_host.rs"
    printf '%s\n' \
        'type Runtime = ThreadedMultiShardRuntime<S, F>;' \
        'fn bad() { Runtime::with_config(shards(), F, C); }' \
        >"$fixtures/src/alias.rs"
    {
        printf '%s\n' 'fn bad() {' '    LocalSystem::single_shard(S, F)'
        for _ in {1..80}; do
            printf '%s\n' '        .configure(|config| config)'
        done
        printf '%s\n' '        .build();' '}'
    } >"$fixtures/src/local_system.rs"
    printf '%s\n' \
        'fn good() { ThreadedRuntime::try_new(S, F); }' \
        'fn also_good() { LocalSystem::single_shard(S, F).try_build(); }' \
        >"$fixtures/src/fallible.rs"
    printf '%s\n' 'fn fixture() { ThreadedRuntime::new(S, F); }' >"$fixtures/tests/fixture.rs"

    runtime_test_hits="$(scan_runtime "$fixtures")"
    alias_test_hits="$(scan_runtime_aliases "$fixtures")"
    local_test_hits="$(scan_local_system "$fixtures")"
    [[ "$(printf '%s\n' "$runtime_test_hits" | grep -c threaded.rs)" -eq 1 ]]
    [[ "$(printf '%s\n' "$runtime_test_hits" | grep -c bridge_host.rs)" -eq 1 ]]
    [[ "$(printf '%s\n' "$alias_test_hits" | grep -c alias.rs)" -eq 1 ]]
    [[ "$(printf '%s\n' "$local_test_hits" | grep -c local_system.rs)" -eq 1 ]]
    [[ "$runtime_test_hits$alias_test_hits$local_test_hits" != *fallible.rs* ]]
    [[ "$runtime_test_hits$alias_test_hits$local_test_hits" != *fixture.rs* ]]
    echo "examples startup API guard self-test: ok"
    exit 0
fi

runtime_hits="$(scan_runtime examples)"
alias_hits="$(scan_runtime_aliases examples)"
local_system_hits="$(scan_local_system examples)"

if [[ -n "$runtime_hits" || -n "$alias_hits" || -n "$local_system_hits" ]]; then
    echo "examples startup API guard: infallible production startup found" >&2
    [[ -z "$runtime_hits" ]] || printf '%s\n' "$runtime_hits" >&2
    [[ -z "$alias_hits" ]] || printf '%s\n' "$alias_hits" >&2
    [[ -z "$local_system_hits" ]] || printf '%s\n' "$local_system_hits" >&2
    echo "Use Threaded*Runtime::try_* or LocalSystem::...try_build() and propagate StartupError." >&2
    exit 1
fi

echo "examples startup API guard: ok"
