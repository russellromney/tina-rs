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

HISTORY_FILE="${TINA_PERF_HISTORY_FILE:-.intent/phases/147-http-turn-allocation-cost/perf_history.jsonl}"
WINDOW="${PERF_CHECK_WINDOW:-5}"
THRESHOLD_PERCENT="${PERF_CHECK_THRESHOLD:-25}"
ABS_TOLERANCE_NS="${PERF_CHECK_ABS_TOLERANCE_NS:-500000}"
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
labels=$(grep '^perf-compare' <<< "$output" | grep -oE 'label=[a-z0-9_]+' | sed 's/label=//' | sort -u)

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
                 | sort -n)

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

exit "$fail"
