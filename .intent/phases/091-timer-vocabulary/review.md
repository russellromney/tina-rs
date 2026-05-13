# Hostile Review

## Findings Fixed

1. **Catch-up could become an unbounded same-turn loop.**
   The plan requires bounded catch-up, visible exhaustion, and a test that
   proves no unbounded zero-delay loop.

2. **Backoff duration math could overflow quietly.**
   The plan requires saturating math or a typed overflow/config error.

3. **Debounce/throttle could smuggle a queue.**
   The plan says first form is one stream, `DelayedLatest` stores at most one
   value, and anything queue-shaped stays explicit user state.

4. **Timer helpers could sample live time internally.**
   The plan requires caller-supplied `now`, usually from `ctx.now()`, and
   forbids ambient clock reads inside helper state.

5. **Effect-builder sugar could hide the sleep.**
   The plan says any helper effect must return the exact
   `sleep(delay).reply(...)` shape, not a background scheduler.

## Net

Plan is grug enough:

- interval + backoff are the main ship target;
- debounce/throttle are conditional;
- helper is math/state;
- runtime sleep remains the truth;
- replay stays protected.
