//! Interned declarations need only a storage namespace and an optional renamed Ajika path.
//! Field types and the declaration's name supply the rest of the contract.

use proc_macro2::TokenStream;
use syn::{LitStr, Path, parse::Parser as _};

pub(super) struct Options {
    pub(super) runtime: Path,
    pub(super) namespace: Option<LitStr>,
}

impl Options {
    pub(super) fn parse(tokens: TokenStream) -> syn::Result<Self> {
        let mut options = Self {
            runtime: syn::parse_quote!(::ajika),
            namespace: None,
        };
        let mut seen = Vec::new();
        syn::meta::parser(|meta| {
            let name = meta
                .path
                .get_ident()
                .ok_or_else(|| meta.error("expected an interned option"))?
                .to_string();
            if seen.contains(&name) {
                return Err(meta.error("duplicate interned option"));
            }
            match name.as_str() {
                "crate" => options.runtime = meta.value()?.parse()?,
                "namespace" => options.namespace = Some(meta.value()?.parse()?),
                _ => return Err(meta.error("unsupported interned option")),
            }
            seen.push(name);
            Ok(())
        })
        .parse2(tokens)?;
        Ok(options)
    }
}
