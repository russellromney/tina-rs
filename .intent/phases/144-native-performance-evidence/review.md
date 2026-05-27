# Phase 144 Review

## Hostile Pass 1

- The plan must not let `mini_saas_api` remain the headline. It now says native
  rows are the headline and the API service is only a whole-service specimen.
- Paired benchmarks can lie if the Tokio side is unbounded. The plan requires
  same caps and names `partial` when semantics do not match.
- Allocation work can sprawl. The plan pins only warmed counts for headline
  paths and keeps setup/warmup/shutdown separate.
- "Improve the worst overhead" can become vague. The implementation must record
  before rows, pick the top two Tina-owned overheads, and record after rows.
- Debug numbers are useless for comparison. The plan requires release mode and
  a debug-mode invalid-comparison proof.
- There is a temptation to strip trace/capacity truth for speed. The plan bans
  semantic weakening.

## Hostile Pass 2

- HTTP/1 keepalive comparison is only fair if both sides reuse connections.
  Implementation must prove connection count or keepalive reuse for both rows.
- TCP echo comparison is only fair if payload size, connection count, and
  concurrency cap match. The row must print those inputs.
- Runtime send/call rows should not benchmark thread startup. Warmup and start
  barriers are required.
- JSON output is load-bearing. Tests must parse fields, not just grep strings.
- If tungstenite dependency/setup is too large, WebSocket comparison may be
  native-only for this phase. That is acceptable only if the report says
  `comparison_baseline=none` and the review names the deferral.
