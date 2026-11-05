//! Input policy syntax. A field inherits its declaration's policy unless it overrides it.
//! Fixed durability/lifetime names are enum values; other paths name functions of the value.

use proc_macro2::TokenStream;
use syn::{Ident, LitStr, Path, Type, parse::Parser as _};

#[derive(Clone)]
pub(super) struct Options {
    pub(super) runtime: Path,
    pub(super) key: Option<Type>,
    pub(super) name: Option<LitStr>,
    pub(super) stamp: Option<Path>,
    pub(super) initialize: Option<Path>,
    pub(super) durability: Option<Policy>,
    pub(super) lifetime: Option<Policy>,
    pub(super) arc: bool,
}

impl Options {
    pub(super) fn parse(tokens: TokenStream) -> syn::Result<Self> {
        let mut options = Self {
            runtime: syn::parse_quote!(::ajika),
            key: None,
            name: None,
            stamp: None,
            initialize: None,
            durability: None,
            lifetime: None,
            arc: false,
        };
        options.extend(tokens, true)?;
        Ok(options)
    }

    pub(super) fn extend(&mut self, tokens: TokenStream, declaration: bool) -> syn::Result<()> {
        let mut seen = Vec::new();
        syn::meta::parser(|meta| {
            let name = meta
                .path
                .get_ident()
                .ok_or_else(|| meta.error("expected an input option"))?
                .to_string();
            if seen.contains(&name) {
                return Err(meta.error("duplicate input option"));
            }
            match name.as_str() {
                "crate" if declaration => self.runtime = meta.value()?.parse()?,
                "key" if declaration => self.key = Some(meta.value()?.parse()?),
                "name" if declaration => self.name = Some(meta.value()?.parse()?),
                "arc" if !declaration => self.arc = true,
                "stamp" => self.stamp = Some(meta.value()?.parse()?),
                "initialize" => self.initialize = Some(meta.value()?.parse()?),
                "durability" => {
                    self.durability = Some(Policy::parse(
                        meta.value()?.parse()?,
                        &["Low", "Medium", "High"],
                    ));
                }
                "lifetime" => {
                    self.lifetime = Some(Policy::parse(
                        meta.value()?.parse()?,
                        &["Operation", "Session", "Persistent"],
                    ));
                }
                _ => return Err(meta.error("unsupported input option")),
            }
            seen.push(name);
            Ok(())
        })
        .parse2(tokens)
    }
}

#[derive(Clone)]
pub(super) enum Policy {
    Fixed(Ident),
    Dynamic(Path),
}

impl Policy {
    fn parse(path: Path, variants: &[&str]) -> Self {
        match path.get_ident() {
            Some(name) if variants.iter().any(|variant| name == variant) => {
                Self::Fixed(name.clone())
            }
            _ => Self::Dynamic(path),
        }
    }
}
