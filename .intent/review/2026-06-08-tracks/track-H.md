# Track H — Macros and Public API Contracts

Checkout: working tree, HEAD `49c3580`.
Scope: `tina-macros`, `tina-rpc-macros`, plus the public API surface of `tina`
and `tina-runtime` (`runtime_internal`, `isolate_types!`).

All repros were compiled against the workspace path-deps on the repo's nightly
toolchain (`cargo +nightly build`) in a throwaway crate; `cargo expand` was used
to inspect generated tokens. No source was modified.

## Key context: the prior H-fix wave is NOT on this checkout

The prior review's fix commits exist only on *other* branches and are **not
ancestors of HEAD `49c3580`**:

- `57767cf` "Fix H4: gate must-answer-rail escape hatch behind unsafe" — not in HEAD
- `78d5665` "Fix H3: reject reserved arg names" — not in HEAD
- `8144818` "Fix H2: drop deny(unused_variables)" — not in HEAD
- `0adba89` "Fix H5 …", `a503f86` "Fix H6 …" — not in HEAD

So **H2, H3, H4 are all live on this checkout** and were re-confirmed by
compilation, not just by reading. Verified with
`git merge-base --is-ancestor <fix> HEAD` → all "NO".

---

## H4 — Public `runtime_internal::request_effect_from_consumed_effect` defeats the must-answer-caller rail (CONFIRMED, live)

- Severity: **High** · Confidence: **High**
- `tina/src/lib.rs:418-425`
- Invariant: split-service `handle_request` must answer its caller exactly once;
  `noop()` is deliberately *not* a `RequestEffect` so "forgot to answer the
  caller" is a type error (`tina/src/effect.rs:157-178`). The whole
  `safety_rails_compile_fail` suite exists to enforce this.
- Bug: `request_effect_from_consumed_effect` is a plain **safe `pub fn`** (only
  doc-hidden under `pub mod runtime_internal`). It re-wraps an arbitrary
  `Effect<I>` — including `tina::noop()` — into a `RequestEffect<I>`. Any foreign
  app crate can call it and answer the caller never, manufacturing the exact bad
  state the rail forbids.
- Real-use trigger: app author copies a runtime adapter pattern, or just reaches
  for `runtime_internal` because the type checker is "in the way", and writes
  `tina::runtime_internal::request_effect_from_consumed_effect(tina::noop())`.
- Repro (compiles cleanly on HEAD, no `unsafe` needed):
  ```rust
  fn handle_request(&mut self, _request: Request, call: tina::RequestCall<'_, Self>)
      -> tina::RequestEffect<Self> {
      let _ = &call; // never answer the caller
      tina::runtime_internal::request_effect_from_consumed_effect(tina::noop())
  }
  ```
  Built green. The function is safe: wrapping the call in `unsafe {}` produced
  only an `unused_unsafe` warning.
- Coverage gap: `tina-runtime/tests/safety_rails_compile_fail/split_request_forged_effect.rs`
  pins only the **private** constructor `tina::RequestEffect::from_consumed_effect(noop())`.
  The public door `tina::runtime_internal::request_effect_from_consumed_effect` is
  never pinned — the compile-fail suite tests the wrong door.
- Fix (already done on `57767cf`, just not merged to this line): make the hatch
  an `unsafe fn` (documented as the one rail hole, not memory safety), add a
  single `pub(crate)` safe wrapper in `tina-runtime` that discharges the
  obligation, repoint the ~8 runtime adapter call sites, and add a foreign-crate
  compile-fail fixture asserting the bare call no longer compiles (E0133).
- LLM-pattern: yes — "expose every internal as `pub` under a `_internal`
  module" defeats a type-system safety rail that the rest of the design pays for.

## H7 — RPC client builder silently clobbers a trait arg named `encoding` (NEW, data-corruption)

- Severity: **High** · Confidence: **High**
- `tina-rpc-macros/src/lib.rs:559-588` (`build_client_method`)
- Invariant: the generated `*_request` builder must encode the caller's argument
  values onto the wire unchanged.
- Bug: the builder emits `let encoding = <Enc>::default();` and
  `let payload = Enc::encode(&encoding, &#tuple_expr, …)`. If a trait method has
  an argument literally named `encoding`, the generated `let encoding` **shadows
  the user's argument**, and `#tuple_expr` (`(encoding, payload, …)`) then encodes
  the encoder object's default in that slot instead of the caller's value. The
  caller's `encoding` argument is silently dropped.
- Real-use trigger: a service method like
  `fn set(&self, encoding: ContentEncoding, payload: Vec<u8>)` — perfectly
  natural names. With a `Serialize`-able first-arg type this compiles and ships
  the **wrong bytes**; only because the default encoding `Json` happens not to be
  `Serialize` did my minimal repro surface as a type error rather than silent
  corruption.
- Repro (expansion proof):
  ```rust
  #[tina_rpc::service]
  trait Bank { fn charge(&self, encoding: u64, payload: u64) -> u64; }
  ```
  `cargo expand` shows the builder body:
  ```rust
  let encoding = <Json as Default>::default();
  let payload = <Json as Encoding>::encode(&encoding, &(encoding, payload), max_payload)?;
  //                                              ^^^^^^^^ user's `encoding` arg lost
  ```
- Fix: rename the generated locals to non-collidable identifiers
  (`__tina_encoding`, `__tina_payload`) via `format_ident!`, AND reject reserved
  arg names in `extract_method` (shared with H3). `payload` as an arg name is
  *almost* safe (used in the tuple before the rebinding) but still fragile —
  reject it too.
- LLM-pattern: yes — natural-name local bindings in `quote!` with no hygiene
  discipline; "tested with `fn charge(amount)`", never with an arg called
  `encoding`.

## H3 — RPC client builder collides on reserved arg names with an opaque error (CONFIRMED, live)

- Severity: **Medium** · Confidence: **High**
- `tina-rpc-macros/src/lib.rs:561-566` (request builder fixed params)
- Invariant: a macro should reject malformed input with a diagnostic on the
  offending token, not emit code that fails downstream with an unrelated span.
- Bug: the request builder appends `deadline`, `correlator`, `reply_to`,
  `max_payload` as fixed params after the user's method args. A trait method arg
  with any of those names produces `E0415: identifier '…' is bound more than once
  in this parameter list`, with the span pointing at `#[tina_rpc::service]`, not
  the arg.
- Repro (each fails):
  ```rust
  #[tina_rpc::service] trait Bank { fn a(&self, deadline: u64) -> u64; }     // E0415
  #[tina_rpc::service] trait Bank { fn a(&self, correlator: u64) -> u64; }   // E0415
  #[tina_rpc::service] trait Bank { fn a(&self, max_payload: usize) -> u64; }// E0415
  // reply_to likewise
  ```
  Confirmed: error span is `--> src/main.rs:2:1` (the attribute), message
  "used as parameter more than once".
- Fix: in `extract_method`, reject arg names in
  `{deadline, correlator, reply_to, max_payload, encoding, payload}` with
  `Error::new_spanned(&p.ident, "…reserved by the generated client builder…")`.
  Add trybuild fixtures.
- LLM-pattern: yes — same hygiene blind spot as H7.

## H2 — Generated `#[deny(unused_variables)]` on split `handle_call` turns a warning into a hard error (CONFIRMED, live)

- Severity: **Medium** · Confidence: **High**
- `tina-macros/src/lib.rs:309` (inside the synthesized split `handle_call`)
- Invariant: a macro should not silently impose a stricter lint level than the
  user's crate; an unused argument is a warning, not an error, everywhere else.
- Bug: the synthesized `handle_call` carries `#[deny(unused_variables)]`. A
  split-service request that legitimately ignores its payload (e.g. a `Ping`
  request carrying no data) is forced to hard-error unless the user prefixes the
  binding with `_`. The "lint level defined here" note points at the
  `#[tina_runtime::isolate(...)]` attribute the user never wrote.
- Repro (fails to compile):
  ```rust
  fn handle_request(&mut self, request: Request, call: tina::RequestCall<'_, Self>)
      -> tina::RequestEffect<Self> { call.reply(0) }   // error: unused variable: `request`
  ```
  Confirmed: `error: unused variable: 'request'`; the user must rename to
  `_request`.
- Fix: drop the `#[deny(unused_variables)]` (done on `8144818`). The must-answer
  rail is already enforced by the `RequestEffect` type + the
  `require_call_authority_mentioned` check; the `deny` adds no safety, only
  friction.
- LLM-pattern: yes — "add a `deny` to be safe" without realizing it leaks a
  stricter-than-crate lint level onto user code through macro expansion.

## H8 — `require_call_authority_mentioned` is a textual token-string heuristic with false negatives (NEW)

- Severity: **Medium** · Confidence: **High**
- `tina-macros/src/lib.rs:585-601`
- Invariant (claimed): split `handle_request` must *use* caller authority
  (`call`) — reply, reject, or defer it.
- Bug: the check stringifies the body's token stream
  (`body.to_token_stream().to_string()`) and looks for the bare token `call`.
  Token-stream stringification preserves **string-literal contents**, so a body
  that merely mentions the word `call` inside a `&str` literal passes the check
  while never touching caller authority. Combined with H4 it lets app code both
  pass the heuristic and skip answering the caller.
- Real-use trigger: any handler with a string like `"must answer the call"` (or a
  variable/field/type elsewhere literally named `call`) satisfies the heuristic
  regardless of whether authority is used.
- Repro (compiles green; both H8 and H4 fire):
  ```rust
  fn handle_request(&mut self, _request: Request, call: tina::RequestCall<'_, Self>)
      -> tina::RequestEffect<Self> {
      let _note = "must answer the call somehow";   // satisfies the heuristic
      tina::runtime_internal::request_effect_from_consumed_effect(tina::noop()) // never uses `call`
  }
  ```
- False-positive risk too: a handler that consumes `call` only by *moving* it
  into a helper whose receiver the token text spells differently is fine, but a
  handler that consumes authority via a re-bound alias is still matched only by
  the literal name — the heuristic is name-coupled, not data-flow-coupled.
- Fix: replace the string scan with a real visitor over the parsed
  `syn::Block` (`syn::visit`) that counts identifier uses of `call_name` in
  expression position (ignoring strings/comments), or — better — lean entirely on
  the type system: require the body to *produce* a `RequestEffect` whose only safe
  constructors consume a `RequestCall` (which it already does), and delete the
  heuristic. The heuristic adds a fragile second gate that the type system
  already covers except for the H4 hole; fix H4 and this check can go away.
- LLM-pattern: yes — "grep the token text for the variable name" is a classic
  LLM substitute for real AST/data-flow analysis.

## H6 — `isolate_types!` mixes `::std` and `::core` for `Infallible` (Low / latent)

- Severity: **Low** · Confidence: **High**
- `tina/src/lib.rs:272, 290, 311` use `::std::convert::Infallible`; line 309 uses
  `::core::convert::Infallible`. Inconsistent within the same macro.
- Impact today: none — `tina` is not `#![no_std]` (uses `std` throughout), so the
  macro cannot break in a downstream no_std crate because `tina` itself requires
  std. Purely a latent/consistency issue: if `tina` ever pursues no_std-capable
  isolate authoring, three arms of this macro would fail to expand.
- Fix: use `::core::convert::Infallible` uniformly (done on the unmerged H6
  commit).
- LLM-pattern: mild — reaching for `::std` reflexively where `::core` is correct.

---

## Disproven / does-not-reproduce (recorded with proof)

### H1 (prior) — "hardcoded `msg` / `request_effect` collide with user names in split `handle_call`": NOT reproducible as a user-facing bug

- The synthesized split `handle_call` (`tina-macros/src/lib.rs:310-326`) does use
  hardcoded `msg` (param) and `request_effect` (local), but **neither is
  reachable from user code**:
  - Users never author `handle_call` in split mode; they write `handle_request`.
    The `msg` param exists only in generated scope.
  - The user's `handle_request` body is spliced as the RHS of
    `let request_effect: RequestEffect<Self> = { <user body> };`. The synthetic
    `request_effect` binding lives *outside* that block, so user code inside the
    body cannot collide with it.
- Proofs (both compiled green via `cargo +nightly build`):
  - User named the request arg `msg` **and** declared a local `request_effect`
    inside the body → compiled; `cargo expand` shows correct shadowing.
  - User named the request arg `request_effect` and captured it by reference in a
    closure → compiled.
- Conclusion: the hardcoded identifiers are real but **unreachable**, so H1 is a
  hygiene smell, not a bug. (Still worth `format_ident!`-style `__tina_*` names as
  defense-in-depth, but no user can trigger it.) Mark H1 **disproven as a bug**.

### RPC generated-fn name collision (`charge` vs `charge_request`): does not collide

- `charge` → `charge_request`; `charge_request` → `charge_request_request`. No
  duplicate item. Repro with both methods compiled green.

### `state` / `args` arg names in dispatch closure: no collision

- `emit_method_entry` indexes the args tuple (`args.0`, `args.1`) and uses the
  user arg names only to build the tuple *type*, never as bindings. A trait method
  `fn charge(&self, state: u64, args: u64)` compiled green.

### Service/method byte-length checks: correct boundary, minor drift risk only

- `tina-rpc-macros/src/lib.rs:142, 251` check `> 255`; wire encode uses
  `len > MAX_SERVICE_LEN`/`MAX_METHOD_LEN` where both consts are `u8::MAX as usize`
  (`tina-rpc/src/frame.rs:54,57,585,590`). Boundary matches (255 allowed, 256
  rejected). Only nit: macro hardcodes the literal `255` instead of importing the
  const, so a future wire change would drift silently (Low).

### Crate-rename support: works on both macros

- `tina-macros` threads `#tina_crate` / `#runtime_crate` through every generated
  path (e.g. `:213, :251, :262, :313-322, :404, :418-432`). `tina-rpc-macros`
  threads `#tina_crate` / `#rpc_crate` (`:127-130` and throughout). `::std::string`
  in the client builder (`tina-rpc-macros/src/lib.rs:581-582`) is std-only but not
  rename-sensitive. No hardcoded `::tina::` that bypasses the rename knobs found.

---

## Ranked list

1. `[High/High] tina/src/lib.rs:418` — safe `pub` `runtime_internal::request_effect_from_consumed_effect` lets foreign crates forge a `RequestEffect` from `noop()`, defeating the must-answer-caller rail (H4; fix exists on unmerged `57767cf`).
2. `[High/High] tina-rpc-macros/src/lib.rs:574` — generated `let encoding`/`payload` shadow a trait arg of the same name, silently encoding the encoder default instead of the caller's value (H7, data corruption).
3. `[Medium/High] tina-rpc-macros/src/lib.rs:561` — trait arg named `deadline`/`correlator`/`reply_to`/`max_payload` collides with builder params; opaque E0415 spanned at the attribute (H3).
4. `[Medium/High] tina-macros/src/lib.rs:309` — `#[deny(unused_variables)]` on synthesized split `handle_call` turns an unused request binding into a hard error with a misleading "lint defined here" at the attribute (H2).
5. `[Medium/High] tina-macros/src/lib.rs:585` — `require_call_authority_mentioned` is a token-string scan; matches `call` inside a string literal, passing handlers that never use caller authority (H8). Compounds H4.
6. `[Low/High] tina/src/lib.rs:272,290,311` — `isolate_types!` mixes `::std` and `::core` for `Infallible`; latent no_std break, no impact today (H6).

## Coverage note

Both proc-macro crates (`tina-macros`, `tina-rpc-macros`) fully read; every
finding above was confirmed by compiling a throwaway path-dep crate on the repo's
nightly and by `cargo expand`. Disproven items (incl. prior H1) carry compile
proofs. Crate-rename and length-boundary checks verified clean. **Neither macro
crate has its own trybuild/ui test directory** — all macro compile-fail coverage
lives in `tina-runtime/tests/safety_rails_compile_fail/` and it does **not** cover:
(a) the public `runtime_internal` hatch (H4 — it pins only the private
constructor), (b) reserved/colliding RPC arg names (H3/H7), or (c) the
string-literal false-negative of the authority heuristic (H8). Suggested new
trybuild fixtures: foreign-crate `runtime_internal::request_effect_from_consumed_effect(noop())`
must fail; `fn m(&self, deadline/correlator/reply_to/max_payload/encoding/payload: …)`
must fail with a clear span; and a runtime test asserting an `encoding`-named arg
round-trips its value (guards against the H7 silent clobber regressing).
