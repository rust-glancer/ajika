//! Turn a keyed struct declaration into an input handle and independently tracked fields.
//! Parsing and policy interpretation happen here and in `options`; `emit` only prints the
//! prepared declaration. Generated code delegates storage and updates to the ordinary runtime.

mod emit;
mod options;

use proc_macro2::TokenStream;
use quote::format_ident;
use syn::{Fields, Ident, ItemStruct, Type};

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
    key: Type,
    attributes: Attributes,
    fields: Vec<Field>,
}

impl Declaration {
    fn prepare(options: Options, item: ItemStruct) -> syn::Result<Self> {
        if !item.generics.params.is_empty() || item.generics.where_clause.is_some() {
            return Err(syn::Error::new_spanned(
                &item.generics,
                "inputs require a concrete key and fields",
            ));
        }
        let key = options.key.clone().ok_or_else(|| {
            syn::Error::new_spanned(&item.ident, "specify key = Type (use () for a singleton)")
        })?;
        let Fields::Named(named) = &item.fields else {
            return Err(syn::Error::new_spanned(
                &item,
                "inputs require named fields",
            ));
        };
        let mut fields = Vec::new();
        for field in &named.named {
            let name = field.ident.clone().expect("named input field");
            let mut policies = options.clone();
            let mut attributes = Vec::new();
            for attribute in &field.attrs {
                if attribute.path().is_ident("input") {
                    policies.extend(attribute.meta.require_list()?.tokens.clone(), false)?;
                } else {
                    attributes.push(attribute.clone());
                }
            }
            let payload = &field.ty;
            let value = if policies.arc {
                syn::parse_quote!(::std::sync::Arc<#payload>)
            } else {
                payload.clone()
            };
            fields.push(Field {
                marker: format_ident!("__Ajika_{}_{}", item.ident, name),
                selector: format_ident!(
                    "{}",
                    name.to_string().trim_start_matches("r#").to_uppercase()
                ),
                setter: format_ident!("set_{}", name.to_string().trim_start_matches("r#")),
                name,
                value,
                attributes: Attributes::prepare(&attributes)?,
                options: policies,
            });
        }
        let attributes = Attributes::prepare(&item.attrs)?;
        Ok(Self {
            options,
            item,
            key,
            attributes,
            fields,
        })
    }
}

struct Field {
    name: Ident,
    marker: Ident,
    selector: Ident,
    setter: Ident,
    value: Type,
    attributes: Attributes,
    options: Options,
}
