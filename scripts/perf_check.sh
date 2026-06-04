#!/usr/bin/env bash
# Runs `make perf` and compares each row's tina_p50_ns against the median of
# the most recent N runs in the perf history. Exits non-zero on any row
# regressing by more than THRESHOLD_PERCENT.
#
# Designed for a pre-merge gate: a small p50 wobble across runs is normal,
# but a real regression (e.g. allocations doubled, latency tripled) should
# fail loudly. Uses median-of-N so a single jittery outlier doesn't trip
# the gate; uses the most recent N so a long-stale baseline doesn't either.

set -euo pipefail

HISTORY_FILE="${TINA_PERF_HISTORY_FILE:-.intent/phases/149-structural-http-runtime-performance/perf_history.jsonl}"
WINDOW="${PERF_CHECK_WINDOW:-5}"
THRESHOLD_PERCENT="${PERF_CHECK_THRESHOLD:-25}"
ABS_TOLERANCE_NS="${PERF_CHECK_ABS_TOLERANCE_NS:-500000}"
HOTPATH_STAGE_SLACK_PERCENT="${PERF_CHECK_HOTPATH_STAGE_SLACK:-25}"
HOTPATH_PROCESS_ALLOC_SLACK_PERCENT="${PERF_CHECK_HOTPATH_PROCESS_ALLOC_SLACK:-50}"
MIN_HISTORY_ROWS="${PERF_CHECK_MIN_HISTORY_ROWS:-3}"
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

if [[ ! -f $HISTORY_FILE ]]; then
  echo "no history at $HISTORY_FILE — run scripts/perf_record.sh first" >&2
  exit 0
fi

echo "Running make perf for platform=$platform arch=$arch profile=$profile..." >&2
output=$(make perf 2>&1)

fail=0

# Iterate each label observed in the current run.
labels=$(grep '^perf-compare' <<< "$output" \
           | grep -oE 'label=[a-z0-9_]+' \
           | sed 's/label=//' \
           | sort -u \
           || true)

printf '%-32s %12s %12s %12s %8s %s\n' "label" "current_ns" "median_ns" "delta_ns" "delta%" "verdict"
printf '%-32s %12s %12s %12s %8s %s\n' "-----" "----------" "---------" "--------" "------" "-------"

for label in $labels; do
  current=$(grep "^perf-compare label=$label " <<< "$output" | grep -oE 'tina_p50_ns=[0-9]+' | head -1 | cut -d= -f2)
  if [[ -z $current ]]; then
    printf '%-32s %12s %12s %12s %8s %s\n' "$label" "?" "?" "?" "?" "skipped (no current p50)"
    continue
  fi

  # Pull p50 from history for this label's compare rows. tail to the most
  # recent WINDOW entries, then sort numerically to take the median.
  historical=$(grep '"kind":"compare"' "$HISTORY_FILE" \
                 | grep "\"platform\":\"$platform\"" \
                 | grep "\"arch\":\"$arch\"" \
                 | grep "\"profile\":\"$profile\"" \
                 | grep "\"label\":\"$label\"" \
                 | grep -oE '"tina_p50_ns":[0-9]+' \
                 | cut -d: -f2 \
                 | tail -n "$WINDOW" \
                 | sort -n \
                 || true)

  if [[ -z $historical ]]; then
    printf '%-32s %12s %12s %12s %8s %s\n' "$label" "$current" "-" "-" "-" "no history (first run for this label)"
    continue
  fi

  count=$(wc -l <<< "$historical" | tr -d ' ')
  if (( count < MIN_HISTORY_ROWS )); then
    printf '%-32s %12s %12s %12s %8s %s\n' "$label" "$current" "-" "-" "-" "warming history (${count}/${MIN_HISTORY_ROWS})"
    continue
  fi
  # Median index (1-based).
  median_idx=$(( (count + 1) / 2 ))
  median=$(sed -n "${median_idx}p" <<< "$historical")

  if [[ -z $median || $median -eq 0 ]]; then
    printf '%-32s %12s %12s %12s %8s %s\n' "$label" "$current" "$median" "-" "-" "median zero — skipped"
    continue
  fi

  delta_pct=$(( (current - median) * 100 / median ))
  delta_ns=$(( current - median ))

  if (( delta_pct > THRESHOLD_PERCENT && delta_ns > ABS_TOLERANCE_NS )); then
    printf '%-32s %12s %12s %+12d %+8d %s\n' "$label" "$current" "$median" "$delta_ns" "$delta_pct" "REGRESSION (> +${THRESHOLD_PERCENT}% and > ${ABS_TOLERANCE_NS}ns)"
    fail=1
  elif (( delta_pct < -THRESHOLD_PERCENT )); then
    printf '%-32s %12s %12s %+12d %+8d %s\n' "$label" "$current" "$median" "$delta_ns" "$delta_pct" "improvement"
  else
    printf '%-32s %12s %12s %+12d %+8d %s\n' "$label" "$current" "$median" "$delta_ns" "$delta_pct" "ok"
  fi
done

hotpath_labels=$(grep '^hotpath' <<< "$output" \
                   | grep -oE 'label=[a-zA-Z0-9_]+' \
                   | sed 's/label=//' \
                   | sort -u \
                   || true)

if [[ -n $hotpath_labels ]]; then
  echo
  printf '%-40s %12s %12s %12s %8s %s\n' "hotpath" "stage_count" "stage_base" "proc_alloc" "base" "verdict"
  printf '%-40s %12s %12s %12s %8s %s\n' "-------" "-----------" "----------" "----------" "----" "-------"
fi

for label in $hotpath_labels; do
  line=$(grep "^hotpath label=$label " <<< "$output" | head -1 || true)
  stage_count=$(grep -oE 'stage_count=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
  event_stage_count=$(grep -oE 'event_stage_count=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
  handler_turn_count=$(grep -oE 'handler_turn_count=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
  runtime_call_count=$(grep -oE 'runtime_call_count=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
  service_call_count=$(grep -oE 'service_call_count=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
  completion_count=$(grep -oE 'completion_count=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
  rejected_completion_count=$(grep -oE 'rejected_completion_count=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
  process_allocs=$(grep -oE 'process_allocations=[0-9]+' <<< "$line" | head -1 | cut -d= -f2 || true)
  if [[ -z $stage_count ]]; then
    printf '%-40s %12s %12s %12s %8s %s\n' "$label" "?" "?" "${process_allocs:-?}" "?" "skipped (no stage_count)"
    continue
  fi
  if [[ -z $event_stage_count || -z $handler_turn_count || -z $runtime_call_count || -z $service_call_count || -z $completion_count || -z $rejected_completion_count ]]; then
    printf '%-40s %12s %12s %12s %8s %s\n' "$label" "$stage_count" "?" "${process_allocs:-?}" "?" "MISSING NEW COUNTERS"
    fail=1
    continue
  fi
  if (( event_stage_count != stage_count )); then
    printf '%-40s %12s %12s %12s %8s %s\n' "$label" "$stage_count" "$event_stage_count" "${process_allocs:-?}" "?" "event_stage_count != stage_count"
    fail=1
    continue
  fi

  historical_stage=$(grep '"kind":"hotpath"' "$HISTORY_FILE" \
                       | grep "\"platform\":\"$platform\"" \
                       | grep "\"arch\":\"$arch\"" \
                       | grep "\"profile\":\"$profile\"" \
                       | grep "\"label\":\"$label\"" \
                       | grep -oE '"stage_count":[0-9]+' \
                       | cut -d: -f2 \
                       | tail -n "$WINDOW" \
                       | sort -n \
                       || true)
  stage_base="-"
  stage_verdict="warming/no stage history"
  stage_fail=0
  stage_count_history=$(wc -l <<< "$historical_stage" | tr -d ' ')
  if [[ -n $historical_stage && $stage_count_history -ge $MIN_HISTORY_ROWS ]]; then
    stage_idx=$(( (stage_count_history + 1) / 2 ))
    stage_base=$(sed -n "${stage_idx}p" <<< "$historical_stage")
    stage_limit=$(( stage_base + (stage_base * HOTPATH_STAGE_SLACK_PERCENT + 99) / 100 ))
    if (( stage_count > stage_limit )); then
      stage_verdict="STAGE REGRESSION (> ${stage_limit})"
      stage_fail=1
    else
      stage_verdict="stage ok"
    fi
  elif [[ -n $historical_stage ]]; then
    stage_verdict="warming stage (${stage_count_history}/${MIN_HISTORY_ROWS})"
  fi

  alloc_base="-"
  alloc_verdict=""
  alloc_fail=0
  if [[ -n ${process_allocs:-} && $process_allocs != "unknown" ]]; then
    historical_allocs=$(grep '"kind":"hotpath"' "$HISTORY_FILE" \
                         | grep "\"platform\":\"$platform\"" \
                         | grep "\"arch\":\"$arch\"" \
                         | grep "\"profile\":\"$profile\"" \
                         | grep "\"label\":\"$label\"" \
                         | grep -oE '"process_allocations":[0-9]+' \
                         | cut -d: -f2 \
                         | tail -n "$WINDOW" \
                         | sort -n \
                         || true)
    alloc_count=$(wc -l <<< "$historical_allocs" | tr -d ' ')
    if [[ -n $historical_allocs && $alloc_count -ge $MIN_HISTORY_ROWS ]]; then
      alloc_idx=$(( (alloc_count + 1) / 2 ))
      alloc_base=$(sed -n "${alloc_idx}p" <<< "$historical_allocs")
      alloc_limit=$(( alloc_base + (alloc_base * HOTPATH_PROCESS_ALLOC_SLACK_PERCENT + 99) / 100 ))
      if (( process_allocs > alloc_limit )); then
        alloc_verdict=" / PROCESS ALLOC REGRESSION (> ${alloc_limit})"
        alloc_fail=1
      else
        alloc_verdict=" / alloc ok"
      fi
    elif [[ -n $historical_allocs ]]; then
      alloc_verdict=" / warming alloc (${alloc_count}/${MIN_HISTORY_ROWS})"
    fi
  fi

  printf '%-40s %12s %12s %12s %8s %s%s\n' "$label" "$stage_count" "$stage_base" "${process_allocs:-unknown}" "$alloc_base" "$stage_verdict" "$alloc_verdict"
  if (( stage_fail || alloc_fail )); then
    fail=1
  fi
done

exit "$fail"
