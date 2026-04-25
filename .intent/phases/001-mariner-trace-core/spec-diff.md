# 001 Mariner Trace Core

## What changes

- Add a new crate called `tina-runtime-current`.
- Add the first runtime event trace types and causal IDs.
- Add a tiny single-isolate step harness that:
  - pulls one message from an injected mailbox
  - runs one handler
  - records the matching runtime trace events
- Start proving runtime behavior through the trace model instead of only through
  prose in the roadmap.

## What does not change

- No supervisor mechanism yet.
- No Tokio poll loop yet.
- No TCP echo server yet.
- No cross-isolate routing yet.
- No simulator yet.
- No new runtime helper surface in `tina`.
- No change to mailbox semantics.

## How we will verify it

- One accepted message becomes exactly one handler invocation.
- Repeated `step_once` calls preserve FIFO order from the mailbox.
- `Stop` prevents later delivery to the stopped isolate.
- Two identical runs produce the same runtime event sequence.
- Two identical runs produce the same causal links in the trace.
