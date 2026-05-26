# Phase 143 Review

## Hostile Pass 1

- This must not become a logging wrapper. It has to produce replayable cases or
  typed unsupported truth.
- "Overload" needs concrete facts: capacity high water, full counts, broadcast
  outcomes, pool waiters, cancellation/timeout, protocol pressure facts.
- Saved cases need config. Replay without config is fake.
- Keep capture bounded. A bugbox helper that stores every event forever is the
  same unbounded mistake with a nicer name.

