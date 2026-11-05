//! Keep associated computations in their owning impl so `Self` and helper calls remain ordinary
//! Rust. A private provider calls that computation; the public function calls the provider.

use proc_macro2::{Span, TokenStream};
use quote::{ToTokens as _, format_ident, quote};
use syn::{
    Attribute, ExprPath, FnArg, ImplItem, ImplItemFn, ItemImpl, LitStr, Meta, Path, Type, TypePath,
    spanned::Spanned as _,
    visit_mut::{self, VisitMut as _},
};

use super::provider::Provider;
use crate::{
    attributes::Attributes,
    declaration::{Options, QuerySignature},
};

pub(super) fn expand(args: TokenStream, implementation: ItemImpl) -> syn::Result<TokenStream> {
    let options = Options::parse(args, &[], syn::parse_quote!(::ajika))?;
    Ok(Implementation::prepare(options.runtime, implementation)?.emit())
}

struct Implementation {
    implementation: ItemImpl,
    providers: Vec<Provider>,
    attributes: Vec<Attribute>,
}

impl Implementation {
    fn prepare(runtime: Path, mut implementation: ItemImpl) -> syn::Result<Self> {
        if implementation.trait_.is_some()
            || !implementation.generics.params.is_empty()
            || implementation.generics.where_clause.is_some()
        {
            return Err(syn::Error::new_spanned(
                &implementation,
                "query requires a concrete inherent impl without generic parameters or a where clause",
            ));
        }
        let Type::Path(owner) = implementation.self_ty.as_ref() else {
            return Err(syn::Error::new_spanned(
                &implementation.self_ty,
                "expected a concrete type path",
            ));
        };
        let attributes = Attributes::prepare(&implementation.attrs)?;
        let mut providers = Vec::new();
        let mut items = Vec::new();
        for item in implementation.items {
            let ImplItem::Fn(mut function) = item else {
                items.push(item);
                continue;
            };
            let Some(options) = Self::take_query_options(&mut function, &runtime)? else {
                items.push(ImplItem::Fn(function));
                continue;
            };
            let method = Method::prepare(options, owner, function)?;
            items.push(ImplItem::Fn(method.wrapper));
            items.push(ImplItem::Fn(method.computation));
            providers.push(method.provider);
        }
        implementation.items = items;
        Ok(Self {
            implementation,
            providers,
            attributes: attributes.helpers(),
        })
    }

    fn take_query_options(
        function: &mut ImplItemFn,
        runtime: &Path,
    ) -> syn::Result<Option<Options>> {
        let mut options = None;
        for attribute in std::mem::take(&mut function.attrs) {
            if !Self::is_query(attribute.path(), runtime) {
                function.attrs.push(attribute);
                continue;
            }
            if options.is_some() {
                return Err(syn::Error::new_spanned(
                    attribute,
                    "duplicate query attribute",
                ));
            }
            let tokens = match attribute.meta {
                Meta::Path(_) => TokenStream::new(),
                Meta::List(list) => list.tokens,
                Meta::NameValue(value) => {
                    return Err(syn::Error::new_spanned(
                        value,
                        "expected query options in parentheses",
                    ));
                }
            };
            options = Some(Options::parse(
                tokens,
                &["name", "transient", "validate", "forward"],
                runtime.clone(),
            )?);
        }
        Ok(options)
    }

    fn is_query(path: &Path, runtime: &Path) -> bool {
        let declared: Path = syn::parse_quote!(#runtime::query);
        path.is_ident("query")
            || [syn::parse_quote!(ajika::query), declared]
                .iter()
                .any(|candidate: &Path| {
                    path.segments
                        .iter()
                        .map(|segment| &segment.ident)
                        .eq(candidate.segments.iter().map(|segment| &segment.ident))
                })
    }

    fn emit(self) -> TokenStream {
        let Self {
            implementation,
            providers,
            attributes,
        } = self;
        let providers = providers.into_iter().map(Provider::emit);
        // An anonymous const gives each impl its own provider names without introducing a
        // module (which would change imports, `super`, visibility and default query names).
        // The original impl stays together, including its ordinary items and lint scope.
        quote! {
            #(#attributes)*
            const _: () = {
                #implementation
                #(#providers)*
            };
        }
    }
}

struct Method {
    wrapper: ImplItemFn,
    computation: ImplItemFn,
    provider: Provider,
}

impl Method {
    fn prepare(options: Options, owner: &TypePath, mut function: ImplItemFn) -> syn::Result<Self> {
        let signature = QuerySignature::analyze(&function.sig)?;
        let attributes = Attributes::prepare(&function.attrs)?;
        let name = &function.sig.ident;
        let marker = format_ident!("__AjikaQuery_{name}", span = Span::mixed_site());
        let helper = format_ident!("__ajika_execute_{name}", span = Span::mixed_site());

        // The computation retains its original signature, patterns and body in the owner's impl.
        // Only the provider's signature needs to replace `Self` with the concrete owner type.
        let mut concrete = function.sig.clone();
        let mut paths = OwnerPaths { owner };
        paths.visit_signature_mut(&mut concrete);
        let concrete = QuerySignature::analyze(&concrete)?;
        let context = &concrete.context;
        let runtime = &options.runtime;
        let value = signature.value_type(&options);
        let db = &signature.context_name;
        let key = &signature.key;
        let arguments = signature.arguments.iter().map(|argument| &argument.pat);
        let parameter = syn::Ident::new("__ajika_key", Span::mixed_site());
        let call = quote!(<#owner>::#helper(#db, #(#arguments),*));
        let result = if options.forward {
            call
        } else {
            quote!(#call.map(::std::sync::Arc::new))
        };
        let execute = syn::parse_quote! {
            fn execute(#context, #parameter: Self::Key) -> #runtime::Result<::std::sync::Arc<Self::Value>> {
                let #key = #parameter;
                #result
            }
        };
        let output = syn::parse_quote!(-> #runtime::Result<::std::sync::Arc<#value>>);
        let owner_name = LitStr::new(
            &owner.to_token_stream().to_string().replace(' ', ""),
            owner.span(),
        );
        let mut provider = Provider::prepare(
            options,
            &concrete,
            marker.clone(),
            syn::Visibility::Inherited,
            attributes.helpers(),
            syn::parse_quote!(concat!(module_path!(), "::", #owner_name, "::", stringify!(#name))),
            execute,
        );
        if let Some(validate) = &mut provider.validate {
            paths.visit_expr_mut(validate);
        }

        let mut computation = function.clone();
        computation.sig.ident = helper;
        computation.vis = syn::Visibility::Inherited;
        computation.attrs = attributes.gates;
        computation.attrs.extend(attributes.computation);
        computation.attrs.push(syn::parse_quote!(#[doc(hidden)]));

        function.attrs = attributes.wrapper;
        function.sig.output = output;
        function.sig.inputs = std::iter::once(signature.context)
            .chain(signature.arguments)
            .map(FnArg::Typed)
            .collect();
        function.block = syn::parse_quote!({ #db.query::<#marker>(#key) });
        Ok(Self {
            wrapper: function,
            computation,
            provider,
        })
    }
}

/// Qualify the owner's `Self` only where provider types live outside the inherent impl.
/// Bodies are deliberately untouched: macros, nested impls and helpers keep normal Rust scope.
struct OwnerPaths<'a> {
    owner: &'a TypePath,
}

impl OwnerPaths<'_> {
    fn suffix(path: &Path) -> Option<Path> {
        (path.segments.first()?.ident == "Self").then(|| Path {
            leading_colon: None,
            segments: path.segments.iter().skip(1).cloned().collect(),
        })
    }
}

impl visit_mut::VisitMut for OwnerPaths<'_> {
    fn visit_type_mut(&mut self, ty: &mut Type) {
        if let Type::Path(path) = ty
            && path.qself.is_none()
            && let Some(suffix) = Self::suffix(&path.path)
        {
            let owner = self.owner;
            *ty = if suffix.segments.is_empty() {
                Type::Path(owner.clone())
            } else {
                syn::parse_quote!(<#owner>::#suffix)
            };
        }
        visit_mut::visit_type_mut(self, ty);
    }

    fn visit_expr_path_mut(&mut self, expression: &mut ExprPath) {
        if expression.qself.is_none()
            && let Some(suffix) = Self::suffix(&expression.path)
        {
            let owner = self.owner;
            if suffix.segments.is_empty() {
                expression.qself = owner.qself.clone();
                expression.path = owner.path.clone();
                for segment in &mut expression.path.segments {
                    if let syn::PathArguments::AngleBracketed(arguments) = &mut segment.arguments {
                        arguments.colon2_token = Some(Default::default());
                    }
                }
            } else {
                let attrs = &expression.attrs;
                *expression = syn::parse_quote!(#(#attrs)* <#owner>::#suffix);
            }
        }
        visit_mut::visit_expr_path_mut(self, expression);
    }
}
