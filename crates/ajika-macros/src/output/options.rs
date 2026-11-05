//! Output syntax only chooses the runtime path, slot names, and whether a slot may be absent.
//! A Rust `Option<T>` payload is not optional emission; that requires `#[output(optional)]`.

use proc_macro2::TokenStream;
use syn::{Attribute, LitStr, Path, parse::Parser as _};

pub(super) struct Options {
    pub(super) runtime: Path,
}

impl Options {
    pub(super) fn parse(tokens: TokenStream) -> syn::Result<Self> {
        let mut runtime = None;
        syn::meta::parser(|meta| {
            if !meta.path.is_ident("crate") {
                return Err(meta.error("unsupported output option (expected crate = path)"));
            }
            if runtime.is_some() {
                return Err(meta.error("duplicate output option"));
            }
            runtime = Some(meta.value()?.parse()?);
            Ok(())
        })
        .parse2(tokens)?;
        Ok(Self {
            runtime: runtime.unwrap_or_else(|| syn::parse_quote!(::ajika)),
        })
    }
}

#[derive(Default)]
pub(super) struct FieldOptions {
    pub(super) name: Option<LitStr>,
    pub(super) optional: bool,
}

impl FieldOptions {
    pub(super) fn extend(&mut self, attribute: &Attribute) -> syn::Result<()> {
        attribute.parse_nested_meta(|meta| {
            if meta.path.is_ident("name") {
                if self.name.is_some() {
                    return Err(meta.error("duplicate output field name"));
                }
                self.name = Some(meta.value()?.parse()?);
            } else if meta.path.is_ident("optional") {
                if self.optional {
                    return Err(meta.error("duplicate optional output flag"));
                }
                self.optional = true;
            } else {
                return Err(meta.error("unsupported output field option"));
            }
            Ok(())
        })
    }
}
