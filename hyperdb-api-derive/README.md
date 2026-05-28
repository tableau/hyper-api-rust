# hyperdb-api-derive

⚠️ **This crate is an implementation detail of
[`hyperdb-api`](https://crates.io/crates/hyperdb-api).**
Use `hyperdb-api` directly; don't add `hyperdb-api-derive` to your dependencies.

This crate provides the procedural macros that `hyperdb-api` re-exports
(currently just `#[derive(FromRow)]`). Use them through `hyperdb-api`.

## Quick example

```rust
use hyperdb_api::{Connection, ConnectionBuilder, FromRow, Result};

#[derive(Debug, FromRow)]
struct User {
    id: i32,
    name: String,
    #[hyperdb(rename = "email_address")]
    email: Option<String>,
}

fn main() -> Result<()> {
    let conn: Connection = ConnectionBuilder::new("localhost:7483").connect()?;

    let alice: User = conn.fetch_one_as("SELECT * FROM users WHERE id = 1")?;
    println!("{alice:?}");

    let everyone: Vec<User> = conn.fetch_all_as("SELECT * FROM users ORDER BY id")?;
    for u in &everyone {
        println!("{u:?}");
    }
    Ok(())
}
```

## Field-to-column mapping rules

- **Field name = column name** by default. A field `name: String` reads
  the column called `name`.
- **`#[hyperdb(rename = "...")]`** overrides the column name. Use this
  when the SQL column doesn't match the Rust field — snake_case
  mismatches, reserved words, columns named after Rust keywords, etc.
- **`#[hyperdb(index = N)]`** switches that field to positional access
  at column `N` (zero-based). Useful for queries with computed/unnamed
  columns where there's no stable name to match — e.g. `SELECT id,
  COUNT(*) FROM ... GROUP BY id`. Mutually exclusive with `rename`.
- **`Option<T>` fields tolerate SQL NULL** (become `None`). Non-`Option`
  fields error with `Error::Column { kind: Null, .. }` if the cell is
  NULL.
- **Missing columns** (the column isn't in the result schema) error
  with `Error::Column { kind: Missing, .. }` at fetch time.

```rust
#[derive(FromRow)]
struct Aggregate {
    #[hyperdb(index = 0)]
    id: i32,
    #[hyperdb(index = 1)]
    total: Option<i64>,
}
// Works against `SELECT id, COUNT(*) FROM ... GROUP BY id`
```

## When to hand-write `FromRow` instead

The derive emits a straightforward mapping. If you need transformation
in the mapping — parsing a string column into a Rust enum, splitting a
single column into multiple fields, defaulting NULLs to a non-`Option`
value, etc. — write the impl directly:

```rust
impl FromRow for User {
    fn from_row(row: hyperdb_api::RowAccessor<'_>) -> Result<Self> {
        Ok(User {
            id:    row.get("id")?,
            name:  row.get("full_name")?,        // SQL column "full_name"
            email: row.get_opt("email_address")?,
        })
    }
}
```

In a hand-written impl, the string passed to `row.get(...)` /
`row.get_opt(...)` *is* the column name — no `#[hyperdb(rename)]` is
needed, since you're spelling the column out yourself. Your `SELECT`
just needs to actually return that column (use `AS full_name` if the
underlying table column has a different name).

### `RowAccessor` accessor cheat sheet

`RowAccessor` exposes four accessors. Pick by access mode (name vs.
index) and required vs. optional. Indices are **zero-based**.

| | Required (`T`) | Optional (`Option<T>`) |
|---|---|---|
| **By name** | `row.get(name)?` | `row.get_opt(name)?` |
| **By index** | `row.position(idx)?` | `row.position_opt(idx)?` |

NULL handling differs between the two columns of the table:

- **`get` / `position`** — NULL errors with `Error::Column { kind: Null, .. }`.
  Use these for required fields where NULL is a problem.
- **`get_opt` / `position_opt`** — NULL becomes `Ok(None)`. Use these for
  fields whose Rust type is `Option<T>`.

The Rust field type and the accessor must agree: `position` returns
`Result<T>`, `position_opt` returns `Result<Option<T>>`. Mixing them
across types is a compile error, not a runtime mismatch:

```rust
// ✅ field type matches accessor return
let email: Option<String> = row.position_opt(2)?;
let id:    i32            = row.position(0)?;

// ❌ compile errors
let email: Option<String> = row.position(2)?;     // returns T, not Option<T>
let id:    i32            = row.position_opt(0)?; // returns Option<T>
```

If you want to silently default a NULL on a non-`Option` field, opt in
explicitly:

```rust
name: row.position_opt(1)?.unwrap_or_default(),   // NULL → ""
```

See the [`hyperdb-api` docs](https://docs.rs/hyperdb-api) for full usage.

This crate has no stable API. Breaking changes land here without a major
version bump of `hyperdb-api-derive`; your build may break on any
`hyperdb-api` patch release if you depend on `hyperdb-api-derive` directly.
