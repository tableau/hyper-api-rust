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
//! - Field types of `Option<T>` use [`RowAccessor::get_opt`]
//!   (NULL → `None`); other field types use [`RowAccessor::get`]
//!   (NULL → error).
//!
//! [`hyperdb_api::FromRow`]: https://docs.rs/hyperdb-api
//! [`RowAccessor::get_opt`]: https://docs.rs/hyperdb-api
//! [`RowAccessor::get`]: https://docs.rs/hyperdb-api

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{
    parse_macro_input, Data, DataStruct, DeriveInput, Field, Fields, GenericArgument, LitStr,
    PathArguments, Type, TypePath,
};

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

/// Generates `field_name: row.get("col")?` (or `get_opt` for `Option<T>` fields).
fn field_assignment(field: &Field) -> syn::Result<TokenStream2> {
    let ident = field
        .ident
        .as_ref()
        .ok_or_else(|| syn::Error::new_spanned(field, "tuple-struct fields are not supported"))?;
    let column_name = column_name_for(field, ident)?;
    let column_lit = LitStr::new(&column_name, ident.span());

    let getter = if is_option_type(&field.ty) {
        quote!(row.get_opt(#column_lit)?)
    } else {
        quote!(row.get(#column_lit)?)
    };

    Ok(quote! { #ident: #getter })
}

/// Reads `#[hyperdb(rename = "...")]` from a field's attributes; falls back
/// to the field's identifier as the column name.
fn column_name_for(field: &Field, default: &syn::Ident) -> syn::Result<String> {
    for attr in &field.attrs {
        if !attr.path().is_ident("hyperdb") {
            continue;
        }
        let mut rename: Option<String> = None;
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename") {
                let s: LitStr = meta.value()?.parse()?;
                rename = Some(s.value());
                Ok(())
            } else {
                Err(meta.error(format!(
                    "unrecognized hyperdb attribute `{}`; expected `rename = \"...\"`",
                    meta.path
                        .get_ident()
                        .map_or_else(|| "?".to_string(), ToString::to_string)
                )))
            }
        })?;
        if let Some(name) = rename {
            return Ok(name);
        }
    }
    Ok(default.to_string())
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
