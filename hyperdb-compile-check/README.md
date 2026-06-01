# hyperdb-compile-check

Internal crate — compile-time SQL validation logic for [`hyperdb-api`](../hyperdb-api/README.md).

This is a **regular library** (not a proc-macro crate) so its validation logic can be unit-tested with standard `cargo test`. The proc-macro shells in `hyperdb-api-derive` call into this crate when the `compile-time` feature is enabled.

Not intended for direct use. Enable via `hyperdb-api-derive`'s `compile-time` cargo feature.
