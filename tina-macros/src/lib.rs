#![forbid(unsafe_code)]

//! Proc macros for Tina isolate authoring.
//!
//! These macros remove Rust trait-impl ceremony while keeping Tina's runtime
//! behavior explicit in the handler body.

use proc_macro::TokenStream;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::{
    Error, FnArg, ImplItem, ImplItemFn, ItemImpl, Pat, Path, Result, ReturnType, Token, Type,
    parse_macro_input,
};

struct IsolateArgs {
    message: Option<Type>,
    reply: Option<Type>,
    send: Option<Type>,
    spawn: Option<Type>,
    call: Option<Type>,
    shard: Option<Type>,
}

impl Parse for IsolateArgs {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut args = Self {
            message: None,
            reply: None,
            send: None,
            spawn: None,
            call: None,
            shard: None,
        };

        while !input.is_empty() {
            let key: Path = input.parse()?;
            input.parse::<Token![=]>()?;
            let value: Type = input.parse()?;

            let Some(ident) = key.get_ident().map(|ident| ident.to_string()) else {
                return Err(Error::new_spanned(
                    key,
                    "expected a simple isolate option name",
                ));
            };

            match ident.as_str() {
                "message" => set_once(&mut args.message, value, "message")?,
                "reply" => set_once(&mut args.reply, value, "reply")?,
                "send" => set_once(&mut args.send, value, "send")?,
                "spawn" => set_once(&mut args.spawn, value, "spawn")?,
                "call" => set_once(&mut args.call, value, "call")?,
                "shard" => set_once(&mut args.shard, value, "shard")?,
                _ => {
                    return Err(Error::new_spanned(
                        key,
                        "expected one of: message, reply, send, spawn, call, shard",
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
    // Phase 047 Rock 5: `shard = ...` is now optional. Single-shard
    // programs default to `tina::SingleShard`; multi-shard programs
    // continue to declare their own shard type explicitly. The default is
    // a real type (not a global mutable singleton), so registration still
    // requires the user to construct the shard at runtime startup.
    let shard = args
        .shard
        .unwrap_or_else(|| syn::parse_quote!(::tina::SingleShard));

    let reply = args.reply.unwrap_or_else(|| syn::parse_quote!(()));
    let send = args
        .send
        .unwrap_or_else(|| syn::parse_quote!(::tina::Outbound<::std::convert::Infallible>));
    let spawn = args
        .spawn
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

    let attrs = &handle.attrs;
    let body = &handle.block;
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
            type Call = #call;
            type Shard = #shard;

            #(#attrs)*
            fn handle(
                &mut self,
                #msg_name: Self::Message,
                #ctx_name: &mut ::tina::Context<'_, Self::Shard>,
            ) -> ::tina::Effect<Self> {
                #body
            }
        }
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
