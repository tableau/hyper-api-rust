# What's New in 0.4.0

`0.4.0` is **purely additive** — there are no breaking changes from `0.3.x`.
Existing code keeps compiling unchanged. Two new capabilities land:

1. Streaming typed row mapping (`stream_as`)
2. Opt-in compile-time SQL validation (`query_as!` / `query_scalar!`)

> Still pre-1.0: the public API may change in future `0.x` releases. See the
> Project Status note in the [README](../README.md).

---

## Streaming `FromRow` mapping — `stream_as`

`fetch_all_as::<T>()` maps rows to a struct but collects **every** row into a
`Vec` first (memory O(total rows)). `0.4.0` adds `stream_as::<T>()`: the same
`FromRow` struct mapping, but **lazy** — only one transport chunk (~64K rows) is
held in memory at a time, regardless of how large the result set is. The
column-name → index lookup is built once and reused across all chunks.

```rust
use hyperdb_api::{Connection, FromRow};

#[derive(hyperdb_api_derive::FromRow)]
struct User { id: i32, name: String }

// Sync — lazy iterator of Result<User>, constant memory
for row in conn.stream_as::<User>("SELECT id, name FROM users")? {
    let user = row?;
    // ...
}
```

There's an async equivalent on `AsyncConnection` returning
`impl Stream<Item = Result<T>>`. Driving the stream needs the `StreamExt` /
`TryStreamExt` traits from the [`futures`](https://crates.io/crates/futures)
crate (add `futures = "0.3"` to your `Cargo.toml`; `hyperdb-api` itself only
pulls in `futures-core` for the `Stream` type):

```rust
use futures::StreamExt;

let stream = conn.stream_as::<User>("SELECT id, name FROM users");
tokio::pin!(stream);
while let Some(row) = stream.next().await {
    let user = row?;
    // ...
}
```

**When to use it:** large or unbounded result sets where buffering every row
would blow the memory budget. For small results, `fetch_all_as` is just as good.

This is "Form 5" in the row-mapping guide — see
[docs/ROW_MAPPING.md](ROW_MAPPING.md) for how it relates to the other four
mapping styles, and the runnable
[`row_mapping_forms`](../hyperdb-api/examples/additional_examples/row_mapping_forms.rs)
example (`cargo run -p hyperdb-api --example row_mapping_forms`).

---

## Compile-time SQL validation (opt-in)

The `hyperdb-api-derive` crate gains a `compile-time` feature that validates SQL
against your registered schema **at build time**. Mark structs with
`#[derive(Table)]`, then use the `query_as!` / `query_scalar!` macros: unknown
columns, typos, and struct/SQL mismatches become **compile errors** (with red
squigglies in VS Code) instead of runtime surprises.

```toml
[dependencies]
hyperdb-api = "0.4"
hyperdb-api-derive = { version = "0.4", features = ["compile-time"] }
```

```rust
use hyperdb_api_derive::{query_as, Table, FromRow};

#[derive(Table, FromRow)]
#[hyperdb(table = "users", register)]
struct User { id: i64, name: String, email: Option<String> }

// SQL is checked against User's columns when you `cargo build`.
let users: Vec<User> = query_as!(User, "SELECT id, name, email FROM users")
    .fetch_all(&conn)?;
```

It's **entirely opt-in** and off by default — without the feature flag the
macros pass through and validation is skipped, so there's no build-time cost or
`hyperd` dependency unless you ask for it.

**Known limitations** (today):

- Column *names* are validated, not types — type drift is still caught at
  runtime via `Error::Column`.
- Bind parameters are opaque to the validator.
- Validates struct-vs-SQL, not SQL-vs-production-DB.
- `INSERT`/`UPDATE`/`DELETE` without `RETURNING` aren't supported by
  `query_as!`; use `Connection::execute_command` for those.

Full setup (including VS Code configuration and the module-ordering constraint)
is in [hyperdb-api-derive/README.md](../hyperdb-api-derive/README.md).
