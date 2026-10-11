# sea-query-hyperdb Development

Contributor guide for the `sea-query-hyperdb` crate.

## Dialect Implementation

`HyperQueryBuilder` implements all sea-query backend traits (`QueryBuilder`,
`SchemaBuilder`, `GenericBuilder`, and their sub-traits) by delegating every
method to `PostgresQueryBuilder`. This is a deliberate design choice: HyperDB's
SQL dialect is currently PostgreSQL-compatible for identifier quoting, placeholder
syntax (`$1, $2, ...`), operator precedence, and DDL/DML syntax.

The delegation is explicit per-method rather than using a blanket `Deref` or
macro, so that individual methods can be overridden independently as the dialect
diverges. See the doc comment on `HyperQueryBuilder` in `src/lib.rs` for the
full rationale.

## When Hyper Diverges from PostgreSQL

As HyperDB adds dialect-specific features, override individual trait methods
to emit Hyper-specific SQL while leaving the rest delegated. Likely candidates:

- **Type names** -- Hyper may introduce types not in PostgreSQL
- **Function syntax** -- Hyper-specific built-in functions
- **DDL extensions** -- Hyper-specific table or column options

When adding an override:

1. Override the relevant method on the appropriate trait impl
2. Add a unit test that asserts the Hyper output differs from `PostgresQueryBuilder`
3. Document the divergence in the `HyperQueryBuilder` struct doc comment

## Testing

All tests live in the `#[cfg(test)] mod tests` block in `src/lib.rs`. The test
strategy verifies that `HyperQueryBuilder` produces identical SQL to
`PostgresQueryBuilder` for all standard operations (SELECT, INSERT, UPDATE,
DELETE, CREATE TABLE).

Run tests:

```bash
cargo test -p sea-query-hyperdb
```

Run the example:

```bash
cargo run -p sea-query-hyperdb --example basic_usage
```

When Hyper-specific overrides are added, tests should assert both that the
override produces the expected Hyper-specific SQL and that non-overridden
operations still match `PostgresQueryBuilder`.
