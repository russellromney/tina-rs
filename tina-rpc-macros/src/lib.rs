#![forbid(unsafe_code)]

//! Proc macros for `tina-rpc` service traits.
//!
//! This crate ships the `#[tina_rpc::service]` attribute macro. The
//! `tina-rpc` crate re-exports it; do not depend on this crate
//! directly.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::ext::IdentExt;
use syn::parse::{Parse, ParseStream};
use syn::{
    Error, FnArg, Ident, ItemTrait, Pat, Path, Result, ReturnType, Token, TraitItem, TraitItemFn,
    Type, parse_macro_input,
};

/// `#[tina_rpc::service]` — turns a Rust trait into a typed RPC
/// service surface.
///
/// The expansion emits the trait unchanged plus two zero-sized
/// companion structs:
///
/// - `<Trait>Service` — `dispatch::<H, Sh>(state, limits)` builds a
///   `tina_rpc::Dispatch` from a user impl `H: <Trait>` plus
///   `tina_rpc::PayloadLimits`. Drop into a topology adapter
///   (`tina_rpc::SingleService`) and register with the runtime.
///
/// - `<Trait>Client` — per-method `name_request(...)` builders that
///   produce a `tina_rpc::ClientRequest` with the args tuple
///   pre-encoded, plus `name_decode_reply(...)` for the inverse.
///   Caller still owns deadline, correlator, reply_to, and the
///   wiring to the `tina_rpc::Client` isolate.
///
/// Synchronous trait methods only. JSON is the default
/// `tina_rpc::Encoding` (the only one shipped today); other
/// encodings can be requested via
/// `#[tina_rpc::service(encoding = SomeOther)]`.
///
/// Renamed dependencies can be passed explicitly:
/// `#[tina_rpc::service(tina_crate = my_tina, rpc_crate = my_rpc)]`.
#[proc_macro_attribute]
pub fn service(args: TokenStream, input: TokenStream) -> TokenStream {
    let args = parse_macro_input!(args as ServiceArgs);
    let item = parse_macro_input!(input as ItemTrait);
    match expand(args, item) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.into_compile_error().into(),
    }
}

// ---------------------------------------------------------------------------
// Attribute argument parsing
// ---------------------------------------------------------------------------

struct ServiceArgs {
    encoding: Option<Type>,
    name: Option<String>,
    tina_crate: Option<Path>,
    rpc_crate: Option<Path>,
}

impl Parse for ServiceArgs {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut args = Self {
            encoding: None,
            name: None,
            tina_crate: None,
            rpc_crate: None,
        };
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            match key.to_string().as_str() {
                "encoding" => {
                    let ty: Type = input.parse()?;
                    if args.encoding.is_some() {
                        return Err(Error::new_spanned(ty, "duplicate `encoding`"));
                    }
                    args.encoding = Some(ty);
                }
                "name" => {
                    let lit: syn::LitStr = input.parse()?;
                    if args.name.is_some() {
                        return Err(Error::new_spanned(lit, "duplicate `name`"));
                    }
                    args.name = Some(lit.value());
                }
                "tina_crate" => {
                    let path: Path = input.parse()?;
                    if args.tina_crate.is_some() {
                        return Err(Error::new_spanned(path, "duplicate `tina_crate`"));
                    }
                    args.tina_crate = Some(path);
                }
                "rpc_crate" => {
                    let path: Path = input.parse()?;
                    if args.rpc_crate.is_some() {
                        return Err(Error::new_spanned(path, "duplicate `rpc_crate`"));
                    }
                    args.rpc_crate = Some(path);
                }
                other => {
                    return Err(Error::new_spanned(
                        key,
                        format!(
                            "unknown service option `{other}`; expected `encoding`, `name`, `tina_crate`, or `rpc_crate`"
                        ),
                    ));
                }
            }
            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }
        Ok(args)
    }
}

// ---------------------------------------------------------------------------
// Expansion
// ---------------------------------------------------------------------------

fn expand(args: ServiceArgs, item: ItemTrait) -> Result<TokenStream2> {
    let trait_ident = item.ident.clone();
    let service_name_lit = args
        .name
        .unwrap_or_else(|| trait_ident.unraw().to_string());
    let tina_crate = args.tina_crate.unwrap_or_else(|| syn::parse_quote!(::tina));
    let rpc_crate = args
        .rpc_crate
        .unwrap_or_else(|| syn::parse_quote!(::tina_rpc));
    // Validate the service name against the wire frame's
    // MAX_SERVICE_LEN (255 bytes) and reject the empty string. A
    // zero-byte service name encodes successfully but no registry
    // can name it through `register("")`-equivalent flow without
    // confusion.
    if service_name_lit.is_empty() {
        return Err(Error::new_spanned(
            &item.ident,
            "service `name` must be non-empty (defaults to the trait identifier)",
        ));
    }
    if service_name_lit.len() > 255 {
        return Err(Error::new_spanned(
            &item.ident,
            format!(
                "service `name` length {} exceeds wire MAX_SERVICE_LEN (255 bytes)",
                service_name_lit.len()
            ),
        ));
    }
    let encoding_ty: Type = match args.encoding {
        Some(ty) => ty,
        None => syn::parse_quote!(#rpc_crate::Json),
    };

    let mut methods = Vec::new();
    for trait_item in &item.items {
        if let TraitItem::Fn(method) = trait_item {
            methods.push(extract_method(method)?);
        }
    }

    // An empty trait expands to an empty MethodTable; every wire
    // call returns UnknownMethod. Slightly weird but coherent and
    // useful as a placeholder during refactoring; not worth a hard
    // rejection.

    let service_struct = format_ident!("{}Service", trait_ident);
    let client_struct = format_ident!("{}Client", trait_ident);

    let dispatch_impl = build_dispatch_impl(
        &trait_ident,
        &service_struct,
        &service_name_lit,
        &encoding_ty,
        &methods,
        &tina_crate,
        &rpc_crate,
    );
    let client_impl = build_client_impl(
        &client_struct,
        &service_name_lit,
        &encoding_ty,
        &methods,
        &tina_crate,
        &rpc_crate,
    );

    Ok(quote! {
        #item

        #dispatch_impl

        #client_impl
    })
}

struct MethodSig {
    name: Ident,
    name_str: String,
    request_fn: Ident,
    decode_reply_fn: Ident,
    args: Vec<MethodArg>,
    return_ty: Type,
}

struct MethodArg {
    name: Ident,
    ty: Type,
}

/// Names the generated client builder binds and would clobber a same-named
/// trait arg. `deadline`/`correlator`/`reply_to`/`max_payload` are appended
/// params (collide as E0415); `encoding`/`payload` are builder-body locals that
/// *silently shadow* the user's arg, encoding the wrong value onto the wire.
/// [`extract_method`] rejects all of them with a spanned diagnostic.
const RESERVED_REQUEST_PARAMS: [&str; 6] = [
    "deadline",
    "correlator",
    "reply_to",
    "max_payload",
    "encoding",
    "payload",
];

fn extract_method(method: &TraitItemFn) -> Result<MethodSig> {
    if method.sig.asyncness.is_some() {
        return Err(Error::new_spanned(
            method.sig.asyncness,
            "service trait methods are synchronous; async-handler \
             support is not yet available",
        ));
    }
    if method.sig.unsafety.is_some() {
        return Err(Error::new_spanned(
            method.sig.unsafety,
            "service trait methods cannot be `unsafe fn`; \
             dispatch must call them from a safe context",
        ));
    }
    if method.sig.constness.is_some() {
        return Err(Error::new_spanned(
            method.sig.constness,
            "service trait methods cannot be const",
        ));
    }
    if !method.sig.generics.params.is_empty() {
        return Err(Error::new_spanned(
            &method.sig.generics,
            "service trait methods cannot be generic",
        ));
    }
    if method.default.is_some() {
        return Err(Error::new_spanned(
            method.sig.ident.clone(),
            "service trait methods may not have default bodies",
        ));
    }

    // Validate the method-name length against the wire frame's
    // MAX_METHOD_LEN (255 bytes). Catching this at compile time
    // turns an `EncodeError::MethodTooLong` runtime error into a
    // clear macro diagnostic.
    let name_str = method.sig.ident.unraw().to_string();
    if name_str.len() > 255 {
        return Err(Error::new_spanned(
            &method.sig.ident,
            format!(
                "method name length {} exceeds wire MAX_METHOD_LEN (255 bytes)",
                name_str.len()
            ),
        ));
    }

    let mut inputs = method.sig.inputs.iter();
    let receiver = inputs.next().ok_or_else(|| {
        Error::new_spanned(
            &method.sig,
            "service trait methods need `&self` or `&mut self` as the first argument",
        )
    })?;
    match receiver {
        // Accept either `&self` or `&mut self`. The dispatch core
        // owns `&mut St` at the call site; `&self` methods are
        // dispatched via deref-coercion so read-only operations
        // (like `balance() -> Cents`) don't need to falsely
        // advertise mutation.
        FnArg::Receiver(rec) if rec.reference.is_some() && rec.colon_token.is_none() => {}
        _ => {
            return Err(Error::new_spanned(
                receiver,
                "service trait methods need `&self` or `&mut self` as the first argument",
            ));
        }
    }

    let mut args = Vec::new();
    for input in inputs {
        let typed = match input {
            FnArg::Typed(t) => t,
            FnArg::Receiver(r) => {
                return Err(Error::new_spanned(
                    r,
                    "unexpected receiver after the first argument",
                ));
            }
        };
        let name = match &*typed.pat {
            Pat::Ident(p) => p.ident.clone(),
            other => {
                return Err(Error::new_spanned(
                    other,
                    "service trait method arguments must be plain `name: Type` patterns",
                ));
            }
        };
        // The generated `<method>_request` constructor appends these
        // reserved parameters after the user's args. A method arg with one
        // of these names collides (E0415 duplicate parameter) deep inside
        // generated code; reject it here with a clear, spanned diagnostic.
        let unraw_name = name.unraw();
        let unraw_name_str = unraw_name.to_string();
        if RESERVED_REQUEST_PARAMS.contains(&unraw_name_str.as_str()) {
            return Err(Error::new_spanned(
                &name,
                format!(
                    "argument name `{unraw_name_str}` is reserved by the generated \
                     `{}_request` constructor; rename this parameter",
                    method.sig.ident
                ),
            ));
        }
        args.push(MethodArg {
            name,
            ty: (*typed.ty).clone(),
        });
    }

    let return_ty: Type = match &method.sig.output {
        ReturnType::Default => syn::parse_quote!(()),
        ReturnType::Type(_, ty) => (**ty).clone(),
    };

    let name = method.sig.ident.clone();
    let name_str = name.unraw().to_string();
    let request_fn = format_ident!("{}_request", name);
    let decode_reply_fn = format_ident!("{}_decode_reply", name);

    Ok(MethodSig {
        name,
        name_str,
        request_fn,
        decode_reply_fn,
        args,
        return_ty,
    })
}

fn build_dispatch_impl(
    trait_ident: &Ident,
    service_struct: &Ident,
    service_name_lit: &str,
    encoding_ty: &Type,
    methods: &[MethodSig],
    tina_crate: &Path,
    rpc_crate: &Path,
) -> TokenStream2 {
    let method_inserts = methods
        .iter()
        .map(|m| emit_method_entry(trait_ident, m, rpc_crate));

    let doc_dispatch = format!(
        "Builds a [`tina_rpc::Dispatch`] from a user `H: {trait_ident}` impl. \
         The dispatcher routes wire calls through the typed trait \
         and a [`tina_rpc::MethodTable`] of one entry per trait \
         method. Hand the result to a topology adapter \
         (e.g. [`tina_rpc::SingleService`]) and register it with \
         the runtime.\n\
         \n\
         # What the user still controls\n\
         \n\
         - **Mailbox capacity**: set at runtime registration via \
           `runtime.register_with_capacity(adapter, n)`. Pressure \
           past this surfaces as wire `Error(Full)`.\n\
         - **Per-call payload size**: `limits` (a \
           [`tina_rpc::PayloadLimits`]) bounds inbound request \
           decode and outbound response encode. Oversize request \
           → wire `Error(Decode)`; oversize response → wire \
           `Error(Internal)`.\n\
         - **Service-call timeout**: lives on \
           [`tina_rpc::RegistryConfig::service_call_timeout`] and \
           bounds *one* registry → service hop. Pool/sharded \
           adapters add a second hop; see \
           [`tina_rpc::service`] module docs for the per-hop \
           budget rules.\n\
         \n\
         # Error mapping (default)\n\
         \n\
         The macro does not wire user-defined errors to wire \
         server-error codes; if the trait method returns \
         `Result<Ok, MyErr>`, the encoder serializes both arms as \
         the success-path payload and the client decodes the \
         `Result` directly. The wire `Error` codes \
         (`UnknownMethod`, `Decode`, `Internal`) are reserved for \
         transport-level conditions the dispatch core handles \
         automatically. A future macro form may accept \
         `#[tina_rpc::service(map_error = ...)]` for explicit \
         opt-in to wire-error mapping; the default today is \
         encode-in-payload."
    );
    let doc_service_name = format!("Wire service name (`\"{service_name_lit}\"`).");

    quote! {
        #[doc = #doc_dispatch]
        #[derive(::core::fmt::Debug)]
        pub struct #service_struct {
            _private: (),
        }

        impl #service_struct {
            #[doc = #doc_service_name]
            pub const SERVICE_NAME: &'static str = #service_name_lit;

            #[doc = #doc_dispatch]
            pub fn dispatch<H, Sh>(
                state: H,
                limits: #rpc_crate::PayloadLimits,
            ) -> #rpc_crate::Dispatch<H, #encoding_ty, Sh>
            where
                H: #trait_ident + 'static,
                Sh: #tina_crate::Shard + 'static,
                #encoding_ty: #rpc_crate::Encoding + ::core::default::Default + 'static,
            {
                let table: #rpc_crate::MethodTable<H, #encoding_ty> =
                    #rpc_crate::MethodTable::new()
                        #(#method_inserts)*;
                #rpc_crate::Dispatch::new(
                    state,
                    table,
                    <#encoding_ty as ::core::default::Default>::default(),
                    limits,
                )
            }
        }
    }
}

fn emit_method_entry(trait_ident: &Ident, m: &MethodSig, rpc_crate: &Path) -> TokenStream2 {
    let method_name_lit = &m.name_str;
    let method_name_ident = &m.name;

    let arg_types: Vec<&Type> = m.args.iter().map(|a| &a.ty).collect();
    let arg_indices: Vec<TokenStream2> = (0..m.args.len())
        .map(|i| {
            let lit = syn::Index::from(i);
            quote!(#lit)
        })
        .collect();

    let tuple_ty: TokenStream2 = if arg_types.is_empty() {
        quote!(())
    } else if arg_types.len() == 1 {
        let ty = arg_types[0];
        quote!((#ty,))
    } else {
        quote!((#(#arg_types),*))
    };

    let call: TokenStream2 = if arg_indices.is_empty() {
        quote!(<H as #trait_ident>::#method_name_ident(state))
    } else {
        quote!(<H as #trait_ident>::#method_name_ident(state #(, args.#arg_indices)*))
    };

    let return_ty = &m.return_ty;

    quote! {
        .method(#rpc_crate::Method::new(
            #method_name_lit,
            move |state: &mut H, args: #tuple_ty| -> #return_ty {
                #call
            },
        ))
    }
}

fn build_client_impl(
    client_struct: &Ident,
    service_name_lit: &str,
    encoding_ty: &Type,
    methods: &[MethodSig],
    tina_crate: &Path,
    rpc_crate: &Path,
) -> TokenStream2 {
    let per_method = methods
        .iter()
        .map(|m| build_client_method(encoding_ty, m, tina_crate, rpc_crate));

    let doc_struct = "Client-side companion: typed request builders \
                     and reply decoders for each trait method. The \
                     wire surface stays raw so the caller controls \
                     deadline, correlator, reply_to, and any retry \
                     policy.";
    let doc_service_name = format!("Wire service name (`\"{service_name_lit}\"`).");

    quote! {
        #[doc = #doc_struct]
        #[derive(::core::fmt::Debug)]
        pub struct #client_struct {
            _private: (),
        }

        impl #client_struct {
            #[doc = #doc_service_name]
            pub const SERVICE_NAME: &'static str = #service_name_lit;

            #(#per_method)*
        }
    }
}

fn build_client_method(
    encoding_ty: &Type,
    m: &MethodSig,
    tina_crate: &Path,
    rpc_crate: &Path,
) -> TokenStream2 {
    let method_name_lit = &m.name_str;
    let request_fn = &m.request_fn;
    let decode_fn = &m.decode_reply_fn;
    let return_ty = &m.return_ty;

    // Non-collidable local names: a trait arg called `encoding`/`payload` would
    // otherwise shadow these builder locals and encode the wrong value. Reserved
    // names reject such args, but keep the `__tina_` prefix as defense-in-depth.
    let enc_local = format_ident!("__tina_encoding");
    let payload_local = format_ident!("__tina_payload");

    let arg_decls: Vec<TokenStream2> = m
        .args
        .iter()
        .map(|a| {
            let n = &a.name;
            let t = &a.ty;
            quote!(#n: #t)
        })
        .collect();
    let arg_names: Vec<&Ident> = m.args.iter().map(|a| &a.name).collect();

    let tuple_expr: TokenStream2 = if arg_names.is_empty() {
        quote!(())
    } else if arg_names.len() == 1 {
        let n = arg_names[0];
        quote!((#n,))
    } else {
        quote!((#(#arg_names),*))
    };

    let request_doc = format!(
        "Builds a [`tina_rpc::ClientRequest`] for the `{method_name_lit}` method.\n\
         \n\
         The args are encoded as a positional tuple (zero-arg → \
         `null`, single-arg → `[a]`, multi-arg → `[a, b, ...]` in \
         JSON). **The wire shape is positional**: adding, removing, \
         or reordering args changes the payload layout and breaks \
         existing clients silently. Versioning helpers (struct-shaped \
         payloads, named field tags) are out of scope; the wire \
         carries no public compatibility promise.\n\
         \n\
         Submit the result to a [`tina_rpc::Client`] via \
         `ClientMsg::Request`. The reply arrives at `reply_to` as \
         a [`tina_rpc::ClientResultMsg`] tagged with `correlator`; \
         decode it with [`Self::{decode_fn}`]."
    );
    let decode_doc = format!(
        "Decodes the wire bytes of a `{method_name_lit}` reply into \
         the typed return type the trait declared.\n\
         \n\
         **This is only the second half of the outcome path.** Read \
         [`tina_rpc::ClientResultMsg::result`] first: only the \
         `ClientResult::Ok(bytes)` arm carries a wire reply you \
         can decode. `ServerError(FrameError)`, `Timeout`, \
         `ConnectionClosed`, `Idle`, `Full`, `LocalEncodeFailed`, \
         `IoError(_)` are all conditions the typed decoder cannot \
         see. The caller is responsible for routing\n\
         \n\
         - `request_id` from the request → `correlator` on the reply\n\
         - the matching method's `*_decode_reply` for the bytes\n\
         \n\
         A typo here (e.g., calling `charge_decode_reply` on a \
         `refund` reply's bytes) can silently corrupt logic if the \
         shapes happen to overlap."
    );

    quote! {
        #[doc = #request_doc]
        pub fn #request_fn(
            #(#arg_decls,)*
            deadline: ::core::time::Duration,
            correlator: u64,
            reply_to: #tina_crate::Address<#rpc_crate::ClientResultMsg>,
            max_payload: usize,
        ) -> ::core::result::Result<
            #rpc_crate::ClientRequest,
            #rpc_crate::EncodingError,
        >
        where
            #encoding_ty: #rpc_crate::Encoding + ::core::default::Default,
        {
            let #enc_local = <#encoding_ty as ::core::default::Default>::default();
            let #payload_local = <#encoding_ty as #rpc_crate::Encoding>::encode(
                &#enc_local,
                &#tuple_expr,
                max_payload,
            )?;
            ::core::result::Result::Ok(#rpc_crate::ClientRequest {
                service: ::std::string::String::from(Self::SERVICE_NAME),
                method: ::std::string::String::from(#method_name_lit),
                payload: #payload_local,
                deadline,
                correlator,
                reply_to,
            })
        }

        #[doc = #decode_doc]
        pub fn #decode_fn(
            bytes: &[u8],
            max_payload: usize,
        ) -> ::core::result::Result<#return_ty, #rpc_crate::EncodingError>
        where
            #encoding_ty: #rpc_crate::Encoding + ::core::default::Default,
        {
            let encoding = <#encoding_ty as ::core::default::Default>::default();
            <#encoding_ty as #rpc_crate::Encoding>::decode(&encoding, bytes, max_payload)
        }
    }
}
