//! Declare the fixed outputs of one query as a set of output handles.
//!
//! The source fields name payload types; consumers receive separately readable `Output<T>`
//! handles. Preparation resolves slot names and attribute placement. The constructor generated
//! by `emit` only calls `Db::output`: publication and invalidation stay with the query runtime.

mod emit;
mod options;

use std::collections::HashSet;

use proc_macro2::TokenStream;
use syn::{Attribute, Fields, Ident, ItemStruct, LitStr, Type, ext::IdentExt as _};

use self::options::{FieldOptions, Options};
use crate::attributes::Attributes;

pub(crate) fn expand(args: TokenStream, input: TokenStream) -> syn::Result<TokenStream> {
    let options = Options::parse(args)?;
    let item = syn::parse2(input)?;
    Ok(Declaration::prepare(options, item)?.emit())
}

struct Declaration {
    options: Options,
    item: ItemStruct,
    attributes: Attributes,
    fields: Vec<Field>,
}

impl Declaration {
    fn prepare(options: Options, item: ItemStruct) -> syn::Result<Self> {
        if !item.generics.params.is_empty() || item.generics.where_clause.is_some() {
            return Err(syn::Error::new_spanned(
                &item.generics,
                "output declarations require concrete fields",
            ));
        }
        let Fields::Named(named) = &item.fields else {
            return Err(syn::Error::new_spanned(
                &item,
                "output declarations require named fields",
            ));
        };

        let mut fields = Vec::new();
        let mut slots = HashSet::new();
        for field in &named.named {
            let name = field.ident.clone().expect("named output field");
            let mut options = FieldOptions::default();
            let mut attributes = Vec::new();
            for attribute in &field.attrs {
                if attribute.path().is_ident("output") {
                    options.extend(attribute)?;
                } else {
                    attributes.push(attribute.clone());
                }
            }
            let slot = options
                .name
                .unwrap_or_else(|| LitStr::new(&name.unraw().to_string(), name.span()));
            // Output declarations do not introduce another namespace: these names belong directly
            // to the producing query. Reject collisions even if the Rust field names differ.
            if !slots.insert(slot.value()) {
                return Err(syn::Error::new_spanned(
                    &slot,
                    format!("duplicate output slot `{}`", slot.value()),
                ));
            }
            fields.push(Field {
                name,
                value: field.ty.clone(),
                slot,
                optional: options.optional,
                attributes: Attributes::prepare(&attributes)?,
                field_attributes: attributes,
            });
        }
        Ok(Self {
            options,
            attributes: Attributes::prepare(&item.attrs)?,
            item,
            fields,
        })
    }
}

struct Field {
    name: Ident,
    value: Type,
    slot: LitStr,
    optional: bool,
    attributes: Attributes,
    field_attributes: Vec<Attribute>,
}

#[cfg(test)]
mod tests {
    #[test]
    fn field_names_and_overrides_share_one_slot_namespace() {
        let declarations = [
            quote::quote! {
                struct Outputs { value: u32, #[output(name = "value")] renamed: u32 }
            },
            quote::quote! {
                struct Outputs {
                    #[output(name = "value")] first: u32,
                    #[output(name = "value")] second: u32,
                }
            },
        ];
        for declaration in declarations {
            let error = super::expand(proc_macro2::TokenStream::new(), declaration)
                .expect_err("a slot may be emitted only once");
            assert_eq!(error.to_string(), "duplicate output slot `value`");
        }
    }
}
