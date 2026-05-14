# Phase 097 — Cancelable Deferred Admission

## Status

Planned. Runs after Phase 095 call-context defer ergonomics lands.

## Grug Truth

Normal multi-turn reply:

```rust
call_ctx.defer(work).reply(Msg::Done)
```

Cancelable multi-turn reply:

```rust
let (pending, effect) =
    call_ctx.defer_cancelable(call_cancelable(worker, req, timeout))
        .reply(key, Msg::Done);
```

The second shape is powerful and sharp. `pending` owns the original caller
authority plus the cancel handle. The child `effect` must not run unless the
service first stores `pending` in bounded state.

If storage is full or the key is duplicate:

- do not return the child effect;
- recover the original request with `pending.into_request_context()`;
- reply or reject the caller visibly;
- do not strand caller authority in a dropped token.

This phase makes that copied path boring.

## Non-goals

- No hidden global pending table.
- No automatic dispatch before admission.
- No unbounded `HashMap`.
- No magic request context carry.
- No fake cancellation for external work.
- No broad flow syntax.

## Rock 0 — Find One Real Caller

Pick or create one specimen that naturally has many cancelable deferred calls.
Good candidates:

- request fanout where each child call can be cancelled;
- HTTP request that starts several downstream Tina-owned calls and may abandon;
- worker pool request where each admitted job has caller authority plus a
  cancel handle.

Do not start by writing a generic helper.

Proof:

- specimen has at least two concurrent cancelable pending calls;
- bounded pending capacity is lower than possible input count;
- duplicate key is exercised;
- caller-visible outcome is asserted, not logged.

## Rock 1 — Local Bounded Storage First

In the specimen, write the local state by hand:

```rust
pending: BoundedPendingCancelableCalls<...>
```

or an example-local table with the same semantics.

Required behavior:

- fixed capacity;
- duplicate-key reject;
- insert returns `Full { pending }` or `DuplicateKey { pending }`;
- completion removes by key;
- cancel removes by key;
- owner stop drains all pending requests and replies/rejects visibly;
- no insert path dispatches the child effect before storage succeeds.

Proof:

- fill capacity, reject next call, caller gets a typed outcome;
- duplicate key rejects and caller gets a typed outcome;
- rejected admission does not run child work;
- completion frees capacity;
- cancel frees capacity;
- owner stop frees capacity and settles callers.

## Rock 2 — Extract Only The Repeated Shape

If Rock 1 is clean, extract:

```rust
PendingCancelableCallSet<K, Q, R>
```

Likely API:

```rust
let mut pending = PendingCancelableCallSet::with_capacity(cap);

match pending.try_insert(key, token) {
    Ok(()) => effects.push(effect),
    Err(PendingCancelableInsertError::Full { token }) => {
        return reply_to_request(token.into_request_context(), Reply::Busy);
    }
    Err(PendingCancelableInsertError::DuplicateKey { token }) => {
        return reply_to_request(token.into_request_context(), Reply::Duplicate);
    }
}
```

Names may change. Semantics may not.

Rules:

- insert error returns the token;
- token is move-only;
- table is bounded;
- key removal must not have ABA footguns if an old completion arrives after a
  key is reused;
- if key reuse is allowed, removal must require a generation/token witness;
- if no good ABA-safe shape exists, forbid key reuse until old completion is
  observed and document it.

Proof:

- fill/cancel/refill;
- duplicate/recover/reply;
- late completion for old key cannot remove newer pending call;
- panic/owner-stop cleanup settles or rejects all callers;
- live and sim if the helper touches runtime-visible call/cancel semantics.

## Rock 3 — Better Copied Helper If Possible

Consider a tiny admission helper only after the set exists:

```rust
pending.admit(key, token, effect)
```

It may return:

```rust
AdmittedEffect<I>
AdmissionRejected<K, Q, R, I>
```

Do this only if it makes user code harder to misuse without hiding the rule.
The user must still see:

- storage succeeded;
- effect is now safe to return;
- full/duplicate paths still own the token and effect;
- caller must be answered/rejected on failure.

If it feels clever, do not ship it.

## Required Docs

Add one short box to the request/reply guide:

**Cancelable deferred calls: admit before dispatch.**

Show:

- good path: `defer_cancelable` -> insert token -> return effect;
- full path: insert rejects -> `into_request_context()` -> reply busy;
- duplicate path: insert rejects -> reply duplicate;
- why returning the effect before insertion is wrong.

## Required Tests

- unit tests for the set/helper;
- specimen smoke test;
- at least one trace assertion that rejected admission does not dispatch child
  work;
- capacity reclaimed after completion/cancel/reject;
- owner stop drains/rejects pending callers;
- ABA/key-reuse proof or key-reuse rejection proof.

## Success

The next cheap model writing a cancelable multi-turn service copies one shape:

1. build cancelable deferred call;
2. store token in bounded state;
3. return effect only after storage succeeds;
4. on storage failure, recover caller authority and answer now.

No stranded caller authority. No invisible child work. No unbounded table.
