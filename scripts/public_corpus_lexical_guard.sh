#!/usr/bin/env bash
# Public-corpus lexical guard.
#
# Rejects, in the public corpus:
#   - shared-state: Arc<Mutex> / Mutex<Option> / Condvar / atomics in
#     examples/**/src code lines (result-sidecar signatures);
#   - poll-loop: `loop { ... sleep(...) }` result polling in examples/**/src;
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

strip_perl='
    s{r(\#*)".*?"\1}{ my $literal = $&; $literal =~ s/[^\n]/ /g; $literal }gse;
    s{"(?:\\.|[^"\\])*"}{ my $literal = $&; $literal =~ s/[^\n]/ /g; $literal }gse;
    s{//[^\n]*}{ }g;
    s{/\*.*?\*/}{ my $comment = $&; $comment =~ s/[^\n]/ /g; $comment }gse;
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
        while (/\b(?:Arc\s*<\s*(?:std::sync::)?Mutex|Mutex\s*<\s*Option|Condvar|Atomic(?:Bool|I8|I16|I32|I64|Isize|U8|U16|U32|U64|Usize|Ptr))/g) {
            my $line = 1 + (substr($_, 0, $-[0]) =~ tr/\n//);
            print "$line: shared-state $&\n";
        }
    '
}

poll_loop_hits() {
    scan_rs '
        '"$strip_perl"'
        while (/\bloop\s*\{(?:[^{}]|\{[^{}]*\})*?sleep\s*\(/gs) {
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

# Lines "path: rest" minus allowlisted paths for $2 = rule.
without_allowlisted() { # stdin hits, $1 = rule
    local allowed
    allowed="$(allowlisted_paths "$1")"
    if [[ -z "$allowed" ]]; then
        cat
        return
    fi
    grep -Fv -f <(printf '%s\n' "$allowed") || true
}

# Allowlisted paths for $1 = rule that no longer appear in the live hits.
stale_entries() { # stdin hits, $1 = rule
    local allowed
    allowed="$(allowlisted_paths "$1")"
    [[ -n "$allowed" ]] || return 0
    while IFS= read -r path; do
        grep -qF -- "$path" <<< "${2:-}" && continue
        if ! grep -qF "$path" /dev/stdin <<< "$1"; then :; fi
    done <<< "$allowed"
}

status=0
report() { # $1 = rule name, $2 = live hits (may be empty), $3 = all hits incl. allowlisted
    local rule="$1" live="$2"
    if [[ -n "$live" ]]; then
        status=1
        echo "public-corpus lexical guard: unexplained $rule hit(s):" >&2
        printf '%s\n' "$live" >&2
    fi
}

check_rule() { # $1 = rule, $2 = all-hits-command
    local rule="$1"
    local all_hits live stale
    all_hits="$($2 || true)"
    live="$(printf '%s\n' "$all_hits" | without_allowlisted "$rule")"
    # Stale: an allowlisted path for this rule with no live or exempted hit.
    stale="$(allowlisted_paths "$rule" | while IFS= read -r path; do
        [[ -n "$path" ]] || continue
        if ! printf '%s\n' "$all_hits" | grep -qF "$path"; then
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
    printf '// Arc<Mutex> in a comment is fine\nfn ok() {}\n' \
        > "$fx/examples/specimen_demo/src/comment_only.rs"
    printf 'fn poll() { loop { std::thread::sleep(std::time::Duration::from_millis(1)); break; } }\n' \
        > "$fx/examples/specimen_demo/src/poller.rs"
    printf 'fn pace() { for _ in 0..3 { std::thread::sleep(std::time::Duration::from_millis(1)); } }\n' \
        > "$fx/examples/specimen_demo/src/pacer.rs"
    printf 'use tina_runtime::ThreadedRuntime;\n' > "$fx/docs/guide.md"
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

    export PUBLIC_CORPUS_ALLOWLIST="$fx/allow.toml"
    ALLOWLIST="$fx/allow.toml"
    SCAN_ROOT="$fx"

    # Path validation is bypassed in self-test (fixture paths live outside
    # the repo); run the scanners directly.
    shared="$(shared_state_hits)"
    poll="$(poll_loop_hits)"
    obs="$(obsolete_vocabulary_hits)"
    phrase="$(intent_phrase_hits)"

    printf '%s\n' "$shared" | grep -q 'sidecar.rs:1: shared-state' \
        || { echo "self-test: shared-state miss" >&2; exit 1; }
    printf '%s\n' "$shared$poll$obs$phrase" | grep -q 'comment_only' && {
        echo "self-test: comment-only false positive" >&2; exit 1; }
    printf '%s\n' "$poll" | grep -q 'poller.rs:1: poll-loop' \
        || { echo "self-test: poll-loop miss" >&2; exit 1; }
    printf '%s\n' "$poll" | grep -q 'pacer' && {
        echo "self-test: for-loop pacing false positive" >&2; exit 1; }
    printf '%s\n' "$obs" | grep -q 'guide.md:1: obsolete-vocabulary' \
        || { echo "self-test: obsolete-vocabulary miss" >&2; exit 1; }
    printf '%s\n' "$phrase" | grep -q 'leak.md:1: intent-phrase' \
        || { echo "self-test: intent-phrase miss" >&2; exit 1; }
    printf '%s\n' "$phrase" | grep -q 'notes.md' && {
        echo "self-test: bare 163 false positive" >&2; exit 1; }

    live="$(printf '%s\n' "$shared" | without_allowlisted shared-state)"
    [[ -z "$live" ]] || { echo "self-test: allowlist did not exempt" >&2; exit 1; }
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
