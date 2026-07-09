#![forbid(unsafe_code)]

//! Proc macros for Tina isolate authoring.
//!
//! These macros remove Rust trait-impl ceremony while keeping Tina's runtime
//! behavior explicit in the handler body.

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::visit::Visit;
use syn::{
    Error, FnArg, Ident, ImplItem, ImplItemFn, ItemImpl, Pat, Path, Result, ReturnType, Token,
    Type, Visibility, braced, parenthesized, parse_macro_input,
};

struct IsolateArgs {
    message: Option<Type>,
    event: Option<Type>,
    request: Option<Type>,
    reply: Option<Type>,
    send: Option<Type>,
    spawn: Option<Type>,
    spawn_observed: Option<Type>,
    spawn_observed_remote: Option<Type>,
    io: Option<Type>,
    fact: Option<Type>,
    shard: Option<Type>,
    tina_crate: Option<Path>,
    runtime_crate: Option<Path>,
    send_only: Option<Ident>,
}

impl Parse for IsolateArgs {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut args = Self {
            message: None,
            event: None,
            request: None,
            reply: None,
            send: None,
            spawn: None,
            spawn_observed: None,
            spawn_observed_remote: None,
            io: None,
            fact: None,
            shard: None,
            tina_crate: None,
            runtime_crate: None,
            send_only: None,
        };

        while !input.is_empty() {
            let key: Path = input.parse()?;
            let Some(ident_owned) = key.get_ident().cloned() else {
                return Err(Error::new_spanned(
                    key,
                    "expected a simple isolate option name",
                ));
            };
            let name = ident_owned.to_string();

            // `send_only` is a bare flag, not `key = value`.
            if name == "send_only" {
                if args.send_only.is_some() {
                    return Err(Error::new_spanned(
                        ident_owned,
                        "duplicate isolate option `send_only`",
                    ));
                }
                args.send_only = Some(ident_owned);
            } else {
                input.parse::<Token![=]>()?;
                if name == "tina_crate" || name == "runtime_crate" {
                    let value: Path = input.parse()?;
                    match name.as_str() {
                        "tina_crate" => set_once_path(&mut args.tina_crate, value, "tina_crate")?,
                        "runtime_crate" => {
                            set_once_path(&mut args.runtime_crate, value, "runtime_crate")?
                        }
                        _ => unreachable!("checked above"),
                    }

                    if input.peek(Token![,]) {
                        input.parse::<Token![,]>()?;
                    }
                    continue;
                }
                let value: Type = input.parse()?;

                match name.as_str() {
                    "message" => set_once(&mut args.message, value, "message")?,
                    "event" => set_once(&mut args.event, value, "event")?,
                    "request" => set_once(&mut args.request, value, "request")?,
                    "reply" => set_once(&mut args.reply, value, "reply")?,
                    "send" => set_once(&mut args.send, value, "send")?,
                    "spawn" => set_once(&mut args.spawn, value, "spawn")?,
                    "spawn_observed" => {
                        set_once(&mut args.spawn_observed, value, "spawn_observed")?
                    }
                    "spawn_observed_remote" => set_once(
                        &mut args.spawn_observed_remote,
                        value,
                        "spawn_observed_remote",
                    )?,
                    "io" => set_once(&mut args.io, value, "io")?,
                    "fact" => set_once(&mut args.fact, value, "fact")?,
                    "shard" => set_once(&mut args.shard, value, "shard")?,
                    _ => {
                        return Err(Error::new_spanned(
                            key,
                            "expected one of: message, event, request, reply, send, spawn, spawn_observed, spawn_observed_remote, io, fact, shard, tina_crate, runtime_crate, send_only",
                        ));
                    }
                }
            }

            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }

        Ok(args)
    }
}

fn set_once(slot: &mut Option<Type>, value: Type, name: &str) -> Result<()> {
    if slot.is_some() {
        return Err(Error::new_spanned(
            value,
            format!("duplicate isolate option `{name}`"),
        ));
    }

    *slot = Some(value);
    Ok(())
}

fn set_once_path(slot: &mut Option<Path>, value: Path, name: &str) -> Result<()> {
    if slot.is_some() {
        return Err(Error::new_spanned(
            value,
            format!("duplicate isolate option `{name}`"),
        ));
    }

    *slot = Some(value);
    Ok(())
}

#[proc_macro_attribute]
pub fn isolate(args: TokenStream, input: TokenStream) -> TokenStream {
    expand_isolate(args, input, IoDefault::Infallible)
}

/// Like [`isolate`], but the I/O payload defaults to `RuntimeCall<Message>`.
///
/// The expansion still roots the rest of the authoring vocabulary
/// (`Isolate`, `Effect`, `Context`, ...) at `::tina`, so the using crate must
/// depend on `tina` and have it reachable as `::tina`. Only the I/O payload
/// is rooted at `::tina_runtime`. Override with `tina_crate = ...` /
/// `runtime_crate = ...` when the crate names differ.
#[proc_macro_attribute]
pub fn runtime_isolate(args: TokenStream, input: TokenStream) -> TokenStream {
    expand_isolate(args, input, IoDefault::RuntimeCall)
}

/// Generates an explicit continuation enum and dispatcher for a linear flow.
///
/// The macro does not add runtime behavior. Each `step` expands to one enum
/// variant plus one `match` arm in `handle_<flow>`. A step is one of two
/// shapes, chosen by its arrow:
///
/// - `step Name(captures) -> T { .. }` — a runtime-call continuation. The
///   variant carries `RequestContext<Reply>`, the captures, and
///   `CallOutcome<T>`; the body must mention `req` so caller authority is
///   explicitly replied, threaded, or intentionally dropped.
/// - `step Name(captures) -> raw T { .. }` — an ordinary (non-call)
///   continuation, e.g. a `sleep(..).then(..)` timer wake-up. The variant
///   carries only the captures and `T` verbatim (no `CallOutcome` wrap, no
///   `RequestContext` slot); use this when the step's caller authority, if
///   any, is parked elsewhere and resumed by an explicit key rather than
///   carried in the message.
#[proc_macro]
pub fn flow(input: TokenStream) -> TokenStream {
    let flow = parse_macro_input!(input as FlowInput);
    match build_flow(flow) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.into_compile_error().into(),
    }
}

mod flow_kw {
    syn::custom_keyword!(flow);
    syn::custom_keyword!(raw);
    syn::custom_keyword!(reply);
    syn::custom_keyword!(runtime_crate);
    syn::custom_keyword!(step);
    syn::custom_keyword!(tina_crate);
}

struct FlowInput {
    visibility: Visibility,
    name: Ident,
    isolate: Type,
    reply: Type,
    tina_crate: Option<Path>,
    runtime_crate: Option<Path>,
    steps: Vec<FlowStep>,
}

struct FlowStep {
    name: Ident,
    captures: Vec<FlowCapture>,
    outcome: StepOutcome,
    body: Box<syn::Block>,
}

/// Shape of a step's final tuple field.
///
/// `Call(T)` is the original shape: the step is a runtime-call
/// continuation, so the variant carries caller authority
/// (`RequestContext<Reply>`) and the field is wrapped as
/// `CallOutcome<T>`. `Raw(T)` is a non-call continuation (e.g. a
/// `sleep(..).then(..)` timer wake-up): the field is `T` verbatim, with
/// no `RequestContext` slot and no requirement that the body mention
/// `req`. Use `Raw` when the step's caller authority (if any) is parked
/// elsewhere and resumed by an explicit key, not carried in the message.
enum StepOutcome {
    Call(Type),
    Raw(Type),
}

struct FlowCapture {
    name: Ident,
    ty: Type,
}

impl Parse for FlowInput {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let visibility: Visibility = input.parse()?;
        input.parse::<flow_kw::flow>()?;
        let name: Ident = input.parse()?;
        input.parse::<Token![for]>()?;
        let isolate: Type = input.parse()?;

        let content;
        braced!(content in input);

        let mut tina_crate = None;
        let mut runtime_crate = None;
        loop {
            if content.peek(flow_kw::tina_crate) {
                content.parse::<flow_kw::tina_crate>()?;
                content.parse::<Token![=]>()?;
                let value: Path = content.parse()?;
                set_once_path(&mut tina_crate, value, "tina_crate")?;
                content.parse::<Token![;]>()?;
            } else if content.peek(flow_kw::runtime_crate) {
                content.parse::<flow_kw::runtime_crate>()?;
                content.parse::<Token![=]>()?;
                let value: Path = content.parse()?;
                set_once_path(&mut runtime_crate, value, "runtime_crate")?;
                content.parse::<Token![;]>()?;
            } else {
                break;
            }
        }

        content.parse::<flow_kw::reply>()?;
        let reply: Type = content.parse()?;
        content.parse::<Token![;]>()?;

        let mut steps = Vec::new();
        let mut step_names = std::collections::BTreeSet::new();
        while !content.is_empty() {
            content.parse::<flow_kw::step>()?;
            let step_name: Ident = content.parse()?;
            if !step_names.insert(ident_key(&step_name)) {
                return Err(Error::new_spanned(step_name, "duplicate flow step name"));
            }

            let fields;
            parenthesized!(fields in content);
            let mut captures = Vec::new();
            let mut capture_names = std::collections::BTreeSet::new();
            while !fields.is_empty() {
                let field_name: Ident = fields.parse()?;
                if ident_is(&field_name, "req") || ident_is(&field_name, "outcome") {
                    return Err(Error::new_spanned(
                        field_name,
                        "flow capture name is reserved; use a name other than `req` or `outcome`",
                    ));
                }
                if !capture_names.insert(ident_key(&field_name)) {
                    return Err(Error::new_spanned(
                        field_name,
                        "duplicate flow capture name",
                    ));
                }
                fields.parse::<Token![:]>()?;
                let field_ty: Type = fields.parse()?;
                captures.push(FlowCapture {
                    name: field_name,
                    ty: field_ty,
                });
                if fields.peek(Token![,]) {
                    fields.parse::<Token![,]>()?;
                }
            }

            content.parse::<Token![->]>()?;
            let outcome = if content.peek(flow_kw::raw) {
                content.parse::<flow_kw::raw>()?;
                StepOutcome::Raw(content.parse()?)
            } else {
                StepOutcome::Call(content.parse()?)
            };
            let body: syn::Block = content.parse()?;
            steps.push(FlowStep {
                name: step_name,
                captures,
                outcome,
                body: Box::new(body),
            });
        }

        if steps.is_empty() {
            return Err(Error::new_spanned(
                name,
                "`tina::flow!` needs at least one `step`",
            ));
        }

        Ok(Self {
            visibility,
            name,
            isolate,
            reply,
            tina_crate,
            runtime_crate,
            steps,
        })
    }
}

fn build_flow(flow: FlowInput) -> Result<proc_macro2::TokenStream> {
    let visibility = &flow.visibility;
    let flow_name = &flow.name;
    let isolate = &flow.isolate;
    let reply = &flow.reply;
    let tina_crate = flow
        .tina_crate
        .clone()
        .unwrap_or_else(|| syn::parse_quote!(::tina));
    let runtime_crate = flow
        .runtime_crate
        .clone()
        .unwrap_or_else(|| syn::parse_quote!(::tina_runtime));
    let handler = format_ident!("handle_{}", ident_to_snake(&flow.name));

    for step in &flow.steps {
        // Raw steps carry no `RequestContext` slot (their caller authority,
        // if any, is parked elsewhere and resumed by an explicit key), so
        // there is nothing for `req` to name and no policy to enforce here.
        if matches!(step.outcome, StepOutcome::Call(_))
            && !block_mentions_unshadowed_ident(&step.body, "req")
        {
            return Err(Error::new_spanned(
                &step.name,
                "flow step body must mention `req` so caller authority is explicitly replied, threaded, or intentionally dropped",
            ));
        }
    }

    let variants = flow.steps.iter().map(|step| {
        let step_name = &step.name;
        let capture_types = step.captures.iter().map(|capture| &capture.ty);
        match &step.outcome {
            StepOutcome::Call(outcome) => quote! {
                #step_name(
                    #tina_crate::RequestContext<#reply>,
                    #(#capture_types,)*
                    #runtime_crate::CallOutcome<#outcome>,
                )
            },
            StepOutcome::Raw(outcome) => quote! {
                #step_name(
                    #(#capture_types,)*
                    #outcome,
                )
            },
        }
    });

    let arms = flow.steps.iter().map(|step| {
        let step_name = &step.name;
        let capture_names = step.captures.iter().map(|capture| &capture.name);
        let body = &step.body;
        match &step.outcome {
            StepOutcome::Call(_) => quote! {
                #flow_name::#step_name(req, #(#capture_names,)* outcome) => #body
            },
            StepOutcome::Raw(_) => quote! {
                #flow_name::#step_name(#(#capture_names,)* outcome) => #body
            },
        }
    });

    Ok(quote! {
        #visibility enum #flow_name {
            #(#variants,)*
        }

        impl #isolate {
            #visibility fn #handler(&mut self, msg: #flow_name) -> #tina_crate::Effect<Self> {
                match msg {
                    #(#arms,)*
                }
            }
        }
    })
}

fn ident_to_snake(ident: &Ident) -> String {
    let mut out = String::new();
    let chars: Vec<_> = ident.to_string().chars().collect();
    for (idx, ch) in chars.iter().copied().enumerate() {
        if ch.is_uppercase() {
            let prev = idx.checked_sub(1).and_then(|prev| chars.get(prev));
            let next = chars.get(idx + 1);
            if idx > 0
                && (prev.is_some_and(|prev| prev.is_lowercase() || prev.is_ascii_digit())
                    || next.is_some_and(|next| next.is_lowercase()))
            {
                out.push('_');
            }
            for lower in ch.to_lowercase() {
                out.push(lower);
            }
        } else {
            out.push(ch);
        }
    }
    out
}

fn ident_is(ident: &Ident, expected: &str) -> bool {
    ident_key(ident) == expected
}

fn ident_key(ident: &Ident) -> String {
    ident.to_string().trim_start_matches("r#").to_owned()
}

fn path_is_ident(path: &Path, expected: &str) -> bool {
    path.leading_colon.is_none()
        && path.segments.len() == 1
        && ident_is(&path.segments[0].ident, expected)
}

fn pat_contains_ident(pat: &Pat, ident: &str) -> bool {
    struct Finder<'a> {
        ident: &'a str,
        found: bool,
    }

    impl<'ast> Visit<'ast> for Finder<'_> {
        fn visit_pat_ident(&mut self, node: &'ast syn::PatIdent) {
            if ident_is(&node.ident, self.ident) {
                self.found = true;
            }
            syn::visit::visit_pat_ident(self, node);
        }
    }

    let mut finder = Finder {
        ident,
        found: false,
    };
    finder.visit_pat(pat);
    finder.found
}

fn block_mentions_unshadowed_ident(block: &syn::Block, ident: &str) -> bool {
    fn token_stream_mentions_ident(tokens: &proc_macro2::TokenStream, ident: &str) -> bool {
        tokens.clone().into_iter().any(|token| match token {
            proc_macro2::TokenTree::Ident(candidate) => ident_is(&candidate, ident),
            proc_macro2::TokenTree::Group(group) => {
                token_stream_mentions_ident(&group.stream(), ident)
            }
            proc_macro2::TokenTree::Punct(_) | proc_macro2::TokenTree::Literal(_) => false,
        })
    }

    struct Finder<'a> {
        ident: &'a str,
        found: bool,
        shadowed: usize,
    }

    impl<'ast> Visit<'ast> for Finder<'_> {
        fn visit_block(&mut self, node: &'ast syn::Block) {
            let shadowed_at_entry = self.shadowed;
            for stmt in &node.stmts {
                match stmt {
                    syn::Stmt::Local(local) => {
                        if let Some(init) = &local.init {
                            self.visit_expr(&init.expr);
                            if let Some((_, diverge)) = &init.diverge {
                                self.visit_expr(diverge);
                            }
                        }
                        if pat_contains_ident(&local.pat, self.ident) {
                            self.shadowed += 1;
                        }
                    }
                    syn::Stmt::Item(_) => {}
                    other => self.visit_stmt(other),
                }

                if self.found {
                    break;
                }
            }
            self.shadowed = shadowed_at_entry;
        }

        fn visit_expr_path(&mut self, node: &'ast syn::ExprPath) {
            if self.shadowed == 0 && node.qself.is_none() && path_is_ident(&node.path, self.ident) {
                self.found = true;
            }
            syn::visit::visit_expr_path(self, node);
        }

        fn visit_macro(&mut self, node: &'ast syn::Macro) {
            // Macro bodies are opaque to `syn::Visit`. When no Rust binding
            // has shadowed the caller authority, a raw token hit is enough:
            // false accepts still fail later by move semantics, while false
            // rejects block valid helper-macro adoption.
            if self.shadowed == 0 && token_stream_mentions_ident(&node.tokens, self.ident) {
                self.found = true;
            }
            syn::visit::visit_macro(self, node);
        }

        fn visit_expr_closure(&mut self, node: &'ast syn::ExprClosure) {
            let shadows = node
                .inputs
                .iter()
                .any(|input| pat_contains_ident(input, self.ident));
            if shadows {
                self.shadowed += 1;
            }
            self.visit_expr(&node.body);
            if shadows {
                self.shadowed -= 1;
            }
        }

        fn visit_expr_for_loop(&mut self, node: &'ast syn::ExprForLoop) {
            self.visit_expr(&node.expr);
            let shadows = pat_contains_ident(&node.pat, self.ident);
            if shadows {
                self.shadowed += 1;
            }
            self.visit_block(&node.body);
            if shadows {
                self.shadowed -= 1;
            }
        }

        fn visit_expr_if(&mut self, node: &'ast syn::ExprIf) {
            if let syn::Expr::Let(expr_let) = node.cond.as_ref() {
                self.visit_expr(&expr_let.expr);
                let shadows = pat_contains_ident(&expr_let.pat, self.ident);
                if shadows {
                    self.shadowed += 1;
                }
                self.visit_block(&node.then_branch);
                if shadows {
                    self.shadowed -= 1;
                }
            } else {
                self.visit_expr(&node.cond);
                self.visit_block(&node.then_branch);
            }

            if let Some((_, else_branch)) = &node.else_branch {
                self.visit_expr(else_branch);
            }
        }

        fn visit_expr_while(&mut self, node: &'ast syn::ExprWhile) {
            if let syn::Expr::Let(expr_let) = node.cond.as_ref() {
                self.visit_expr(&expr_let.expr);
                let shadows = pat_contains_ident(&expr_let.pat, self.ident);
                if shadows {
                    self.shadowed += 1;
                }
                self.visit_block(&node.body);
                if shadows {
                    self.shadowed -= 1;
                }
            } else {
                self.visit_expr(&node.cond);
                self.visit_block(&node.body);
            }
        }

        fn visit_arm(&mut self, node: &'ast syn::Arm) {
            let shadows = pat_contains_ident(&node.pat, self.ident);
            if shadows {
                self.shadowed += 1;
            }
            if let Some((_, guard)) = &node.guard {
                self.visit_expr(guard);
            }
            self.visit_expr(&node.body);
            if shadows {
                self.shadowed -= 1;
            }
        }
    }

    let mut finder = Finder {
        ident,
        found: false,
        shadowed: 0,
    };
    finder.visit_block(block);
    finder.found
}

enum IoDefault {
    Infallible,
    RuntimeCall,
}

fn expand_isolate(args: TokenStream, input: TokenStream, io_default: IoDefault) -> TokenStream {
    let args = parse_macro_input!(args as IsolateArgs);
    let mut item = parse_macro_input!(input as ItemImpl);

    match build_isolate(&mut item, args, io_default) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.into_compile_error().into(),
    }
}

fn build_isolate(
    item: &mut ItemImpl,
    args: IsolateArgs,
    io_default: IoDefault,
) -> Result<proc_macro2::TokenStream> {
    if item.trait_.is_some() {
        return Err(Error::new_spanned(
            &item.self_ty,
            "`#[tina::isolate]` belongs on an inherent impl block, not a trait impl",
        ));
    }

    let split_service = match (&args.event, &args.request, &args.message) {
        (Some(_), Some(_), None) => true,
        (None, None, Some(_)) => false,
        (Some(_), None, _) | (None, Some(_), _) => {
            return Err(Error::new_spanned(
                &item.self_ty,
                "`event = ...` and `request = ...` must be supplied together",
            ));
        }
        (Some(_), Some(_), Some(_)) => {
            return Err(Error::new_spanned(
                &item.self_ty,
                "`message = ...` cannot be combined with `event = ...` / `request = ...`",
            ));
        }
        (None, None, None) => {
            return Err(Error::new_spanned(
                &item.self_ty,
                "missing required isolate option `message = ...` or `event = ... , request = ...`",
            ));
        }
    };
    let tina_crate = args
        .tina_crate
        .clone()
        .unwrap_or_else(|| syn::parse_quote!(::tina));
    let runtime_crate = args
        .runtime_crate
        .clone()
        .unwrap_or_else(|| syn::parse_quote!(::tina_runtime));
    let message = if split_service {
        let event = args.event.clone().expect("checked above");
        let request = args.request.clone().expect("checked above");
        syn::parse_quote!(#tina_crate::ServiceMessage<#event, #request>)
    } else {
        args.message.expect("checked above")
    };
    // `shard = ...` is optional. Single-shard
    // programs default to `tina::SingleShard`; multi-shard programs
    // continue to declare their own shard type explicitly. The default is
    // a real type (not a global mutable singleton), so registration still
    // requires the user to construct the shard at runtime startup.
    let shard = args
        .shard
        .unwrap_or_else(|| syn::parse_quote!(#tina_crate::SingleShard));

    // `send_only` declares the isolate has no public callable surface. The
    // reply type defaults to `()` so it can be registered through
    // `register_service_send_only` (which requires `Reply = ()`). Authoring
    // `handle_call` on a `send_only` isolate is a compile error: the
    // capability-typed `SendOnlyServiceHandle` returned by registration has no
    // `.call` lane, so any reachable call would be a routing bug, not a
    // user-visible API.
    if let Some(send_only) = &args.send_only {
        if args.reply.is_some() {
            return Err(Error::new_spanned(
                send_only,
                "`send_only` isolates must not declare a `reply` (the reply type is forced to `()`)",
            ));
        }
        if args.io.is_some() {
            return Err(Error::new_spanned(
                send_only,
                "`send_only` isolates must not declare an `io` channel",
            ));
        }
    }

    let reply = args.reply.unwrap_or_else(|| syn::parse_quote!(()));
    let send = args
        .send
        .unwrap_or_else(|| syn::parse_quote!(#tina_crate::Outbound<::core::convert::Infallible>));
    let spawn = args
        .spawn
        .unwrap_or_else(|| syn::parse_quote!(::core::convert::Infallible));
    let spawn_observed = args
        .spawn_observed
        .unwrap_or_else(|| syn::parse_quote!(::core::convert::Infallible));
    let spawn_observed_remote = args
        .spawn_observed_remote
        .unwrap_or_else(|| syn::parse_quote!(::core::convert::Infallible));
    let io = match args.io {
        Some(io) => io,
        None => match io_default {
            IoDefault::Infallible => syn::parse_quote!(::core::convert::Infallible),
            IoDefault::RuntimeCall => syn::parse_quote!(#runtime_crate::RuntimeCall<#message>),
        },
    };
    let fact = args
        .fact
        .unwrap_or_else(|| syn::parse_quote!(::core::convert::Infallible));

    let isolate = item.self_ty.clone();
    let generics = item.generics.clone();
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    let (attrs, msg_name, ctx_name, body, handle_call_tokens, has_handle_call) = if split_service {
        if args.send_only.is_some() {
            return Err(Error::new_spanned(
                &item.self_ty,
                "`send_only` cannot be combined with split `event` / `request` services",
            ));
        }
        let event_index = item
            .items
            .iter()
            .position(|candidate| {
                matches!(candidate, ImplItem::Fn(method) if method.sig.ident == "handle_event")
            })
            .ok_or_else(|| Error::new_spanned(&item.self_ty, "expected a `fn handle_event(...)` method"))?;
        let ImplItem::Fn(event_method) = item.items.remove(event_index) else {
            unreachable!("event_index only matches functions")
        };
        let request_index = item
            .items
            .iter()
            .position(|candidate| {
                matches!(candidate, ImplItem::Fn(method) if method.sig.ident == "handle_request")
            })
            .ok_or_else(|| Error::new_spanned(&item.self_ty, "expected a `fn handle_request(...)` method"))?;
        let ImplItem::Fn(request_method) = item.items.remove(request_index) else {
            unreachable!("request_index only matches functions")
        };
        let (event_name, event_ctx_name) =
            validate_handler(&event_method, "handle_event", "event", "ctx")?;
        let (request_attrs, request_name, call_name, request_body) =
            validate_call_handler(&request_method, "handle_request", "request", "call")?;
        require_call_authority_mentioned(&request_body, &call_name)?;
        let service_message_name =
            Ident::new("__tina_service_message", proc_macro2::Span::mixed_site());
        let event_attrs = event_method.attrs.clone();
        let event_body = Box::new(event_method.block.clone());
        // No `#[deny(unused_variables)]` here: it would override the caller's
        // own lint level on the spliced request body and hard-error a handler
        // that answers the caller without reading a unit/marker request. The
        // `RequestEffect<Self>` linear type below already enforces the real
        // invariant (the caller is answered); reading the payload is optional.
        let handle_call_tokens = quote! {
            #(#request_attrs)*
            fn handle_call(
                &mut self,
                #service_message_name: Self::Message,
                #call_name: #tina_crate::CallContext<'_, Self>,
            ) -> #tina_crate::Effect<Self> {
                let #call_name = #tina_crate::RequestCall::new(#call_name);
                match #service_message_name {
                    #tina_crate::ServiceMessage::Request(#request_name) => {
                        let request_effect: #tina_crate::RequestEffect<Self> =
                            (|| -> #tina_crate::RequestEffect<Self> #request_body)();
                        request_effect.into_effect()
                    }
                    #tina_crate::ServiceMessage::Event(_) => {
                        #call_name.reject(#tina_crate::CallRejectedReason::UnsupportedMessage)
                            .into_effect()
                    }
                }
            }
        };
        let body = syn::parse_quote!({
            match #event_name {
                #tina_crate::ServiceMessage::Event(#event_name) => #event_body,
                #tina_crate::ServiceMessage::Request(_) => {
                    #tina_crate::reject(#tina_crate::CallRejectedReason::UnsupportedMessage)
                }
            }
        });
        (
            event_attrs,
            event_name,
            event_ctx_name,
            body,
            handle_call_tokens,
            true,
        )
    } else {
        let handle_index = item
            .items
            .iter()
            .position(
                |candidate| matches!(candidate, ImplItem::Fn(method) if method.sig.ident == "handle"),
            )
            .ok_or_else(|| Error::new_spanned(&item.self_ty, "expected a `fn handle(...)` method"))?;

        let ImplItem::Fn(handle) = item.items.remove(handle_index) else {
            unreachable!("handle_index only matches functions")
        };

        let (msg_name, ctx_name) = validate_handle(&handle)?;
        let handle_call_index = item.items.iter().position(
            |candidate| matches!(candidate, ImplItem::Fn(method) if method.sig.ident == "handle_call"),
        );
        let handle_call = if let Some(index) = handle_call_index {
            let ImplItem::Fn(method) = item.items.remove(index) else {
                unreachable!("handle_call_index only matches functions")
            };
            if let Some(send_only) = &args.send_only {
                return Err(Error::new_spanned(
                    send_only,
                    "`send_only` isolates must not define `handle_call`; remove the flag or remove `handle_call`",
                ));
            }
            Some(validate_handle_call(&method)?)
        } else {
            None
        };

        let attrs = handle.attrs.clone();
        let body = Box::new(handle.block.clone());
        let has_handle_call = handle_call.is_some();
        let handle_call_tokens = if let Some((attrs, msg_name, call_name, body)) = handle_call {
            quote! {
                #(#attrs)*
                fn handle_call(
                    &mut self,
                    #msg_name: Self::Message,
                    #call_name: #tina_crate::CallContext<'_, Self>,
                ) -> #tina_crate::Effect<Self> {
                    #body
                }
            }
        } else {
            quote! {}
        };
        (
            attrs,
            msg_name,
            ctx_name,
            body,
            handle_call_tokens,
            has_handle_call,
        )
    };
    let callable_marker_impl = if has_handle_call {
        quote! {
            impl #impl_generics #tina_crate::CallableIsolate for #isolate #ty_generics #where_clause {}
        }
    } else {
        quote! {}
    };
    let remaining_impl = if item.items.is_empty() {
        quote! {}
    } else {
        quote! { #item }
    };

    Ok(quote! {
        #remaining_impl

        impl #impl_generics #tina_crate::Isolate for #isolate #ty_generics #where_clause {
            type Message = #message;
            type Reply = #reply;
            type Send = #send;
            type Spawn = #spawn;
            type SpawnObserved = #spawn_observed;
            type SpawnObservedRemote = #spawn_observed_remote;
            type Io = #io;
            type Fact = #fact;
            type Shard = #shard;

            #(#attrs)*
            fn handle(
                &mut self,
                #msg_name: Self::Message,
                #ctx_name: &mut #tina_crate::Context<'_, Self::Shard, Self::Reply>,
            ) -> #tina_crate::Effect<Self> {
                #body
            }

            #handle_call_tokens
        }

        #callable_marker_impl
    })
}

fn validate_handle(handle: &ImplItemFn) -> Result<(syn::Ident, syn::Ident)> {
    validate_handler(handle, "handle", "msg", "ctx")
}

fn validate_handler(
    handle: &ImplItemFn,
    method_name: &str,
    message_arg: &str,
    context_arg: &str,
) -> Result<(syn::Ident, syn::Ident)> {
    if handle.sig.asyncness.is_some() {
        return Err(Error::new_spanned(
            handle.sig.asyncness,
            format!(
                "Tina {method_name} handlers are synchronous; return an Effect instead of `async fn`"
            ),
        ));
    }

    if handle.sig.constness.is_some() {
        return Err(Error::new_spanned(
            handle.sig.constness,
            format!("Tina {method_name} handlers cannot be const"),
        ));
    }

    let inputs = &handle.sig.inputs;
    if inputs.len() != 3 {
        return Err(Error::new_spanned(
            &handle.sig,
            format!(
                "expected `fn {method_name}(&mut self, {message_arg}, {context_arg}) -> Effect<Self>`"
            ),
        ));
    }

    match inputs.first() {
        Some(FnArg::Receiver(receiver))
            if receiver.reference.is_some() && receiver.mutability.is_some() => {}
        _ => {
            return Err(Error::new_spanned(
                &handle.sig,
                format!("first {method_name} argument must be `&mut self`"),
            ));
        }
    }

    let msg_name = simple_argument_name(handle, 1, message_arg)?;
    let ctx_name = simple_argument_name(handle, 2, context_arg)?;

    if handle.sig.generics.lt_token.is_some() {
        return Err(Error::new_spanned(
            &handle.sig.generics,
            format!("{method_name} cannot have its own generic parameters"),
        ));
    }

    if !matches!(handle.sig.output, ReturnType::Default) {
        return Ok((msg_name, ctx_name));
    }

    Err(Error::new_spanned(
        &handle.sig,
        format!("{method_name} must return `tina::Effect<Self>`"),
    ))
}

fn validate_handle_call(
    handle_call: &ImplItemFn,
) -> Result<(Vec<syn::Attribute>, syn::Ident, syn::Ident, Box<syn::Block>)> {
    validate_call_handler(handle_call, "handle_call", "msg", "call")
}

fn validate_call_handler(
    handle_call: &ImplItemFn,
    method_name: &str,
    message_arg: &str,
    call_arg: &str,
) -> Result<(Vec<syn::Attribute>, syn::Ident, syn::Ident, Box<syn::Block>)> {
    if handle_call.sig.asyncness.is_some() {
        return Err(Error::new_spanned(
            handle_call.sig.asyncness,
            format!(
                "Tina {method_name} handlers are synchronous; return an Effect instead of `async fn`"
            ),
        ));
    }

    if handle_call.sig.constness.is_some() {
        return Err(Error::new_spanned(
            handle_call.sig.constness,
            format!("Tina {method_name} handlers cannot be const"),
        ));
    }

    let inputs = &handle_call.sig.inputs;
    if inputs.len() != 3 {
        return Err(Error::new_spanned(
            &handle_call.sig,
            format!(
                "expected `fn {method_name}(&mut self, {message_arg}, {call_arg}) -> Effect<Self>`"
            ),
        ));
    }

    match inputs.first() {
        Some(FnArg::Receiver(receiver))
            if receiver.reference.is_some() && receiver.mutability.is_some() => {}
        _ => {
            return Err(Error::new_spanned(
                &handle_call.sig,
                format!("first {method_name} argument must be `&mut self`"),
            ));
        }
    }

    let msg_name = simple_argument_name(handle_call, 1, message_arg)?;
    let call_name = simple_argument_name(handle_call, 2, call_arg)?;

    if handle_call.sig.generics.lt_token.is_some() {
        return Err(Error::new_spanned(
            &handle_call.sig.generics,
            format!("{method_name} cannot have its own generic parameters"),
        ));
    }

    if matches!(handle_call.sig.output, ReturnType::Default) {
        return Err(Error::new_spanned(
            &handle_call.sig,
            format!("{method_name} must return `tina::Effect<Self>`"),
        ));
    }

    Ok((
        handle_call.attrs.clone(),
        msg_name,
        call_name,
        Box::new(handle_call.block.clone()),
    ))
}

/// Counts genuine *expression-position* uses of `call_name` in the handler body.
///
/// Walks the parsed AST instead of the token text: a path expression like
/// `call`, `call.reply(...)`, or `helper(call)` counts; the identifier inside a
/// string literal (`"answer the call"`) or a comment does not, because the
/// visitor never reaches literal contents.
struct CallAuthorityUse<'a> {
    needle: &'a syn::Ident,
    used: bool,
}

impl<'a, 'ast> syn::visit::Visit<'ast> for CallAuthorityUse<'a> {
    fn visit_expr_path(&mut self, node: &'ast syn::ExprPath) {
        if node.qself.is_none() && node.path.is_ident(self.needle) {
            self.used = true;
        }
        syn::visit::visit_expr_path(self, node);
    }
}

fn require_call_authority_mentioned(body: &syn::Block, call_name: &syn::Ident) -> Result<()> {
    let mut visitor = CallAuthorityUse {
        needle: call_name,
        used: false,
    };
    syn::visit::visit_block(&mut visitor, body);
    if visitor.used {
        return Ok(());
    }

    Err(Error::new_spanned(
        body,
        format!(
            "split `handle_request` must use caller authority `{call_name}`; reply, reject, or defer it"
        ),
    ))
}

fn simple_argument_name(
    handle: &ImplItemFn,
    index: usize,
    argument_name: &str,
) -> Result<syn::Ident> {
    let Some(FnArg::Typed(argument)) = handle.sig.inputs.iter().nth(index) else {
        return Err(Error::new_spanned(
            &handle.sig,
            "handle arguments after self must be typed bindings",
        ));
    };

    match &*argument.pat {
        Pat::Ident(ident) => Ok(ident.ident.clone()),
        other => Err(Error::new_spanned(
            other,
            format!("handle `{argument_name}` argument must use a simple binding"),
        )),
    }
}
