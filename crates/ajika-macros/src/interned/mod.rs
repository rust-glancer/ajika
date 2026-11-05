//! Turn immutable fields into an ID declaration. The ID is what callers copy and persist;
//! the fields live once in the interner. Preparation validates the declaration and
//! assigns attribute destinations; `emit` prints that already-decided shape.

mod emit;
mod options;

use proc_macro2::TokenStream;
use quote::format_ident;
use syn::{Attribute, Fields, Ident, ItemStruct, Type};

use self::options::Options;
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
    stored_fields: Ident,
    fields: Vec<Field>,
}

impl Declaration {
    fn prepare(options: Options, item: ItemStruct) -> syn::Result<Self> {
        if !item.generics.params.is_empty() || item.generics.where_clause.is_some() {
            return Err(syn::Error::new_spanned(
                &item.generics,
                "interned declarations require concrete, owned fields",
            ));
        }
        let Fields::Named(named) = &item.fields else {
            return Err(syn::Error::new_spanned(
                &item,
                "interned declarations require named fields",
            ));
        };
        let mut fields = Vec::new();
        for field in &named.named {
            let name = field.ident.clone().expect("named interned field");
            if ["new", "from_raw", "as_raw"]
                .iter()
                .any(|reserved| name == reserved)
            {
                return Err(syn::Error::new_spanned(
                    &name,
                    "field name conflicts with an interned ID method",
                ));
            }
            fields.push(Field {
                name,
                ty: field.ty.clone(),
                attributes: Attributes::prepare(&field.attrs)?,
                getter_attributes: field.attrs.clone(),
            });
        }
        Ok(Self {
            options,
            attributes: Attributes::prepare(&item.attrs)?,
            stored_fields: format_ident!("__Ajika_{}_Fields", item.ident),
            fields,
            item,
        })
    }
}

struct Field {
    name: Ident,
    ty: Type,
    attributes: Attributes,
    getter_attributes: Vec<Attribute>,
}
