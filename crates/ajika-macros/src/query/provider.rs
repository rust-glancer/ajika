//! The runtime-facing half of a query: a marker, a Query implementation and replay registration.
//! Callers prepare the execution method and its concrete types before reaching this template.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{Attribute, Expr, Ident, ImplItemFn, Path, Type, Visibility};

use crate::declaration::{Options, QuerySignature};

pub(super) struct Provider {
    runtime: Path,
    marker: Ident,
    visibility: Visibility,
    attributes: Vec<Attribute>,
    services: Type,
    key: Type,
    value: Type,
    identity: Expr,
    storage: Expr,
    pub(super) validate: Option<Expr>,
    execute: ImplItemFn,
}

impl Provider {
    pub(super) fn prepare(
        options: Options,
        signature: &QuerySignature,
        marker: Ident,
        visibility: Visibility,
        attributes: Vec<Attribute>,
        default_identity: Expr,
        execute: ImplItemFn,
    ) -> Self {
        let runtime = &options.runtime;
        let context_type = &signature.context_type;
        // Let Rust resolve aliases and elided Db lifetimes, rather than interpreting the
        // application's type names in the macro.
        let services =
            syn::parse_quote!(<fn(&#context_type) as #runtime::__private::DbType>::Services);
        let value = signature.value_type(&options);
        let identity = options
            .name
            .map_or(default_identity, |name| syn::parse_quote!(#name));
        let storage = if options.transient {
            syn::parse_quote!(#runtime::Storage::transient())
        } else {
            syn::parse_quote!(#runtime::Storage::persistent())
        };
        Self {
            runtime: options.runtime,
            marker,
            visibility,
            attributes,
            services,
            key: signature.key_type.clone(),
            value,
            identity,
            storage,
            validate: options.validate.map(|path| syn::parse_quote!(#path)),
            execute,
        }
    }

    pub(super) fn emit(self) -> TokenStream {
        let Self {
            runtime,
            marker,
            visibility,
            attributes,
            services,
            key,
            value,
            identity,
            storage,
            validate,
            execute,
        } = self;
        let validate = validate.map(|function| {
            quote! {
                fn validate(value: &Self::Value) -> #runtime::Result<()> { #function(value) }
            }
        });
        quote! {
            #(#attributes)*
            #[doc(hidden)]
            #[allow(non_camel_case_types)]
            #visibility struct #marker { _private: () }

            #(#attributes)*
            impl #runtime::Query for #marker {
                type Services = #services;
                type Key = #key;
                type Value = #value;
                const NAME: &'static str = #identity;

                fn storage() -> #runtime::Storage<Self::Value> { #storage }
                #execute
                #validate
            }

            #(#attributes)*
            const _: () = {
                #runtime::__private::inventory::submit! {
                    #runtime::__private::Registration { install: #runtime::__private::install_query::<#marker> }
                }
            };
        }
    }
}
