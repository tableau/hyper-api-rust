// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Procedural macros for `hyperdb-api`.
//!
//! Currently exposes `#[derive(FromRow)]`, which generates an
//! [`hyperdb_api::FromRow`] impl for a struct by mapping each field
//! to a column with the matching name.
//!
//! Re-exported by `hyperdb-api` so callers don't need to add this
//! crate as a direct dependency. Use it as `use hyperdb_api::FromRow;`
//! and `#[derive(FromRow)]` on a struct.
//!
//! # Example
//!
//! ```ignore
//! use hyperdb_api::FromRow;
//!
//! #[derive(FromRow)]
//! struct User {
//!     id: i32,
//!     name: String,
//!     // Map to a different column name with `rename`:
//!     #[hyperdb(rename = "email_address")]
//!     email: Option<String>,
//! }
//! ```
//!
//! # Attributes
//!
//! - `#[hyperdb(rename = "...")]` on a field uses the given column
//!   name instead of the field name.
//! - `#[hyperdb(index = N)]` on a field uses positional access
//!   ([`RowAccessor::position`] / [`RowAccessor::position_opt`]) at
//!   column index `N` instead of name-based lookup. Mutually exclusive
//!   with `rename`.
//! - Field types of `Option<T>` use [`RowAccessor::get_opt`] /
//!   [`RowAccessor::position_opt`] (NULL → `None`); other field types
//!   use [`RowAccessor::get`] / [`RowAccessor::position`] (NULL →
//!   error).
//!
//! [`hyperdb_api::FromRow`]: https://docs.rs/hyperdb-api
//! [`RowAccessor::get_opt`]: https://docs.rs/hyperdb-api
//! [`RowAccessor::get`]: https://docs.rs/hyperdb-api
//! [`RowAccessor::position`]: https://docs.rs/hyperdb-api
//! [`RowAccessor::position_opt`]: https://docs.rs/hyperdb-api

mod table_derive;

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{
    parse_macro_input, spanned::Spanned, Data, DataStruct, DeriveInput, Field, Fields,
    GenericArgument, LitInt, LitStr, PathArguments, Type, TypePath,
};

/// How a field maps to a column. Either by name (the default or
/// `#[hyperdb(rename = "...")]`) or by ordinal position
/// (`#[hyperdb(index = N)]`).
enum FieldSource {
    Name(String),
    Index(usize),
}

/// Derives `hyperdb_api::Table` for a struct.
///
/// Generates `impl Table` with `NAME` and `CREATE_SQL` consts. When the
/// `compile-time` cargo feature is enabled and `#[hyperdb(register)]` is
/// present, also registers the table with the compile-time validator.
///
/// # Attributes (struct level)
///
/// - `#[hyperdb(table = "name")]` — override the SQL table name (default:
///   lower_snake_case of the struct ident).
/// - `#[hyperdb(register)]` — register for compile-time `query_as!` validation.
///
/// # Attributes (field level)
///
/// - `#[hyperdb(primary_key)]` — marks the column as NOT NULL (always true
///   for non-`Option` fields, but documents intent).
/// - `#[hyperdb(rename = "col")]` — use a different SQL column name.
#[proc_macro_derive(Table, attributes(hyperdb))]
pub fn table_derive(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match table_derive::expand(&input) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

/// Compile-time validated typed query macro.
///
/// Syntax: `query_as!(Type, "SQL")` or `query_as!(Type, "SQL", arg1, arg2, …)`
///
/// Returns a [`hyperdb_api::QueryAs<Type>`] builder. `Type` must implement
/// [`hyperdb_api::FromRow`] and must be registered via
/// `#[derive(Table)] #[hyperdb(register)]`.
///
/// With the `compile-time` cargo feature enabled, validates at build time that
/// the SQL is syntactically valid, all referenced tables are registered, and
/// all struct fields appear in the projected columns.
///
/// # Module ordering constraint (`compile-time` feature)
///
/// Registration happens at proc-macro expansion time in the proc-macro host
/// process. Rust expands macros in the order modules are declared in `mod`
/// statements (top-to-bottom in `lib.rs`/`main.rs`). If `derive(Table)` and
/// `query_as!` are in different modules, the module containing `derive(Table)`
/// structs **must be declared (via `mod`) before** the module containing
/// `query_as!` calls, otherwise a false `StructNotRegistered` compile error
/// is emitted.
///
/// Within a single file, struct-level derives always expand before
/// function-body macros, so ordering within a file is not a concern.
#[proc_macro]
pub fn query_as(input: TokenStream) -> TokenStream {
    match expand_query_as(&input.into()) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand_query_as(input: &TokenStream2) -> syn::Result<TokenStream2> {
    use syn::{parse::Parser, punctuated::Punctuated, Expr, Token};

    // Parse: Type, "sql_literal" [, expr, expr, ...]
    let parser = Punctuated::<Expr, Token![,]>::parse_terminated;
    let args = parser.parse2(input.clone())?;
    let mut iter = args.iter();

    let ty_expr = iter.next().ok_or_else(|| {
        syn::Error::new_spanned(
            input,
            "query_as! expects at least two arguments: query_as!(Type, \"SQL\")",
        )
    })?;

    // Re-parse the first token as a type (not an expression).
    let ty: Type = syn::parse2(quote!(#ty_expr))?;

    let sql_expr = iter.next().ok_or_else(|| {
        syn::Error::new_spanned(
            ty_expr,
            "query_as! expects a SQL string literal as the second argument",
        )
    })?;

    // Remaining args are the bind parameters.
    let rest: Vec<&Expr> = iter.collect();

    // Compile-time validation: runs inside the proc-macro host at expansion time.
    // The `compile-time` feature gates this — without it the macro is a
    // pure pass-through with zero overhead. The variables are extracted inside
    // the cfg block to avoid unused-variable warnings in the feature-off build.
    #[cfg(feature = "compile-time")]
    {
        let struct_name = last_type_ident(&ty).map(ToString::to_string);
        let sql_lit: Option<LitStr> = syn::parse2(quote!(#sql_expr)).ok();
        if let (Some(struct_name), Some(sql_lit)) = (struct_name, sql_lit) {
            let sql_str = sql_lit.value();
            if let Err(e) = hyperdb_compile_check::validate_query_as(&struct_name, &sql_str) {
                let msg = e.to_diagnostic();
                return Ok(quote! {
                    ::std::compile_error!(#msg)
                });
            }
        }
    }

    Ok(quote! {
        ::hyperdb_api::QueryAs::<#ty>::new(#sql_expr, &[#(&#rest),*])
    })
}

/// Extract the last path segment ident from a type path (e.g. `User` from `crate::User`).
/// Only needed when `compile-time` feature is enabled (used for registry lookup).
#[cfg(feature = "compile-time")]
fn last_type_ident(ty: &Type) -> Option<&syn::Ident> {
    let Type::Path(syn::TypePath { path, qself: None }) = ty else {
        return None;
    };
    path.segments.last().map(|s| &s.ident)
}

/// Validated single-column query macro.
///
/// Syntax: `query_scalar!(Type, "SQL")` or `query_scalar!(Type, "SQL", arg1, …)`
///
/// Returns a [`hyperdb_api::QueryScalar<Type>`] builder. `Type` must implement
/// [`hyperdb_api::RowValue`]. No `derive(Table)` is required — scalars project
/// a single column and don't map to a struct.
///
/// With the `compile-time` feature enabled, validates at build time that the
/// SQL returns exactly one column.
#[proc_macro]
pub fn query_scalar(input: TokenStream) -> TokenStream {
    match expand_query_scalar(&input.into()) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand_query_scalar(input: &TokenStream2) -> syn::Result<TokenStream2> {
    use syn::{parse::Parser, punctuated::Punctuated, Expr, Token};

    let parser = Punctuated::<Expr, Token![,]>::parse_terminated;
    let args = parser.parse2(input.clone())?;
    let mut iter = args.iter();

    let ty_expr = iter.next().ok_or_else(|| {
        syn::Error::new_spanned(
            input,
            "query_scalar! expects at least two arguments: query_scalar!(Type, \"SQL\")",
        )
    })?;

    let ty: Type = syn::parse2(quote!(#ty_expr))?;

    let sql_expr = iter.next().ok_or_else(|| {
        syn::Error::new_spanned(
            ty_expr,
            "query_scalar! expects a SQL string literal as the second argument",
        )
    })?;

    let rest: Vec<&Expr> = iter.collect();

    // Compile-time validation: verify the SQL returns exactly one column.
    #[cfg(feature = "compile-time")]
    {
        let sql_lit: Option<LitStr> = syn::parse2(quote!(#sql_expr)).ok();
        if let Some(sql_lit) = sql_lit {
            let sql_str = sql_lit.value();
            // Validate SQL structure (syntax + table existence) using a dummy
            // struct name that won't be in the registry — we only care about
            // one-column check, not struct-field matching.
            match hyperdb_compile_check::validate_scalar_sql(&sql_str) {
                Ok(()) => {}
                Err(e) => {
                    let msg = e.to_diagnostic();
                    return Ok(quote! { ::std::compile_error!(#msg) });
                }
            }
        }
    }

    Ok(quote! {
        ::hyperdb_api::QueryScalar::<#ty>::new(#sql_expr, &[#(&#rest),*])
    })
}

/// Derives `hyperdb_api::FromRow` for a struct.
///
/// See the crate-level documentation for the full feature list.
#[proc_macro_derive(FromRow, attributes(hyperdb))]
pub fn from_row_derive(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand(&input) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let name = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    let fields = match &input.data {
        Data::Struct(DataStruct {
            fields: Fields::Named(named),
            ..
        }) => &named.named,
        Data::Struct(_) => {
            return Err(syn::Error::new_spanned(
                &input.ident,
                "FromRow can only be derived on structs with named fields",
            ));
        }
        Data::Enum(_) => {
            return Err(syn::Error::new_spanned(
                &input.ident,
                "FromRow cannot be derived on enums",
            ));
        }
        Data::Union(_) => {
            return Err(syn::Error::new_spanned(
                &input.ident,
                "FromRow cannot be derived on unions",
            ));
        }
    };

    let assignments = fields
        .iter()
        .map(field_assignment)
        .collect::<syn::Result<Vec<_>>>()?;

    Ok(quote! {
        #[automatically_derived]
        impl #impl_generics ::hyperdb_api::FromRow for #name #ty_generics #where_clause {
            fn from_row(
                row: ::hyperdb_api::RowAccessor<'_>,
            ) -> ::hyperdb_api::Result<Self> {
                Ok(Self {
                    #(#assignments),*
                })
            }
        }
    })
}

/// Generates `field_name: row.get("col")?` (or `get_opt`/`position`/`position_opt`
/// for `Option<T>` fields and/or `#[hyperdb(index = N)]`).
fn field_assignment(field: &Field) -> syn::Result<TokenStream2> {
    let ident = field
        .ident
        .as_ref()
        .ok_or_else(|| syn::Error::new_spanned(field, "tuple-struct fields are not supported"))?;
    let source = field_source_for(field, ident)?;
    let is_opt = is_option_type(&field.ty);

    let getter = match (source, is_opt) {
        (FieldSource::Name(name), true) => {
            let lit = LitStr::new(&name, ident.span());
            quote!(row.get_opt(#lit)?)
        }
        (FieldSource::Name(name), false) => {
            let lit = LitStr::new(&name, ident.span());
            quote!(row.get(#lit)?)
        }
        (FieldSource::Index(idx), true) => quote!(row.position_opt(#idx)?),
        (FieldSource::Index(idx), false) => quote!(row.position(#idx)?),
    };

    Ok(quote! { #ident: #getter })
}

/// Reads `#[hyperdb(rename = "...")]` or `#[hyperdb(index = N)]` from a field's
/// attributes. Falls back to a name-based source using the field's identifier.
/// `rename` and `index` are mutually exclusive.
fn field_source_for(field: &Field, default: &syn::Ident) -> syn::Result<FieldSource> {
    let mut rename: Option<(String, proc_macro2::Span)> = None;
    let mut index: Option<(usize, proc_macro2::Span)> = None;

    for attr in &field.attrs {
        if !attr.path().is_ident("hyperdb") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename") {
                let s: LitStr = meta.value()?.parse()?;
                rename = Some((s.value(), meta.path.span()));
                Ok(())
            } else if meta.path.is_ident("index") {
                let n: LitInt = meta.value()?.parse()?;
                let parsed: usize = n.base10_parse()?;
                index = Some((parsed, meta.path.span()));
                Ok(())
            } else if meta.path.is_ident("primary_key") {
                // Table-derive attribute; silently ignored by FromRow.
                Ok(())
            } else {
                Err(meta.error(format!(
                    "unrecognized hyperdb attribute `{}`; supported attributes: rename, index",
                    meta.path
                        .get_ident()
                        .map_or_else(|| "?".to_string(), ToString::to_string)
                )))
            }
        })?;
    }

    match (rename, index) {
        (Some(_), Some((_, idx_span))) => Err(syn::Error::new(
            idx_span,
            "`#[hyperdb(rename = ...)]` and `#[hyperdb(index = N)]` are mutually exclusive",
        )),
        (Some((name, _)), None) => Ok(FieldSource::Name(name)),
        (None, Some((idx, _))) => Ok(FieldSource::Index(idx)),
        (None, None) => Ok(FieldSource::Name(default.to_string())),
    }
}

/// Detects `Option<T>` (any path ending in `Option<T>`).
fn is_option_type(ty: &Type) -> bool {
    let Type::Path(TypePath { path, qself: None }) = ty else {
        return false;
    };
    let Some(last) = path.segments.last() else {
        return false;
    };
    if last.ident != "Option" {
        return false;
    }
    matches!(
        last.arguments,
        PathArguments::AngleBracketed(ref args)
            if matches!(args.args.first(), Some(GenericArgument::Type(_)))
    )
}
