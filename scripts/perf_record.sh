#!/usr/bin/env bash
# Runs `make perf` and appends one JSON line per row to a history file.
#
# The history file is plain JSONL (newline-delimited JSON), append-only, and
# checked into the repo so reviewers can see how the rows have moved over
# time. Each line covers ONE row from ONE `make perf` invocation. Three row
# shapes are emitted today:
#
#   {"kind":"compare", "timestamp":..., "git_sha":..., "platform":..., "arch":..., "profile":"release", "label":..., "tina_p50_ns":..., "tina_p90_ns":..., "tina_p99_ns":..., "tina_allocations":..., "tina_allocated_bytes":...}
#   {"kind":"process", "timestamp":..., "git_sha":..., "platform":..., "arch":..., "profile":"release", "label":..., "process_allocations":..., "process_allocated_bytes":..., "rss_delta_kb":...}
#   {"kind":"hotpath", "timestamp":..., "git_sha":..., "platform":..., "arch":..., "profile":"release", "label":..., "p50_ns":..., "stage_count":..., "event_stage_count":..., "handler_turn_count":..., "runtime_call_count":..., "service_call_count":..., "completion_count":..., "rejected_completion_count":..., "host_allocations":..., "process_allocations":...}
#
# A `perf-compare` line stays the canonical "row" — `perf-process` lines are
# extras emitted by HTTP rows (whole-process allocation + RSS delta).
#
# `native` lines come from the Tina-only protocol rows (HTTP/2, WebSocket) that
# carry no fair external baseline. They record p50/p90/p99 in microseconds (the
# field the row's flat line exposes) plus ops/ok and the per-op host allocation
# count:
#
#   {"kind":"native", "timestamp":..., ..., "label":..., "row_kind":..., "samples":..., "sample_policy":..., "p50_us":..., "p90_us":..., "p99_us":..., "ops":..., "ok":..., "allocations":..., "allocated_bytes":...}
#
# Usage:
#   ./scripts/perf_record.sh            # append latest run
#   ./scripts/perf_record.sh --dry-run  # print, don't append
#   ./scripts/perf_record.sh --read-from FILE  # parse FILE instead of running perf

set -euo pipefail

HISTORY_FILE="${TINA_PERF_HISTORY_FILE:-.intent/phases/152-protocol-perf-byte-path/perf_history.jsonl}"

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
untracked=$(git ls-files --others --exclude-standard)
if ! git diff --quiet --ignore-submodules -- . \
  || ! git diff --cached --quiet --ignore-submodules -- . \
  || [[ -n $untracked ]]; then
  git_sha="${git_sha}-dirty"
fi
case "$(uname -s)" in
  Darwin) platform=macos ;;
  Linux) platform=linux ;;
  *) platform=$(uname -s | tr '[:upper:]' '[:lower:]') ;;
esac
case "$(uname -m)" in
  arm64) arch=aarch64 ;;
  x86_64|aarch64) arch=$(uname -m) ;;
  *) arch=$(uname -m | tr '[:upper:]' '[:lower:]') ;;
esac
profile=release

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
    p90=$(grep -oE 'tina_p90_ns=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    p99=$(grep -oE 'tina_p99_ns=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    allocs=$(grep -oE 'tina_allocations=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    allocated_bytes=$(grep -oE 'tina_allocated_bytes=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    base_p50=$(grep -oE 'baseline_p50_ns=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    base_allocated_bytes=$(grep -oE 'baseline_allocated_bytes=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    if [[ -n $label && -n $p50 ]]; then
      printf '{"kind":"compare","timestamp":"%s","git_sha":"%s","platform":"%s","arch":"%s","profile":"%s","label":"%s","tina_p50_ns":%s,"tina_p90_ns":%s,"tina_p99_ns":%s,"tina_allocations":%s,"tina_allocated_bytes":%s,"baseline_p50_ns":%s,"baseline_allocated_bytes":%s}\n' \
        "$timestamp" "$git_sha" "$platform" "$arch" "$profile" "$label" "$p50" "${p90:-null}" "${p99:-null}" "${allocs:-null}" "${allocated_bytes:-null}" "${base_p50:-null}" "${base_allocated_bytes:-null}"
    fi
  done < <(grep '^perf-compare' <<< "$output" || true)
}

emit_process_lines() {
  while IFS= read -r line; do
    [[ -z $line ]] && continue
    label=$(grep -oE 'label=[a-zA-Z0-9_]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    process_allocs=$(grep -oE 'process_allocations=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    process_allocated_bytes=$(grep -oE 'process_allocated_bytes=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    rss=$(grep -oE 'rss_delta_kb=-?[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    if [[ -n $label && -n $process_allocs ]]; then
      printf '{"kind":"process","timestamp":"%s","git_sha":"%s","platform":"%s","arch":"%s","profile":"%s","label":"%s","process_allocations":%s,"process_allocated_bytes":%s,"rss_delta_kb":%s}\n' \
        "$timestamp" "$git_sha" "$platform" "$arch" "$profile" "$label" "$process_allocs" "${process_allocated_bytes:-null}" "${rss:-null}"
    fi
  done < <(grep '^perf-process' <<< "$output" || true)
}

emit_hotpath_lines() {
  while IFS= read -r line; do
    [[ -z $line ]] && continue
    label=$(grep -oE 'label=[a-zA-Z0-9_]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    p50=$(grep -oE 'p50_ns=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    stage_count=$(grep -oE 'stage_count=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    event_stage_count=$(grep -oE 'event_stage_count=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    handler_turn_count=$(grep -oE 'handler_turn_count=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    runtime_call_count=$(grep -oE 'runtime_call_count=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    service_call_count=$(grep -oE 'service_call_count=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    completion_count=$(grep -oE 'completion_count=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    rejected_completion_count=$(grep -oE 'rejected_completion_count=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    host_allocs=$(grep -oE 'host_allocations=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    process_allocs=$(grep -oE 'process_allocations=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    # Tail fields (phase 150). Optional so old rows without them still record.
    traced=$(grep -oE 'traced=(true|false)' <<< "$line" | head -1 | cut -d= -f2 || true)
    p90=$(grep -oE ' p90_ns=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    p99=$(grep -oE ' p99_ns=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    gap_count=$(grep -oE 'scheduler_gap_count=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    max_gap=$(grep -oE 'max_scheduler_gap_ns=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    gap_threshold=$(grep -oE 'scheduler_gap_threshold_ns=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    if [[ -n $label && -n $p50 && -n $stage_count ]]; then
      printf '{"kind":"hotpath","timestamp":"%s","git_sha":"%s","platform":"%s","arch":"%s","profile":"%s","label":"%s","traced":%s,"p50_ns":%s,"p90_ns":%s,"p99_ns":%s,"scheduler_gap_threshold_ns":%s,"scheduler_gap_count":%s,"max_scheduler_gap_ns":%s,"stage_count":%s,"event_stage_count":%s,"handler_turn_count":%s,"runtime_call_count":%s,"service_call_count":%s,"completion_count":%s,"rejected_completion_count":%s,"host_allocations":%s,"process_allocations":%s}\n' \
        "$timestamp" "$git_sha" "$platform" "$arch" "$profile" "$label" "${traced:-null}" "$p50" "${p90:-null}" "${p99:-null}" "${gap_threshold:-null}" "${gap_count:-null}" "${max_gap:-null}" "$stage_count" "${event_stage_count:-null}" "${handler_turn_count:-null}" "${runtime_call_count:-null}" "${service_call_count:-null}" "${completion_count:-null}" "${rejected_completion_count:-null}" "${host_allocs:-null}" "${process_allocs:-null}"
    fi
  done < <(grep '^hotpath' <<< "$output" || true)
}

emit_native_lines() {
  while IFS= read -r line; do
    [[ -z $line ]] && continue
    label=$(grep -oE 'label=[a-z0-9_]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    row_kind=$(grep -oE ' kind=[a-z_]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    samples=$(grep -oE ' samples=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    sample_policy=$(grep -oE ' sample_policy=[a-zA-Z0-9_]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    p50=$(grep -oE 'p50_us=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    p90=$(grep -oE 'p90_us=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    p99=$(grep -oE 'p99_us=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    ops=$(grep -oE ' ops=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    ok=$(grep -oE ' ok=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    allocs=$(grep -oE ' allocations=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    allocated_bytes=$(grep -oE ' allocated_bytes=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
    # Only the dedicated Tina-only protocol rows carry a setup-vs-reuse kind.
    # Every per-side line of a `perf-compare` row is also a `perf ` line, so
    # filter on the kind allowlist to keep those out of the native family.
    case "$row_kind" in
      connection_setup | connection_setup_amortized | steady_state_reuse) ;;
      *) continue ;;
    esac
    if [[ -n $label && -n $p50 ]]; then
      printf '{"kind":"native","timestamp":"%s","git_sha":"%s","platform":"%s","arch":"%s","profile":"%s","label":"%s","row_kind":"%s","samples":%s,"sample_policy":"%s","p50_us":%s,"p90_us":%s,"p99_us":%s,"ops":%s,"ok":%s,"allocations":%s,"allocated_bytes":%s}\n' \
        "$timestamp" "$git_sha" "$platform" "$arch" "$profile" "$label" "${row_kind:-unknown}" "${samples:-null}" "${sample_policy:-unknown}" "$p50" "${p90:-null}" "${p99:-null}" "${ops:-null}" "${ok:-null}" "${allocs:-null}" "${allocated_bytes:-null}"
    fi
  done < <(grep -E '^perf ' <<< "$output" || true)
}

emit_all() {
  emit_compare_lines
  emit_process_lines
  emit_hotpath_lines
  emit_native_lines
}

if [[ $mode == "dry-run" ]]; then
  emit_all
else
  mkdir -p "$(dirname "$HISTORY_FILE")"
  emit_all >> "$HISTORY_FILE"
  compare_count=$(grep -c '^perf-compare' <<< "$output" || true)
  process_count=$(grep -c '^perf-process' <<< "$output" || true)
  hotpath_count=$(grep -c '^hotpath' <<< "$output" || true)
  native_count=$(emit_native_lines | grep -c '"kind":"native"' || true)
  echo "Appended ${compare_count} compare + ${process_count} process + ${hotpath_count} hotpath + ${native_count} native rows to $HISTORY_FILE (git_sha=$git_sha)"
fi
