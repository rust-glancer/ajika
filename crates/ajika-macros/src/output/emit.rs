//! Generate the output handles and their emitting constructor.
//!
//! An optional field changes both the handle and constructor argument to `Option`. Its `None`
//! branch does not call the runtime. Required fields accept either owned payloads or existing
//! Arcs, so a producer need not wrap each result by hand.

use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::Ident;

use super::Declaration;

impl Declaration {
    pub(super) fn emit(self) -> TokenStream {
        let runtime = &self.options.runtime;
        let name = &self.item.ident;
        let visibility = &self.item.vis;
        let attributes = &self.item.attrs;
        let gates = &self.attributes.gates;
        let lint_policy = &self.attributes.inherited_lints;
        let context = Ident::new("__ajika_context", Span::mixed_site());
        let services = Ident::new("__AjikaServices", Span::mixed_site());

        let mut fields = Vec::new();
        let mut parameters = Vec::new();
        let mut initializers = Vec::new();
        for field in &self.fields {
            let name = &field.name;
            let value = &field.value;
            let slot = &field.slot;
            let attributes = &field.field_attributes;
            let gates = &field.attributes.gates;
            let lint_policy = &field.attributes.inherited_lints;

            let (handle, parameter, emit) = if field.optional {
                (
                    quote!(::core::option::Option<#runtime::Output<#value>>),
                    quote!(::core::option::Option<::std::sync::Arc<#value>>),
                    quote!(#name.map(|value| #context.output(#slot, value)).transpose()?),
                )
            } else {
                (
                    quote!(#runtime::Output<#value>),
                    quote!(impl ::core::convert::Into<::std::sync::Arc<#value>>),
                    quote!(#context.output(#slot, ::core::convert::Into::into(#name))?),
                )
            };
            fields.push(quote!(#(#attributes)* pub #name: #handle));
            parameters.push(quote!(#(#gates)* #(#lint_policy)* #name: #parameter));
            initializers.push(quote!(#(#gates)* #name: #emit));
        }

        quote! {
            #(#attributes)*
            #[derive(Debug, Clone, PartialEq, Eq,
                #runtime::__private::wincode::SchemaRead,
                #runtime::__private::wincode::SchemaWrite)]
            #[wincode(crate = #runtime::__private::wincode)]
            #visibility struct #name { #(#fields),* }

            #(#gates)*
            #(#lint_policy)*
            impl #name {
                /// Emit these named outputs from the executing query.
                /// Required values accept owned payloads or Arcs. An optional `None` emits no slot.
                /// The query publishes all outputs together when its execution succeeds.
                #[allow(clippy::too_many_arguments)]
                pub fn new<#services: #runtime::Services>(
                    #context: &#runtime::Db<'_, #services>,
                    #(#parameters),*
                ) -> #runtime::Result<Self> {
                    Ok(Self { #(#initializers),* })
                }
            }
        }
    }
}
