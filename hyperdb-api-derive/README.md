# hyperdb-api-derive

⚠️ **This crate is an implementation detail of
[`hyperdb-api`](https://crates.io/crates/hyperdb-api).**
Use `hyperdb-api` directly; don't add `hyperdb-api-derive` to your dependencies.

This crate provides the procedural macros that `hyperdb-api` re-exports
(currently just `#[derive(FromRow)]`). Use them through `hyperdb-api`:

```rust
use hyperdb_api::FromRow;

#[derive(FromRow)]
struct User {
    id: i32,
    name: String,
    #[hyperdb(rename = "email_address")]
    email: Option<String>,
}
```

See the [`hyperdb-api` docs](https://docs.rs/hyperdb-api) for full usage.

This crate has no stable API. Breaking changes land here without a major
version bump of `hyperdb-api-derive`; your build may break on any
`hyperdb-api` patch release if you depend on `hyperdb-api-derive` directly.
