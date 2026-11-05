//! Declarations for Ajika queries, typed inputs, output handles, and interned IDs. Storage and dependency
//! tracking live in the runtime; these macros connect functions and field declarations to it.
//!
//! Each entry point parses Rust syntax, prepares a validated declaration, and emits its template.
//! `declaration` owns signatures/options and `attributes` owns attribute scope. `query`
//! prepares free or associated wrappers, providers and registration. `input` turns a keyed struct into a
//! handle whose getters and setters read and update independently tracked fields.
//! `interned` gives immutable fields a compact ID through the database's interner.
//! `output` turns payload fields into handles and a constructor that emits named query outputs.
//!
//! For example, a declaration `fn words(db: &Db, file: File) -> Result<Words>` becomes:
//!
//! - a function `words(db, file)` that calls `db.query::<words>(file)` and returns `Result<Arc<Words>>`;
//! - a marker type `words` whose Query implementation runs the original body and wraps its value;
//! - a registration entry so validation can replay a saved child without its caller running.
//!
//! The runtime cannot tell whether the trait implementation was written by hand or generated.
//! Changes to persistence, scheduling, or dependency validation therefore belong in Ajika itself.

mod attributes;
mod declaration;
mod input;
mod interned;
mod output;
mod query;

use proc_macro::TokenStream;

/// Declare a synchronous query whose body returns `Result<T>`.
/// The generated function returns `Result<Arc<T>>`, wrapping each successfully computed value.
/// Use `forward` when the body already returns `Result<Arc<T>>`, for example by forwarding
/// another query's result. Ajika then reuses that Arc without wrapping it again.
/// Handwritten `Query::execute` implementations return `Result<Arc<T>>`.
///
/// Remaining owned arguments form the key. `transient` releases the return value at operation
/// end while retaining dependency metadata; the default persists values and compares them with
/// PartialEq.
/// Optional `name`, `validate`, and `crate` options customize identity, validation,
/// and a renamed runtime path. `forward` can be combined with these options and `transient`.
///
/// On a concrete inherent impl, mark individual associated functions with `#[ajika::query]`.
/// They follow the same calling convention and cannot take a self receiver. `Self` in their
/// signatures and bodies refers to the owning type; ordinary impl items are unchanged.
/// Default query names include the owner type and function name. The enclosing impl accepts
/// only `crate = path`; function attributes accept the usual query options.
#[proc_macro_attribute]
pub fn query(args: TokenStream, input: TokenStream) -> TokenStream {
    query::expand(args.into(), input.into())
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Register a concrete handwritten `Input` or `Query` implementation for replay.
#[proc_macro_attribute]
pub fn register(args: TokenStream, input: TokenStream) -> TokenStream {
    query::register(args.into(), input.into())
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Declare keyed input fields with tracked getters, batch setters, and automatic registration.
/// `key = Type` is required. Fields may override `stamp`, `initialize`, `durability`, and `lifetime`
/// with `#[input(...)]`; ordinary fields use their encoded value as a fingerprint.
/// `#[input(arc)] field: T` shares its value through an Arc: setters and initializers accept
/// owned values or existing Arcs, and getters return `Result<Arc<T>>`. Stamp and policy callbacks
/// inspect `&T`. See the book's [input guide](https://rust-glancer.github.io/ajika/using/inputs.html).
///
/// Fields describe inputs rather than Rust struct storage. All generated getters, setters and
/// field selectors are public on the handle; the declaration's visibility controls access to
/// that handle. Field visibility does not restrict the generated API, so fields need no `pub`.
#[proc_macro_attribute]
pub fn input(args: TokenStream, input: TokenStream) -> TokenStream {
    input::expand(args.into(), input.into())
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Declare a set of output handles for separately readable query outputs.
///
/// A field `value: T` becomes a public `Output<T>` handle. `Type::new(db, values...)` emits the
/// fields through `Db::output`, accepting owned payloads or Arcs. `#[output(optional)]` instead
/// generates `Option<Output<T>>` and accepts `Option<Arc<T>>`; `None` emits no slot. A plain
/// `value: Option<T>` remains one required output whose payload can be absent.
///
/// Field names supply slot names; `#[output(name = "...")]` preserves a slot across a Rust rename.
/// Slots belong to the producing query, not the declared type, and may be emitted only once.
/// The generated type derives Debug, Clone, PartialEq, Eq and wincode serialization for its handles,
/// without requiring Debug, Clone or Eq on payloads. Payloads keep `Db::output`'s bounds.
///
/// The type's visibility is preserved; constructor and fields are public regardless of declared
/// field visibility. Additional derives apply to the generated type. `crate = path` supports a
/// renamed Ajika import. Declarations require concrete, named fields.
#[proc_macro_attribute]
pub fn output(args: TokenStream, input: TokenStream) -> TokenStream {
    output::expand(args.into(), input.into())
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Declare a compact, serializable ID for a set of immutable, owned fields.
///
/// `Type::new(&interner, fields...)` interns the complete set of fields; each field gets an
/// owned getter returning `Result<T>`. IDs implement Copy, equality, ordering, Hash, Debug and
/// wincode serialization. Additional derives apply to the ID, not its stored fields.
///
/// The optional `namespace = "..."` overrides the declaration's module-qualified name.
/// Each declaration has one numbering space per database; include context in the fields
/// when it contributes to identity.
/// `crate = path` supports a renamed Ajika import. Declarations cannot be generic.
///
/// As with input declarations, fields describe stored data, not Rust struct storage: generated
/// methods are public on the handle regardless of field visibility. Getters decode all fields
/// and return the requested one; they do not keep decoded data resident or track dependencies.
#[proc_macro_attribute]
pub fn interned(args: TokenStream, input: TokenStream) -> TokenStream {
    interned::expand(args.into(), input.into())
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}
