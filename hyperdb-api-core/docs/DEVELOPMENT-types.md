# hyperdb-api-core `types` module: Development Guide

Contributor-facing documentation for the `types` module of the internal `hyperdb-api-core` crate: internal architecture, how to extend the type system, testing, and known design decisions.

For user-facing documentation (API surface and usage examples), see the [hyperdb-api README](../../hyperdb-api/README.md).

---

## Internal Architecture

### Module Layout

| Module | Responsibility |
|---|---|
| `lib.rs` | Crate root; re-exports public API |
| `traits.rs` | `ToHyperBinary`, `FromHyperBinary`, `IsNull` trait definitions and helpers |
| `types.rs` | Trait implementations for Rust primitives (`bool`, integers, floats, `String`, `Vec<u8>`, `Option<T>`) |
| `special.rs` | Hyper-specific types: `Date`, `Time`, `Timestamp`, `OffsetTimestamp`, `Interval`, `Numeric`, `Geography` |
| `oid.rs` | `Oid` newtype, well-known OID constants, `Type` struct (OID + modifier) |
| `sql_type.rs` | `SqlType` enum, `Nullability`, `ColumnDefinition` |
| `chrono_integration.rs` | `TryFrom`/`From` conversions between Hyper types and `chrono` types |
| `proofs.rs` | Kani formal verification proof harnesses (compiled only under `#[cfg(kani)]`) |

### Data Flow

```text
User Rust value
  │
  ▼
ToHyperBinary::to_hyper_binary[_not_null]()     ← serialization
  │
  ▼
bytes::BytesMut (LittleEndian wire bytes)
  │
  ▼
protocol COPY writer / Inserter                 ← sent to Hyper
  │
  ...round-trip through hyperd...
  │
  ▼
protocol COPY reader / RowDescription           ← received from Hyper
  │
  ▼
FromHyperBinary::from_hyper_binary()            ← deserialization
  │
  ▼
User Rust value
```

### Key Design Decisions

**LittleEndian everywhere.** Unlike standard PostgreSQL (BigEndian / network byte order), Hyper uses LittleEndian for performance on x86/ARM-LE. This is a fundamental divergence and affects every serialization path.

**Numeric has no `FromHyperBinary` impl.** The binary wire format for `NUMERIC` does not carry scale or width information -- both must come from column metadata (`SqlType::Numeric { precision, scale }`). Implementing `FromHyperBinary` would require defaulting scale to 0, silently corrupting decimal values. Use `Numeric::from_binary_with_scale()` instead. See the detailed rationale in `special.rs`.

**Dual wire form for Numeric.** Hyper sends `NUMERIC` as 8 bytes (i64) when precision <= 18 and 16 bytes (i128) when precision > 18. `Numeric::from_binary_with_scale()` dispatches on buffer length. The 18-digit threshold is `Numeric::SMALL_NUMERIC_MAX_PRECISION` and matches `Type::maxPrecisionNumeric` in the Hyper C++ source.

**Date uses Julian Day Numbers.** Hyper stores dates as Julian Day Numbers on the wire, not as days-since-epoch. The `Date::encode()`/`decode()` methods handle the conversion. The epoch is 2000-01-01 (Julian Day 2,451,545), matching PostgreSQL.

**Geography has two binary formats.** Data read from Hyper is in a proprietary legacy format; data created via `from_wkt()`/`from_wkb()` is in standard WKB. The `GeographyBinaryFormat` enum tracks which format a value holds, preventing accidental WKB-only operations on legacy data.

**Binary vs text format detection.** `FromHyperBinary` for `i32`/`i64`/`f32`/`f64` uses a heuristic to distinguish binary from text format. See the module-level doc comment in `types.rs` for details. Fixed-size types like `i16` and `u32` always treat exact-size buffers as binary.

---

## Adding a New SQL Type

Follow this checklist when adding a type to the Hyper type system. Each step references the file where the change goes.

### 1. Define the OID (`oid.rs`)

Add a constant to the `oids` module:

```rust
/// MY_TYPE description
pub const MY_TYPE: Oid = Oid(<oid_value>);
```

If the type has a modifier (precision, length), add convenience constructors to `Type`:

```rust
impl Type {
    pub const fn my_type() -> Self { Type::new(oids::MY_TYPE) }
}
```

### 2. Add the SqlType variant (`sql_type.rs`)

Add a variant to the `SqlType` enum, a constructor method, and handle it in:

- `SqlType::internal_oid()`
- `SqlType::from_oid()`
- `SqlType::from_oid_and_modifier()` (if the type has a modifier)
- `SqlType::Display` impl
- Category methods (`is_numeric()`, `is_string()`, `is_temporal()`) if applicable

### 3. Define the Rust type (`special.rs` or inline)

For types that need a dedicated struct (like `Date`, `Numeric`), add it to `special.rs` with:

- `encode()` / `decode()` methods for the wire format
- `Display` impl
- Descriptive doc comments including wire format details

For types that map directly to a Rust primitive (like `SMALLINT` -> `i16`), skip this step.

### 4. Implement serialization traits (`types.rs` or `special.rs`)

Implement `ToHyperBinary` and `FromHyperBinary`:

```rust
impl ToHyperBinary for MyType {
    fn to_hyper_binary(&self, buf: &mut BytesMut) -> Result<(), Box<dyn Error + Send + Sync>> {
        write_not_null_indicator(buf);
        self.to_hyper_binary_not_null(buf)
    }

    fn to_hyper_binary_not_null(&self, buf: &mut BytesMut) -> Result<(), Box<dyn Error + Send + Sync>> {
        // Write LittleEndian bytes
        Ok(())
    }

    fn hyper_binary_size(&self) -> usize { NULL_INDICATOR_SIZE + <size> }
    fn hyper_binary_size_not_null(&self) -> usize { <size> }
}
```

### 5. Wire up higher layers

These changes are outside the `types` module but complete the integration:

- `protocol` module: Handle the type in COPY row parsing (if variable-length)
- `hyperdb-api`: Add `row.get::<MyType>()` support in `result.rs`
- `hyperdb-api`: Add `Inserter` support if the type can be inserted

### 6. Add tests

- Unit tests in the same file as the implementation
- Roundtrip tests: serialize then deserialize and compare
- Edge cases: minimum/maximum values, NULL handling
- Integration tests in `hyperdb-api/tests/` with a real `hyperd` server

### 7. Optional: Add chrono integration (`chrono_integration.rs`)

If the type is temporal, implement `TryFrom`/`From` conversions to/from `chrono` types.

### 8. Optional: Add Kani proofs (`proofs.rs`)

For fixed-size types, add roundtrip and no-panic proof harnesses.

---

## Testing

### Running Tests

```bash
# All hyperdb-api-core tests (narrow with a `types::` filter)
cargo test -p hyperdb-api-core

# A specific test
cargo test -p hyperdb-api-core test_numeric_from_binary_with_scale

# Doc tests only
cargo test -p hyperdb-api-core --doc
```

### Test Patterns

**Roundtrip tests** verify that `encode` -> `decode` (or `to_hyper_binary` -> `from_hyper_binary`) yields the original value:

```rust
#[test]
fn test_date_roundtrip() {
    let date = Date::new(2024, 6, 15);
    let (y, m, d) = date.to_ymd();
    assert_eq!((y, m, d), (2024, 6, 15));
}
```

**Wire-pattern tests** lock in exact byte sequences observed from `hyperd`, guarding against regressions:

```rust
#[test]
fn test_numeric_decode_int64_wire_pattern_from_hyperd() {
    let bytes: [u8; 8] = [0x80, 0x84, 0x1e, 0x00, 0x00, 0x00, 0x00, 0x00];
    let numeric = Numeric::decode_int64(&bytes, 6);
    assert_eq!(numeric.to_f64(), 2.0);
}
```

**Modifier bounds tests** verify that malformed type modifiers (from `RowDescription` messages) are handled gracefully rather than panicking or producing garbage.

### Formal Verification (Kani)

The `proofs.rs` module contains [Kani](https://model-checking.github.io/kani/) proof harnesses that use bounded model checking to verify:

- **Roundtrip correctness**: For all possible values of a type, `encode` then `decode` yields the original.
- **No-panic guarantees**: `from_le_bytes` never panics on arbitrary input.
- **Size constant correctness**: `NULL_INDICATOR_SIZE == 1`.

Kani proofs are gated behind `#[cfg(kani)]` and are not compiled during normal builds. To run them:

```bash
# Install Kani (one-time)
cargo install --locked kani-verifier
cargo kani setup

# Run all proofs
cargo kani -p hyperdb-api-core

# Run a specific proof
cargo kani -p hyperdb-api-core --harness date_encode_decode_roundtrip
```

**Note:** Kani proofs avoid calling trait methods that return `Box<dyn Error>` because Kani's model of dynamic dispatch causes infinite unwinding on `fmt::Debug` vtables. Instead, proofs verify at the byte level using `to_le_bytes` / `from_le_bytes` directly.

---

## Known Tech Debt

- **Binary/text heuristic in `FromHyperBinary`**: The `i32`/`i64`/`f32`/`f64` implementations use a heuristic to detect text vs binary format. This works but is fragile -- binary values that happen to be all-ASCII-digits are misclassified. The `i16` and `u32` implementations show the better pattern (always binary for exact-size buffers). The remaining types should be migrated to the same approach once the protocol layer consistently sends binary format.

- **`FromHyperBinary` not implemented for `Numeric`**: This is intentional (see Design Decisions above) but means `Numeric` requires special handling in the result-set deserialization path.

---

## Related Documentation

- [hyperdb-api README](../../hyperdb-api/README.md) -- User-facing overview and usage examples
- [../../AGENTS.md](../../AGENTS.md) -- Repository-wide AI assistant guidance
- [../../docs/RUST_DOCUMENTATION_STYLE.md](../../docs/RUST_DOCUMENTATION_STYLE.md) -- Documentation conventions
- Source-level docs: `cargo doc -p hyperdb-api-core --open`
