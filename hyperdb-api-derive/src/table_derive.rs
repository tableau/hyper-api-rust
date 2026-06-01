// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `#[derive(Table)]` implementation.
//!
//! Generates a `hyperdb_api::Table` impl with `NAME` and `CREATE_SQL` consts.
//! When the `compile-time` cargo feature is enabled AND the struct carries
//! `#[hyperdb(register)]`, also calls into `hyperdb_compile_check::registry`
//! at macro expansion time to register the table for validation.

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{
    Data, DataStruct, DeriveInput, Field, Fields, GenericArgument, LitStr, PathArguments, Type,
    TypePath,
};

/// Top-level entry point called from `lib.rs`.
pub(crate) fn expand(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let struct_name = &input.ident;

    let fields = match &input.data {
        Data::Struct(DataStruct {
            fields: Fields::Named(named),
            ..
        }) => &named.named,
        Data::Struct(_) => {
            return Err(syn::Error::new_spanned(
                &input.ident,
                "Table can only be derived on structs with named fields",
            ));
        }
        Data::Enum(_) => {
            return Err(syn::Error::new_spanned(
                &input.ident,
                "Table cannot be derived on enums",
            ));
        }
        Data::Union(_) => {
            return Err(syn::Error::new_spanned(
                &input.ident,
                "Table cannot be derived on unions",
            ));
        }
    };

    // Parse struct-level attributes: `#[hyperdb(table = "...", register)]`.
    let struct_opts = parse_struct_opts(input)?;
    let table_name = struct_opts
        .table_name
        .unwrap_or_else(|| to_snake_case(&struct_name.to_string()));

    // Build the column definitions.
    let col_defs = fields
        .iter()
        .map(|f| column_def(f, &table_name))
        .collect::<syn::Result<Vec<_>>>()?;

    let create_sql = format!(
        "CREATE TABLE IF NOT EXISTS {} ({})",
        table_name,
        col_defs.join(", ")
    );

    let name_lit = LitStr::new(&table_name, struct_name.span());
    let create_sql_lit = LitStr::new(&create_sql, struct_name.span());

    // When `compile-time` feature is enabled and `#[hyperdb(register)]` is
    // present, register this table directly in the proc-macro host process at
    // expansion time. The registry lives in the proc-macro host, and `query_as!`
    // (also expanding in the same host process) finds the entry when it runs.
    // No code is emitted into the user's binary — registration is a side-effect
    // of macro expansion only.
    #[cfg(feature = "compile-time")]
    if struct_opts.register {
        // Named column field names: exclude index-based fields (column_name_for
        // returns "" for them) to avoid spurious MissingColumns{ missing: [""] }.
        let field_names: Vec<String> = fields
            .iter()
            .filter_map(|f| {
                let ident = f.ident.as_ref()?;
                let col = column_name_for(f, ident).ok()?;
                if col.is_empty() {
                    None
                } else {
                    Some(col)
                }
            })
            .collect();
        hyperdb_compile_check::registry::register(
            struct_name.to_string(),
            table_name.clone(),
            create_sql.clone(),
            field_names,
        );
    }

    Ok(quote! {
        #[automatically_derived]
        impl ::hyperdb_api::Table for #struct_name {
            const NAME: &'static str = #name_lit;
            const CREATE_SQL: &'static str = #create_sql_lit;
        }
    })
}

// ---------------------------------------------------------------------------
// Struct-level attribute parsing
// ---------------------------------------------------------------------------

struct StructOpts {
    table_name: Option<String>,
    /// Whether `#[hyperdb(register)]` was present.
    /// Only used when `compile-time` feature is enabled.
    #[allow(dead_code, reason = "only used when compile-time feature is enabled")]
    register: bool,
}

fn parse_struct_opts(input: &DeriveInput) -> syn::Result<StructOpts> {
    let mut table_name = None;
    let mut register = false;

    for attr in &input.attrs {
        if !attr.path().is_ident("hyperdb") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("table") {
                let s: LitStr = meta.value()?.parse()?;
                table_name = Some(s.value());
                Ok(())
            } else if meta.path.is_ident("register") {
                register = true;
                Ok(())
            } else if meta.path.is_ident("primary_key")
                || meta.path.is_ident("rename")
                || meta.path.is_ident("index")
            {
                // Field-level attrs; silently skip at struct level.
                Ok(())
            } else {
                Err(meta.error(format!(
                    "unrecognized hyperdb attribute `{}`; \
                     supported struct attributes: table, register",
                    meta.path
                        .get_ident()
                        .map_or_else(|| "?".to_string(), ToString::to_string)
                )))
            }
        })?;
    }

    Ok(StructOpts {
        table_name,
        register,
    })
}

// ---------------------------------------------------------------------------
// Field-level parsing
// ---------------------------------------------------------------------------

struct FieldOpts {
    /// Override column name (from `#[hyperdb(rename = "...")]`).
    rename: Option<String>,
    /// Positional access (from `#[hyperdb(index = N)]`). Named columns are
    /// excluded from the column-subset validation check.
    index: Option<usize>,
    /// Whether the field is the primary key.
    /// Parsed but not yet used — reserved for v2 (schema enforcement).
    #[allow(dead_code, reason = "reserved for v2 schema enforcement")]
    primary_key: bool,
}

fn parse_field_opts(field: &Field) -> syn::Result<FieldOpts> {
    let mut rename = None;
    let mut index = None;
    let mut primary_key = false;

    for attr in &field.attrs {
        if !attr.path().is_ident("hyperdb") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename") {
                let s: syn::LitStr = meta.value()?.parse()?;
                rename = Some(s.value());
                Ok(())
            } else if meta.path.is_ident("index") {
                let n: syn::LitInt = meta.value()?.parse()?;
                index = Some(n.base10_parse::<usize>()?);
                Ok(())
            } else if meta.path.is_ident("primary_key") {
                primary_key = true;
                Ok(())
            } else {
                Err(meta.error(format!(
                    "unrecognized hyperdb attribute `{}`; \
                     supported field attributes: rename, index, primary_key",
                    meta.path
                        .get_ident()
                        .map_or_else(|| "?".to_string(), ToString::to_string)
                )))
            }
        })?;
    }

    Ok(FieldOpts {
        rename,
        index,
        primary_key,
    })
}

/// Compute the SQL column name for a field (honoring `rename`, excluding
/// `index`-based fields by returning `None`). Used to build the field-name list
/// for the compile-time registry.
#[cfg(feature = "compile-time")]
fn column_name_for(field: &Field, default: &syn::Ident) -> syn::Result<String> {
    let opts = parse_field_opts(field)?;
    if opts.index.is_some() {
        // Positional fields have no stable name in the compile-time check.
        return Ok(String::new()); // caller filters on empty
    }
    Ok(opts.rename.unwrap_or_else(|| default.to_string()))
}

/// Build one SQL column definition string, e.g. `id BIGINT NOT NULL`.
fn column_def(field: &Field, _table_name: &str) -> syn::Result<String> {
    let ident = field
        .ident
        .as_ref()
        .ok_or_else(|| syn::Error::new_spanned(field, "tuple-struct fields are not supported"))?;

    let opts = parse_field_opts(field)?;
    let col_name = opts.rename.unwrap_or_else(|| ident.to_string());

    let (inner_ty, nullable) = unwrap_option(&field.ty);
    let sql_type = rust_type_to_sql(field, inner_ty)?;
    let nullability = if nullable || opts.index.is_some() {
        // Index-based fields omit NOT NULL (we can't infer it from position alone).
        ""
    } else {
        " NOT NULL"
    };

    Ok(format!("{col_name} {sql_type}{nullability}"))
}

/// Map a Rust type to a SQL type string.
fn rust_type_to_sql<'a>(field: &Field, ty: &'a Type) -> syn::Result<&'a str> {
    // We match on the last path segment's ident string. Only simple types are
    // supported; callers with newtypes or aliases should impl Table manually.
    let type_name = last_path_ident(ty).map(ToString::to_string);

    match type_name.as_deref() {
        Some("i16") => Ok("SMALLINT"),
        Some("i32") => Ok("INTEGER"),
        Some("i64") => Ok("BIGINT"),
        Some("f32") => Ok("REAL"),
        Some("f64") => Ok("DOUBLE PRECISION"),
        Some("bool") => Ok("BOOLEAN"),
        Some("String") => Ok("TEXT"),
        // Only Vec<u8> maps to BYTES. Vec<T> for any other T is unsupported —
        // silently mapping Vec<String>/Vec<i32>/etc. to BYTES would produce an
        // incorrect CREATE TABLE that fails at runtime. We inspect the generic
        // argument to distinguish Vec<u8> from other Vec<T>.
        Some("Vec") => {
            if is_vec_u8(ty) {
                Ok("BYTES")
            } else {
                Err(syn::Error::new_spanned(
                    field,
                    format!(
                        "unsupported field type `{}` for derive(Table): \
                         only Vec<u8> is supported (maps to BYTES); \
                         other Vec<T> types have no Hyper SQL equivalent. \
                         Use a manual `impl Table` for this field.",
                        quote::quote!(#ty)
                    ),
                ))
            }
        }
        Some("NaiveDate") => Ok("DATE"),
        Some("NaiveDateTime") => Ok("TIMESTAMP"),
        Some("NaiveTime") => Ok("TIME"),
        // chrono::DateTime<Utc> last segment is `DateTime`
        Some("DateTime") => Ok("TIMESTAMPTZ"),
        Some("Numeric") => Ok("NUMERIC"),
        _ => Err(syn::Error::new_spanned(
            field,
            format!(
                "unsupported field type `{}` for derive(Table); \
                 supported: i16, i32, i64, f32, f64, bool, String, Vec<u8>, \
                 NaiveDate, NaiveDateTime, NaiveTime, DateTime<Utc>, Numeric. \
                 Use a manual `impl Table` for custom types.",
                quote::quote!(#ty)
            ),
        )),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// If `ty` is `Option<T>`, return `(T, true)`. Otherwise return `(ty, false)`.
fn unwrap_option(ty: &Type) -> (&Type, bool) {
    let Type::Path(TypePath { path, qself: None }) = ty else {
        return (ty, false);
    };
    let Some(last) = path.segments.last() else {
        return (ty, false);
    };
    if last.ident != "Option" {
        return (ty, false);
    }
    let PathArguments::AngleBracketed(ref args) = last.arguments else {
        return (ty, false);
    };
    if let Some(GenericArgument::Type(inner)) = args.args.first() {
        (inner, true)
    } else {
        (ty, false)
    }
}

/// Returns `true` if `ty` is exactly `Vec<u8>` (the only `Vec<_>` that maps to BYTES).
fn is_vec_u8(ty: &Type) -> bool {
    let Type::Path(TypePath { path, qself: None }) = ty else {
        return false;
    };
    let Some(last) = path.segments.last() else {
        return false;
    };
    if last.ident != "Vec" {
        return false;
    }
    let PathArguments::AngleBracketed(ref args) = last.arguments else {
        return false;
    };
    matches!(
        args.args.first(),
        Some(GenericArgument::Type(Type::Path(TypePath { path, qself: None })))
            if path.is_ident("u8")
    )
}

/// Extract the last path segment ident from a type, if it's a simple `TypePath`.
fn last_path_ident(ty: &Type) -> Option<&syn::Ident> {
    let Type::Path(TypePath { path, qself: None }) = ty else {
        return None;
    };
    path.segments.last().map(|s| &s.ident)
}

/// Convert a PascalCase ident to lower_snake_case (e.g. `UserOrder` → `user_order`).
fn to_snake_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for (i, ch) in s.chars().enumerate() {
        if ch.is_uppercase() && i > 0 {
            out.push('_');
        }
        out.extend(ch.to_lowercase());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snake_case_conversion() {
        assert_eq!(to_snake_case("User"), "user");
        assert_eq!(to_snake_case("UserOrder"), "user_order");
        assert_eq!(to_snake_case("HTTPResponse"), "h_t_t_p_response");
        assert_eq!(to_snake_case("already_snake"), "already_snake");
    }
}
