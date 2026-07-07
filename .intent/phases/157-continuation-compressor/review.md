# Hostile Review

## Branching Flows

The chosen macro does not compress branches beyond what Rust already spells in
the step body. A branch that starts different later steps is still readable:
the body returns whichever runtime call it needs and names the target generated
variant. That is acceptable for the notify endpoint.

Failure mode: a highly branched workflow may become a large body inside one
step. The answer is not more macro magic; split helper methods on the isolate
or keep the flow hand-written. The generated enum is additive, so users can
mix generated and manual arms.

## Cancelable Calls

`call_cancelable(...).then(...)` returns an effect and a handle. Admission of
that handle into bounded state is the hard part. The macro must not hide it,
because full or duplicate admission has to keep caller authority recoverable.

Failure mode: a user can write a step body that dispatches the cancelable
effect before storing the handle. The macro cannot prove that. The guide must
teach that cancelable admission stays explicit inside the step body.

## `defer_cancelable` / `try_admit`

The `CallContext::defer_cancelable(...).try_admit(...)` vocabulary is already
the safer first-turn spelling when the caller itself becomes a pending token.
The selected `flow!` macro starts after the first child call has a
`RequestContext`; it does not replace `try_admit`.

Failure mode: users may expect `flow!` to turn any `CallContext` into a
cancelable request scope. That would be a hidden runtime contract change. The
macro should remain just enum/dispatch generation.

## Facts

Facts are not special in a flow. A step body can return `batch(vec![fact(...),
next_call])` if that is already legal for the isolate. The macro does not
generate facts because generated trace facts would be a new observability
contract and could surprise replay.

Failure mode: without generated facts, trace readability depends on message
variant names. That is why step names must stay real workflow names and become
the enum variant names.

## Batch Effects

The macro permits a body to return any `Effect<Self>`, including `batch`.
It does not build batches automatically and does not sequence multiple calls
from the step declaration.

Failure mode: a user can batch two calls against the same resource and get
`ResourceBusy` from the runtime. That is not introduced by the macro; the docs
must repeat the existing one-resource-one-call-per-turn rule.

## Error Arms

The first implementation only hands the full `CallOutcome<T>` to the body. It
does not statically prove that every arm was matched.

Failure mode: users may match only `Replied(Ok(_))` and dump every other
outcome into a generic `_`. Tina cannot prevent that today in manual code
either. The ergonomic win should not hide the outcome. A later additive
`success/else` sub-syntax could make the error path prominent without reducing
expressiveness.

## Authority

The generated enum stores `RequestContext<Reply>` as the first field of every
step. The generated dispatcher moves it into the step body as `req`.
`RequestContext` remains non-`Clone`, so double reply is still a type error.
The macro rejects a step body that does not mention the generated `req`, and
the check is shadow-aware so a closure or match binding named `req` does not
count as caller authority.

Failure mode: a user can intentionally drop `req`. That is an existing Tina
escape hatch, not a macro escape hatch. The guide should tell users the final
step must consume `req` through `reply_to_request` unless dropping is the
intentional answer.
