# 083 Hostile Review

## Verdict

Right phase, high risk.

This can become the most useful Tina artifact so far, or it can become a fake
"framework" that hides the truths Tina is good at.

The plan is acceptable only if implementation builds one real service first and
adds helpers second.

## Findings Folded Into Plan

- Required a concrete service path and route list at implementation start.
- Required the post-086 `RequestContext` pattern for multi-turn requests.
- Required DB pool and outbound keepalive pool pressure reports.
- Required readiness to name reasons, not just return 200/503.
- Required shutdown order and terminal report proof.
- Required a DST/live-replay hook with materialized ops and typed facts.
- Added a helper cut line so this does not become a web framework phase.
- Added helper-home rules so small APIs do not scatter across crates.
- Required the system to be actually tested, not prose-only.
- Required a two-or-more-turn reply proof so RequestContext is really exercised.

## Must Not Slip

- Do not build a router framework.
- Do not use `Arc<Mutex<AppState>>` for domain state.
- Do not call raw DB/outbound bridges everywhere if pool consumers exist.
- Do not let health/readiness become static strings.
- Do not hide retry or backoff.
- Do not use sleeps as shutdown proof.
- Do not let capacity reports be stale config math.
- Do not infer replay history from raw trace text.
- Do not make docs prettier than the code.
- Do not put generic helpers in `tina-http` or HTTP helpers in `tina`.
- Do not bypass the service coordinator by doing every DB/outbound call from
  host/test code.

## Watch During Implementation

- Multi-turn request replies must not time out because caller authority was
  dropped.
- DB and outbound pools must close/drain in shutdown.
- Pressure tests must actually hit `Full`/timeout/closed, not just happy path.
- The specimen/system smoke must run from repo root with documented commands.
- The service should expose a small report so tests assert facts without log
  scraping.
- Helper names should be boring. If naming feels clever, stop.
- If a helper is only used once, strongly prefer specimen-local glue.

## Likely Deferrals

Acceptable to defer:

- auth/session framework;
- HTTPS if native HTTP/1 proof is otherwise strong;
- SQLx/Postgres if SQLite gives a reliable CI service;
- browser/manual demos;
- pretty CLI.

Less acceptable to defer:

- DB pool consumer;
- outbound keepalive consumer;
- readiness reasons;
- graceful shutdown;
- capacity report;
- one service-shaped DST/live-replay fact;
- runnable smoke test.
- use of the existing live-replay/saved-case shape.

Without those, this is not "production service layers"; it is just another
specimen.
