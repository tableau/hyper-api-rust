# hyperdb-api-core `protocol` module: Development Guide

Contributor-facing documentation for the `protocol` module of the internal `hyperdb-api-core` crate. For the
user-facing overview, see the [hyperdb-api README](../../hyperdb-api/README.md).

---

## Internal Architecture

### Two-Layer Byte Order Design

The Hyper wire protocol uses two byte orders at different layers:

| Layer | Byte Order | Where | Why |
|---|---|---|---|
| Message framing | BigEndian | `message/` module | PostgreSQL specification compatibility |
| Data encoding | LittleEndian | `copy`, `types` modules | Hyper's native format; avoids byte-swapping on x86/ARM-LE |

This separation is the most important architectural decision in the crate.
PostgreSQL clients and tools expect BigEndian message framing, but Hyper stores
and processes data in LittleEndian. Rather than converting at the storage layer,
the protocol keeps framing BigEndian (for compatibility) and data LittleEndian
(for performance).

**Practical implication:** When adding a new message type, use BigEndian for all
structural fields (tags, lengths, counts, OIDs). When adding a new data type
encoding, use LittleEndian for the value bytes. The inline doc comments in
`message/mod.rs` and `copy.rs` document this at the point of use.

### Module Layout

```text
src/
  lib.rs              # Crate root, re-exports
  message/
    mod.rs            # Message format docs, re-exports
    frontend.rs       # Client-to-server messages (stateless buffer writers)
    backend.rs        # Server-to-client message parsing (Message enum)
  copy.rs             # HyperBinary COPY format (header, row encoding, readers)
  escape.rs           # SQL identifier/literal escaping (newtype wrappers)
  types.rs            # Rust <-> HyperBinary type conversions
  proofs.rs           # Kani formal verification harnesses
```

### Message Parsing Design

Backend message parsing (`backend.rs`) uses a two-phase approach:

1. **Header check** -- Read the 5-byte header (tag + length). If the buffer
   doesn't contain enough bytes, return `Ok(None)` and reserve the shortfall.
2. **Body parse** -- Split the complete message from the buffer into an internal
   `Buffer` struct that tracks a read cursor, then parse tag-specific fields.

This design avoids copying: the parsed message body types hold `Bytes` handles
into the original buffer via `bytes::Bytes::split_to()`.

### SQL Escaping Design

The `escape` module uses the newtype-with-Display pattern (`SqlIdentifier`,
`SqlLiteral`) rather than functions returning `String`. This avoids intermediate
allocations when escaping is used inside `format!()` calls -- which is the
primary use case in `hyperdb-api`'s SQL generation code. See the module-level doc
comment in `escape.rs` for the full rationale.

---

## Formal Verification (Kani)

The crate includes proof harnesses in `proofs.rs` that use [Kani](https://model-checking.github.io/kani/)
for bounded model checking. These verify:

- **SQL escaping invariants**: `is_valid_unquoted_identifier` rejects empty
  strings, digit-prefixed strings, strings with spaces/hyphens; accepts letters
  and underscores.
- **COPY read safety**: `read_i16`, `read_i32`, `read_i64`, `read_data128`,
  `read_varbinary` never panic on arbitrary input of any length.
- **Roundtrip correctness**: For every possible value of each integer type,
  encoding to LE bytes and decoding returns the original value.
- **Error correctness**: Short/wrong-sized buffers always produce the expected
  error variant (never a panic).
- **Header consistency**: `HYPER_BINARY_HEADER.len() == HYPER_BINARY_HEADER_SIZE`
  and the header starts with `HYPER_BINARY_SIGNATURE`.

### Running Kani Proofs

```bash
# Install Kani (one-time)
cargo install --locked kani-verifier
cargo kani setup

# Run all proofs
cargo kani -p hyperdb-api-core

# Run a specific proof
cargo kani -p hyperdb-api-core --harness read_i32_no_panic
```

### Limitations

Kani cannot handle the `bytes` crate's internal allocator or dynamic
`format!()` machinery, so write functions (`write_*`) and `Display`
implementations are not covered by proofs. These are covered by conventional
unit tests instead.

---

## How to Extend the Protocol

### Adding a New Backend Message Type

1. Add a tag constant in `backend.rs` (e.g. `pub const NEW_MSG_TAG: u8 = b'?';`).
2. Add a variant to the `Message` enum.
3. Add a body struct with accessor methods, following the pattern of existing
   body types (private fields, `#[inline]` getters).
4. Add a match arm in `Message::parse()` that reads the message-specific fields
   from the `Buffer`.
5. Add unit tests verifying parse behavior for valid and truncated input.

### Adding a New Frontend Message

1. Add a public function in `frontend.rs` following the existing pattern:
   write tag byte, write BigEndian length, write payload fields.
2. Document the function with `///`, including the message's purpose, arguments,
   and which server response to expect.
3. Add unit tests if the encoding is non-trivial.

### Adding a New COPY Data Type

1. Add `write_<type>` and `write_<type>_not_null` functions in `copy.rs`.
   The `_not_null` variant omits the 1-byte null indicator.
2. Add a `read_<type>` function returning `Result<T, CopyReadError>`.
3. Add a corresponding method to `CopyDataBuilder`.
4. Add Kani proof harnesses in `proofs.rs` for the read function:
   - `read_<type>_no_panic` -- arbitrary input never panics
   - `read_<type>_roundtrip` -- encode-then-decode is identity
   - `read_<type>_short_buffer_is_err` -- buffers shorter than the type width
     always return `Err`
5. Add unit tests for both read and write.

### Adding a New Conversion in `types.rs`

1. Add `<type>_to_hyper_binary` and `<type>_from_hyper_binary` functions.
2. Use LittleEndian encoding for all data values.
3. Return `ParseError` variants for invalid input.
4. Add Kani roundtrip and no-panic proofs in `proofs.rs`.
5. Add unit tests.

---

## Testing

### Unit Tests

Each module has inline `#[cfg(test)] mod tests` blocks:

```bash
cargo test -p hyperdb-api-core
```

### Doc Tests

Public functions with `/// # Example` blocks are compiled and run by:

```bash
cargo test -p hyperdb-api-core --doc
```

### Checking Documentation

```bash
RUSTDOCFLAGS="-D warnings" cargo doc -p hyperdb-api-core --no-deps
```

This catches broken intra-doc links and missing docs (the crate enables
`#![warn(missing_docs)]`).

---

## Known Tech Debt

- **`write_tuple_start` and `write_trailer` are no-ops.** They exist for API
  compatibility with callers that were written against the PostgreSQL COPY
  format. They could be removed if callers are updated, but the cost is zero
  (they compile to nothing).
- **`CopyReadError` vs `ParseError` overlap.** Both represent "buffer too
  short" conditions. `CopyReadError` is in `copy.rs` (used by COPY read
  functions), `ParseError` is in `types.rs` (used by type conversions). A
  future cleanup could unify them, but they are stable and the separation
  matches the module boundary.
- **`is_valid_unquoted_identifier` does not check reserved words.** It checks
  syntax only, so `select` or `order` pass. This is why `SqlIdentifier` quotes
  unconditionally rather than consulting it: a bare reserved word is a syntax
  error in Hyper, and `"users"` and `users` are the same identifier.

---

## Related Documentation

- [hyperdb-api README](../../hyperdb-api/README.md) -- User-facing overview; see [Key Differences from PostgreSQL](../../DEVELOPMENT.md#key-differences-from-postgresql) for protocol deviations
- [Types development guide](DEVELOPMENT-types.md) -- Type system and binary serialization
- [Client development guide](DEVELOPMENT-client.md) -- Connection management
- [AGENTS.md](../../AGENTS.md) -- AI assistant guidance for the full workspace
