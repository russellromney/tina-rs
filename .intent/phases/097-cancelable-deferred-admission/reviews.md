# Hostile Review

## Finding 1 [P2] Example-local success is not enough

The old plan allowed the worker to stop after a local specimen table. That
would prove the bug and leave the copied path sharp.

Resolution: Rock 2 now says extraction is the default if Rock 1 is clean. If no
helper ships, status must record exactly why the shape is not reusable yet.

## Finding 2 [P2] ABA/key reuse is the main bug class

If completion removes by key only, an old completion can remove a newer pending
call after key reuse.

Resolution: the plan now requires a ticket/generation witness or explicit
key-reuse rejection until the old completion is observed, with tests either
way.

## Finding 3 [P2] Admission rejection must prove child work did not start

Answering the caller with `Busy` is not enough if the child effect was already
dispatched.

Resolution: Rock 1 and the required tests demand a trace assertion that failed
admission produces no child-call dispatch.

## Finding 4 [P2] Panic/owner stop can recreate timeout purgatory

Pending tokens own caller authority. Dropping them without settlement brings
back the bug Phase 086 killed.

Resolution: owner-stop drain and panic/owner-stop behavior are required tests.
The helper must return tokens on drain so callers can be settled.

## Finding 5 [P3] An admit helper can hide the actual safety rule

`pending.admit(token, effect)` could become magic sugar that hides storage
truth.

Resolution: Rock 3 is optional and gated. It may ship only if it makes misuse
harder while keeping storage success and failure-token recovery visible.
