# Transaction Support

This document describes the transaction API in the Hyper Rust API, covering ACID semantics (A, C, I guaranteed; D not provided by this API), the RAII `Transaction` / `AsyncTransaction` guards, behavioral notes, and the test inventory.

## Overview

Hyper transactions in the Rust API guarantee **A**tomicity, **C**onsistency, and **I**solation. **Durability is not provided by this API.** Committed data is held in the server's memory; the database becomes durable only when it is closed, unloaded, detached, or released — at which point its data is flushed to disk. An unexpected process termination (crash, SIGKILL) before that flush can lose committed transactions.

The recommended way to drive transactions is the **RAII guard** (`Transaction<'conn>` / `AsyncTransaction<'conn>`), which auto-rolls back on drop and uses Rust's borrow checker to make several classes of misuse compile errors.

> The `Connection::begin_transaction` / `commit` / `rollback` methods, deprecated since v0.3.0, were **removed in 1.0.0**. Where the guard's `&mut self` borrow is genuinely impossible, use `begin_transaction_unguarded` / `commit_unguarded` / `rollback_unguarded` instead. See ["Unguarded transaction control"](#unguarded-transaction-control) at the bottom of this doc.

## API Reference

### RAII Transaction Guard (Sync)

```rust
use hyperdb_api::{Connection, CreateMode, Transaction};

let mut conn = Connection::connect("localhost:7483", "db.hyper", CreateMode::DoNotCreate)?;
let txn: Transaction<'_> = conn.transaction()?; // exclusively borrows conn
txn.execute_command("INSERT INTO t VALUES (1, 'hello')")?;
txn.commit()?;
// If commit() is not called, Drop issues ROLLBACK automatically.
```

`Transaction<'conn>` exclusively borrows `&'conn mut Connection`, preventing any other use of the connection while the transaction is active. This is enforced at compile time by Rust's borrow checker. The design leverages three Rust language features to provide safety guarantees that would be impossible (or require runtime checks) in most other languages:

#### 1. Exclusive Borrowing

`Connection::transaction(&mut self)` takes a mutable (exclusive) borrow of the
connection, and `Transaction<'conn>` holds `&'conn mut Connection`. While the
`Transaction` exists, the Rust borrow checker prevents any other code from
accessing the raw connection — not even for read-only operations. This
eliminates an entire class of bugs where application code accidentally issues
SQL statements outside the transaction scope, causing data races or logic
errors. The protection is enforced at compile time with zero runtime cost.

```rust
let mut conn = Connection::connect(endpoint, "db.hyper", CreateMode::DoNotCreate)?;
let txn = conn.transaction()?;
// conn.execute_command("SELECT 1")?;  // COMPILE ERROR: cannot borrow `conn` because
//                                      // it is already mutably borrowed by `txn`
txn.execute_command("SELECT 1")?;       // OK: use the transaction instead
txn.commit()?;
conn.execute_command("SELECT 1")?;      // OK: txn consumed, conn is free again
```

#### 2. Panic Safety (Drop during Stack Unwinding)

If the code panics (the Rust equivalent of an unhandled exception), the `Drop` implementation still runs during stack unwinding, issuing a best-effort `ROLLBACK`. This ensures the database does not remain in a locked or half-committed state even in the face of unexpected failures. The rollback error is intentionally ignored during drop since panicking inside `Drop` during unwinding would abort the process.

```rust
let txn = conn.transaction()?;
txn.execute_command("INSERT INTO accounts VALUES (1, 'Alice', 1000.0)")?;
panic!("something went wrong");
// Drop runs here automatically → issues ROLLBACK → database stays consistent
```

#### 3. Consuming `self` Prevents Misuse After Commit/Rollback

Both `commit(self)` and `rollback(self)` take ownership of `self` by value (they "consume" the guard). After calling either method, the `Transaction` value is moved and the Rust compiler prevents any further use. You cannot accidentally commit twice, rollback after commit, or execute queries on a finished transaction. These are all compile-time errors, not runtime checks.

```rust
let txn = conn.transaction()?;
txn.execute_command("INSERT INTO t VALUES (1)")?;
txn.commit()?;
// txn.commit()?;                       // COMPILE ERROR: use of moved value `txn`
// txn.rollback()?;                     // COMPILE ERROR: use of moved value `txn`
// txn.execute_command("SELECT 1")?;    // COMPILE ERROR: use of moved value `txn`
```

#### Method Reference

`Transaction<'conn>` delegates these methods:

| Method | Description |
|--------|-------------|
| `commit(self)` | Commits and consumes the guard |
| `rollback(self)` | Rolls back and consumes the guard |
| `execute_command(&self, sql)` | Executes a SQL command |
| `execute_query(&self, query)` | Returns streaming `Rowset` results |
| `fetch_one(&self, query)` | Fetches a single row |
| `fetch_optional(&self, query)` | Fetches an optional row |
| `fetch_all(&self, query)` | Fetches all rows |
| `fetch_scalar(&self, query)` | Fetches a single scalar value |
| `fetch_optional_scalar(&self, query)` | Fetches an optional scalar |
| `query_count(&self, query)` | Queries for a count (defaults to 0 if NULL) |
| `connection(&self)` | Returns `&Connection` for direct access |

**Drop behavior:** If the guard is dropped without `commit()` or `rollback()`, it issues a best-effort `ROLLBACK`. Hyper produces a WARNING (not error) if there's no active transaction, so this is safe.

### RAII Transaction Guard (Async)

```rust
use hyperdb_api::{AsyncConnection, AsyncTransaction, CreateMode};

let mut conn = AsyncConnection::connect("localhost:7483", "db.hyper", CreateMode::DoNotCreate).await?;
let txn: AsyncTransaction<'_> = conn.transaction().await?; // exclusively borrows conn
txn.execute_command("INSERT INTO t VALUES (1)").await?;
txn.commit().await?;
```

`AsyncTransaction<'conn>` exclusively borrows `&'conn mut AsyncConnection`, providing the same compile-time guarantees as the sync version: exclusive borrowing prevents raw connection use, consuming `self` prevents double-commit, and the borrow checker enforces it all at zero runtime cost.

**Important limitation (panic safety):** Rust does not support `async Drop`. Unlike the sync `Transaction` which issues a `ROLLBACK` in its `Drop` implementation, `AsyncTransaction` can only print a warning to stderr if dropped without an explicit `commit()` or `rollback()`. The server will implicitly handle the stale transaction on the next command. Always explicitly commit or rollback async transactions.

## Behavioral Notes

### Transactions

- **Error in transaction:** After a SQL error inside a transaction, the entire transaction enters an aborted state (SQLSTATE `25P02`). You must drop or rollback the guard before using the connection for anything else; the next `txn.execute_command(...)` would error.
- **DDL after DML:** Executing DDL (e.g. `CREATE TABLE`) after DML (e.g. `INSERT`) in the same transaction produces error `0A000`. DDL-only transactions work fine.
- **Nested transactions:** Hyper does not support nested transactions. Issuing `BEGIN` while a transaction is open produces a WARNING; the second BEGIN is ignored. The RAII guard's `&mut self` borrow already prevents this in safe Rust code.

## What Works

- RAII `Transaction` guard with auto-rollback on drop (sync)
- RAII `AsyncTransaction` guard (async, with warning-only drop)
- DDL inside transactions (subject to the DDL-after-DML restriction)
- Multi-table atomic rollback

## What Doesn't Work / Limitations

- **Async Drop rollback:** `AsyncTransaction` cannot issue ROLLBACK in Drop due to Rust's sync-only Drop trait. It only prints a warning. Always explicitly commit or rollback async transactions before drop.
- **Error recovery within transactions:** After a SQL error inside a transaction, the transaction is fully aborted (SQLSTATE `25P02`). You must rollback — you cannot continue executing statements.
- **`information_schema.tables`:** Does not exist in Hyper. Cannot be used to check table existence.

## Unguarded transaction control

`Connection::begin_transaction` / `commit` / `rollback` were deprecated in
v0.3.0 and **removed in 1.0.0**. Their replacements are
`begin_transaction_unguarded` / `commit_unguarded` / `rollback_unguarded` (and
the matching `AsyncConnection` versions), which are not deprecated but are also
not the path you should normally take.

Migration recipe — prefer the guard:

```rust
// Before — removed in 1.0.0
conn.begin_transaction()?;
conn.execute_command("INSERT INTO t VALUES (1, 'hello')")?;
conn.commit()?;

// After — RAII guard
let txn = conn.transaction()?;          // requires &mut conn
txn.execute_command("INSERT INTO t VALUES (1, 'hello')")?;
txn.commit()?;
```

The `&mut conn` requirement is intentional — it's the borrow-checker mechanism
that makes the safety story compile-enforced.

If your code holds the connection through a shared reference (e.g. inside an
`&self` method on a wrapper struct), the guard is not available to you at all,
and the `_unguarded` methods are the supported escape hatch:

```rust
// Only when `&mut self` is impossible. You own the pairing.
conn.begin_transaction_unguarded()?;
let result = do_work(&conn);
if result.is_ok() {
    conn.commit_unguarded()?;
} else {
    let _ = conn.rollback_unguarded();
}
```

Pairing is entirely on the caller, on **every** path including panics and
cancelled futures. An unmatched begin wedges the session: later statements fail
with "transaction already in progress" on a connection that is otherwise
healthy, so reconnect logic won't clear it. That burden is why the MCP server's
`engine.rs::execute_in_transaction` moved off these methods in
[issue #72](https://github.com/tableau/hyper-api-rust/issues/72) — it now holds
a `Transaction` and lets `Drop` discharge the obligation.

Two in-tree callers remain, and they are the reason these methods exist:
`KvStore::{pop, set_batch, set_batch_if_absent}` and their `AsyncKvStore`
counterparts. Both hold `connection: &'conn Connection` — a shared reference —
so the guard's `&mut self` is unavailable without making
`Connection::kv_store()` take `&mut self`, which would break the public API and
forbid two open stores on one connection. Both pair every begin with a commit
or a best-effort rollback, so the `Ok` and `Err` paths are covered; what is
*not* covered is a panic (sync) or a cancellation (async) landing between the
two. The async three have no remedy even in principle — no async `Drop` — so
treat them as the worked example of the hazard this section describes, not as
code awaiting a mechanical fix.

## Test Inventory

### transaction_tests.rs

Basic transaction behavior. The `test_raw_*` tests pin behavior of the deprecated raw methods until they are removed.

| Test | Description |
|------|-------------|
| `test_raw_begin_commit_methods` | **(deprecated API)** Raw `begin_transaction()` / `commit()` methods |
| `test_raw_begin_rollback_methods` | **(deprecated API)** Raw `begin_transaction()` / `rollback()` methods |
| `test_begin_commit` | BEGIN + INSERT + COMMIT via SQL strings |
| `test_begin_rollback` | BEGIN + INSERT + ROLLBACK via SQL strings |
| `test_transaction_guard_commit` | RAII guard: `txn.execute_command()` + `txn.commit()` |
| `test_transaction_guard_rollback_explicit` | RAII guard: explicit `txn.rollback()` |
| `test_transaction_guard_auto_rollback` | RAII guard: drop without commit triggers auto-rollback |
| `test_multiple_operations_in_transaction` | Multiple INSERTs + UPDATE + DELETE in one transaction |
| `test_ddl_in_transaction` | CREATE TABLE inside transaction + commit |
| `test_query_within_transaction` | SELECT within active transaction sees uncommitted data |
| `test_rollback_after_error` | Invalid SQL + rollback, verify connection still usable |
| `test_nested_begin_warning` | BEGIN inside active transaction produces warning, not error |
| `test_rollback_outside_transaction` | ROLLBACK with no active transaction produces warning |
| `test_fetch_methods_in_transaction` | `fetch_scalar`, `fetch_one`, `fetch_optional`, `fetch_all` via RAII guard |

### Running the Tests

```bash
HYPERD_PATH=/path/to/hyperd cargo test -p hyperdb-api --test transaction_tests
```
