#!/usr/bin/env bash
# Prevent production examples from collapsing runtime and auxiliary teardown truth.
set -euo pipefail

cd "$(dirname "$0")/.."

scan() {
    find "$@" -type f -name '*.rs' \
        ! -path '*/target/*' ! -path '*/tests/*' ! -path '*/benches/*' \
        -exec perl -0777 -ne '
            # Remove literals before comments so `//` and lifecycle-looking
            # fixture text inside a string cannot corrupt or trigger a rule.
            s{r(\#*)".*?"\1}{ my $literal = $&; $literal =~ s/[^\n]/ /g; $literal }gse;
            s{"(?:\\.|[^"\\])*"}{ my $literal = $&; $literal =~ s/[^\n]/ /g; $literal }gse;
            s{//[^\n]*}{ }g;
            s{/\*.*?\*/}{ my $comment = $&; $comment =~ s/[^\n]/ /g; $comment }gse;
            my @rules = (
                [qr/\bArc::try_unwrap\s*\(\s*(?:[A-Za-z_][A-Za-z0-9_]*\.)*(?:rt|[A-Za-z_]*(?:runtime|system)[A-Za-z0-9_]*)\s*\)/i, "exclusive-owner runtime shutdown"],
                [qr/\b(?:rt|runtime|[A-Za-z_][A-Za-z0-9_]*(?:_rt|_runtime)|self\s*\.\s*runtime)\s*\.\s*shutdown\s*\(\s*\)/i, "transport-only runtime shutdown()"],
                [qr/\blet\s+_\s*=\s*[^;]*?\.\s*(?:join|shutdown)\s*\(/s, "ignored join/shutdown result"],
                [qr/\.\s*(?:join|shutdown)\s*\(\s*\)\s*\.\s*ok\s*\(/, "join/shutdown result converted to Option"],
                [qr/\blet\s+_\s*=\s*(?!(?:[A-Za-z_][A-Za-z0-9_]*)_(?:rx|receiver)\b)[A-Za-z_][A-Za-z0-9_]*\s*\.\s*await\b/s, "ignored task join result"],
                [qr/\blet\s+_\s*=\s*[^;]*?\.\s*close_and_drain\s*\(/s, "ignored bridge drain report"],
                [qr/\blet\s+_\s*=\s*[^;]*?try_send\s*\([^;]*?::(?:Stop|Shutdown)\b/s, "ignored service stop result"],
            );
            for my $rule (@rules) {
                my ($pattern, $label) = @$rule;
                pos($_) = 0;
                while ($_ =~ /$pattern/g) {
                    my $prefix = substr($_, 0, $-[0]);
                    my $line = 1 + ($prefix =~ tr/\n//);
                    print "$ARGV:$line: $label\n";
                }
            }
        ' {} + || true
}

if [[ "${1:-}" == "--self-test" ]]; then
    fixtures="$(mktemp -d)"
    trap 'rm -rf "$fixtures"' EXIT
    mkdir -p "$fixtures/src" "$fixtures/tests" "$fixtures/target/generated"
    cat >"$fixtures/src/bad.rs" <<'EOF'
fn a(runtime: Arc<R>) { let _ = Arc::try_unwrap(runtime); }
fn b(runtime: R) { let _ = runtime.shutdown(); }
fn c(worker: JoinHandle<()>) { let _ = worker.join(); }
fn d(runtime: &R, listener: A) { let _ = runtime.try_send(listener, Msg::Stop); }
fn e(worker: JoinHandle<()>) { worker.join().ok(); }
async fn f(server: JoinHandle<()>) { let _ = server.await; }
fn g(bridge: B) { let _ = bridge.close_and_drain(TIMEOUT); }
fn h(engine_runtime: Arc<R>) { let _ = Arc::try_unwrap(engine_runtime); }
EOF
    cat >"$fixtures/src/good.rs" <<'EOF'
fn good(runtime: R, shutdown: H) -> Result<()> {
    let terminal = shutdown.request_and_wait_report(TIMEOUT)?;
    drop(runtime);
    terminal.ensure_clean()?;
    Ok(())
}
const TEXT: &str = "let _ = runtime.shutdown(); // not code";
EOF
    printf '%s\n' 'fn fixture(runtime: R) { let _ = runtime.shutdown(); }' >"$fixtures/tests/fixture.rs"
    printf '%s\n' 'fn generated(runtime: R) { let _ = runtime.shutdown(); }' >"$fixtures/target/generated.rs"
    hits="$(scan "$fixtures")"
    [[ "$(printf '%s\n' "$hits" | grep -c 'bad.rs')" -eq 9 ]]
    [[ "$hits" != *good.rs* ]]
    [[ "$hits" != *fixture.rs* ]]
    [[ "$hits" != *generated.rs* ]]
    echo "examples shutdown truth guard self-test: ok"
    exit 0
fi

hits="$(scan examples)"
if [[ -n "$hits" ]]; then
    echo "examples shutdown truth guard: collapsed teardown outcome found" >&2
    printf '%s\n' "$hits" >&2
    echo "Observe bounded terminal truth, propagate explicit stop/join outcomes, and call ensure_clean()." >&2
    exit 1
fi

echo "examples shutdown truth guard: ok"
