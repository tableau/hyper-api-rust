// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Example: the five forms of row mapping
//!
//! Runnable companion to `docs/ROW_MAPPING.md`. Loads the same `products`
//! table the doc uses, then maps its rows five different ways — from fully
//! manual to fully automatic, plus the streaming variant:
//!
//! - **Form 1** — manual streaming: `execute_query` + `next_chunk`, positional
//!   `row.get(N)` returning `Option<T>`.
//! - **Form 2** — named access: `fetch_all` + `Row::get_by_name`.
//! - **Form 3** — hand-written `FromRow` impl + `fetch_all_as`.
//! - **Form 4** — `#[derive(FromRow)]` + `fetch_all_as`.
//! - **Form 5** — streaming `FromRow`: `stream_as`, constant memory.
//!
//! Every form prints the same four products, so you can see they are
//! equivalent. Form 5 is shown on both the sync `Connection` and the async
//! `AsyncConnection` (which returns an `impl Stream` instead of an iterator).
//! Run with:
//!
//!   cargo run -p hyperdb-api --example row_mapping_forms
//!
//! `#[derive(FromRow)]` lives in the `hyperdb-api-derive` crate (it is not
//! re-exported from `hyperdb-api`), so Forms 4 and 5 import it directly.

use futures::StreamExt;
use hyperdb_api::{
    AsyncConnection, Connection, CreateMode, FromRow, HyperProcess, Parameters, Result, RowAccessor,
};
use hyperdb_api_derive::FromRow;

const QUERY: &str = "SELECT id, name, price, in_stock FROM products ORDER BY id";

fn main() -> Result<()> {
    std::fs::create_dir_all("test_results")?;

    let mut params = Parameters::new();
    params.set("log_dir", "test_results");
    let hyper = HyperProcess::new(None, Some(&params))?;

    let conn = Connection::new(
        &hyper,
        "test_results/row_mapping_forms.hyper",
        CreateMode::CreateAndReplace,
    )?;
    seed_products(&conn)?;

    form1_manual_streaming(&conn)?;
    form2_named_access(&conn)?;
    form3_manual_from_row(&conn)?;
    form4_derive_from_row(&conn)?;
    form5_streaming_from_row(&conn)?;

    // Form 5 also has an async flavor. Drop the sync connection first so the
    // async one reopens the same database file cleanly, then drive the stream
    // on a small Tokio runtime built just for this section.
    drop(conn);
    let endpoint = hyper.require_endpoint()?.to_string();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| hyperdb_api::Error::config(format!("failed to build Tokio runtime: {e}")))?
        .block_on(form5_streaming_from_row_async(&endpoint))?;

    Ok(())
}

/// Creates the `products` table from `docs/ROW_MAPPING.md` and inserts four
/// rows that every form below reads back.
fn seed_products(conn: &Connection) -> Result<()> {
    conn.execute_command(
        "CREATE TABLE products (
            id       INT              NOT NULL,
            name     TEXT             NOT NULL,
            price    DOUBLE PRECISION NOT NULL,
            in_stock BOOLEAN          NOT NULL
        )",
    )?;
    conn.execute_command(
        "INSERT INTO products VALUES
            (1, 'Widget',  9.99,  true),
            (2, 'Gadget',  19.95, false),
            (3, 'Gizmo',   4.50,  true),
            (4, 'Doohickey', 14.00, true)",
    )?;
    Ok(())
}

/// Form 1 — manual streaming. `execute_query` returns a `Rowset` drained chunk
/// by chunk; column access is positional and returns `Option<T>`. Maximum
/// control, minimum allocation, but indices are fragile and every value is an
/// `Option`.
fn form1_manual_streaming(conn: &Connection) -> Result<()> {
    println!("== Form 1 — manual streaming (execute_query + next_chunk) ==");

    let mut result = conn.execute_query(QUERY)?;
    while let Some(chunk) = result.next_chunk()? {
        for row in &chunk {
            // Positional access — column order must match the SELECT list.
            let id: Option<i32> = row.get(0);
            let name: Option<String> = row.get(1);
            let price: Option<f64> = row.get(2);
            let in_stock: Option<bool> = row.get(3);

            print_row(
                id.unwrap_or(-1),
                &name.unwrap_or_default(),
                price.unwrap_or(0.0),
                in_stock.unwrap_or(false),
            );
        }
    }
    println!();
    Ok(())
}

/// Form 2 — named access. `fetch_all` collects every row into a `Vec<Row>`;
/// `Row::get_by_name` looks each field up by column name (order-independent)
/// and returns `Result<T>` (NULL or missing column → error). The name lookup
/// is a linear scan per call — fine for small results.
fn form2_named_access(conn: &Connection) -> Result<()> {
    println!("== Form 2 — named access (fetch_all + Row::get_by_name) ==");

    let rows = conn.fetch_all(QUERY)?;
    for row in &rows {
        // Named access — column order in the SELECT doesn't matter.
        let id: i32 = row.get_by_name("id")?;
        let name: String = row.get_by_name("name")?;
        let price: f64 = row.get_by_name("price")?;
        let in_stock: bool = row.get_by_name("in_stock")?;

        print_row(id, &name, price, in_stock);
    }
    println!();
    Ok(())
}

/// Form 3 — hand-written `FromRow`. The struct controls its own field mapping;
/// `fetch_all_as` builds the column-name → index map once per query and hands
/// each `from_row` call a `RowAccessor` that reuses it (one `HashMap` lookup
/// per field, not a linear scan). Use when you need custom mapping logic or
/// can't use the derive.
#[derive(Debug)]
struct ProductManual {
    id: i32,
    name: String,
    price: f64,
    in_stock: bool,
}

impl FromRow for ProductManual {
    fn from_row(row: RowAccessor<'_>) -> Result<Self> {
        Ok(ProductManual {
            id: row.get("id")?,
            name: row.get("name")?,
            price: row.get("price")?,
            in_stock: row.get("in_stock")?,
        })
    }
}

fn form3_manual_from_row(conn: &Connection) -> Result<()> {
    println!("== Form 3 — manual FromRow impl (fetch_all_as) ==");

    let products: Vec<ProductManual> = conn.fetch_all_as(QUERY)?;
    for p in &products {
        print_row(p.id, &p.name, p.price, p.in_stock);
    }
    println!();
    Ok(())
}

/// Form 4 — `#[derive(FromRow)]`. The proc-macro generates the same impl as
/// Form 3; field names match column names by default (use
/// `#[hyperdb(rename = "...")]` or `#[hyperdb(index = N)]` to override).
/// `Option<T>` fields map NULL to `None`. Zero boilerplate.
#[derive(Debug, FromRow)]
struct ProductDerived {
    id: i32,
    name: String,
    price: f64,
    in_stock: bool,
}

fn form4_derive_from_row(conn: &Connection) -> Result<()> {
    println!("== Form 4 — #[derive(FromRow)] (fetch_all_as) ==");

    let products: Vec<ProductDerived> = conn.fetch_all_as(QUERY)?;
    for p in &products {
        print_row(p.id, &p.name, p.price, p.in_stock);
    }
    println!();
    Ok(())
}

/// Form 5 — streaming `FromRow`. `stream_as` returns a lazy iterator of
/// `Result<T>`, mapping each row via `FromRow` (here the derived
/// `ProductDerived`) while holding only one transport chunk in memory at a
/// time. The column-index map is built once on the first chunk and reused —
/// peak memory is bounded by the chunk size, not the total row count, so this
/// is the form to reach for on large or unbounded result sets.
///
/// `stream_as` reports errors in two places, and this loop handles both: the
/// `?` after `stream_as(...)` surfaces stream-open failures, and each
/// `row_result?` surfaces a per-row mapping error (or a transport error hit
/// while fetching a later chunk).
fn form5_streaming_from_row(conn: &Connection) -> Result<()> {
    println!("== Form 5 — streaming FromRow (stream_as, constant memory) ==");

    for row_result in conn.stream_as::<ProductDerived>(QUERY)? {
        let p = row_result?;
        print_row(p.id, &p.name, p.price, p.in_stock);
    }
    println!();
    Ok(())
}

/// Form 5, async flavor. `AsyncConnection::stream_as` returns an
/// `impl Stream<Item = Result<T>>` rather than an iterator — otherwise the
/// shape is identical to the sync version: lazy, one chunk in memory at a
/// time, index map built once. The stream is `!Unpin`, so it must be pinned
/// (here with `tokio::pin!`) before polling with `StreamExt::next`.
async fn form5_streaming_from_row_async(endpoint: &str) -> Result<()> {
    println!("== Form 5 (async) — AsyncConnection::stream_as (impl Stream) ==");

    let conn = AsyncConnection::connect(
        endpoint,
        "test_results/row_mapping_forms.hyper",
        CreateMode::DoNotCreate,
    )
    .await?;

    // Scope the stream so it (and its borrow of `conn`) is dropped before the
    // `conn.close()` below, which moves `conn`.
    {
        let stream = conn.stream_as::<ProductDerived>(QUERY);
        tokio::pin!(stream);
        while let Some(row_result) = stream.next().await {
            let p = row_result?;
            print_row(p.id, &p.name, p.price, p.in_stock);
        }
    }

    conn.close().await?;
    println!();
    Ok(())
}

/// Shared row formatter so every form prints identically — making it obvious
/// the five forms return the same data.
fn print_row(id: i32, name: &str, price: f64, in_stock: bool) {
    println!("{id:>2}  {name:<10}  ${price:.2}  in_stock={in_stock}");
}
