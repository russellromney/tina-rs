# Hostile Review

## Finding 1 [P2] The plan can accidentally ship example-local code only

Rock 1 says local bounded storage first, which is right, but success should
not stop there if the shape is general. The phase exists because the copied
path is a safety footgun. If the specimen proves the shape, extraction should
be the default, not optional drift.

Resolution: Rock 2 says extract if Rock 1 is clean. Success requires either a
shipped helper or an explicit rejection note explaining why the local shape is
not reusable yet.

## Finding 2 [P2] ABA/key reuse is load-bearing

If completion removes by key only, an old completion can remove a newer pending
call after the key is reused. This is the exact bug class seen in earlier
pending-call helpers.

Resolution: Rock 2 requires a generation/token witness for key reuse or an
explicit key-reuse rejection rule until old completion is observed.

## Finding 3 [P2] Admission failure must prove child work did not start

It is not enough that the caller gets `Busy`. The child effect must not be
dispatched after failed storage.

Resolution: Required tests include a trace assertion that rejected admission
does not dispatch child work.

## Finding 4 [P3] Helper can hide too much

An `admit(token, effect)` helper could make code shorter but obscure the rule:
storage first, dispatch second.

Resolution: Rock 3 is optional and says not to ship clever sugar unless it
makes misuse harder while keeping full/duplicate/caller-recovery visible.

## Finding 5 [P3] Panic and owner-stop cleanup can be forgotten

Pending tokens own caller authority. Dropping the owner without settling them
recreates timeout purgatory in another coat.

Resolution: Rocks 1 and 2 require owner-stop drain/reject, panic/owner-stop
tests, and capacity cleanup.
