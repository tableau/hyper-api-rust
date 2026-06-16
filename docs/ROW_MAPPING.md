# Row Mapping: Six Forms

When querying Hyper, there are several ways to map result rows into Rust values
— from fully manual to fully automatic. Forms 1–4 trade manual control for
convenience; Form 5 combines the automatic struct mapping of Form 4 with the
constant-memory streaming of Form 1; Form 6 adds `$1` parameter binding to the
struct mapping. Start with the simplest that fits your situation.

All five forms are demonstrated end-to-end in one runnable example:

```
cargo run -p hyperdb-api --example row_mapping_forms
```

The examples below all use the same schema:

```sql
CREATE TABLE products (
    id       INT         NOT NULL,
    name     TEXT        NOT NULL,
    price    DOUBLE PRECISION NOT NULL,
    in_stock BOOLEAN     NOT NULL
)
```

---

## Form 1 — Manual streaming (lowest level)

`Connection::execute_query` returns a `Rowset` that you drain chunk by chunk.
Column access is positional (`row.get(0)`) and returns `Option<T>`.

This is the right choice when you need streaming (constant memory for huge
result sets), want to process rows without allocating a `Vec`, or are building
infrastructure that works with arbitrary schemas.

```rust
use hyperdb_api::{Connection, CreateMode, HyperProcess, Result};

fn main() -> Result<()> {
    let hyper = HyperProcess::new(None, None)?;
    let conn = Connection::new(&hyper, "products.hyper", CreateMode::DoNotCreate)?;

    let mut result = conn.execute_query(
        "SELECT id, name, price, in_stock FROM products ORDER BY id",
    )?;

    while let Some(chunk) = result.next_chunk()? {
        for row in &chunk {
            // Positional access — column order must match the SELECT list.
            let id:       Option<i32>    = row.get(0);
            let name:     Option<String> = row.get(1);
            let price:    Option<f64>    = row.get(2);
            let in_stock: Option<bool>   = row.get(3);

            println!(
                "{:>2}  {:<10}  ${:.2}  in_stock={}",
                id.unwrap_or(-1),
                name.unwrap_or_default(),
                price.unwrap_or(0.0),
                in_stock.unwrap_or(false),
            );
        }
    }
    Ok(())
}
```

**Trade-offs:** Maximum control and minimum allocations. Column indices are
fragile — a reordered `SELECT` silently breaks the mapping. All values come
back as `Option<T>`, so you handle nullability at every call site.

---

## Form 2 — Named access with `fetch_all` + `Row::get_by_name`

`Connection::fetch_all` collects every row into a `Vec<Row>`. Access each field
by name with `row.get_by_name("col")`, which returns `Result<T>` (error on NULL
or missing column).

Use this when you want name-based safety without defining a struct — good for
one-off scripts, exploration, or when the struct would only be used in one place.

```rust
use hyperdb_api::{Connection, CreateMode, HyperProcess, Result};

fn main() -> Result<()> {
    let hyper = HyperProcess::new(None, None)?;
    let conn = Connection::new(&hyper, "products.hyper", CreateMode::DoNotCreate)?;

    let rows = conn.fetch_all(
        "SELECT id, name, price, in_stock FROM products ORDER BY id",
    )?;

    for row in &rows {
        // Named access — column order in the SELECT doesn't matter.
        let id:       i32    = row.get_by_name("id")?;
        let name:     String = row.get_by_name("name")?;
        let price:    f64    = row.get_by_name("price")?;
        let in_stock: bool   = row.get_by_name("in_stock")?;

        println!(
            "{:>2}  {:<10}  ${:.2}  in_stock={}",
            id, name, price, in_stock,
        );
    }
    Ok(())
}
```

**Trade-offs:** Column order independence and `Result<T>` on every access (NULL
→ error, missing column → error). The name-to-index lookup is a linear scan per
call — fine for small result sets, but for large ones prefer Form 3 or 4 which
build the lookup once.

---

## Form 3 — Manual `FromRow` impl + `fetch_all_as`

Implement `FromRow` on your struct, then call `Connection::fetch_all_as::<T>`.
The engine builds the column-name → index map once per query and hands every
`from_row` call a `RowAccessor` that reuses it — a single `HashMap` lookup
per field instead of a linear scan.

Use this when you need a named struct but can't use derive (generic struct, custom
mapping logic, non-matching field/column names without `rename`, etc.).

```rust
use hyperdb_api::{
    Connection, CreateMode, FromRow, HyperProcess, Result, RowAccessor,
};

#[derive(Debug)]
struct Product {
    id:       i32,
    name:     String,
    price:    f64,
    in_stock: bool,
}

impl FromRow for Product {
    fn from_row(row: RowAccessor<'_>) -> Result<Self> {
        Ok(Product {
            id:       row.get("id")?,
            name:     row.get("name")?,
            price:    row.get("price")?,
            in_stock: row.get("in_stock")?,
        })
    }
}

fn main() -> Result<()> {
    let hyper = HyperProcess::new(None, None)?;
    let conn = Connection::new(&hyper, "products.hyper", CreateMode::DoNotCreate)?;

    let products: Vec<Product> = conn.fetch_all_as(
        "SELECT id, name, price, in_stock FROM products ORDER BY id",
    )?;

    for p in &products {
        println!(
            "{:>2}  {:<10}  ${:.2}  in_stock={}",
            p.id, p.name, p.price, p.in_stock,
        );
    }
    Ok(())
}
```

**Trade-offs:** Explicit control — you see every field mapping, can add
transformation logic, and can map columns to differently-named fields. The
downside is boilerplate: adding or renaming a field means updating the `impl`
block by hand. Form 4 removes that boilerplate.

---

## Form 4 — `#[derive(FromRow)]` (simplest)

Add `#[derive(FromRow)]` to the struct. The proc-macro generates the same
`FromRow` impl as Form 3 — field names are matched to column names by default,
and `Option<T>` fields use `get_opt` (NULL → `None`) instead of `get` (NULL → error).

Use `#[hyperdb(rename = "col_name")]` when a field name doesn't match its
column name, or `#[hyperdb(index = N)]` for positional access.

```rust
use hyperdb_api::{
    Connection, CreateMode, FromRow, HyperProcess, Result,
};

// The derive generates: impl FromRow for Product { fn from_row(...) { ... } }
// Each field maps to the column with the same name.
#[derive(Debug, FromRow)]
struct Product {
    id:       i32,
    name:     String,
    price:    f64,
    in_stock: bool,
}

fn main() -> Result<()> {
    let hyper = HyperProcess::new(None, None)?;
    let conn = Connection::new(&hyper, "products.hyper", CreateMode::DoNotCreate)?;

    let products: Vec<Product> = conn.fetch_all_as(
        "SELECT id, name, price, in_stock FROM products ORDER BY id",
    )?;

    for p in &products {
        println!(
            "{:>2}  {:<10}  ${:.2}  in_stock={}",
            p.id, p.name, p.price, p.in_stock,
        );
    }
    Ok(())
}
```

**Trade-offs:** Zero boilerplate — add or rename a struct field and the mapping
updates automatically. Use Form 3 when you need custom logic in `from_row`; use
Form 4 for everything else.

### Attribute reference

| Attribute | Effect |
|---|---|
| *(none)* | Field `foo` maps to column `"foo"` |
| `#[hyperdb(rename = "col")]` | Field maps to column `"col"` |
| `#[hyperdb(index = N)]` | Field maps to column at position `N` (positional, not named) |
| Field type `Option<T>` | NULL → `None`; non-NULL decoded as `T` |
| Field type `T` (non-Option) | NULL → error; non-NULL decoded as `T` |

---

## Form 5 — Streaming `FromRow` mapping

Forms 1–4 leave a gap: Form 4 (`fetch_all_as`) gives you automatic struct
mapping but calls `fetch_all` first — collecting **all rows** into a `Vec<Row>`
before any mapping happens, so memory is O(total rows). Form 1 streams with
constant memory but is positional and untyped.

`Connection::stream_as::<T>()` closes the gap: it returns a **lazy iterator**
that maps each row to `T` via `FromRow` (hand-written or `#[derive(FromRow)]`,
exactly as in Forms 3 and 4) while holding only one chunk in memory at a time.
The column-name → index lookup is built once from the first chunk's schema and
reused for every row, so per-row mapping stays O(1) in the column count. Rows
arrive one transport chunk at a time (up to ~64K rows per chunk), and only the
current chunk is held in memory — so peak memory is bounded by the chunk size,
not by how many rows the query returns.

```rust
use hyperdb_api::{Connection, CreateMode, FromRow, HyperProcess, Result};

fn main() -> Result<()> {
    let hyper = HyperProcess::new(None, None)?;
    let conn = Connection::new(&hyper, "products.hyper", CreateMode::DoNotCreate)?;

    // Product derives FromRow (see Form 4); fields match the column names.
    for row_result in conn.stream_as::<Product>(
        "SELECT id, name, price, in_stock FROM products ORDER BY id",
    )? {
        let p: Product = row_result?;
        println!("{:>2}  {:<10}  ${:.2}  in_stock={}", p.id, p.name, p.price, p.in_stock);
    }
    Ok(())
}
```

### Error handling

`stream_as` reports errors in two places, and robust code handles both:

- The outer `Result` (the `?` after `stream_as(...)`) carries failures detected
  while *opening* the stream. On the gRPC transport that includes SQL parse and
  server errors; on the default TCP transport the query streams lazily, so a SQL
  error such as a missing table is usually reported as the **first iterator
  item** instead.
- Each item is itself a `Result<T>` — `Err` for a per-row mapping failure
  (missing column, type mismatch, NULL in a non-`Option` field) or for a
  server/transport error hit while fetching a later chunk.

Do not assume a successfully-returned iterator means the query succeeded; always
handle the per-item `Result` too (the `let p = row_result?;` above does this).

### Async

`AsyncConnection::stream_as::<T>()` is the async equivalent, returning
`impl Stream<Item = Result<T>>`. The stream is lazy — nothing executes until
first polled — so a submission failure surfaces as the first `Err` item. Pin the
stream before polling:

```rust
use futures::StreamExt;

async fn print_products(conn: &hyperdb_api::AsyncConnection) -> hyperdb_api::Result<()> {
    let stream = conn.stream_as::<Product>(
        "SELECT id, name, price, in_stock FROM products ORDER BY id",
    );
    tokio::pin!(stream);
    while let Some(row_result) = stream.next().await {
        let p: Product = row_result?;
        println!("{}: {}", p.id, p.name);
    }
    Ok(())
}
```

---

## Form 6 — Parameterized struct mapping

Forms 3–5 map rows into structs but take a plain `&str` query — no parameter
binding. [`query_params`](https://docs.rs/hyperdb-api/latest/hyperdb_api/struct.Connection.html#method.query_params)
binds `$1`, `$2`, … placeholders safely but returns raw `Row`s. The `_as_params`
methods are the intersection: they bind parameters via `ToSqlParam` *and* map
each result row into a `FromRow` struct, in a single call — no manual
`RowAccessor` loop and no SQL-injection risk.

There are three, mirroring the non-param trio:

- `fetch_one_as_params::<T>(query, params)` → `Result<T>`
- `fetch_all_as_params::<T>(query, params)` → `Result<Vec<T>>`
- `stream_as_params::<T>(query, params)` → `Result<impl Iterator<Item = Result<T>>>`
  (constant memory, like Form 5)

```rust
use hyperdb_api::{Connection, CreateMode, FromRow, HyperProcess, Result};

fn main() -> Result<()> {
    let hyper = HyperProcess::new(None, None)?;
    let conn = Connection::new(&hyper, "products.hyper", CreateMode::DoNotCreate)?;

    // Product derives FromRow (see Form 4); $1 is bound from `params`.
    let max_price = 15.0f64;
    let affordable: Vec<Product> = conn.fetch_all_as_params(
        "SELECT id, name, price, in_stock FROM products WHERE price < $1 ORDER BY id",
        &[&max_price],
    )?;
    for p in &affordable {
        println!("{:>2}  {:<10}  ${:.2}  in_stock={}", p.id, p.name, p.price, p.in_stock);
    }
    Ok(())
}
```

Error handling matches the underlying methods: `fetch_one_as_params` /
`fetch_all_as_params` return their errors on the `Result`; `stream_as_params`
(sync) reports stream-open errors — including `FeatureNotSupported` on the gRPC
transport, since prepared statements are TCP-only — on the outer `Result`, and
per-row mapping errors as each item's `Result`. The async `stream_as_params`
returns an `impl Stream` with no outer `Result`, so submission failures surface
as the first yielded `Err` item (as with the async Form 5 above).

### Async

Each `_as_params` method has an `AsyncConnection` equivalent — `await` the
`fetch_*` variants, and pin the `stream_as_params` stream just like Form 5.

---

## Choosing a form

| Need | Use |
|---|---|
| Streaming / billion-row result sets, no struct | Form 1 (`execute_query` + `next_chunk`) |
| Ad-hoc access, no struct needed | Form 2 (`fetch_all` + `get_by_name`) |
| Named struct, custom mapping logic | Form 3 (`impl FromRow` manually) |
| Named struct, fields match columns | Form 4 (`#[derive(FromRow)]`) |
| Streaming + named struct (constant memory) | Form 5 (`stream_as`) |
| Named struct from a parameterized (`$1`) query | Form 6 (`fetch_*_as_params` / `stream_as_params`) |

For scalar values (a single `COUNT(*)`, `MAX`, etc.), use
[`fetch_scalar`](https://docs.rs/hyperdb-api/latest/hyperdb_api/struct.Connection.html#method.fetch_scalar)
instead — it skips the struct entirely.
