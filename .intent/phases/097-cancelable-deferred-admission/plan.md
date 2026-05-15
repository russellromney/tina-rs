# 097 Cancelable Deferred Admission

## Status

- IDD phase.
- Runs after Phase 095 call-context defer ergonomics.
- One PR.
- Owns cancelable multi-turn admission helpers, one specimen/system proof,
  request/reply docs, and focused runtime/helper tests.
- Do not run beside broad `CallContext` redesign. That just landed. This phase
  only makes the copied cancelable-defer path safe.

## Grug Truth

Normal multi-turn reply is now boring:

```rust
call_ctx.defer(work).reply(Msg::Done)
```

Cancelable multi-turn reply is still sharp:

```rust
let (pending, effect) = call_ctx
    .defer_cancelable(call_cancelable(worker, req, timeout))
    .reply(key, Msg::Done);
```

`pending` owns:

- the original caller authority;
- the cancel handle for the child call.

The child `effect` must not run until `pending` is stored in bounded state.

If storage rejects it:

- do not return the child effect;
- recover the caller with `pending.into_request_context()`;
- answer or reject the caller now;
- do not strand caller authority;
- do not start child work.

This phase makes that rule hard to copy wrong.

## Goal

After this phase, a service with many cancelable deferred calls has one blessed
shape:

1. build cancelable deferred work;
2. admit the pending token into bounded storage;
3. return the effect only after admission succeeds;
4. on `Full` or duplicate key, recover the caller and do not dispatch work;
5. on completion/cancel/owner stop, remove the token and reclaim capacity.

Likely output:

```rust
PendingCancelableCallSet<K, Q, R>
```

Do not force the exact name if the code wants a better one. Do not weaken the
semantics.

## Non-Goals

- no hidden global pending table;
- no automatic dispatch before admission;
- no unbounded `HashMap`;
- no magic request context carry;
- no fake cancellation for external work;
- no broad flow syntax;
- no storage helper that silently swallows duplicate keys;
- no helper that makes caller recovery optional on admission failure.

## Rock 0: Read First, Pick One Real Caller

Read:

- `.intent/phases/095-call-context-defer-ergonomics/plan.md`;
- `tina/src/lib.rs` around `CallContext::defer_cancelable`;
- `tina-runtime/src/call.rs` around `PendingCancelableCall`;
- `tina/src/pending_call_set.rs`;
- `tina-runtime/tests/request_context.rs`;
- `tina-runtime/tests/pending_call_set.rs`;
- `examples/specimen_cancellation_chain`;
- `examples/specimen_pool_cancel_reclaim`;
- `docs/tina-user-guide/04-request-reply.md`;
- `docs/tina-user-guide/11-ergonomics-checklist.md`.

Pick or create one real caller. Good candidates:

- request fanout where each child call can be cancelled;
- HTTP/controller route that starts several downstream Tina-owned calls;
- worker-pool request where each admitted job has caller authority plus cancel
  handle.

Do not start by writing a generic helper. First prove the pain with local
bounded state.

Status update before coding:

- chosen specimen/system path;
- chosen key type;
- chosen pending capacity;
- whether key reuse is allowed or rejected;
- exact caller outcomes for `Full`, duplicate, cancel, completion, and owner
  stop;
- expected helper home if extracted.

## Rock 1: Local Bounded Storage First

Implement the specimen with local storage first.

Required behavior:

- fixed capacity;
- no growing map pretending to be bounded;
- duplicate-key rejection;
- insert error returns the pending token;
- completion removes by key plus token/generation witness, or key reuse is
  rejected until old completion is observed;
- cancel removes by key plus token/generation witness;
- owner stop drains all pending tokens and settles callers visibly;
- failed admission does not dispatch the child effect.

Possible local shape:

```rust
pending: Vec<Option<PendingSlot<K, Q, R>>>
```

or another fixed-cap table. Keep it boring.

Required proof:

- at least two concurrent cancelable pending calls;
- fill capacity, reject next call, caller sees typed `Busy`/`Full`;
- duplicate key rejects and caller sees typed duplicate outcome;
- rejected admission produces no child-call dispatch trace;
- completion frees capacity;
- cancel frees capacity;
- fill -> cancel all -> refill works;
- owner stop settles every pending caller and frees capacity.

## Rock 2: Extract The Helper If The Shape Holds

If Rock 1 is clean, extract the reusable shape. Default expectation: extract.
Do not stop at example-local code unless the plan status records exactly why the
shape is not reusable yet.

Candidate API:

```rust
let mut pending = PendingCancelableCallSet::with_capacity(cap);

match pending.try_insert(key, token) {
    Ok(ticket) => effects.push(effect),
    Err(PendingCancelableInsertError::Full { token }) => {
        return reply_to_request(token.into_request_context(), Reply::Busy);
    }
    Err(PendingCancelableInsertError::DuplicateKey { token }) => {
        return reply_to_request(token.into_request_context(), Reply::Duplicate);
    }
}
```

The API may return a ticket/witness. That is probably the right way to prevent
ABA bugs.

Rules:

- token is move-only;
- insert error returns the token;
- table has fixed capacity;
- remove uses key plus ticket/witness, or key reuse is rejected until old
  completion is observed;
- old completion cannot remove a newer pending call;
- cancel path returns enough truth to report already-completed, missing, or
  cancelled;
- owner-stop drain returns tokens so callers can be answered/rejected;
- helper does not dispatch effects itself unless the admission result makes the
  rule clearer, not more hidden.

Required helper tests:

- insert/full/duplicate;
- complete removes exact ticket;
- old completion after key reuse cannot remove new token, or key reuse is
  rejected and tested;
- cancel removes exact ticket;
- fill/cancel/refill;
- drain returns all tokens;
- dropped set in tests does not leave caller authority silently timing out.

## Rock 3: Optional Admit Helper

Only after Rock 2, consider a tiny copied helper:

```rust
match pending.admit(key, token, effect) {
    Ok(admitted_effect) => return admitted_effect.into_effect(),
    Err(err) => { /* recover token, answer caller */ }
}
```

Ship this only if it makes misuse harder.

It must still show:

- storage succeeded;
- effect is now safe to return;
- `Full`/duplicate paths own the token;
- caller must be answered on failure.

If it feels clever, do not ship it.

## Rock 4: Docs And Migration

Add one short box to `docs/tina-user-guide/04-request-reply.md`:

**Cancelable deferred calls: admit before dispatch.**

Show:

- good path;
- `Full` path;
- duplicate path;
- why returning the child effect before storage succeeds is wrong.

Update the ergonomics checklist if it still teaches raw manual storage without
the new helper.

Do not rewrite all examples. Migrate only the chosen specimen and any tiny
call sites needed to prove the helper.

## Required Tests

- helper unit tests;
- one specimen/system smoke test;
- one trace assertion that rejected admission does not dispatch child work;
- capacity reclaimed after completion;
- capacity reclaimed after cancel;
- capacity reclaimed after admission rejection;
- owner stop drains/rejects pending callers;
- ABA/key-reuse proof or explicit key-reuse rejection proof;
- panic/owner-stop behavior does not leave callers waiting until timeout.

Run at least:

```text
cargo fmt --all --check
cargo test -p tina-runtime request_context -- --nocapture
cargo test -p tina-runtime pending_call_set -- --nocapture
cargo clippy -p tina-runtime -p tina --tests -- -D warnings
```

Add exact specimen test commands to the status block.

## Success

The next cheap model writing a cancelable multi-turn service copies one shape
and cannot accidentally launch child work before admitting caller authority.

No stranded caller authority.

No invisible child work.

No unbounded table.

No ABA key bug.
