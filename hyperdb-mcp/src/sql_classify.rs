// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Comment- and literal-aware classification of SQL text.
//!
//! The read-only tools (`query`, `query_data`, `query_file`, `chart`,
//! `export`, ...) refuse SQL that [`is_read_only_sql`] does not accept, and
//! nothing on the server backs that up: Hyper does not enforce
//! `BEGIN READ ONLY`, and `default_transaction_read_only` cannot be set. So
//! the classifier must see the statement exactly the way Hyper does.
//!
//! The tokenizer below mirrors Hyper's lexer
//! (`hyper/parser/sql/SQLLexer.cpp`) for everything that decides where a
//! token starts and ends:
//!
//! - Block comments do **not** nest: `/* a /* b */` is one comment, and the
//!   text after the first `*/` is live SQL. An unterminated one is an error.
//! - `--` comments end at `\r` or `\n`.
//! - `'...'` doubles `''` and keeps `\` literal; `E'...'` also skips the
//!   byte after a `\`. A string followed by whitespace holding a newline
//!   (comments count as newlines) and another `'` continues the same
//!   string, in the same escape mode. (`B'`, `N'`, `X'`, `U&'` and a
//!   `UESCAPE 'c'` clause end where a word plus plain strings would, so
//!   they need no case of their own.)
//! - `"..."` doubles `""`.
//! - `$` followed by a digit is a parameter. Any other `$` opens a dollar
//!   quote whose tag is *everything* up to the next `$` (not just an
//!   identifier), closed by the next occurrence of that whole `$tag$`.
//! - Identifiers are ASCII letters, digits and `_` plus any non-ASCII
//!   character except Hyper's Unicode whitespace list.
//!
//! On top of the tokens, statements are classified by Hyper's grammar
//! (`hyper/parser/sql/sql.ypp`), measured against hyperd `0.0.26359`:
//!
//! - `WITH ... DELETE` / `INSERT` / `UPDATE` / `MERGE` run the write, so a
//!   `WITH` statement takes the kind of its main statement. A CTE cannot be
//!   named `SELECT`, `VALUES` or `TABLE` unquoted, so the first of those
//!   words at depth 0 is the main statement. A CTE body that is itself a
//!   write is refused too, although Hyper rejects writable CTEs today.
//! - `EXPLAIN (ANALYZE) DELETE ...` runs the `DELETE`. The option list
//!   accepts any (even quoted) option name, so an `EXPLAIN` with an option
//!   list takes the kind of the statement it explains. A plain `EXPLAIN`
//!   only plans.
//! - Hyper refuses more than one statement per call on both protocols, but
//!   anything after a top-level `;` is classified as [`StatementKind::Other`]
//!   anyway rather than relying on that.

/// Coarse classification of a single SQL statement.
///
/// Used by the read-only tools through [`is_read_only_sql`], and by the
/// atomic-batch `execute` tool to enforce "a batch must be either all-DDL
/// singletons or all-DML; mixing the two aborts the transaction with
/// SQLSTATE 0A000".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementKind {
    /// `SELECT` / `VALUES` / `TABLE` (without `INTO`) / `SHOW`, a
    /// parenthesized query, a
    /// `WITH` whose main statement is one of those, or an `EXPLAIN` that
    /// does not run its statement.
    ReadOnly,
    /// `CREATE` / `DROP` / `ALTER` / `TRUNCATE` / `RENAME` — Hyper auto-commits.
    Ddl,
    /// `INSERT` / `UPDATE` / `DELETE` / `COPY` / `MERGE` / `SELECT ... INTO`,
    /// including behind a `WITH` or an `EXPLAIN (ANALYZE)` — transactional.
    Dml,
    /// `BEGIN` / `START` / `COMMIT` / `END` / `ROLLBACK` / `ABORT` /
    /// `SAVEPOINT` / `RELEASE`. Rejected inside a batch because the
    /// `execute` tool already manages the transaction; an explicit
    /// COMMIT mid-batch would defeat atomicity.
    TransactionControl,
    /// Empty or comment-only input, an unrecognized first keyword, an
    /// unterminated literal or comment, unbalanced parentheses, or more
    /// than one statement. Never read-only; the batch validator passes it
    /// through to Hyper, which is the final authority on syntax.
    Other,
}

/// Returns `true` if `sql` is a single read-only statement
/// ([`StatementKind::ReadOnly`]).
///
/// Comments, string literals and quoted identifiers are tokenized the way
/// Hyper does (see the module docs), so neither `/* x */ DROP TABLE t`
/// nor `WITH x AS (SELECT 1) DELETE FROM t` nor
/// `EXPLAIN (ANALYZE) DELETE FROM t` passes.
#[must_use]
pub fn is_read_only_sql(sql: &str) -> bool {
    classify_statement(sql) == StatementKind::ReadOnly
}

/// Classify `sql`, which should hold one statement (a trailing `;` is
/// fine). See [`StatementKind`] for the kinds and the module docs for the
/// rules.
#[must_use]
pub fn classify_statement(sql: &str) -> StatementKind {
    parse(sql).map_or(StatementKind::Other, |items| classify_items(&items))
}

/// Strips leading whitespace, line comments (`--`), and block comments
/// (`/* */`) from SQL text. Block comments do not nest, as in Hyper; an
/// unterminated one leaves nothing.
pub(crate) fn strip_leading_sql_comments(sql: &str) -> &str {
    let mut s = sql;
    loop {
        s = s.trim_start();
        if let Some(rest) = s.strip_prefix("--") {
            match rest.find(['\n', '\r']) {
                Some(pos) => s = &rest[pos..],
                None => return "",
            }
        } else if let Some(rest) = s.strip_prefix("/*") {
            match rest.find("*/") {
                Some(pos) => s = &rest[pos + 2..],
                None => return "",
            }
        } else {
            return s;
        }
    }
}

/// A token, with parenthesized groups already nested.
#[derive(Debug)]
enum Item {
    /// An unquoted identifier or keyword, ASCII-uppercased.
    Word(String),
    /// The items between a `(` and its `)`.
    Group(Vec<Item>),
    Semicolon,
    /// Anything else: literals, quoted identifiers, operators, numbers.
    Other,
}

const READ_ONLY_KEYWORDS: [&str; 3] = ["SELECT", "VALUES", "TABLE"];
const DML_KEYWORDS: [&str; 5] = ["INSERT", "UPDATE", "DELETE", "MERGE", "COPY"];

/// Deepest parenthesis nesting accepted. Parsing, classifying and dropping
/// the item tree each recurse once per level, so unbounded nesting would
/// overflow the stack and abort the server; deeper input is `Other`.
const MAX_GROUP_DEPTH: usize = 256;
fn classify_items(items: &[Item]) -> StatementKind {
    let (statement, rest) = match items.iter().position(|i| matches!(i, Item::Semicolon)) {
        Some(pos) => (&items[..pos], &items[pos + 1..]),
        None => (items, &[][..]),
    };
    if !rest.is_empty() {
        return StatementKind::Other;
    }
    classify_single(statement)
}

fn classify_single(items: &[Item]) -> StatementKind {
    let rest = items.get(1..).unwrap_or_default();
    match items.first() {
        // Only a query can start with `(` (`select_with_parens`).
        Some(Item::Group(inner)) => classify_items(inner),
        Some(Item::Word(word)) => match word.as_str() {
            "SELECT" | "VALUES" | "TABLE" => classify_query(rest),
            "SHOW" => StatementKind::ReadOnly,
            "WITH" => classify_with(rest),
            "EXPLAIN" => classify_explain(rest),
            "CREATE" | "DROP" | "ALTER" | "TRUNCATE" | "RENAME" => StatementKind::Ddl,
            "INSERT" | "UPDATE" | "DELETE" | "COPY" | "MERGE" => StatementKind::Dml,
            "BEGIN" | "START" | "COMMIT" | "END" | "ROLLBACK" | "ABORT" | "SAVEPOINT"
            | "RELEASE" => StatementKind::TransactionControl,
            _ => StatementKind::Other,
        },
        _ => StatementKind::Other,
    }
}

/// The items after a query's first keyword. `INTO` is a reserved word, so
/// at depth 0 it can only be `SELECT ... INTO new_table`, which creates a
/// table (Hyper refuses it unless `enable_select_into` is set).
fn classify_query(items: &[Item]) -> StatementKind {
    if items
        .iter()
        .any(|i| matches!(i, Item::Word(w) if w == "INTO"))
    {
        StatementKind::Dml
    } else {
        StatementKind::ReadOnly
    }
}

/// The items after `WITH`: the CTE list, then the main statement.
fn classify_with(items: &[Item]) -> StatementKind {
    let mut saw_read_only_group = false;
    for (index, item) in items.iter().enumerate() {
        match item {
            Item::Word(word) if READ_ONLY_KEYWORDS.contains(&word.as_str()) => {
                return classify_query(&items[index + 1..]);
            }
            Item::Word(word) if DML_KEYWORDS.contains(&word.as_str()) => {
                return StatementKind::Dml;
            }
            // CTE bodies, column lists and options before the main
            // statement, or a parenthesized main query.
            Item::Group(inner) => match classify_items(inner) {
                StatementKind::ReadOnly => saw_read_only_group = true,
                StatementKind::Other => {}
                kind => return kind,
            },
            _ => {}
        }
    }
    if saw_read_only_group {
        StatementKind::ReadOnly
    } else {
        StatementKind::Other
    }
}

/// The items after `EXPLAIN`.
fn classify_explain(items: &[Item]) -> StatementKind {
    match items.first() {
        None => StatementKind::Other,
        // `EXPLAIN (ANALYZE) ...` executes; any option list is treated so.
        Some(Item::Group(_)) => classify_single(&items[1..]),
        // Not Hyper syntax, but PostgreSQL's executing form.
        Some(first) if is_bare_explain_option(first) => {
            let options = items
                .iter()
                .take_while(|i| is_bare_explain_option(i))
                .count();
            classify_single(&items[options..])
        }
        Some(_) => StatementKind::ReadOnly,
    }
}

fn is_bare_explain_option(item: &Item) -> bool {
    matches!(item, Item::Word(w) if matches!(w.as_str(), "ANALYZE" | "ANALYSE" | "VERBOSE"))
}

/// Tokenize `sql` and nest its parentheses. `None` on an unterminated
/// literal, quoted identifier or comment, unbalanced parentheses, or
/// nesting deeper than [`MAX_GROUP_DEPTH`].
fn parse(sql: &str) -> Option<Vec<Item>> {
    let bytes = sql.as_bytes();
    let mut stack: Vec<Vec<Item>> = vec![Vec::new()];
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        let item = match b {
            b'\t' | b'\n' | 0x0B | 0x0C | b'\r' | b' ' => {
                i += 1;
                continue;
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                i = bytes[i + 2..]
                    .iter()
                    .position(|&c| c == b'\n' || c == b'\r')
                    .map_or(bytes.len(), |pos| i + 2 + pos);
                continue;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i = i + 2 + find(&bytes[i + 2..], b"*/")? + 2;
                continue;
            }
            b'(' => {
                if stack.len() > MAX_GROUP_DEPTH {
                    return None;
                }
                stack.push(Vec::new());
                i += 1;
                continue;
            }
            b')' => {
                let group = stack.pop()?;
                stack.last_mut()?.push(Item::Group(group));
                i += 1;
                continue;
            }
            b';' => {
                i += 1;
                Item::Semicolon
            }
            b'\'' => {
                i = skip_string(bytes, i + 1, false)?;
                Item::Other
            }
            b'"' => {
                i = skip_quoted_identifier(bytes, i + 1)?;
                Item::Other
            }
            b'$' if bytes.get(i + 1).is_some_and(u8::is_ascii_digit) => {
                i += 1;
                while bytes.get(i).is_some_and(u8::is_ascii_digit) {
                    i += 1;
                }
                Item::Other
            }
            b'$' => {
                i = skip_dollar_quote(bytes, i)?;
                Item::Other
            }
            // A client command (`\x`) is one token with its letters.
            b'\\' => {
                i += 1;
                if bytes
                    .get(i)
                    .is_some_and(|&c| c.is_ascii_alphanumeric() || c == b'?')
                {
                    i += 1;
                    while bytes.get(i).is_some_and(u8::is_ascii_alphanumeric) {
                        i += 1;
                    }
                }
                Item::Other
            }
            b'e' | b'E' if bytes.get(i + 1) == Some(&b'\'') => {
                i = skip_string(bytes, i + 2, true)?;
                Item::Other
            }
            _ if b.is_ascii_alphabetic() || b == b'_' => {
                let start = i;
                i = skip_identifier(sql, i);
                Item::Word(sql[start..i].to_ascii_uppercase())
            }
            _ if b.is_ascii() => {
                i += 1;
                Item::Other
            }
            _ => {
                // Every token boundary above is an ASCII byte, so `i` is
                // on a char boundary here.
                let c = sql[i..].chars().next()?;
                if is_unicode_whitespace(c) {
                    i += c.len_utf8();
                    continue;
                }
                let start = i;
                i = skip_identifier(sql, i);
                Item::Word(sql[start..i].to_ascii_uppercase())
            }
        };
        stack.last_mut()?.push(item);
    }
    let root = stack.pop()?;
    stack.is_empty().then_some(root)
}

/// Hyper's non-ASCII whitespace (`SQLLexer::getNextSingle`).
fn is_unicode_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{A0}' | '\u{2000}'..='\u{200F}' | '\u{2028}' | '\u{2029}' | '\u{3000}' | '\u{FEFF}'
    )
}

/// End of the identifier starting at byte `i`.
fn skip_identifier(sql: &str, mut i: usize) -> usize {
    let bytes = sql.as_bytes();
    while let Some(&b) = bytes.get(i) {
        if b.is_ascii_alphanumeric() || b == b'_' {
            i += 1;
        } else if b.is_ascii() {
            break;
        } else {
            match sql[i..].chars().next() {
                Some(c) if !is_unicode_whitespace(c) => i += c.len_utf8(),
                _ => break,
            }
        }
    }
    i
}

/// Offset of the first `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// End of a string literal whose body starts at `i` (after the opening
/// `'`), including any continuation. `escapes` is `E'...'`.
fn skip_string(bytes: &[u8], mut i: usize, escapes: bool) -> Option<usize> {
    loop {
        let b = *bytes.get(i)?;
        i += 1;
        if b == b'\'' {
            if bytes.get(i) == Some(&b'\'') {
                i += 1;
            } else if let Some(next) = string_continuation(bytes, i) {
                i = next;
            } else {
                return Some(i);
            }
        } else if escapes && b == b'\\' && i < bytes.len() {
            i += 1;
        }
    }
}

/// `SQLLexer.cpp`'s `checkStringContinuation` (pedantic): if the bytes
/// from `i` are whitespace and comments holding a newline and then `'`,
/// the offset after that `'`.
fn string_continuation(bytes: &[u8], mut i: usize) -> Option<usize> {
    let mut had_newline = false;
    while let Some(&b) = bytes.get(i) {
        i += 1;
        match b {
            b' ' | b'\t' | 0x0C => {}
            b'\n' | b'\r' => had_newline = true,
            b'\'' => return had_newline.then_some(i),
            b'-' if bytes.get(i) == Some(&b'-') => {
                i += 1;
                while bytes.get(i).is_some_and(|&c| c != b'\n' && c != b'\r') {
                    i += 1;
                }
                had_newline = true;
            }
            b'/' if bytes.get(i) == Some(&b'*') => {
                i = i + 1 + find(bytes.get(i + 1..)?, b"*/")? + 2;
                had_newline = true;
            }
            _ => return None,
        }
    }
    None
}

/// End of a quoted identifier whose body starts at `i`.
fn skip_quoted_identifier(bytes: &[u8], mut i: usize) -> Option<usize> {
    loop {
        let b = *bytes.get(i)?;
        i += 1;
        if b == b'"' {
            if bytes.get(i) == Some(&b'"') {
                i += 1;
            } else {
                return Some(i);
            }
        }
    }
}

/// End of a dollar quote opening at `i` (on its first `$`).
fn skip_dollar_quote(bytes: &[u8], i: usize) -> Option<usize> {
    let tag_end = i + 1 + bytes[i + 1..].iter().position(|&c| c == b'$')?;
    let marker = &bytes[i..=tag_end];
    let body = tag_end + 1;
    Some(body + find(&bytes[body..], marker)? + marker.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_statement_recognizes_each_kind() {
        assert_eq!(classify_statement("SELECT 1"), StatementKind::ReadOnly);
        assert_eq!(
            classify_statement("with x as (..) select * from x"),
            StatementKind::ReadOnly
        );
        assert_eq!(
            classify_statement("CREATE TABLE t (i INT)"),
            StatementKind::Ddl
        );
        assert_eq!(classify_statement("drop table t"), StatementKind::Ddl);
        assert_eq!(
            classify_statement("INSERT INTO t VALUES (1)"),
            StatementKind::Dml
        );
        assert_eq!(classify_statement("update t set i = 2"), StatementKind::Dml);
        assert_eq!(classify_statement("delete from t"), StatementKind::Dml);
        assert_eq!(classify_statement(""), StatementKind::Other);
    }

    #[test]
    fn classify_statement_recognizes_transaction_control() {
        for kw in [
            "BEGIN",
            "Begin transaction",
            "START TRANSACTION",
            "COMMIT",
            "Commit work",
            "END",
            "ROLLBACK",
            "Rollback to savepoint sp1",
            "ABORT",
            "SAVEPOINT sp1",
            "RELEASE SAVEPOINT sp1",
        ] {
            assert_eq!(
                classify_statement(kw),
                StatementKind::TransactionControl,
                "expected TransactionControl for `{kw}`"
            );
        }
    }

    #[test]
    fn classify_statement_strips_comments() {
        assert_eq!(
            classify_statement("/* harmless */ DROP TABLE t"),
            StatementKind::Ddl
        );
        assert_eq!(
            classify_statement("-- pretend to be readonly\nINSERT INTO t VALUES (1)"),
            StatementKind::Dml
        );
    }

    /// The `execute` batch validator now sees the write behind a `WITH` or
    /// an `EXPLAIN (ANALYZE)`.
    #[test]
    fn classify_statement_sees_wrapped_writes_as_dml() {
        for sql in [
            "WITH x AS (SELECT 1) DELETE FROM t",
            "EXPLAIN (ANALYZE) DELETE FROM t",
            "EXPLAIN (ANALYZE) WITH x AS (SELECT 1) INSERT INTO t SELECT * FROM x",
        ] {
            assert_eq!(classify_statement(sql), StatementKind::Dml, "{sql}");
        }
        assert_eq!(
            classify_statement("EXPLAIN (ANALYZE) TRUNCATE t"),
            StatementKind::Ddl
        );
    }

    #[test]
    fn strip_leading_sql_comments_does_not_nest() {
        assert_eq!(strip_leading_sql_comments("/* a /* b */ c */"), "c */");
        assert_eq!(strip_leading_sql_comments("-- x\r\nSELECT"), "SELECT");
        assert_eq!(strip_leading_sql_comments("/* open"), "");
    }

    #[test]
    fn deep_nesting_is_other_not_a_stack_overflow() {
        let deep = format!("SELECT {}1{}", "(".repeat(100_000), ")".repeat(100_000));
        assert_eq!(classify_statement(&deep), StatementKind::Other);
        let depth = MAX_GROUP_DEPTH - 1;
        let fine = format!("SELECT {}1{}", "(".repeat(depth), ")".repeat(depth));
        assert_eq!(classify_statement(&fine), StatementKind::ReadOnly);
    }

    #[test]
    fn dollar_quote_tag_is_everything_up_to_the_next_dollar() {
        // Hyper's tag here is `a b`, so the body runs to the next `$a b$`.
        assert_eq!(
            classify_statement("SELECT $a b$ ) DELETE $a b$"),
            StatementKind::ReadOnly
        );
        assert_eq!(classify_statement("SELECT $a b$ x"), StatementKind::Other);
    }
}
