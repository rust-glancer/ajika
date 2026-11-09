//! Parse options and validate query signatures. This module prepares Rust syntax, not emitted
//! items; the individual declaration modules own their emission templates.

use proc_macro2::{Span, TokenStream};
use syn::{
    Block, Expr, FnArg, Ident, Lifetime, LitStr, Pat, PatType, Path, ReturnType, Signature, Type,
    ext::IdentExt as _, parse::Parser as _, spanned::Spanned as _,
};

#[derive(Default)]
enum ReturnMode {
    #[default]
    Arc,
    Ref,
}

/// Shared option syntax, with the accepted names chosen by each macro. A query function takes
/// `validate = path`; an enclosing impl accepts only `crate = path`. All forms accept that runtime
/// path so generated code can address a renamed dependency.
pub(crate) struct Options {
    pub(crate) runtime: Path,
    pub(crate) name: Option<LitStr>,
    pub(crate) validate: Option<Path>,
    pub(crate) transient: bool,
    pub(crate) forward: bool,
    returns: ReturnMode,
}

impl Options {
    pub(crate) const FUNCTION: &[&str] = &["name", "transient", "validate", "forward", "returns"];

    pub(crate) fn parse(tokens: TokenStream, allowed: &[&str], runtime: Path) -> syn::Result<Self> {
        let mut options = Self {
            runtime,
            name: None,
            validate: None,
            transient: false,
            forward: false,
            returns: ReturnMode::default(),
        };
        let mut seen = Vec::new();
        syn::meta::parser(|meta| {
            let ident = meta
                .path
                .get_ident()
                .ok_or_else(|| meta.error("expected an option name"))?;
            let name = ident.to_string();
            if name != "crate" && !allowed.contains(&name.as_str()) {
                return Err(meta.error("unsupported query option"));
            }
            if seen.contains(&name) {
                return Err(meta.error("duplicate query option"));
            }
            match name.as_str() {
                "crate" => options.runtime = meta.value()?.parse()?,
                "name" => options.name = Some(meta.value()?.parse()?),
                "validate" => options.validate = Some(meta.value()?.parse()?),
                "transient" => options.transient = true,
                "forward" => options.forward = true,
                "returns" => {
                    let arguments;
                    syn::parenthesized!(arguments in meta.input);
                    let mode = arguments.call(Ident::parse_any)?;
                    if !arguments.is_empty() {
                        return Err(arguments.error("expected one return mode"));
                    }
                    options.returns = match mode.to_string().as_str() {
                        "arc" => ReturnMode::Arc,
                        "ref" => ReturnMode::Ref,
                        _ => return Err(syn::Error::new(mode.span(), "expected `arc` or `ref`")),
                    };
                }
                _ => unreachable!("options were checked above"),
            }
            seen.push(name);
            Ok(())
        })
        .parse2(tokens)?;
        Ok(options)
    }
}

/// Both declaration forms use the same key layout and calling conventions. Keep the original
/// patterns for execution (`mut key` is useful there), but use plain names in generated wrappers.
///
/// For `fn query(db: &Db, file: File, mut limit: usize) -> Result<Value>`, keep three views:
///
/// - `key_type`: `(File, usize)`, used by the trait implementation;
/// - `key`: `(file, limit)`, packed by the calling wrapper;
/// - `pattern`: `(file, mut limit)`, unpacked before the original body executes.
///
/// The context is never part of the persisted key. Its type is kept as syntax, including aliases;
/// generated trait projections let Rust resolve which host services it uses.
pub(crate) struct QuerySignature {
    pub(crate) context: PatType,
    pub(crate) context_name: Ident,
    context_type: Type,
    pub(crate) arguments: Vec<PatType>,
    pub(crate) key_type: Type,
    pub(crate) key: Expr,
    pub(crate) pattern: Pat,
    pub(crate) result: Type,
}

impl QuerySignature {
    /// Let Rust resolve the context type, including user-defined aliases.
    pub(crate) fn services_type(&self, runtime: &Path) -> Type {
        let context = &self.context_type;
        syn::parse_quote!(<fn(&#context) as #runtime::__private::DbType>::Services)
    }

    /// Rust resolves return-type aliases for us. Forwarding declarations already contain the
    /// shared pointer, while ordinary declarations name the value that Ajika will wrap.
    pub(crate) fn value_type(&self, options: &Options) -> Type {
        let runtime = &options.runtime;
        let result = &self.result;
        let forward = options.forward;
        syn::parse_quote!(<#result as #runtime::__private::ResultType<#forward>>::Value)
    }

    /// Give the generated entry point its caller-facing signature and body.
    pub(crate) fn wrap(
        &self,
        signature: &mut Signature,
        options: &Options,
        provider: &Ident,
    ) -> Block {
        let runtime = &options.runtime;
        let value = self.value_type(options);
        let db = &self.context_name;
        let key = &self.key;
        let mut context = self.context.clone();
        let body = match options.returns {
            ReturnMode::Arc => {
                signature.output = syn::parse_quote!(-> #runtime::Result<::std::sync::Arc<#value>>);
                syn::parse_quote!({ #db.query::<#provider>(#key) })
            }
            ReturnMode::Ref => {
                // Name the lifetime inside Db so a short borrow of the context can still
                // return a reference that lasts for the operation.
                let lifetime = Lifetime::new("'__ajika_db", Span::mixed_site());
                let services = self.services_type(runtime);
                signature.generics.params.push(syn::parse_quote!(#lifetime));
                context.ty = syn::parse_quote!(&#runtime::Db<#lifetime, #services>);
                signature.output = syn::parse_quote!(-> #runtime::Result<&#lifetime #value>);
                syn::parse_quote!({ #db.query_ref::<#provider>(#key) })
            }
        };
        signature.inputs = std::iter::once(context)
            .chain(self.arguments.iter().cloned())
            .map(FnArg::Typed)
            .collect();
        body
    }

    pub(crate) fn analyze(signature: &Signature) -> syn::Result<Self> {
        Self::check_signature(signature)?;
        let mut arguments = signature.inputs.iter();
        // Separate the database from the key. Rust resolves its type, including
        // aliases, through the generated trait projections.
        let Some(first) = arguments.next() else {
            return Err(syn::Error::new(
                signature.span(),
                "a query needs a context argument",
            ));
        };
        let (context, context_name) = Self::argument(first)?;
        let Type::Reference(reference) = context.ty.as_ref() else {
            return Err(syn::Error::new(
                context.ty.span(),
                "expected &Db as the first argument",
            ));
        };
        if reference.mutability.is_some() {
            return Err(syn::Error::new(
                reference.span(),
                "queries take an immutable &Db",
            ));
        }
        let context_type = *reference.elem.clone();

        // Named, owned arguments can be packed into a durable key without changing their meaning.
        // Save the original binding patterns too: the body may need `mut`, even though its caller
        // just passes the same value on to the runtime.
        let mut names = Vec::new();
        let mut patterns = Vec::new();
        let mut types = Vec::new();
        let mut keys = Vec::new();
        for argument in arguments {
            let (argument, name) = Self::argument(argument)?;
            if matches!(argument.ty.as_ref(), Type::Reference(_)) {
                return Err(syn::Error::new(
                    argument.ty.span(),
                    "query keys must be owned",
                ));
            }
            names.push(name);
            patterns.push(&argument.pat);
            types.push(&argument.ty);
            keys.push(argument.clone());
        }
        // Keep a single argument's encoding unchanged; zero or several arguments form a tuple.
        // The expression constructs that key at the call site, and the pattern unpacks it in execute.
        let (key_type, key, pattern) = match names.len() {
            1 => (
                syn::parse_quote!(#(#types)*),
                syn::parse_quote!(#(#names)*),
                syn::parse_quote!(#(#patterns)*),
            ),
            _ => (
                syn::parse_quote!((#(#types),*)),
                syn::parse_quote!((#(#names),*)),
                syn::parse_quote!((#(#patterns),*)),
            ),
        };
        Ok(Self {
            context: Self::plain_argument(context),
            context_name,
            context_type,
            arguments: keys.iter().map(Self::plain_argument).collect(),
            key_type,
            key,
            pattern,
            result: Self::result_type(signature)?.clone(),
        })
    }

    /// One declaration describes one concrete query family. Keep forms that would require a
    /// different calling contract or generic registration out of the generated implementation.
    pub(crate) fn check_signature(signature: &Signature) -> syn::Result<()> {
        if signature.asyncness.is_some()
            || signature.constness.is_some()
            || signature.unsafety.is_some()
            || signature.abi.is_some()
            || signature.variadic.is_some()
            || !signature.generics.params.is_empty()
            || signature.generics.where_clause.is_some()
        {
            return Err(syn::Error::new(
                signature.span(),
                "queries require concrete, safe, synchronous Rust functions",
            ));
        }
        Ok(())
    }

    fn argument(argument: &FnArg) -> syn::Result<(&PatType, Ident)> {
        let FnArg::Typed(argument) = argument else {
            return Err(syn::Error::new(
                argument.span(),
                "query functions cannot take a self receiver",
            ));
        };
        if let Pat::Ident(pattern) = argument.pat.as_ref()
            && pattern.by_ref.is_none()
            && pattern.subpat.is_none()
        {
            return Ok((argument, pattern.ident.clone()));
        }
        Err(syn::Error::new(
            argument.span(),
            "name each query argument; destructure it in the body",
        ))
    }

    fn plain_argument(argument: &PatType) -> PatType {
        let mut argument = argument.clone();
        if let Pat::Ident(pattern) = argument.pat.as_mut() {
            pattern.mutability = None;
        }
        argument
    }

    pub(crate) fn result_type(signature: &Signature) -> syn::Result<&Type> {
        match &signature.output {
            ReturnType::Type(_, ty) => Ok(ty),
            ReturnType::Default => Err(syn::Error::new(
                signature.span(),
                "write the query's Result type explicitly",
            )),
        }
    }
}
