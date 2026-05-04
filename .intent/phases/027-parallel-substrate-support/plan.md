# 027 Parallel Substrate Support Plan

## Purpose

Run safe support work beside 026 without changing Tina core semantics.

026 owns the TCP/time driver contract. 027 is the parallel lane for evidence,
polish, research, and story work that helps 026 and later phases but does not
refactor the runtime substrate.

## Scope

### Upstream Betelgeuse Polish

- Clean `betelgeuse::io::simulated` docs, names, and tests so it reads as a
  generic Betelgeuse simulated TCP backend.
- Keep Tina concepts out of Betelgeuse.
- Record what would need to change before proposing it upstream.

### Performance / Allocation Probes

- Add narrow measurements around current substrate hot paths.
- Prefer allocation counts and simple operation counts before wall-clock
  benchmark claims.
- Use results to inform 026; do not change runtime semantics here.

### Tokio-vs-Tina Comparison Expansion

- Add runnable comparisons that focus on constrained capacity, backpressure,
  timeout, shutdown, and overload behavior.
- Include hardened Tokio variants where possible so the comparison is not a
  strawman.
- Keep this as evidence, not marketing.

### API Ergonomics Polish

- Add only small helpers/macros that reduce boilerplate without creating
  multiple equal ways to do the same thing.
- Prefer existing preferred surface.
- Pause if a helper competes with the core API instead of clarifying it.

### External Review Passes

- Prepare short review prompts for 025 and 026.
- Ask reviewers to focus on code quality, semantic claims, proof strength, and
  whether substrate boundaries are honest.
- Fold actionable findings back into the relevant phase review.

### Research Notes

- Compare Tokio current-thread, Monoio, Glommio, and Compio as possible future
  driver adapters.
- Record what each substrate gives, what it weakens, and the smallest adapter
  shape it would require.
- Do not implement these adapters in 027.

### README / Story Refinement

- Improve language around Tina as a concurrency primitive.
- Keep docs light; Gemini remains the real release/docs phase after 026.
- Avoid claiming broad Tokio replacement or production maturity.

## Refusals

- Do not implement the 026 driver contract here.
- Do not add async isolate handlers.
- Do not expose backend handles to user isolates.
- Do not build Tower/Axum integration.
- Do not add unbounded queues.
- Do not change public Tina semantics.

## Done Means

- Betelgeuse simulated I/O is easier to review as standalone substrate code.
- Current substrate cost evidence is named and narrow.
- Tokio-vs-Tina comparisons include stronger constrained/backpressure cases.
- Any API polish keeps one preferred surface.
- External review prompts/results live in `review.md`.
- Adapter research notes live in `review.md`.
- README/story edits remain honest and brief.
- `make verify` passes for code changes; docs-only changes pass
  `git diff --check`.
