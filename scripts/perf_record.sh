#!/usr/bin/env bash
# Runs `make perf` and appends one JSON line per row to a history file.
#
# The history file is plain JSONL (newline-delimited JSON), append-only, and
# checked into the repo so reviewers can see how the rows have moved over
# time. Each line covers ONE row from ONE `make perf` invocation. Two row
# shapes are emitted today:
#
#   {"kind":"compare", "timestamp":..., "git_sha":..., "label":..., "tina_p50_ns":..., "tina_p99_ns":..., "tina_allocations":...}
#   {"kind":"process", "timestamp":..., "git_sha":..., "label":..., "process_allocations":..., "rss_delta_kb":...}
#
# A `perf-compare` line stays the canonical "row" — `perf-process` lines are
# extras emitted by HTTP rows (whole-process allocation + RSS delta).
#
# Usage:
#   ./scripts/perf_record.sh            # append latest run
#   ./scripts/perf_record.sh --dry-run  # print, don't append
#   ./scripts/perf_record.sh --read-from FILE  # parse FILE instead of running perf

set -euo pipefail

HISTORY_FILE=".intent/phases/145-hot-path-reality-check/perf_history.jsonl"

mode="record"
input_file=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run)
      mode="dry-run"
      shift
      ;;
    --read-from)
      input_file="$2"
      shift 2
      ;;
    *)
      echo "unknown arg: $1" >&2
      exit 2
      ;;
  esac
done

timestamp=$(date -u +%Y-%m-%dT%H:%M:%SZ)
git_sha=$(git rev-parse --short HEAD)

if [[ -n $input_file ]]; then
  output=$(cat "$input_file")
else
  echo "Running make perf..." >&2
  output=$(make perf 2>&1)
fi

# Pull every numeric field off a single perf line. Bash + grep keeps us free of
# jq / Python deps; the JSON shape is trivial so we hand-emit it.
emit_compare_lines() {
  while IFS= read -r line; do
    [[ -z $line ]] && continue
    label=$(grep -oE 'label=[a-z0-9_]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    p50=$(grep -oE 'tina_p50_ns=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    p99=$(grep -oE 'tina_p99_ns=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    allocs=$(grep -oE 'tina_allocations=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    base_p50=$(grep -oE 'baseline_p50_ns=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    if [[ -n $label && -n $p50 ]]; then
      printf '{"kind":"compare","timestamp":"%s","git_sha":"%s","label":"%s","tina_p50_ns":%s,"tina_p99_ns":%s,"tina_allocations":%s,"baseline_p50_ns":%s}\n' \
        "$timestamp" "$git_sha" "$label" "$p50" "${p99:-null}" "${allocs:-null}" "${base_p50:-null}"
    fi
  done < <(grep '^perf-compare' <<< "$output" || true)
}

emit_process_lines() {
  while IFS= read -r line; do
    [[ -z $line ]] && continue
    label=$(grep -oE 'label=[a-zA-Z0-9_]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    process_allocs=$(grep -oE 'process_allocations=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    rss=$(grep -oE 'rss_delta_kb=-?[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    if [[ -n $label && -n $process_allocs ]]; then
      printf '{"kind":"process","timestamp":"%s","git_sha":"%s","label":"%s","process_allocations":%s,"rss_delta_kb":%s}\n' \
        "$timestamp" "$git_sha" "$label" "$process_allocs" "${rss:-null}"
    fi
  done < <(grep '^perf-process' <<< "$output" || true)
}

emit_all() {
  emit_compare_lines
  emit_process_lines
}

if [[ $mode == "dry-run" ]]; then
  emit_all
else
  mkdir -p "$(dirname "$HISTORY_FILE")"
  emit_all >> "$HISTORY_FILE"
  compare_count=$(grep -c '^perf-compare' <<< "$output" || true)
  process_count=$(grep -c '^perf-process' <<< "$output" || true)
  echo "Appended ${compare_count} compare + ${process_count} process rows to $HISTORY_FILE (git_sha=$git_sha)"
fi
