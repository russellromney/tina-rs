#!/usr/bin/env bash
# Public-corpus lexical guard.
#
# Rejects, in the public corpus:
#   - shared-state: Arc<Mutex> / Mutex<Option> / Condvar / atomics in
#     examples/**/src code lines (result-sidecar signatures);
#   - poll-loop: `loop { ... sleep(...) }` / `while ... { ... sleep(...) }`
#     result polling in examples/**/src (one brace-nesting level; deeper
#     nesting is a documented limit with no corpus instance);
#   - obsolete-vocabulary: raw-runtime API names in corpus markdown;
#   - intent-phrase: exact intent-artifact phrases anywhere in scanned text.
#
# Scans examples, docs, public crate sources, scripts, and the root files.
# Excludes .git, .intent, target, vendored/generated code, lockfiles, this
# script, and the structural guard's test file. Allowlist entries live in
# examples/public-corpus-allowlist.toml with path, narrow rule, reason,
# focused test, reviewer, and reviewed SHA; stale paths, stale entries, and
# missing roots fail closed. --self-test drives pass/fail/evasion fixtures
# (including a directory whose path contains spaces) from a temp dir.

set -euo pipefail

cd "$(dirname "$0")/.."

ALLOWLIST="${PUBLIC_CORPUS_ALLOWLIST:-examples/public-corpus-allowlist.toml}"
SCAN_ROOT="${PUBLIC_CORPUS_SCAN_ROOT:-.}"

command -v rg >/dev/null 2>&1 || {
    echo "public-corpus lexical guard: rg is required but not on PATH" >&2
    exit 1
}
command -v perl >/dev/null 2>&1 || {
    echo "public-corpus lexical guard: perl is required but not on PATH" >&2
    exit 1
}

# Single-pass Rust-ish lexer: masks line/block comments (nested), strings,
# chars, byte strings/chars, and raw strings with arbitrary hash counts.
# Regex substitution order is exploitable (`/* " */` and `/* // */`
# confusion); a state machine is not.
strip_perl='
sub mask_code {
    my ($s) = @_;
    my $out = q{};
    my ($i, $n) = (0, length $s);
    my ($state, $depth, $raw_hashes) = ("code", 0, 0);
    my $mask = sub { my ($t) = @_; $t =~ s/[^\n]/ /g; $t };
    while ($i < $n) {
        my $c = substr($s, $i, 1);
        my $two = substr($s, $i, 2);
        if ($state eq "code") {
            if ($two eq "//") { $state = "line"; $out .= "  "; $i += 2; }
            elsif ($two eq "/*") { $state = "block"; $depth = 1; $out .= "  "; $i += 2; }
            elsif ($c eq chr(34)) { $state = "string"; $out .= " "; $i += 1; }
            elsif ($two eq q{b"}) { $state = "string"; $out .= "  "; $i += 2; }
            elsif (substr($s, $i) =~ /\Abr(#*)"/) {
                $state = "raw"; $raw_hashes = length $1;
                my $tok = "br" . ("#" x $raw_hashes) . chr(34);
                $out .= $mask->($tok); $i += length $tok;
            }
            elsif (substr($s, $i) =~ /\Ar(#*)"/) {
                $state = "raw"; $raw_hashes = length $1;
                my $tok = "r" . ("#" x $raw_hashes) . chr(34);
                $out .= $mask->($tok); $i += length $tok;
            }
            elsif (substr($s, $i, 4) =~ /\Ab\x27(?:\\.|[^\\\x27])\x27/) {
                $state = "char"; $out .= " "; $i += 1;
            }
            elsif (substr($s, $i, 3) =~ /\A\x27(?:\\.|[^\\\x27])\x27/) {
                $state = "char"; $out .= " "; $i += 1;
            }
            else { $out .= $c; $i += 1; }
        } elsif ($state eq "line") {
            if ($c eq "\n") { $state = "code"; $out .= "\n"; }
            else { $out .= " "; }
            $i += 1;
        } elsif ($state eq "block") {
            if ($two eq "/*") { $depth++; $out .= "  "; $i += 2; }
            elsif ($two eq "*/") { $depth--; $out .= "  "; $i += 2; $state = "code" if $depth == 0; }
            else { $out .= $c eq "\n" ? "\n" : " "; $i += 1; }
        } elsif ($state eq "string") {
            if ($c eq "\\") { $out .= $mask->(substr($s, $i, 2)); $i += 2; }
            elsif ($c eq chr(34)) { $state = "code"; $out .= " "; $i += 1; }
            else { $out .= $c eq "\n" ? "\n" : " "; $i += 1; }
        } elsif ($state eq "char") {
            if ($c eq "\\") { $out .= "  "; $i += 2; }
            elsif ($c eq chr(39)) { $state = "code"; $out .= " "; $i += 1; }
            else { $out .= " "; $i += 1; }
        } elsif ($state eq "raw") {
            my $close = chr(34) . ("#" x $raw_hashes);
            if (substr($s, $i, length $close) eq $close) {
                $out .= $mask->($close); $i += length $close; $state = "code";
            } else { $out .= $c eq "\n" ? "\n" : " "; $i += 1; }
        }
    }
    return $out;
}
$_ = mask_code($_);
'

# Code-line hits in examples/**/src (tokio_impl.rs is the Tokio control).
scan_rs() { # $1 = perl program body evaluated per file, prints "file:line: detail"
    find "$SCAN_ROOT/examples" -type d -name target -prune -o -type f -name '*.rs' -print 2>/dev/null \
        | grep '/src/' \
        | grep -v 'tokio_impl\.rs$' \
        | while IFS= read -r file; do
            perl -0777 -ne "$1" "$file" 2>/dev/null | sed "s|^|$file:|"
        done
}

shared_state_hits() {
    scan_rs '
        '"$strip_perl"'
        while (/\b(?:Arc\s*<\s*(?:[A-Za-z_][A-Za-z0-9_]*::)*Mutex|Mutex\s*(?:<|::\s*new)|RwLock|Condvar|Atomic(?:Bool|I8|I16|I32|I64|Isize|U8|U16|U32|U64|Usize|Ptr))/g) {
            my $line = 1 + (substr($_, 0, $-[0]) =~ tr/\n//);
            my $hit = $&;
            $hit =~ s/\s+/ /g;
            print "$line: shared-state $hit\n";
        }
    '
}

poll_loop_hits() {
    scan_rs '
        '"$strip_perl"'
        while (/\b(?:loop|while)[^{]*\{(?:[^{}]|\{[^{}]*\})*?sleep\s*\(/gs) {
            my $line = 1 + (substr($_, 0, $-[0]) =~ tr/\n//);
            print "$line: poll-loop\n";
        }
    '
}

OBSOLETE_PATTERN='\bThreadedRuntime\b|\bThreadedMultiShardRuntime\b|\bMultiShardRuntime\b|\bbuild_keepalive_pool\b|\bshutdown_keepalive_pool\b|\bregister_with_capacity[a-z_]*|\brequest_and_wait_report\b|\bshutdown_report\(\)'

obsolete_vocabulary_hits() {
    rg --no-messages -n -e "$OBSOLETE_PATTERN" \
        --glob '*.md' \
        "$SCAN_ROOT/examples" "$SCAN_ROOT/docs" "$SCAN_ROOT/README.md" 2>/dev/null \
        | sed 's/:/: obsolete-vocabulary:/2'
}

INTENT_PATTERN='Public Example Certification|163-public-example-certification|execute\.md|review\.md|Execution Review 1|Execution Review 2'

intent_phrase_hits() {
    rg --no-messages -n -e "$INTENT_PATTERN" \
        "$SCAN_ROOT/examples" "$SCAN_ROOT/docs" "$SCAN_ROOT/scripts" \
        "$SCAN_ROOT/Makefile" "$SCAN_ROOT/README.md" \
        "$SCAN_ROOT"/tina*/src 2>/dev/null \
        | grep -v 'scripts/public_corpus_lexical_guard\.sh:' \
        | grep -v 'tina-runtime/tests/public_corpus_guard\.rs:' \
        | grep -v 'public-corpus-allowlist\.toml:' \
        | sed 's/:/: intent-phrase:/2'
}

# Allowlist entries for one rule as "path" lines.
allowlisted_paths() { # $1 = rule
    awk -v rule="$1" '
        /^\[\[entry\]\]/ { in_entry = 1; path = ""; erule = "" }
        in_entry && /^path = /    { gsub(/path = |"/, ""); path = $0 }
        in_entry && /^rule = /    { gsub(/rule = |"/, ""); erule = $0 }
        in_entry && /^$/ { in_entry = 0 }
        in_entry && erule != "" && path != "" && erule == rule { print path; in_entry = 0 }
    ' "$ALLOWLIST" | sort -u
}

# Normalize a hit line "path:line: detail" to its exact repo-relative path.
# Splits at the first :<digits>: (the line number); corpus paths must not
# contain a :<digits>: segment (none exist today).
hit_paths() {
    sed -E -e 's|^\./||' -e 's|:[0-9]+:.*$||'
}

# Lines "path: rest" minus entries whose EXACT path is allowlisted for $1.
without_allowlisted() { # stdin hits, $1 = rule
    local allowed hit hp
    allowed="$(allowlisted_paths "$1")"
    while IFS= read -r hit; do
        [[ -n "$hit" ]] || continue
        hp="$(printf '%s' "$hit" | hit_paths)"
        if [[ -z "$allowed" ]] || ! grep -Fxq -- "$hp" <<< "$allowed"; then
            printf '%s\n' "$hit"
        fi
    done
}

status=0

check_rule() { # $1 = rule, $2 = all-hits-command
    local rule="$1"
    local all_hits live stale
    all_hits="$($2 || true)"
    live="$(printf '%s\n' "$all_hits" | without_allowlisted "$rule")"
    # Stale: an allowlisted path for this rule with no exact hit path.
    stale="$(allowlisted_paths "$rule" | while IFS= read -r path; do
        [[ -n "$path" ]] || continue
        if ! printf '%s\n' "$all_hits" | hit_paths | grep -Fxq -- "$path"; then
            printf '%s\n' "$path"
        fi
    done)"
    if [[ -n "$live" ]]; then
        status=1
        echo "public-corpus lexical guard: unexplained $rule hit(s):" >&2
        printf '%s\n' "$live" >&2
    fi
    if [[ -n "$stale" ]]; then
        status=1
        echo "public-corpus lexical guard: stale $rule allowlist entrie(s):" >&2
        printf '%s\n' "$stale" >&2
    fi
}

if [[ "${1:-}" == "--self-test" ]]; then
    fixtures="$(mktemp -d)"
    trap 'rm -rf "$fixtures"' EXIT
    fx="$fixtures/work dir with spaces"
    mkdir -p "$fx/examples/specimen_demo/src" "$fx/docs" "$fx/tina-fake/src" "$fx/scripts"
    printf 'fn leaked() { let _x: std::sync::Arc<std::sync::Mutex<u64>> = start(); }\n' \
        > "$fx/examples/specimen_demo/src/sidecar.rs"
    printf 'fn tokio_leak() { let _y: Arc<tokio::sync::Mutex<u64>> = start(); }\n' \
        > "$fx/examples/specimen_demo/src/tokio_sidecar.rs"
    printf '// Arc<Mutex> in a comment is fine\nfn ok() {}\n' \
        > "$fx/examples/specimen_demo/src/comment_only.rs"
    printf 'fn poll() { loop { std::thread::sleep(std::time::Duration::from_millis(1)); break; } }\n' \
        > "$fx/examples/specimen_demo/src/poller.rs"
    printf 'fn whine() { while !done() { std::thread::sleep(std::time::Duration::from_millis(1)); } }\n' \
        > "$fx/examples/specimen_demo/src/whiler.rs"
    printf 'fn pace() { for _ in 0..3 { std::thread::sleep(std::time::Duration::from_millis(1)); } }\n' \
        > "$fx/examples/specimen_demo/src/pacer.rs"
    printf 'fn sneaky() { /* " */ let _z: Arc<Mutex<u64>> = g(); /* " */ }\n' \
        > "$fx/examples/specimen_demo/src/lexer_confusion.rs"
    printf 'fn sneaky2() { /* // */ loop { std::thread::sleep(std::time::Duration::from_millis(1)); break; } }\n' \
        > "$fx/examples/specimen_demo/src/lexer_confusion_poll.rs"
    printf 'use tina_runtime::ThreadedRuntime;\n' > "$fx/docs/guide.md"
    printf 'still teaches `register_with_capacity` on raw hosts\n' > "$fx/README.md"
    printf 'phase 163 alone is fine; the number is not forbidden\n' > "$fx/docs/notes.md"
    printf 'See the Public Example Certification package\n' > "$fx/examples/specimen_demo/src/leak.md"
    printf 'fn main() {}\n' > "$fx/tina-fake/src/lib.rs"
    printf '#!/usr/bin/env bash\ntrue\n' > "$fx/scripts/other.sh"
    touch "$fx/Makefile" "$fx/README.md"

    cat > "$fx/allow.toml" <<'TOML'
schema = 1

[[entry]]
path = "examples/specimen_demo/src/sidecar.rs"
rule = "shared-state"
reason = "fixture"
focused_test = "fixture"
reviewer = "fixture"
reviewed_sha = "00000000"

[[entry]]
path = "examples/specimen_demo/src/stale.rs"
rule = "shared-state"
reason = "fixture stale"
focused_test = "fixture"
reviewer = "fixture"
reviewed_sha = "00000000"
TOML
    # Stale-entry fixture file must exist so path validation passes.
    printf 'fn stale() {}\n' > "$fx/examples/specimen_demo/src/stale.rs"
    cat >> "$fx/allow.toml" <<'TOML2'

[[entry]]
path = "examples/specimen_demo/src/sidecar.rs"
rule = "poll-loop"
reason = "fixture (unused entry must not exempt anything)"
focused_test = "fixture"
reviewer = "fixture"
reviewed_sha = "00000000"
TOML2

    export PUBLIC_CORPUS_ALLOWLIST="$fx/allow.toml"
    ALLOWLIST="$fx/allow.toml"
    # Scan from inside the fixture root so hit paths are repo-relative and
    # exact-path exemption assertions are real, not vacuous.
    SCAN_ROOT="."
    pushd "$fx" >/dev/null

    # Path validation is bypassed in self-test (fixture paths live outside
    # the repo); run the scanners directly.
    shared="$(shared_state_hits)"
    poll="$(poll_loop_hits)"
    obs="$(obsolete_vocabulary_hits)"
    phrase="$(intent_phrase_hits)"

    printf '%s\n' "$shared" | grep -q 'sidecar.rs:1: shared-state' \
        || { echo "self-test: shared-state miss" >&2; exit 1; }
    printf '%s\n' "$shared" | grep -q 'tokio_sidecar.rs:1: shared-state' \
        || { echo "self-test: tokio Mutex evasion missed" >&2; exit 1; }
    printf '%s\n' "$shared$poll$obs$phrase" | grep -q 'comment_only' && {
        echo "self-test: comment-only false positive" >&2; exit 1; }
    printf '%s\n' "$poll" | grep -q 'poller.rs:1: poll-loop' \
        || { echo "self-test: poll-loop miss" >&2; exit 1; }
    printf '%s\n' "$poll" | grep -q 'whiler.rs:1: poll-loop' \
        || { echo "self-test: while-poll evasion missed" >&2; exit 1; }
    printf '%s\n' "$poll" | grep -q 'pacer' && {
        echo "self-test: for-loop pacing false positive" >&2; exit 1; }
    printf '%s\n' "$shared" | grep -q 'lexer_confusion.rs:1: shared-state' \
        || { echo "self-test: block-comment quote confusion evaded the strip" >&2; exit 1; }
    printf '%s\n' "$poll" | grep -q 'lexer_confusion_poll.rs:1: poll-loop' \
        || { echo "self-test: block-comment slash confusion evaded the strip" >&2; exit 1; }
    printf '%s\n' "$obs" | grep -q 'guide.md:1: obsolete-vocabulary' \
        || { echo "self-test: obsolete-vocabulary miss" >&2; exit 1; }
    printf '%s\n' "$obs" | grep -q 'README.md:1: obsolete-vocabulary' \
        || { echo "self-test: root README hit missed" >&2; exit 1; }
    printf '%s\n' "$phrase" | grep -q 'leak.md:1: intent-phrase' \
        || { echo "self-test: intent-phrase miss" >&2; exit 1; }
    printf '%s\n' "$phrase" | grep -q 'notes.md' && {
        echo "self-test: bare 163 false positive" >&2; exit 1; }

    positively="$(printf '%s\n' "$shared" | grep '/sidecar.rs:' | without_allowlisted shared-state)"
    [[ -z "$positively" ]] || {
        echo "self-test: exact-path exemption failed: $positively" >&2; exit 1; }
    live="$(printf '%s\n' "$shared" | without_allowlisted shared-state)"
    printf '%s\n' "$live" | grep -q 'tokio_sidecar' || {
        echo "self-test: unrelated entry wrongly exempted another file" >&2; exit 1; }
    obslive="$(printf '%s\n' "$obs" | without_allowlisted obsolete-vocabulary)"
    printf '%s\n' "$obslive" | grep -q 'README.md' || {
        echo "self-test: unlisted README hit wrongly exempted" >&2; exit 1; }
    popd >/dev/null
    echo "public-corpus lexical guard self-test: ok"
    exit 0
fi

[[ -f "$ALLOWLIST" ]] || {
    echo "public-corpus lexical guard: missing allowlist $ALLOWLIST" >&2
    exit 1
}
[[ -d "$SCAN_ROOT/examples" ]] || {
    echo "public-corpus lexical guard: missing scan root $SCAN_ROOT/examples" >&2
    exit 1
}

# Validate allowlisted paths exist (stale paths fail closed).
allowlisted_paths "shared-state" > /tmp/pclg_paths.$$ || true
allowlisted_paths "poll-loop" >> /tmp/pclg_paths.$$ || true
allowlisted_paths "obsolete-vocabulary" >> /tmp/pclg_paths.$$ || true
allowlisted_paths "intent-phrase" >> /tmp/pclg_paths.$$ || true
while IFS= read -r path; do
    [[ -n "$path" ]] || continue
    if [[ ! -f "$path" ]]; then
        echo "public-corpus lexical guard: allowlist names stale path $path" >&2
        status=1
    fi
done < /tmp/pclg_paths.$$
rm -f /tmp/pclg_paths.$$

check_rule shared-state shared_state_hits
check_rule poll-loop poll_loop_hits
check_rule obsolete-vocabulary obsolete_vocabulary_hits
check_rule intent-phrase intent_phrase_hits

if [[ "$status" -eq 0 ]]; then
    echo "public-corpus lexical guard: ok"
fi
exit "$status"
