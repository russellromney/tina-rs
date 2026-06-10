# Track H: Macros and Public API Contracts — 2026-06-09

HEAD: 0cd6a31 (origin/main). Scope: `tina-macros/src/lib.rs` (666 lines),
`tina-rpc-macros/src/lib.rs` (636 lines), trybuild fixtures in
`tina-rpc/tests/macro_compile_fail/` and
`tina-runtime/tests/safety_rails_compile_fail/` +
`safety_rails_compile_pass/`, and the `::tina` / `::tina_rpc` surfaces the
generated code targets.

Prior-review fixes verified present at HEAD (not re-filed): H2 (no
`deny(unused_variables)` on generated handlers), H3 (4-name reserved set),
H7 (`__tina_encoding`/`__tina_payload` locals + `encoding`/`payload` reserved,
fixtures pin diagnostics), H8 (AST visitor replaces token-text scan; negative
fixture `split_request_call_in_string_literal` + pass fixture
`split_request_call_used_with_string`).

## Direct answers to the two scrutiny questions

**Is RESERVED_REQUEST_PARAMS complete?** I enumerated every binding the
generated `<method>_request` fn introduces:

- appended params: `deadline`, `correlator`, `reply_to`, `max_payload`
  (lib.rs:597-600)
- body locals: `__tina_encoding`, `__tina_payload` (lib.rs:608-609)
- `<method>_decode_reply` takes no user args (`bytes`, `max_payload`, local
  `encoding` at lib.rs:625-633 cannot collide with anything user-named)

The 6-name set covers all of these **for normally-spelled identifiers**. Two
residual gaps: raw-identifier spellings bypass the string compare (H9), and
the `__tina_*` locals are themselves neither reserved nor hygienic (H10).

**Does the H8 fix walk structure?** Yes — `CallAuthorityUse` is a real
`syn::visit::Visit` impl on `visit_expr_path` (tina-macros/src/lib.rs:620-627),
not a better string scan. String literals and comments genuinely cannot
trigger it (visitor never reaches literal contents; comments are not tokens).
What still misbehaves: macro-invocation-only uses and `r#call` spellings are
**false positives** (valid code rejected, H12); shadowing and
conditional-early-return are **false negatives** of the *visitor*, but the
`RequestEffect` type gate catches shadowing — only early `return` escapes both
gates (H11), and the runtime ReplyAbandoned guard then settles the caller.

---

## Findings

### H9 — Raw-identifier args bypass `RESERVED_REQUEST_PARAMS` → opaque E0415 regression

1. **Severity:** Low
2. **Confidence:** High
3. **File/line:** `tina-rpc-macros/src/lib.rs:321` (check), `:595-600`
   (generated params)
4. **Violated invariant:** reserved-arg collisions are rejected "with a clear,
   spanned diagnostic — not an opaque E0415 deep inside generated code"
   (reserved_arg_name.rs fixture header — the exact H3/H7 contract).
5. **Concrete bug:** the check is
   `RESERVED_REQUEST_PARAMS.contains(&name.to_string().as_str())`.
   `proc_macro2::Ident::to_string()` for a raw ident yields `"r#deadline"`,
   which is not in the set, so `fn charge(&mut self, r#deadline: u64)` passes
   validation. Rust treats `r#deadline` and `deadline` as the same identifier
   for non-keywords, so the generated `charge_request(r#deadline: u64, ...,
   deadline: Duration, ...)` hits E0415 "identifier `deadline` is bound more
   than once" inside generated code — exactly the diagnostic regression the
   reserved set exists to prevent.
6. **Why real:** `r#`-spelling is unusual but legal; anyone hitting the
   reserved-name rejection may "work around" it by raw-spelling the arg and
   then gets the opaque error H3 was filed against.
7. **Repro/test:** trybuild fixture: trait method with `r#deadline: u64`
   should produce the reserved-name diagnostic, currently produces E0415.
8. **Fix:** `use syn::ext::IdentExt;` and compare
   `name.unraw().to_string()` against the set (and use the unraw'd name in
   the error message).
9. **LLM pattern:** yes — string-compare on ident text without considering
   raw-ident spelling is a classic plausible-but-incomplete check; the H7 bug
   was already "incomplete set", this is "incomplete comparison".

### H10 — `__tina_encoding`/`__tina_payload` are call-site idents: H7-class shadowing still constructible

1. **Severity:** Low
2. **Confidence:** High (mechanism); the trigger name is pathological
3. **File/line:** `tina-rpc-macros/src/lib.rs:534-535` (`format_ident!`,
   call-site spans), `:608-613` (binding order)
4. **Violated invariant:** the comment at :531 claims "Non-collidable local
   names". They are collidable: `format_ident!` produces call-site-hygiene
   idents, and `__tina_encoding`/`__tina_payload` are not in
   `RESERVED_REQUEST_PARAMS`.
5. **Concrete bug:** a trait arg literally named `__tina_encoding` passes the
   reserved check; the generated `let __tina_encoding = Default::default()`
   then shadows it before the tuple expression is built, so the tuple encodes
   the encoder value instead of the caller's arg — the exact H7 silent-wrong-
   bytes shape. (With the default `Json` encoder this fails to compile because
   `Json: !Serialize`; with any encoding type that derives `Serialize` it is
   silent corruption.) `__tina_payload` is benign: the tuple is consumed in
   the let-initializer before that binding exists.
6. **Why real:** barely — nobody innocently names an arg `__tina_encoding`.
   Filed because the in-code comment asserts non-collidability and the fix is
   one line; tina-macros already does this correctly for
   `__tina_service_message` via `Span::mixed_site()` (tina-macros lib.rs:322-323).
7. **Repro/test:** service trait with arg `__tina_encoding: u64` and a
   Serialize-able encoding type; assert wire payload equals the arg.
8. **Fix:** `Ident::new("__tina_encoding", Span::mixed_site())` (ditto
   payload), or append both names to `RESERVED_REQUEST_PARAMS`.
9. **LLM pattern:** yes — "rename to a prefixed local" is the plausible fix;
   the hygienic fix (mixed_site) was used in the sibling crate but not here.

### H11 — Split `handle_request` linear-authority compile gate is bypassable via conditional early `return`

1. **Severity:** Medium
2. **Confidence:** High
3. **File/line:** `tina-macros/src/lib.rs:339-343` (body spliced as
   let-initializer inside generated `handle_call`)
4. **Violated invariant:** the safety-rails fixture suite
   (`split_request_drop_call`, `split_request_ignores_call`,
   `split_request_forged_effect`, ...) advertises that a split request
   handler **cannot compile** without consuming caller authority into a
   `RequestEffect`.
5. **Concrete bug:** the user body is spliced as the initializer of
   `let request_effect: RequestEffect<Self> = #request_body;` inside the
   generated `fn handle_call(...) -> Effect<Self>`. A `return` expression in
   the user body therefore returns from the *generated fn*, whose return type
   is plain `Effect<Self>` — skipping the `RequestEffect` binding entirely:

   ```rust
   fn handle_request(&mut self, req: Request, call: RequestCall<'_, Self>)
       -> RequestEffect<Self>
   {
       if self.draining { return tina::noop(); }   // compiles; authority dropped
       call.reply(Reply)
   }
   ```

   The `CallAuthorityUse` gate passes (`call` is mentioned on the other
   path), the type gate is skipped (`return` targets the outer fn), and at
   runtime the caller on the `draining` path gets
   `CallRejectedReason::ReplyAbandoned` from the dispatch backstop
   (`tina-runtime/src/dispatch.rs:403-416`) instead of a reply.
6. **Why real:** conditional early-return ("if shutting down / not ready,
   do nothing") is an idiomatic shape; the author believes the compiler
   forces them to answer the caller, and instead ships a runtime
   ReplyAbandoned on that branch. No hang, no double-settle (backstop holds
   "every call settles exactly once"), but the compile-time rail the fixtures
   pin is quietly dynamic on this path.
7. **Repro/test:** trybuild `compile_fail` fixture with the snippet above —
   currently it would have to live in `compile_pass`. Runtime test: drive the
   draining branch and observe ReplyAbandoned at the caller.
8. **Fix:** splice the body as an immediately-invoked closure so `return` is
   local to it:
   `let request_effect: #tina_crate::RequestEffect<Self> = (move || #request_body)();`
   (closure borrows `self`/bindings only for the call; return type annotation
   keeps diagnostics anchored). Add the compile-fail fixture.
9. **LLM pattern:** yes — "wrap body in a typed let" looks airtight and passes
   every straight-line test; `return`-through-splice is a known proc-macro
   trap (same reason `async`/`try` blocks exist).

### H12 — `CallAuthorityUse` rejects valid code: macro-invocation-only use and `r#call` spelling

1. **Severity:** Low
2. **Confidence:** High
3. **File/line:** `tina-macros/src/lib.rs:620-627`
4. **Violated invariant:** the gate's contract is "a body that genuinely uses
   `call` authority still compiles" (pass-fixture header).
5. **Concrete bug:** two false-positive holes. (a) `syn::visit` does not
   descend into macro token streams (`ExprMacro.mac.tokens` is an opaque
   `TokenStream`), so a handler whose only authority use is
   `respond!(call, value)` or `if cond { reply_or_reject!(call) }` fails the
   gate with "must use caller authority `call`" even though it compiles and
   answers the caller. (b) `proc_macro2::Ident` equality includes the raw
   flag, so a body writing `r#call.reply(x)` (same binding as `call`) is also
   rejected.
6. **Why real:** user-side helper macros around reply/reject boilerplate are
   the natural way to deduplicate split services; first one written gets a
   misleading hard error. (The error direction is safe — no invariant breach —
   but the diagnostic blames correct code.)
7. **Repro/test:** compile-pass fixture:
   `macro_rules! ok { ($c:expr) => { $c.reply(Reply) } }` with body
   `ok!(call)` — currently fails the gate.
8. **Fix:** in the visitor, also implement `visit_macro` and fall back to a
   token-stream scan for the (unraw'd) needle inside `mac.tokens` — treating a
   token-level match as "used" is conservative in the safe direction, since
   the `RequestEffect` type gate remains the real enforcement. Compare idents
   via `unraw()`.
9. **LLM pattern:** yes — AST visitors that forget macro tokens are the
   canonical "walks structure, misses the escape hatch" follow-up to a
   string-scan fix.

### H13 — Split mode silently ignores a user-authored `handle_call` (left as dead inherent method)

1. **Severity:** Low
2. **Confidence:** High (mechanism); Medium (impact — `dead_code` may warn)
3. **File/line:** `tina-macros/src/lib.rs:290-366` (split branch extracts only
   `handle_event`/`handle_request`), `:432-436` (`remaining_impl` re-emits
   everything else verbatim)
4. **Violated invariant:** the macro's magic method names (`handle`,
   `handle_call`, `handle_event`, `handle_request`) are an API contract;
   defining one that the chosen mode ignores should be rejected the way
   `send_only` + `handle_call` is (`:388-393`).
5. **Concrete bug:** in split (`event = ..., request = ...`) mode, a leftover
   `fn handle_call(...)` (or `fn handle(...)`) in the impl block is re-emitted
   as a plain inherent method. The generated trait impl supplies its own
   `handle_call`, which is what the runtime invokes; the user's version is
   never called, with no macro diagnostic. Mitigation: if the method is
   private and unused, `dead_code` warns; if `pub` (or the warning is
   ignored), it is fully silent.
6. **Why real:** migration from message-mode (`handle` + `handle_call`) to
   split mode leaves the old `handle_call` behind; the author believes their
   call path still runs.
7. **Repro/test:** trybuild compile-fail fixture: split isolate that also
   defines `handle_call` should be rejected; today it compiles.
8. **Fix:** in the split branch, scan `item.items` for leftover
   `handle`/`handle_call` fns and reject with a spanned "split services
   define handle_event/handle_request; remove `handle_call`" — symmetric with
   the existing `send_only` checks.
9. **LLM pattern:** mild — "remove the items I consume, re-emit the rest"
   is correct-looking and misses the reserved-name leftovers.

### H14 — Isolate macro drops `mut`/`ref`/`@`-subpatterns from handler arg bindings → contradictory diagnostic

1. **Severity:** Low
2. **Confidence:** High
3. **File/line:** `tina-macros/src/lib.rs:659-660` (`Pat::Ident(ident) =>
   Ok(ident.ident.clone())`), generated params at `:455-456`, `:407-408`,
   `:336`
4. **Violated invariant:** diagnostics must not contradict the source the
   user wrote.
5. **Concrete bug:** `simple_argument_name` accepts `Pat::Ident` and keeps
   only the ident, discarding `mutability`/`by_ref`/`subpat`. For
   `fn handle(&mut self, mut msg: Msg, ctx: ...)` the generated trait fn
   declares `msg: Self::Message` (no `mut`), so a body that mutates `msg`
   fails with E0384/E0596 "cannot assign to immutable argument" — while the
   user is staring at `mut msg` in their own source. `ref msg` and
   `msg @ pat` are likewise silently stripped.
6. **Why real:** `mut msg` on an owned message arg is everyday Rust
   (e.g. draining a Vec out of the message).
7. **Repro/test:** compile-pass fixture with `mut msg` whose body does
   `msg = ...;` — currently fails to compile.
8. **Fix:** carry the `PatIdent`'s mutability through to the generated param
   (`#mutability #msg_name: Self::Message`), and reject `by_ref`/`subpat`
   explicitly with a spanned error.
9. **LLM pattern:** yes — extracting "just the name" from a pattern and
   regenerating the binding loses binding modes; tested only with plain names.

### H15 — `#[tina_rpc::service]` on a generic trait → opaque E0107 in generated code; `#[cfg]` on methods ignored

1. **Severity:** Low
2. **Confidence:** High
3. **File/line:** `tina-rpc-macros/src/lib.rs:124-130` (no `item.generics`
   validation), `:428` (`H: #trait_ident` bound), `:157-161` (method attrs
   never inspected/forwarded)
4. **Violated invariant:** unsupported shapes get spanned macro diagnostics,
   not generated-code errors (the H3 standard).
5. **Concrete bug:** (a) `#[service] trait Repo<T> { ... }` passes parsing;
   the generated `where H: Repo` then fails with E0107 "missing generics for
   trait `Repo`" pointing into macro output. Lifetime params same. (b) a
   `#[cfg(feature = "x")]` trait method is re-emitted with its cfg on the
   trait, but the method-table entry and client builders are generated
   unconditionally, so with the feature off the expansion references
   `<H as Trait>::method` that no longer exists — unresolvable error in
   generated code.
6. **Why real:** generic service traits are the first thing someone tries
   when factoring storage backends; cfg-gated methods appear when staging
   features.
7. **Repro/test:** trybuild fixtures: generic trait under `#[service]`;
   cfg-gated method with the cfg off.
8. **Fix:** reject `!item.generics.params.is_empty()` with a spanned
   "service traits cannot be generic"; either forward each method's
   `cfg`-class attrs onto its table entry + builders or reject `#[cfg]` on
   methods.
9. **LLM pattern:** yes — happy-path trait shapes only; attrs/generics
   unhandled rather than rejected.

### H16 — Raw-ident method/trait names leak `r#` into wire strings

1. **Severity:** Low
2. **Confidence:** High (mechanism); Medium (impact)
3. **File/line:** `tina-rpc-macros/src/lib.rs:126`
   (`trait_ident.to_string()` → default service name), `:264`
   (`method.sig.ident.to_string()` → wire method name)
4. **Violated invariant:** generated ABI strings should name the method, not
   its Rust spelling.
5. **Concrete bug:** `fn r#move(&mut self, ...)` produces wire method
   `"r#move"` (and `MAX_METHOD_LEN` counts the two extra bytes). Macro-built
   client and dispatch table agree, so Rust↔Rust works, but any hand-rolled
   or cross-language caller sending `"move"` gets `UnknownMethod`. Default
   service name from a raw-ident trait has the same wart.
6. **Why real:** keyword-shaped method names (`move`, `type`, `match`,
   `loop`) are natural RPC verbs; raw idents are the only way to spell them.
7. **Repro/test:** unit test asserting
   `request.method == "move"` for a `r#move` trait method (currently
   `"r#move"`).
8. **Fix:** `ident.unraw().to_string()` for `name_str` and the default
   service name (also fixes the length check).
9. **LLM pattern:** mild — `to_string()` on idents assumed prefix-free.

### H17 — Declared handler return/param types are decorative (diagnostics drift, visible in committed fixtures)

1. **Severity:** Low (informational)
2. **Confidence:** High
3. **File/line:** `tina-macros/src/lib.rs:525-532` and `:594-599` (only
   "output is not `()`" is checked; declared type discarded), generated
   signatures force `Effect<Self>` / splice into `RequestEffect`
4. **Concrete bug:** `fn handle_request(...) -> tina::Effect<Self>` and
   `-> RequestEffect<Self>` are both accepted (the in-tree fixtures disagree
   with each other: `split_request_call_in_string_literal.rs:31` declares
   `Effect<Self>`, `split_request_call_used_with_string.rs` declares
   `RequestEffect<Self>`). Even `-> u32` is accepted as long as the body
   evaluates to the type the macro actually requires. Same for the declared
   msg/ctx param types. The signature the user reads is not the contract the
   macro enforces, so type errors land on the body with a signature that
   appears to justify the body's type.
5. **Fix idea:** if the declared output parses as a path type, check its last
   segment is `Effect`/`RequestEffect` and error otherwise; or document that
   the declared types are ignored. At minimum make the two fixtures agree.
6. **LLM pattern:** yes — validation checks presence, not content.

---

## Disproven suspicions (with proof)

- **D1 — Generated client fn-name collisions between methods.** Suspected
  `format_ident!("{}_request")` / `"{}_decode_reply"` could collide across
  methods (e.g. `foo` vs `foo_request`). Disproved by suffix algebra: a
  collision needs `A + "_request" == B + "_request"` (⇒ A==B, impossible:
  trait method names are unique, enforced by rustc on the re-emitted trait)
  or `A + "_request" == B + "_decode_reply"` (impossible: distinct suffixes).
- **D2 — `request_effect` call-site local clobbering user args.** Traced all
  three collision cases (request arg named `request_effect`, call arg named
  `request_effect`, both lanes): shadowing order keeps semantics correct —
  the user body always sees its own binding because the generated `let`
  binds *after* the body is evaluated as its initializer
  (tina-macros/src/lib.rs:339-343).
- **D3 — RESERVED_REQUEST_PARAMS incomplete for normal spellings.**
  Enumerated every generated binding (see top of report); the 6-name set is
  complete for non-raw identifiers. Residuals filed as H9/H10.
- **D4 — H8 visitor still string-scan-shaped.** No: it is `syn::visit` on
  `ExprPath` with `qself.is_none()`; literal/comment contents are unreachable
  by construction. Both directions pinned by
  `split_request_call_in_string_literal.{rs,stderr}` (negative) and
  `safety_rails_compile_pass/split_request_call_used_with_string.rs`
  (positive).
- **D5 — `RequestEffect` forgeable without authority.**
  `from_consumed_effect` is `pub(crate)` (tina/src/effect.rs:205);
  `split_request_forged_effect.rs` and
  `runtime_internal_forge_needs_unsafe.rs` pin the rejection.
- **D6 — Abandoned caller authority hangs the caller.** No:
  `tina-runtime/src/dispatch.rs:403-416` detects an uncaptured, unconsumed
  call context after the handler turn and rejects it with
  `CallRejectedReason::ReplyAbandoned`. This is also why H11 is Medium, not
  High.
- **D7 — Crate-rename support for the rpc macro untested/broken.** Exercised:
  `tina-rpc/tests/macro_service.rs:366,400,424` run dispatch, client-build,
  and decode-error paths through `tina_crate = renamed_tina, rpc_crate =
  renamed_tina_rpc` aliases. All generated code paths use
  `#tina_crate`/`#rpc_crate`/`::core`/`::std` — no hardcoded `::tina_rpc`
  outside doc text.
- **D8 — `mut x` reserved-name/binding loss on the rpc side.** Not
  applicable: trait method declarations without bodies cannot carry binding
  patterns (`patterns_in_fns_without_body`), so only the isolate macro is
  affected (H14). The rpc macro's `Pat::Ident`-only check rejecting `_`
  args is deliberate (client builders need names) and gives a clear error.
- **D9 — Generated names colliding on the companion structs.** `SERVICE_NAME`
  const vs per-method fns: builder names always end `_request` /
  `_decode_reply` and methods are snake-case-unique, so no E0201 shape exists;
  user-side collisions (pre-existing `FooService` type) produce an ordinary,
  clearly-spanned duplicate-definition error.
- **D10 — `__tina_service_message` collidable.** No: built with
  `Span::mixed_site()` (tina-macros/src/lib.rs:322-323, the 8ad310c fix),
  with a compile-pass fixture (`split_request_arg_named_msg.rs`).

## Coverage gaps (no bug, worth tests)

- Isolate-macro crate-rename (`tina_crate = ...` / `runtime_crate = ...` on
  `#[isolate]`/`#[runtime_isolate]`) has **zero** test coverage (only doc
  mentions, tina-runtime/src/lib.rs:309). The rpc macro got rename tests;
  the isolate macro never did. The expansion looks uniform, but nothing
  proves it.
- Both arg parsers (`ServiceArgs` :112-114, `IsolateArgs` :83-86/:117-119)
  treat the separating comma as optional: `#[service(name = "a" rpc_crate =
  b)]` parses silently. Harmless today; a future option whose value parse is
  greedy across the missing comma would mis-bind. One-line fix: require
  comma unless input is empty.
- No trybuild fixtures for: raw-ident reserved args (H9), generic trait
  under `#[service]` (H15), split mode + stray `handle_call` (H13),
  early-return bypass (H11), `mut msg` binding (H14).

## Track verdict

Post-#227 the macro surface is in good shape: the H7 reserved set is complete
for sane spellings, the H8 gate genuinely walks the AST, and crate-rename is
real and tested on the rpc side. No Critical/High findings. The one
contract-honesty gap is H11 (early `return` bypasses the advertised
compile-time linear-authority rail; the runtime ReplyAbandoned backstop keeps
"every call settles" true). Everything else is diagnostics-grade: raw-ident
blind spots (H9/H16), gate false-positives (H12), silently-ignored magic
methods (H13), and binding-mode loss (H14).
