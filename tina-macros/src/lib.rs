#![forbid(unsafe_code)]

//! Proc macros for Tina isolate authoring.
//!
//! These macros remove Rust trait-impl ceremony while keeping Tina's runtime
//! behavior explicit in the handler body.

use proc_macro::TokenStream;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::{
    Error, FnArg, Ident, ImplItem, ImplItemFn, ItemImpl, Pat, Path, Result, ReturnType, Token,
    Type, parse_macro_input,
};

struct IsolateArgs {
    message: Option<Type>,
    reply: Option<Type>,
    send: Option<Type>,
    spawn: Option<Type>,
    spawn_observed: Option<Type>,
    call: Option<Type>,
    shard: Option<Type>,
    send_only: Option<Ident>,
}

impl Parse for IsolateArgs {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut args = Self {
            message: None,
            reply: None,
            send: None,
            spawn: None,
            spawn_observed: None,
            call: None,
            shard: None,
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
                let value: Type = input.parse()?;

                match name.as_str() {
                    "message" => set_once(&mut args.message, value, "message")?,
                    "reply" => set_once(&mut args.reply, value, "reply")?,
                    "send" => set_once(&mut args.send, value, "send")?,
                    "spawn" => set_once(&mut args.spawn, value, "spawn")?,
                    "spawn_observed" => {
                        set_once(&mut args.spawn_observed, value, "spawn_observed")?
                    }
                    "call" => set_once(&mut args.call, value, "call")?,
                    "shard" => set_once(&mut args.shard, value, "shard")?,
                    _ => {
                        return Err(Error::new_spanned(
                            key,
                            "expected one of: message, reply, send, spawn, spawn_observed, call, shard, send_only",
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

#[proc_macro_attribute]
pub fn isolate(args: TokenStream, input: TokenStream) -> TokenStream {
    expand_isolate(args, input, CallDefault::Infallible)
}

#[proc_macro_attribute]
pub fn runtime_isolate(args: TokenStream, input: TokenStream) -> TokenStream {
    expand_isolate(args, input, CallDefault::RuntimeCall)
}

enum CallDefault {
    Infallible,
    RuntimeCall,
}

fn expand_isolate(args: TokenStream, input: TokenStream, call_default: CallDefault) -> TokenStream {
    let args = parse_macro_input!(args as IsolateArgs);
    let mut item = parse_macro_input!(input as ItemImpl);

    match build_isolate(&mut item, args, call_default) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.into_compile_error().into(),
    }
}

fn build_isolate(
    item: &mut ItemImpl,
    args: IsolateArgs,
    call_default: CallDefault,
) -> Result<proc_macro2::TokenStream> {
    if item.trait_.is_some() {
        return Err(Error::new_spanned(
            &item.self_ty,
            "`#[tina::isolate]` belongs on an inherent impl block, not a trait impl",
        ));
    }

    let Some(message) = args.message else {
        return Err(Error::new_spanned(
            &item.self_ty,
            "missing required isolate option `message = ...`",
        ));
    };
    // `shard = ...` is optional. Single-shard
    // programs default to `tina::SingleShard`; multi-shard programs
    // continue to declare their own shard type explicitly. The default is
    // a real type (not a global mutable singleton), so registration still
    // requires the user to construct the shard at runtime startup.
    let shard = args
        .shard
        .unwrap_or_else(|| syn::parse_quote!(::tina::SingleShard));

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
        if args.call.is_some() {
            return Err(Error::new_spanned(
                send_only,
                "`send_only` isolates must not declare a `call` channel",
            ));
        }
    }

    let reply = args.reply.unwrap_or_else(|| syn::parse_quote!(()));
    let send = args
        .send
        .unwrap_or_else(|| syn::parse_quote!(::tina::Outbound<::std::convert::Infallible>));
    let spawn = args
        .spawn
        .unwrap_or_else(|| syn::parse_quote!(::std::convert::Infallible));
    let spawn_observed = args
        .spawn_observed
        .unwrap_or_else(|| syn::parse_quote!(::std::convert::Infallible));
    let call = match args.call {
        Some(call) => call,
        None => match call_default {
            CallDefault::Infallible => syn::parse_quote!(::std::convert::Infallible),
            CallDefault::RuntimeCall => syn::parse_quote!(::tina_runtime::RuntimeCall<#message>),
        },
    };

    let isolate = item.self_ty.clone();
    let generics = item.generics.clone();
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

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

    let attrs = &handle.attrs;
    let body = &handle.block;
    let has_handle_call = handle_call.is_some();
    let handle_call_tokens = if let Some((attrs, msg_name, call_name, body)) = handle_call {
        quote! {
            #(#attrs)*
            fn handle_call(
                &mut self,
                #msg_name: Self::Message,
                #call_name: ::tina::CallContext<'_, Self>,
            ) -> ::tina::Effect<Self> {
                #body
            }
        }
    } else {
        quote! {}
    };
    let callable_marker_impl = if has_handle_call {
        quote! {
            impl #impl_generics ::tina::CallableIsolate for #isolate #ty_generics #where_clause {}
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

        impl #impl_generics ::tina::Isolate for #isolate #ty_generics #where_clause {
            type Message = #message;
            type Reply = #reply;
            type Send = #send;
            type Spawn = #spawn;
            type SpawnObserved = #spawn_observed;
            type Call = #call;
            type Shard = #shard;

            #(#attrs)*
            fn handle(
                &mut self,
                #msg_name: Self::Message,
                #ctx_name: &mut ::tina::Context<'_, Self::Shard, Self::Reply>,
            ) -> ::tina::Effect<Self> {
                #body
            }

            #handle_call_tokens
        }

        #callable_marker_impl
    })
}

fn validate_handle(handle: &ImplItemFn) -> Result<(syn::Ident, syn::Ident)> {
    if handle.sig.asyncness.is_some() {
        return Err(Error::new_spanned(
            handle.sig.asyncness,
            "Tina handlers are synchronous; return an Effect instead of `async fn`",
        ));
    }

    if handle.sig.constness.is_some() {
        return Err(Error::new_spanned(
            handle.sig.constness,
            "Tina handlers cannot be const",
        ));
    }

    let inputs = &handle.sig.inputs;
    if inputs.len() != 3 {
        return Err(Error::new_spanned(
            &handle.sig,
            "expected `fn handle(&mut self, msg, ctx) -> Effect<Self>`",
        ));
    }

    match inputs.first() {
        Some(FnArg::Receiver(receiver))
            if receiver.reference.is_some() && receiver.mutability.is_some() => {}
        _ => {
            return Err(Error::new_spanned(
                &handle.sig,
                "first handle argument must be `&mut self`",
            ));
        }
    }

    let msg_name = simple_argument_name(handle, 1, "msg")?;
    let ctx_name = simple_argument_name(handle, 2, "ctx")?;

    if handle.sig.generics.lt_token.is_some() {
        return Err(Error::new_spanned(
            &handle.sig.generics,
            "handle cannot have its own generic parameters",
        ));
    }

    if !matches!(handle.sig.output, ReturnType::Default) {
        return Ok((msg_name, ctx_name));
    }

    Err(Error::new_spanned(
        &handle.sig,
        "handle must return `tina::Effect<Self>`",
    ))
}

fn validate_handle_call(
    handle_call: &ImplItemFn,
) -> Result<(Vec<syn::Attribute>, syn::Ident, syn::Ident, Box<syn::Block>)> {
    if handle_call.sig.asyncness.is_some() {
        return Err(Error::new_spanned(
            handle_call.sig.asyncness,
            "Tina call handlers are synchronous; return an Effect instead of `async fn`",
        ));
    }

    if handle_call.sig.constness.is_some() {
        return Err(Error::new_spanned(
            handle_call.sig.constness,
            "Tina call handlers cannot be const",
        ));
    }

    let inputs = &handle_call.sig.inputs;
    if inputs.len() != 3 {
        return Err(Error::new_spanned(
            &handle_call.sig,
            "expected `fn handle_call(&mut self, msg, call) -> Effect<Self>`",
        ));
    }

    match inputs.first() {
        Some(FnArg::Receiver(receiver))
            if receiver.reference.is_some() && receiver.mutability.is_some() => {}
        _ => {
            return Err(Error::new_spanned(
                &handle_call.sig,
                "first handle_call argument must be `&mut self`",
            ));
        }
    }

    let msg_name = simple_argument_name(handle_call, 1, "msg")?;
    let call_name = simple_argument_name(handle_call, 2, "call")?;

    if handle_call.sig.generics.lt_token.is_some() {
        return Err(Error::new_spanned(
            &handle_call.sig.generics,
            "handle_call cannot have its own generic parameters",
        ));
    }

    if matches!(handle_call.sig.output, ReturnType::Default) {
        return Err(Error::new_spanned(
            &handle_call.sig,
            "handle_call must return `tina::Effect<Self>`",
        ));
    }

    Ok((
        handle_call.attrs.clone(),
        msg_name,
        call_name,
        Box::new(handle_call.block.clone()),
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
