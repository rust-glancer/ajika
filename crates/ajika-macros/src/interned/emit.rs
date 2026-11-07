//! Generate the compact ID, its constructor/getters, and the stored field schema.
//! Serialization and interner access stay in the runtime; there is no query registration
//! because identity reads do not execute providers or participate in invalidation.

use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::{Ident, Path};

use super::{Declaration, Field};

impl Declaration {
    pub(super) fn emit(self) -> TokenStream {
        let runtime = &self.options.runtime;
        let name = &self.item.ident;
        let visibility = &self.item.vis;
        let attributes = &self.item.attrs;
        let gates = &self.attributes.gates;
        let lint_policy = &self.attributes.inherited_lints;
        let stored_fields = &self.stored_fields;
        let namespace = self.options.namespace.as_ref().map_or_else(
            || quote!(concat!(module_path!(), "::", stringify!(#name))),
            |namespace| quote!(#namespace),
        );
        let interner = Ident::new("__ajika_interner", Span::mixed_site());
        let fields = self
            .fields
            .iter()
            .map(|field| {
                let Field {
                    name,
                    ty,
                    attributes,
                    ..
                } = field;
                let gates = &attributes.gates;
                let lint_policy = &attributes.inherited_lints;
                quote!(#(#gates)* #(#lint_policy)* #name: #ty)
            })
            .collect::<Vec<_>>();
        let arguments = self.fields.iter().map(|field| {
            let name = &field.name;
            let gates = &field.attributes.gates;
            quote!(#(#gates)* #name)
        });
        let getters = self
            .fields
            .iter()
            .map(|field| field.getter(runtime, &interner));

        quote! {
            #(#attributes)*
            #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash,
                #runtime::__private::wincode::SchemaRead,
                #runtime::__private::wincode::SchemaWrite)]
            #[wincode(crate = #runtime::__private::wincode)]
            #visibility struct #name(u32);

            #(#gates)*
            #(#lint_policy)*
            impl #name {
                /// Intern these immutable fields in the supplied interner.
                /// Equal encoded fields return the same ID without creating a query dependency.
                #[allow(clippy::too_many_arguments)]
                pub fn new(#interner: &#runtime::Interner, #(#fields),*) -> #runtime::Result<Self> {
                    <Self as #runtime::__private::Interned>::intern(
                        #interner, #stored_fields { #(#arguments),* },
                    )
                }

                /// Recover an ID from an external numeric representation. This does not allocate
                /// or validate it; the number must originate in the same interner.
                pub const fn from_raw(raw: u32) -> Self { Self(raw) }

                /// Extract the number for external formats such as token spans. Keep its interner
                /// alongside it; the number alone does not identify an interned object.
                pub const fn as_raw(self) -> u32 { self.0 }

                #(#getters)*
            }

            #(#gates)*
            #(#lint_policy)*
            #[doc(hidden)]
            #[allow(non_camel_case_types)]
            #[derive(#runtime::__private::wincode::SchemaRead, #runtime::__private::wincode::SchemaWrite)]
            #[wincode(crate = #runtime::__private::wincode)]
            #visibility struct #stored_fields { #(#fields),* }

            #(#gates)*
            #(#lint_policy)*
            impl #runtime::__private::Interned for #name {
                type Fields = #stored_fields;
                const NAMESPACE: &'static str = #namespace;

                fn from_raw(raw: u32) -> Self { Self::from_raw(raw) }

                fn as_raw(self) -> u32 { self.as_raw() }
            }
        }
    }
}

impl Field {
    fn getter(&self, runtime: &Path, interner: &Ident) -> TokenStream {
        let Self {
            name,
            ty,
            getter_attributes,
            ..
        } = self;
        quote! {
            #(#getter_attributes)*
            /// Decode this field as an owned value. The supplied interner must be the one that
            /// issued this ID. An unknown ID or malformed key is an error.
            pub fn #name(self, #interner: &#runtime::Interner) -> #runtime::Result<#ty> {
                Ok(<Self as #runtime::__private::Interned>::fields(self, #interner)?.#name)
            }
        }
    }
}
