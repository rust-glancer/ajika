//! Templates for a prepared input declaration. Each field gets one runtime implementation,
//! one tracked getter, one batch setter, and a selector for host maintenance.

use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::{Ident, Path};

use super::{Declaration, Field, options::Policy};

impl Declaration {
    pub(super) fn emit(self) -> TokenStream {
        let runtime = &self.options.runtime;
        let name = &self.item.ident;
        let visibility = &self.item.vis;
        let attributes = &self.item.attrs;
        let key = &self.key;
        let helpers = self.attributes.helpers();
        let identity = self.options.name.as_ref().map_or_else(
            || quote!(concat!(module_path!(), "::", stringify!(#name))),
            |name| quote!(#name),
        );
        let methods = self.fields.iter().map(|field| field.methods(runtime));
        let implementations = self
            .fields
            .iter()
            .map(|field| field.implementation(runtime, key, visibility, &helpers, &identity));
        quote! {
            #(#attributes)*
            #visibility struct #name { key: #key }

            #(#helpers)*
            impl #name {
                /// Address an input by its stable application key; this does not acquire its value.
                pub fn at(key: #key) -> Self { Self { key } }
                #(#methods)*
            }

            #(#implementations)*
        }
    }
}

impl Field {
    fn methods(&self, runtime: &Path) -> TokenStream {
        let Self {
            name,
            marker,
            selector,
            setter,
            value,
            attributes,
            options,
            ..
        } = self;
        let gates = &attributes.gates;
        let documentation = &attributes.wrapper;
        let lint_policy = &attributes.inherited_lints;
        let services = Ident::new("__AjikaServices", Span::mixed_site());
        // Convert at the input boundary so callers can supply an owned payload or reuse an
        // existing Arc. Unannotated fields keep their declared setter type.
        let (parameter, supplied) = if options.arc {
            (
                quote!(impl ::core::convert::Into<#value>),
                quote!(::core::convert::Into::into(value)),
            )
        } else {
            (quote!(#value), quote!(value))
        };
        quote! {
            #(#gates)*
            #(#lint_policy)*
            pub const #selector: #runtime::Field<#marker> = #runtime::Field::new();

            #(#documentation)*
            pub fn #name<#services: #runtime::Services>(
                &self, db: &#runtime::Db<'_, #services>
            ) -> #runtime::Result<#value> {
                db.input::<#marker>(self.key.clone())
            }

            #(#gates)*
            #(#lint_policy)*
            pub fn #setter(&self, update: &mut #runtime::InputUpdate, value: #parameter) -> #runtime::Result<()> {
                update.set::<#marker>(Self::#selector, self.key.clone(), #supplied)
            }
        }
    }

    fn implementation(
        &self,
        runtime: &Path,
        key: &syn::Type,
        visibility: &syn::Visibility,
        declaration_attributes: &[syn::Attribute],
        identity: &TokenStream,
    ) -> TokenStream {
        let Self {
            name,
            marker,
            value,
            attributes,
            options,
            ..
        } = self;
        let helpers = attributes.helpers();
        // The runtime borrows the stored type. Direct calls let Rust coerce &Arc<T> to &T
        // for application callbacks written against the payload.
        let stamp = options.stamp.as_ref().map_or_else(
            || quote!(#runtime::Stamp::of_value(value)),
            |stamp| quote!(#stamp(value)),
        );
        let initialize = options.initialize.as_ref().map(|initialize| {
            // Lazy acquisition accepts the same owned-or-shared values as an explicit setter.
            let acquire = if options.arc {
                quote!(#initialize(key).map(::core::convert::Into::into))
            } else {
                quote!(#initialize(key))
            };
            quote! {
                fn initialize(key: &Self::Key) -> #runtime::Result<Self::Value> { #acquire }
            }
        });
        let durability = options
            .durability
            .as_ref()
            .map(|policy| policy.emit(runtime, "Durability", "durability"));
        let lifetime = options
            .lifetime
            .as_ref()
            .map(|policy| policy.emit(runtime, "Lifetime", "lifetime"));
        quote! {
            #(#declaration_attributes)* #(#helpers)*
            #[doc(hidden)] #[allow(non_camel_case_types)]
            #visibility struct #marker;

            #(#declaration_attributes)* #(#helpers)*
            impl #runtime::Input for #marker {
                type Key = #key;
                type Value = #value;
                const NAME: &'static str = concat!(#identity, "::", stringify!(#name));

                fn stamp(value: &Self::Value) -> #runtime::Result<#runtime::Stamp> { #stamp }
                #initialize
                #durability
                #lifetime
            }

            #(#declaration_attributes)* #(#helpers)*
            const _: () = {
                #runtime::__private::inventory::submit! {
                    #runtime::__private::Registration { install: #runtime::__private::install_input::<#marker> }
                }
            };
        }
    }
}

impl Policy {
    fn emit(&self, runtime: &Path, kind: &str, method: &str) -> TokenStream {
        let kind_ident = syn::Ident::new(kind, proc_macro2::Span::call_site());
        let method = syn::Ident::new(method, proc_macro2::Span::call_site());
        match self {
            Self::Fixed(variant) => {
                quote! { fn #method(_: &Self::Value) -> #runtime::#kind_ident { #runtime::#kind_ident::#variant } }
            }
            Self::Dynamic(function) => {
                quote! { fn #method(value: &Self::Value) -> #runtime::#kind_ident { #function(value) } }
            }
        }
    }
}
