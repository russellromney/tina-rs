# 002 Mariner Local Send Dispatch

## What changes

- Extend `tina-runtime-current` from a single-isolate step runner to a tiny
  single-shard runtime core that can host more than one isolate.
- Execute local same-shard `Effect::Send` dispatch through real mailboxes.
- Refine the runtime trace so send execution is more specific than the generic
  `EffectObserved` event used in slice 001.
- Start proving real dispatcher behavior for local send delivery and send
  rejection.

## What does not change

- No supervisor mechanism yet.
- No Tokio poll loop yet.
- No TCP echo server yet.
- No cross-shard routing yet.
- No simulator yet.
- No new runtime helper surface in `tina`.
- No mailbox semantics changes.
- `Reply`, `Spawn`, and `RestartChildren` are still not executed yet.

## How we will verify it

- A local same-shard `Send` accepted by the target mailbox results in exactly
  one later handler invocation on the target isolate.
- A rejected local same-shard `Send` is observable in the runtime trace and is
  not silently buffered.
- The runtime trace distinguishes:
  - effect observed
  - send dispatch attempted
  - send accepted
  - send rejected
- Repeated identical runs produce the same event sequence and the same causal
  links.
