#!/usr/bin/env bash
# Keep framework-owned split-service envelopes out of application examples.
set -euo pipefail

cd "$(dirname "$0")/.."

scan() {
    find "$@" -type f -name '*.rs' \
        ! -path '*/target/*' ! -path '*/tests/*' ! -path '*/benches/*' \
        ! -name 'tokio_impl.rs' \
        -exec perl -0777 -ne '
            s{r(\#*)".*?"\1}{ my $literal = $&; $literal =~ s/[^\n]/ /g; $literal }gse;
            s{"(?:\\.|[^"\\])*"}{ my $literal = $&; $literal =~ s/[^\n]/ /g; $literal }gse;
            s{//[^\n]*}{ }g;
            s{/\*.*?\*/}{ my $comment = $&; $comment =~ s/[^\n]/ /g; $comment }gse;
            while (/\bServiceMessage\s*::\s*(?:Event|Request)\s*\(/g) {
                my $prefix = substr($_, 0, $-[0]);
                my $line = 1 + ($prefix =~ tr/\n//);
                print "$ARGV:$line: manual split-service envelope\n";
            }
        ' {} + || true
}

if [[ "${1:-}" == "--self-test" ]]; then
    fixtures="$(mktemp -d)"
    trap 'rm -rf "$fixtures"' EXIT
    mkdir -p "$fixtures/src" "$fixtures/tests" "$fixtures/target/generated"
    printf '%s\n' 'fn bad() { ServiceMessage::Event(Event::Start); }' >"$fixtures/src/bad.rs"
    cat >"$fixtures/src/good.rs" <<'EOF'
fn good(effect: Spawn) -> Effect<App> {
    effect.then_service_event_with_restarts(Event::Started, Event::Restarted)
}
const TEXT: &str = "ServiceMessage::Request(Request::Read)";
// ServiceMessage::Event(Event::Comment)
EOF
    printf '%s\n' 'fn fixture() { ServiceMessage::Event(Event::Test); }' >"$fixtures/tests/fixture.rs"
    printf '%s\n' 'fn generated() { ServiceMessage::Event(Event::Generated); }' >"$fixtures/target/generated.rs"
    hits="$(scan "$fixtures")"
    [[ "$(printf '%s\n' "$hits" | grep -c 'bad.rs')" -eq 1 ]]
    [[ "$hits" != *good.rs* ]]
    [[ "$hits" != *fixture.rs* ]]
    [[ "$hits" != *generated.rs* ]]
    echo "examples service envelope guard self-test: ok"
    exit 0
fi

hits="$(scan examples)"
if [[ -n "$hits" ]]; then
    echo "examples service envelope guard: manual envelope construction found" >&2
    printf '%s\n' "$hits" >&2
    echo "Use typed service-event, service-request, or continuation helpers." >&2
    exit 1
fi

echo "examples service envelope guard: ok"
