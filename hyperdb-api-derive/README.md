# hyperdb-api-derive

Procedural macros for [`hyperdb-api`](../hyperdb-api/README.md):
- `#[derive(FromRow)]` — maps query result rows to Rust structs at runtime
- `#[derive(Table)]` — generates `CREATE TABLE` SQL from a struct and (optionally) registers it for compile-time validation
- `query_as!(T, "sql")` — typed query builder, validated at build time when the `compile-time` feature is enabled
- `query_scalar!(T, "sql")` — single-column query builder, validated at build time

Add `hyperdb-api-derive` directly to your `[dependencies]`:

```toml
hyperdb-api-derive = { version = "0.3", features = ["compile-time"] }
```

> **Without `features = ["compile-time"]`** the macros are pure pass-throughs —
> zero validation overhead, zero new dependencies. Add the feature to opt in.

---

## `#[derive(FromRow)]`

Maps named query result columns to struct fields at runtime.

```rust
use hyperdb_api::FromRow;
use hyperdb_api_derive::FromRow;

#[derive(Debug, FromRow)]
struct User {
    id: i32,
    name: String,
    #[hyperdb(rename = "email_address")]
    email: Option<String>,
}

let users: Vec<User> = conn.fetch_all_as("SELECT id, name, email_address FROM users")?;
```

### Attributes

- `#[hyperdb(rename = "col")]` — use a different SQL column name than the field name.
- `#[hyperdb(index = N)]` — use positional access at column `N` (zero-based) instead of name lookup. Mutually exclusive with `rename`.
- `#[hyperdb(primary_key)]` — documents intent; silently ignored by `FromRow` (relevant to `derive(Table)`).
- `Option<T>` fields tolerate SQL NULL (→ `None`); non-`Option` fields error on NULL.

### Hand-writing `FromRow`

When you need transformation — parsing a string column into an enum, defaulting NULLs, splitting a column across multiple fields — write the impl directly:

```rust
impl FromRow for User {
    fn from_row(row: hyperdb_api::RowAccessor<'_>) -> hyperdb_api::Result<Self> {
        Ok(User {
            id:    row.get("id")?,
            name:  row.get("full_name")?,
            email: row.get_opt("email_address")?,
        })
    }
}
```

#### `RowAccessor` cheat sheet

| | Required (`T`) | Optional (`Option<T>`) |
|---|---|---|
| **By name** | `row.get(name)?` | `row.get_opt(name)?` |
| **By index** | `row.position(idx)?` | `row.position_opt(idx)?` |

---

## `#[derive(Table)]`

Generates a `hyperdb_api::Table` impl with `NAME` and `CREATE_SQL` constants. Useful for runtime migrations, test fixtures, and as the source of truth for compile-time validation.

```rust
use hyperdb_api::Table;
use hyperdb_api_derive::{FromRow, Table};

#[derive(Debug, FromRow, Table)]
#[hyperdb(table = "users", register)]
struct User {
    #[hyperdb(primary_key)]
    id: i64,
    name: String,
    email: Option<String>,
}

// Use the derived CREATE_SQL to create the table at runtime:
conn.execute_command(User::CREATE_SQL)?;
println!("{}", User::NAME);       // "users"
println!("{}", User::CREATE_SQL); // "CREATE TABLE IF NOT EXISTS users (id BIGINT NOT NULL, ...)"
```

### Struct-level attributes

- `#[hyperdb(table = "name")]` — override the SQL table name (default: `lower_snake_case` of the struct ident, e.g. `UserOrder` → `user_order`).
- `#[hyperdb(register)]` — register this struct with the compile-time validator. Required for `query_as!` validation to work. Has no effect without the `compile-time` feature.

### Field-level attributes

- `#[hyperdb(primary_key)]` — documents intent; the column is `NOT NULL` for non-`Option` fields regardless.
- `#[hyperdb(rename = "col")]` — use a different SQL column name.

### Supported field types

| Rust type | SQL type |
|---|---|
| `i16` | `SMALLINT` |
| `i32` | `INTEGER` |
| `i64` | `BIGINT` |
| `f32` | `REAL` |
| `f64` | `DOUBLE PRECISION` |
| `bool` | `BOOLEAN` |
| `String` | `TEXT` |
| `Vec<u8>` | `BYTES` |
| `chrono::NaiveDate` | `DATE` |
| `chrono::NaiveDateTime` | `TIMESTAMP` |
| `chrono::NaiveTime` | `TIME` |
| `chrono::DateTime<Utc>` | `TIMESTAMPTZ` |
| `Numeric` | `NUMERIC` |
| `Option<T>` | nullable version of `T` (no `NOT NULL`) |

Any other type produces a compile error with a suggestion to write a manual `impl Table`.

---

## `query_as!(T, "sql" [, args...])`

Returns a [`hyperdb_api::QueryAs<T>`] builder. Validates the SQL at **build time** when `compile-time` feature is enabled.

```rust
use hyperdb_api_derive::{query_as, FromRow, Table};

let users: Vec<User> = query_as!(User, "SELECT id, name, email FROM users ORDER BY id")
    .fetch_all(&conn)?;

let alice: Option<User> = query_as!(User, "SELECT id, name, email FROM users WHERE id = 1")
    .fetch_optional(&conn)?;
```

Builder methods: `.fetch_all(&conn)`, `.fetch_one(&conn)`, `.fetch_optional(&conn)`.

### Compile-time validation

With `features = ["compile-time"]` and `HYPERD_PATH` set, `query_as!` validates at build time that:
- The target struct is registered via `#[derive(Table)] #[hyperdb(register)]`
- All referenced tables exist (seeded lazily from registered structs)
- All struct fields appear in the projected columns

Bad SQL produces a `compile_error!` pointing at the SQL string literal:

```
error: column "emai1" does not exist on any table in the query;
       check for a typo or a renamed/dropped column
```

```
error: `User` requires column "email" but the query does not project it;
       add it to the SELECT list or remove the field from `User`
```

### Module ordering constraint

`derive(Table)` registers the struct at macro expansion time. Within a single file, struct derives always expand before function-body macros — ordering within a file is never a problem.

Across files: the module containing `derive(Table)` structs must be **declared** (`mod structs;`) **before** the module containing `query_as!` calls in your `lib.rs` / `main.rs`. Reorder the `mod` declarations if you get a false `StructNotRegistered` error.

---

## `query_scalar!(T, "sql" [, args...])`

Like `query_as!` but for single-column queries. `T` must implement `hyperdb_api::RowValue`.

```rust
use hyperdb_api_derive::query_scalar;

let count: i64 = query_scalar!(i64, "SELECT COUNT(*) FROM users").fetch_one(&conn)?;
let names: Vec<String> = query_scalar!(String, "SELECT name FROM users").fetch_all(&conn)?;
```

With `compile-time` feature, validates that the SQL projects exactly one column.

---

## VS Code: squigglies on bad SQL

To see compile-time errors as squigglies in the editor:

**1. Add `HYPERD_PATH` to your shell** (`~/.zshrc` or `~/.bashrc`):

```sh
export HYPERD_PATH=/path/to/your/project/.hyperd/current
```

Restart your terminal and VS Code after changing this.

**2. Add a `.vscode/settings.json`** in your workspace root:

```json
{
  "rust-analyzer.cargo.features": ["hyperdb-api-derive/compile-time"],
  "rust-analyzer.server.extraEnv": {
    "HYPERD_PATH": "${workspaceFolder}/.hyperd/current"
  }
}
```

> **Important:** `rust-analyzer.cargo.features` must be a flat **array** of
> `"package/feature"` strings. RA silently ignores the JSON-object form
> and builds with no features, so validation never fires.

**3. Reload the VS Code window** (`Cmd+Shift+P` → `Developer: Reload Window`).

After RA finishes indexing you'll see squigglies on bad SQL strings and errors in the Problems panel. The first expansion starts an embedded Hyper instance (~156 ms); subsequent expansions reuse it.

**To temporarily disable** (if hyperd is unavailable on a machine):

```json
{
  "rust-analyzer.procMacro.ignored": {
    "hyperdb-api-derive": ["query_as", "query_scalar"]
  }
}
```

---

## Known limitations

- **Type checking not yet implemented** — only column *names* are validated. Runtime `Error::Column { kind: TypeMismatch }` still catches type drift.
- **No parameter type checking** — bind parameters are opaque at compile time.
- **Validates struct vs. SQL, not SQL vs. production DB** — struct/prod schema drift is still a runtime error.
- **`INSERT`/`UPDATE`/`DELETE` without `RETURNING`** are not supported by `query_as!`; use `Connection::execute_command` directly.
