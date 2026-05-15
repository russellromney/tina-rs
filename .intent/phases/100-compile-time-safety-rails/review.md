# Hostile Review

## Finding 1 [P2] The plan can become too broad to merge

Send/call split, address capability split, macro diagnostics, typestate
protocol internals, and config builders could each be a PR.

Resolution: status says one PR only if the first slice stays mechanical. Rock 2
has a cut line for `CallAddress` / `SendAddress` first form instead of broad
runtime migration. Rock 8 can record "not worth it yet" for typestate if it
starts sprawling.

Follow-up tightening: the plan now names optional rocks clearly. Config
typestate and protocol typestate must not block the main user-facing compile
time win.

## Finding 2 [P2] Compile-time claims can lie about runtime facts

Capacity, timeout, closed peers, stale generations, and backend failures are
runtime facts. Trying to type them away would weaken Tina's truth model.

Resolution: grug truth and docs rock explicitly separate compile-time
impossible-program facts from runtime facts.

## Finding 3 [P2] `handle_call` compile errors can break send-only isolates

Many isolates have `Reply = ()` or never expose call addresses. Requiring
`handle_call` everywhere would add noise.

Resolution: Rock 3 requires explicit `send_only` or no-call defaults. Callable
services get the stricter check; internal send-only isolates stay boring.

## Finding 4 [P2] Macro wildcard enforcement may be impossible or annoying

Attribute macros may not be able to reliably reject wildcard arms without
becoming a parser/linter project.

Resolution: Rock 4 treats this as docs/review rule unless macro enforcement is
simple. It does not block the phase.

## Finding 5 [P3] Typestate builders can become ceremony

Typestate config builders can make code worse while only catching errors also
caught by validation.

Resolution: Rock 7 keeps runtime validation as source of truth for env/file
config and allows one targeted builder or a deliberate no-ship note.

## Finding 6 [P3] Protocol typestate can leak into user code

HTTP/2/WebSocket state types belong inside protocol implementation. If they
leak, users pay for internal safety with public complexity.

Resolution: Rock 8 says private/internal only unless a public type is clearly
earned.

## Finding 7 [P2] The proof can be too unit-test-shaped

Compile-time safety only matters if it catches mistakes in code that looks like
what users and LLMs write. The old plan asked for compile-fail tests but did
not require a coherent good/bad user story.

Resolution: added a required user proof matrix: one tiny callable service with
public call messages and internal continuation messages, plus paired positive
and negative fixtures.

## Finding 8 [P2] Diagnostics can pass without being useful

A compile-fail test can pass because Rust emitted 200 lines of trait soup. That
does not help the agent writing the code.

Resolution: negative fixtures must pin at least one stable Tina-facing phrase
where possible. If exact stderr is brittle, assert compile failure plus one
diagnostic phrase.

## Finding 9 [P3] Failing examples without passing examples teach fear

Bad fixtures alone say "don't do this," but the user still needs the copied
shape.

Resolution: the plan now requires nearby passing fixtures for each important
failure family.
