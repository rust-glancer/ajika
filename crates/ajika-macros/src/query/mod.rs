//! Expand free queries and explicitly marked functions in concrete inherent impls.
//! Both forms prepare the same provider and replay registration; the runtime sees no difference.

mod associated;
mod function;
mod provider;

use proc_macro2::TokenStream;
use quote::quote;

use crate::{attributes::Attributes, declaration::Options};

pub(crate) fn expand(args: TokenStream, input: TokenStream) -> syn::Result<TokenStream> {
    match syn::parse2(input)? {
        syn::Item::Fn(function) => function::expand(args, function),
        syn::Item::Impl(implementation) => associated::expand(args, implementation),
        item => Err(syn::Error::new_spanned(
            item,
            "query requires a function or inherent impl",
        )),
    }
}

pub(crate) fn register(args: TokenStream, input: TokenStream) -> syn::Result<TokenStream> {
    let options = Options::parse(args, &[], syn::parse_quote!(::ajika))?;
    let implementation: syn::ItemImpl = syn::parse2(input)?;
    let gates = Attributes::prepare(&implementation.attrs)?.gates;
    let Some((_, trait_path, _)) = &implementation.trait_ else {
        return Err(syn::Error::new_spanned(
            &implementation,
            "register requires an Input or Query implementation",
        ));
    };
    if !implementation.generics.params.is_empty() || implementation.generics.where_clause.is_some()
    {
        return Err(syn::Error::new_spanned(
            &implementation.generics,
            "register requires a concrete implementation",
        ));
    }
    let installer = match trait_path
        .segments
        .last()
        .map(|segment| segment.ident.to_string())
        .as_deref()
    {
        Some("Input") => quote!(install_input),
        Some("Query") => quote!(install_query),
        _ => {
            return Err(syn::Error::new_spanned(
                trait_path,
                "register requires an Input or Query implementation",
            ));
        }
    };
    let runtime = options.runtime;
    let marker = &implementation.self_ty;
    Ok(quote! {
        #implementation
        #(#gates)*
        const _: () = {
            #runtime::__private::inventory::submit! {
                #runtime::__private::Registration { install: #runtime::__private::#installer::<#marker> }
            }
        };
    })
}
