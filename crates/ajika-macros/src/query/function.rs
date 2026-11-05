//! A free query keeps its public function and moves its computation into Query::execute.

use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::{FnArg, Ident, ImplItemFn, ItemFn};

use super::provider::Provider;
use crate::{
    attributes::Attributes,
    declaration::{Options, QuerySignature},
};

pub(super) fn expand(args: TokenStream, function: ItemFn) -> syn::Result<TokenStream> {
    let options = Options::parse(
        args,
        &["name", "transient", "validate", "forward"],
        syn::parse_quote!(::ajika),
    )?;
    Ok(Declaration::prepare(options, function)?.emit())
}

struct Declaration {
    wrapper: ItemFn,
    provider: Provider,
}

impl Declaration {
    fn prepare(options: Options, mut function: ItemFn) -> syn::Result<Self> {
        let signature = QuerySignature::analyze(&function.sig)?;
        let attributes = Attributes::prepare(&function.attrs)?;
        let provider_attributes = attributes.helpers();
        let name = &function.sig.ident;
        let parameter = Ident::new("__ajika_key", Span::mixed_site());
        let context = function.sig.inputs.first();
        let pattern = &signature.pattern;
        let result = &signature.result;
        let runtime = &options.runtime;
        let value = signature.value_type(&options);
        let mut execute: ImplItemFn = syn::parse_quote! {
            fn execute(#context, #parameter: Self::Key) -> #runtime::Result<::std::sync::Arc<Self::Value>> {}
        };
        execute.attrs = attributes.computation;
        execute.block = if options.forward {
            *function.block
        } else {
            // Keep `return` and `?` inside the declared Result<T> body. Wrapping its completed
            // result handles every successful exit without rewriting the user's expressions.
            let body = &function.block;
            syn::parse_quote!({ (|| -> #result #body)().map(::std::sync::Arc::new) })
        };
        execute
            .block
            .stmts
            .insert(0, syn::parse_quote!(let #pattern = #parameter;));
        function.sig.output = syn::parse_quote!(-> #runtime::Result<::std::sync::Arc<#value>>);

        let provider = Provider::prepare(
            options,
            &signature,
            name.clone(),
            function.vis.clone(),
            provider_attributes,
            syn::parse_quote!(concat!(module_path!(), "::", stringify!(#name))),
            execute,
        );
        let db = &signature.context_name;
        let key = &signature.key;
        function.attrs = attributes.wrapper;
        function.sig.inputs = std::iter::once(signature.context.clone())
            .chain(signature.arguments.iter().cloned())
            .map(FnArg::Typed)
            .collect();
        function.block = Box::new(syn::parse_quote!({ #db.query::<#name>(#key) }));

        Ok(Self {
            wrapper: function,
            provider,
        })
    }

    fn emit(self) -> TokenStream {
        let wrapper = self.wrapper;
        let provider = self.provider.emit();
        quote! { #wrapper #provider }
    }
}
