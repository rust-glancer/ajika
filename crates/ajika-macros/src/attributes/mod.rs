//! Decide which generated item inherits each attribute. Leave placement of `#[...]` and
//! `#![...]` to syn's item printers rather than rebuilding that syntax ourselves.

use syn::{AttrStyle, Attribute, Meta, Path, Token, punctuated::Punctuated};

/// Attribute destinations overlap: an `allow` belongs on both wrapper and computation, a `cfg`
/// must cover every generated sibling, and an `expect` belongs only where its lint can occur.
/// Keeping these lists separate prevents generation from making attribute-scope decisions.
#[derive(Default)]
pub(crate) struct Attributes {
    pub(crate) gates: Vec<Attribute>,
    /// Helpers inherit lint levels, but must not duplicate an expectation they cannot fulfill.
    pub(crate) inherited_lints: Vec<Attribute>,
    pub(crate) wrapper: Vec<Attribute>,
    pub(crate) computation: Vec<Attribute>,
}

impl Attributes {
    /// Sibling providers inherit configuration and lint levels, without duplicating expectations
    /// that belong to the user's computation. Inner attributes become outer on these new items.
    pub(crate) fn helpers(&self) -> Vec<Attribute> {
        self.gates
            .iter()
            .chain(&self.inherited_lints)
            .cloned()
            .map(|mut attribute| {
                attribute.style = AttrStyle::Outer;
                attribute
            })
            .collect()
    }

    pub(crate) fn prepare(attributes: &[Attribute]) -> syn::Result<Self> {
        let mut result = Self::default();
        for attribute in attributes {
            // A disabled function must also remove its marker, impls and registration. Those
            // are separate items, so even a source-level inner cfg becomes an outer attribute.
            if let Some(mut gate) = Self::select(attribute, |path| path.is_ident("cfg"))? {
                gate.style = AttrStyle::Outer;
                result.gates.push(gate);
            }

            // An expectation belongs to the code that can fulfill it. Copying it to a trivial
            // calling wrapper would create a second, unfulfilled expectation.
            if let Some(wrapper) = Self::select(attribute, |path| !path.is_ident("expect"))? {
                result.wrapper.push(wrapper);
            }

            if let Some(mut inherited) = Self::select(attribute, |path| {
                ["allow", "warn", "deny", "forbid", "expect"]
                    .iter()
                    .any(|name| path.is_ident(name))
            })? {
                Self::allow_expected_lints(&mut inherited.meta)?;
                result.inherited_lints.push(inherited);
            }

            // Keep body attributes in the body, and carry function-level lint policy with the
            // computation. Attributes such as inline/deprecated still describe the public call.
            let computation = if matches!(attribute.style, AttrStyle::Inner(_)) {
                Some(attribute.clone())
            } else {
                Self::select(attribute, |path| {
                    ["allow", "warn", "deny", "forbid", "expect"]
                        .iter()
                        .any(|name| path.is_ident(name))
                })?
            };
            result.computation.extend(computation);
        }
        Ok(result)
    }

    fn select(attribute: &Attribute, keep: fn(&Path) -> bool) -> syn::Result<Option<Attribute>> {
        let Some(meta) = Self::select_meta(&attribute.meta, keep)? else {
            return Ok(None);
        };
        let mut selected = attribute.clone();
        selected.meta = meta;
        Ok(Some(selected))
    }

    /// A declaration may expect a lint on its own fields, while generated parameters repeat
    /// their names and types. Helpers inherit the suppression, not another obligation to emit
    /// that lint. The original attribute remains where the user wrote it.
    fn allow_expected_lints(meta: &mut Meta) -> syn::Result<()> {
        let Meta::List(list) = meta else {
            return Ok(());
        };
        if list.path.is_ident("expect") {
            list.path = syn::parse_quote!(allow);
        } else if list.path.is_ident("cfg_attr") {
            let mut nested =
                list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)?;
            for meta in nested.iter_mut().skip(1) {
                Self::allow_expected_lints(meta)?;
            }
            list.tokens = quote::quote!(#nested);
        }
        Ok(())
    }

    /// Filter the leaves of cfg_attr without losing their conditions. For example, selecting lint
    /// policy from `cfg_attr(feature = "x", inline, allow(unused_mut))` keeps the condition and
    /// `allow`, but drops `inline`. Nested cfg_attr follows the same rule; an empty one disappears.
    fn select_meta(meta: &Meta, keep: fn(&Path) -> bool) -> syn::Result<Option<Meta>> {
        if let Meta::List(list) = meta
            && list.path.is_ident("cfg_attr")
        {
            let nested = list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)?;
            let mut nested = nested.iter();
            let condition = nested.next();
            let selected = nested
                .map(|meta| Self::select_meta(meta, keep))
                .collect::<syn::Result<Vec<_>>>()?;
            let selected: Vec<_> = selected.into_iter().flatten().collect();
            return Ok((!selected.is_empty())
                .then(|| syn::parse_quote!(cfg_attr(#condition, #(#selected),*))));
        }
        Ok(keep(meta.path()).then(|| meta.clone()))
    }
}
