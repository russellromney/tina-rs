# Phase 151 Hostile Review

## Review 1

Findings:

- [P2] Rock 0 was too broad. "Re-vendor Betelgeuse to latest first" could turn
  the performance phase into a large dependency rewrite before the actual
  wakeup fix. The plan now requires a provenance marker and keeps the current
  fork unless a named upstream change is needed. If a re-vendor is needed, it
  must be a separate first commit with workspace proof.
- [P2] The command-doorbell rule was underspecified for bounded ingress.
  `SyncSender::send` can block; if used blindly it can deadlock or erase the
  existing typed `Full` outcome. The plan now requires a `try_send` doorbell
  path, preserves retry-on-Full shutdown, and forbids new unbounded blocking
  sends in hot paths.
- [P2] The waker could accidentally be modeled as a clone of the `Rc<dyn
  IOLoop>`. That would be unsound for host threads and would let them touch
  backend state. The plan now requires a separate `Send + Sync` OS-handle
  waker; it may wake the worker but cannot mutate backend state.
- [P2] Linux blocking wait was too hand-wavy. `submit_and_wait(1)` plus timeout
  can leak timeout CQEs or confuse timeout completions with real completions if
  user data is not reserved. The plan now requires reserved doorbell/timeout
  user data, no completion-pointer casts for those events, and no blocking wait
  with no possible wake source.
- [P2] Simulated backend behavior was wrong. A "no-op returns immediately"
  `step_blocking(None)` would make threaded runtimes over simulated I/O spin
  after Rock 2 replaces `recv_timeout`. The plan now requires simulated
  threaded proof: no spin, wakes for host commands, while `tina-sim` remains
  deterministic because it does not use the live threaded park.
- [P2] The plan contradicted itself: Non-Goals said no ready-scheduler change,
  then Rock 3 changed the ready scheduler. The plan now says bounded hot-drain
  and completion-drain semantics stay fixed, while ready scheduling may change
  only through mailbox-owned readiness proof.
- [P2] Rock 3 risked relanding the Phase 150 bug. `is_empty()` is correct for
  direct mailbox pushes but is only skip-empty scan, not a true O(ready) queue.
  The plan now makes that distinction explicit: ship skip-empty first, build a
  true ready queue only if mailbox-owned empty -> non-empty notification exists
  and benchmark evidence says the remaining scan matters.
- [P3] The Done block still required a broad re-vendor after Rock 0 stopped
  requiring it. The plan now requires `vendor-betelgeuse/VENDOR.md` plus the
  additive blocking wait/waker, with re-vendor only if separately justified.

Decision:

- Plan is stronger now. The core goal stays the same: block the worker on the
  kernel I/O loop plus a doorbell, kill the timer re-poll gap, and preserve
  Tina's bounded command truth. The important implementation constraints are
  now pinned: no blocking-send footgun, no fake sim spin, no `Rc` waker, no
  timeout CQE confusion, no enqueue-side ready scheduler.
