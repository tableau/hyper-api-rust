// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! CSV/text export and import via COPY protocol.
//!
//! This module provides ergonomic APIs for:
//! - **Exporting** query results or tables as CSV/TSV to files, writers, or strings
//! - **Importing** CSV/TSV data from files, readers, or strings into tables
//! - **Streaming export** for large datasets without buffering all data in memory
//!
//! # CSV Export
//!
//! ```no_run
//! use hyperdb_api::{Connection, CreateMode, Result};
//! use hyperdb_api::copy::CopyOptions;
//!
//! fn main() -> Result<()> {
//!     let conn = Connection::connect("localhost:7483", "test.hyper", CreateMode::DoNotCreate)?;
//!
//!     // Export to a file
//!     conn.export_csv("SELECT * FROM users", &mut std::fs::File::create("users.csv")?)?;
//!
//!     // Export with custom options
//!     let opts = CopyOptions::csv().with_header(true).with_delimiter(b'\t');
//!     conn.export_text("SELECT * FROM users", &opts, &mut std::io::stdout())?;
//!
//!     Ok(())
//! }
//! ```
//!
//! # CSV Import
//!
//! ```no_run
//! use hyperdb_api::{Connection, CreateMode, Result};
//!
//! fn main() -> Result<()> {
//!     let conn = Connection::connect("localhost:7483", "test.hyper", CreateMode::DoNotCreate)?;
//!
//!     // Import from a file
//!     let csv_data = b"1,Alice\n2,Bob\n";
//!     let rows = conn.import_csv("my_table", &csv_data[..])?;
//!     println!("Imported {} rows", rows);
//!
//!     Ok(())
//! }
//! ```

use crate::connection::Connection;
use crate::error::{Error, Result};

/// Default import chunk size (1 MB).
const DEFAULT_IMPORT_CHUNK_SIZE: usize = 1024 * 1024;

/// Options for COPY text format operations (CSV, TSV, etc.).
///
/// # Example
///
/// ```
/// use hyperdb_api::copy::CopyOptions;
///
/// // CSV with header
/// let opts = CopyOptions::csv().with_header(true);
///
/// // TSV (tab-separated)
/// let opts = CopyOptions::tsv();
///
/// // Custom delimiter
/// let opts = CopyOptions::csv().with_delimiter(b'|');
/// ```
#[derive(Debug, Clone)]
pub struct CopyOptions {
    format: CopyFormat,
    header: bool,
    delimiter: Option<u8>,
    null_string: Option<String>,
    quote: Option<u8>,
    escape: Option<u8>,
    /// Import chunk size in bytes. `None` means use the default (1 MB).
    chunk_size: Option<usize>,
}

/// COPY format type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CopyFormat {
    Csv,
    Text,
}

impl CopyOptions {
    /// Creates CSV format options (comma-separated, no header by default).
    #[must_use]
    pub fn csv() -> Self {
        CopyOptions {
            format: CopyFormat::Csv,
            header: false,
            delimiter: None,
            null_string: None,
            quote: None,
            escape: None,
            chunk_size: None,
        }
    }

    /// Creates TSV format options (tab-separated).
    #[must_use]
    pub fn tsv() -> Self {
        CopyOptions {
            format: CopyFormat::Text,
            header: false,
            delimiter: Some(b'\t'),
            null_string: None,
            quote: None,
            escape: None,
            chunk_size: None,
        }
    }

    /// Creates plain text format options (tab-separated, Hyper default).
    #[must_use]
    pub fn text() -> Self {
        CopyOptions {
            format: CopyFormat::Text,
            header: false,
            delimiter: None,
            null_string: None,
            quote: None,
            escape: None,
            chunk_size: None,
        }
    }

    /// Enables or disables the header row.
    #[must_use]
    pub fn with_header(mut self, header: bool) -> Self {
        self.header = header;
        self
    }

    /// Sets the column delimiter character.
    #[must_use]
    pub fn with_delimiter(mut self, delimiter: u8) -> Self {
        self.delimiter = Some(delimiter);
        self
    }

    #[must_use]
    /// Sets the string used to represent NULL values.
    pub fn with_null(mut self, null_string: impl Into<String>) -> Self {
        self.null_string = Some(null_string.into());
        self
    }

    /// Sets the quote character for CSV format.
    #[must_use]
    pub fn with_quote(mut self, quote: u8) -> Self {
        self.quote = Some(quote);
        self
    }

    /// Sets the escape character for CSV format.
    #[must_use]
    pub fn with_escape(mut self, escape: u8) -> Self {
        self.escape = Some(escape);
        self
    }

    /// Sets the import chunk size in bytes.
    ///
    /// Controls the buffer size used when streaming data from a reader
    /// during [`import_text()`](Connection::import_text). Larger values
    /// reduce syscall overhead but use more memory.
    ///
    /// Default is 1 MB. Typical range: 64 KB to 16 MB.
    ///
    /// # Panics
    ///
    /// Panics if `size` is 0.
    #[must_use]
    pub fn with_chunk_size(mut self, size: usize) -> Self {
        assert!(size > 0, "chunk size must be > 0");
        self.chunk_size = Some(size);
        self
    }

    /// Validates that the option combination is legal.
    ///
    /// Catches invalid combinations early (e.g., CSV-only options on TEXT
    /// format) instead of letting them surface as opaque server errors.
    fn validate(&self) -> Result<()> {
        if self.format == CopyFormat::Text {
            if self.quote.is_some() {
                return Err(Error::config(
                    "QUOTE option is only supported with CSV format. \
                     Use CopyOptions::csv() instead of CopyOptions::text().",
                ));
            }
            if self.escape.is_some() {
                return Err(Error::config(
                    "ESCAPE option is only supported with CSV format. \
                     Use CopyOptions::csv() instead of CopyOptions::text().",
                ));
            }
        }
        Ok(())
    }

    /// Builds the WITH clause for COPY TO STDOUT.
    fn to_copy_out_options(&self) -> String {
        let mut parts = Vec::new();
        match self.format {
            CopyFormat::Csv => parts.push("FORMAT csv".to_string()),
            CopyFormat::Text => parts.push("FORMAT text".to_string()),
        }
        if self.header {
            parts.push("HEADER true".to_string());
        }
        if let Some(d) = self.delimiter {
            parts.push(format!("DELIMITER E'\\x{d:02x}'"));
        }
        if let Some(ref n) = self.null_string {
            parts.push(format!("NULL '{}'", n.replace('\'', "''")));
        }
        if let Some(q) = self.quote {
            parts.push(format!("QUOTE E'\\x{q:02x}'"));
        }
        if let Some(e) = self.escape {
            parts.push(format!("ESCAPE E'\\x{e:02x}'"));
        }
        format!("WITH ({})", parts.join(", "))
    }

    /// Builds the WITH clause for COPY FROM STDIN.
    fn to_copy_in_options(&self) -> String {
        self.to_copy_out_options()
    }
}

impl Connection {
    /// Exports query results as CSV to a writer.
    ///
    /// Uses default CSV options (comma-separated, with header row).
    /// For custom options, use [`export_text`](Self::export_text).
    ///
    /// This method streams data directly to the writer without buffering
    /// the entire result set in memory, making it safe for large exports.
    ///
    /// Returns the number of bytes written.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api::{Connection, CreateMode, Result};
    ///
    /// fn main() -> Result<()> {
    ///     let conn = Connection::connect("localhost:7483", "test.hyper", CreateMode::DoNotCreate)?;
    ///
    ///     // Export to file
    ///     let mut file = std::fs::File::create("output.csv")?;
    ///     conn.export_csv("SELECT * FROM users", &mut file)?;
    ///
    ///     // Export to string
    ///     let mut buf = Vec::new();
    ///     conn.export_csv("SELECT * FROM users", &mut buf)?;
    ///     let csv_string = String::from_utf8(buf).unwrap();
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// - Returns [`Error::FeatureNotSupported`] if the connection is using gRPC transport
    ///   (COPY is TCP-only).
    /// - Returns [`Error::Server`] if the server rejects the
    ///   `COPY (<select_query>) TO STDOUT` statement.
    /// - Returns [`Error::Io`] if writing to `writer` fails.
    pub fn export_csv(&self, select_query: &str, writer: &mut dyn std::io::Write) -> Result<u64> {
        let opts = CopyOptions::csv().with_header(true);
        self.export_text(select_query, &opts, writer)
    }

    /// Exports query results as text (CSV/TSV/custom) to a writer.
    ///
    /// This method streams data directly to the writer without buffering
    /// the entire result set in memory.
    ///
    /// Returns the number of bytes written.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api::{Connection, CreateMode, Result};
    /// use hyperdb_api::copy::CopyOptions;
    ///
    /// fn main() -> Result<()> {
    ///     let conn = Connection::connect("localhost:7483", "test.hyper", CreateMode::DoNotCreate)?;
    ///
    ///     // TSV export
    ///     let opts = CopyOptions::tsv().with_header(true);
    ///     let mut buf = Vec::new();
    ///     conn.export_text("SELECT * FROM users", &opts, &mut buf)?;
    ///
    ///     // Pipe-separated with custom NULL
    ///     let opts = CopyOptions::csv()
    ///         .with_delimiter(b'|')
    ///         .with_null("\\N".to_string())
    ///         .with_header(true);
    ///     conn.export_text("SELECT * FROM users", &opts, &mut std::io::stdout())?;
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// - Returns [`Error::Config`] if `options` fail validation (e.g. an
    ///   illegal delimiter/quote combination), or
    ///   [`Error::FeatureNotSupported`] if the connection is on gRPC.
    /// - Returns [`Error::Server`] if the server rejects the
    ///   `COPY TO STDOUT` statement.
    /// - Returns [`Error::Io`] if writing to `writer` fails.
    pub fn export_text(
        &self,
        select_query: &str,
        options: &CopyOptions,
        writer: &mut dyn std::io::Write,
    ) -> Result<u64> {
        options.validate()?;
        let copy_query = format!(
            "COPY ({}) TO STDOUT {}",
            select_query,
            options.to_copy_out_options()
        );
        let client = self.tcp_client().ok_or_else(|| {
            Error::feature_not_supported(
                "CSV export requires a TCP connection. gRPC does not support COPY operations.",
            )
        })?;
        Ok(client.copy_out_to_writer(&copy_query, writer)?)
    }

    /// Exports query results as CSV to a String.
    ///
    /// Convenience method that collects the CSV output into a String.
    /// For large datasets, prefer [`export_csv`](Self::export_csv) with a file writer.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api::{Connection, CreateMode, Result};
    ///
    /// fn main() -> Result<()> {
    ///     let conn = Connection::connect("localhost:7483", "test.hyper", CreateMode::DoNotCreate)?;
    ///     let csv = conn.export_csv_string("SELECT id, name FROM users")?;
    ///     println!("{}", csv);
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// - Returns whatever [`export_csv`](Self::export_csv) returns.
    /// - Returns [`Error::Conversion`] with message
    ///   `"CSV output is not valid UTF-8"` if the server emitted bytes that
    ///   are not valid UTF-8 (a Hyper server only emits UTF-8, so this
    ///   indicates a non-UTF-8 `CLIENT_ENCODING` setting).
    pub fn export_csv_string(&self, select_query: &str) -> Result<String> {
        let mut buf = Vec::new();
        self.export_csv(select_query, &mut buf)?;
        String::from_utf8(buf)
            .map_err(|e| Error::conversion(format!("CSV output is not valid UTF-8: {e}")))
    }

    /// Imports CSV data from a reader into a table.
    ///
    /// Uses default CSV options (comma-separated, no header).
    /// For custom options, use [`import_text`](Self::import_text).
    ///
    /// Returns the number of rows imported.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api::{Connection, CreateMode, Result};
    ///
    /// fn main() -> Result<()> {
    ///     let conn = Connection::connect("localhost:7483", "test.hyper", CreateMode::DoNotCreate)?;
    ///
    ///     // Import from a string
    ///     let csv = "1,Alice\n2,Bob\n";
    ///     let rows = conn.import_csv("users", csv.as_bytes())?;
    ///
    ///     // Import from a file
    ///     let file = std::fs::File::open("data.csv")?;
    ///     let rows = conn.import_csv("users", file)?;
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// See [`import_text`](Self::import_text).
    pub fn import_csv(&self, table_name: &str, reader: impl std::io::Read) -> Result<u64> {
        let opts = CopyOptions::csv();
        self.import_text(table_name, &opts, reader)
    }

    /// Imports CSV data with header from a reader into a table.
    ///
    /// The first line is treated as a header row and skipped.
    ///
    /// Returns the number of rows imported.
    ///
    /// # Errors
    ///
    /// See [`import_text`](Self::import_text).
    pub fn import_csv_with_header(
        &self,
        table_name: &str,
        reader: impl std::io::Read,
    ) -> Result<u64> {
        let opts = CopyOptions::csv().with_header(true);
        self.import_text(table_name, &opts, reader)
    }

    /// Imports text-format data (CSV/TSV/custom) from a reader into a table.
    ///
    /// Streams data in chunks to the server via COPY FROM STDIN, keeping
    /// memory usage constant regardless of input size.
    ///
    /// Returns the number of rows imported.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api::{Connection, CreateMode, Result};
    /// use hyperdb_api::copy::CopyOptions;
    ///
    /// fn main() -> Result<()> {
    ///     let conn = Connection::connect("localhost:7483", "test.hyper", CreateMode::DoNotCreate)?;
    ///
    ///     // Import TSV data
    ///     let opts = CopyOptions::tsv();
    ///     let tsv = "1\tAlice\n2\tBob\n";
    ///     let rows = conn.import_text("users", &opts, tsv.as_bytes())?;
    ///
    ///     // Import pipe-delimited with header
    ///     let opts = CopyOptions::csv().with_delimiter(b'|').with_header(true);
    ///     let data = "id|name\n1|Alice\n2|Bob\n";
    ///     let rows = conn.import_text("users", &opts, data.as_bytes())?;
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// - Returns [`Error::Config`] if `options` fail validation, or
    ///   [`Error::FeatureNotSupported`] if the connection is on gRPC.
    /// - Returns [`Error::Server`] if the server rejects the
    ///   `COPY <table> FROM STDIN` statement or a row during import.
    /// - Returns [`Error::Io`] if reading from `reader` fails.
    pub fn import_text(
        &self,
        table_name: &str,
        options: &CopyOptions,
        mut reader: impl std::io::Read,
    ) -> Result<u64> {
        options.validate()?;
        let escaped_table = table_name.replace('"', "\"\"");
        let copy_query = format!(
            "COPY \"{}\" FROM STDIN {}",
            escaped_table,
            options.to_copy_in_options()
        );

        let client = self.tcp_client().ok_or_else(|| {
            Error::feature_not_supported(
                "CSV import requires a TCP connection. gRPC does not support COPY operations.",
            )
        })?;

        let mut writer = client.copy_in_raw(&copy_query)?;

        // Stream data in chunks (default 1 MB, configurable via with_chunk_size)
        let chunk_size = options.chunk_size.unwrap_or(DEFAULT_IMPORT_CHUNK_SIZE);
        let mut buf = vec![0u8; chunk_size];
        loop {
            let n = reader
                .read(&mut buf)
                .map_err(|e| Error::connection_with_io("Failed to read import data", e))?;
            if n == 0 {
                break;
            }
            writer.send(&buf[..n])?;
        }

        Ok(writer.finish()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_csv_options_valid() {
        let opts = CopyOptions::csv().with_quote(b'"').with_escape(b'\\');
        assert!(opts.validate().is_ok());
    }

    #[test]
    fn test_text_quote_rejected() {
        let opts = CopyOptions::text().with_quote(b'"');
        let err = opts.validate().unwrap_err();
        assert!(err.to_string().contains("QUOTE"));
        assert!(err.to_string().contains("CSV format"));
    }

    #[test]
    fn test_text_escape_rejected() {
        let opts = CopyOptions::text().with_escape(b'\\');
        let err = opts.validate().unwrap_err();
        assert!(err.to_string().contains("ESCAPE"));
        assert!(err.to_string().contains("CSV format"));
    }

    #[test]
    fn test_tsv_quote_rejected() {
        let opts = CopyOptions::tsv().with_quote(b'"');
        let err = opts.validate().unwrap_err();
        assert!(err.to_string().contains("QUOTE"));
    }

    #[test]
    fn test_text_without_csv_options_valid() {
        let opts = CopyOptions::text().with_header(true).with_delimiter(b'|');
        assert!(opts.validate().is_ok());
    }

    #[test]
    fn test_chunk_size_custom() {
        let opts = CopyOptions::csv().with_chunk_size(4 * 1024 * 1024);
        assert_eq!(opts.chunk_size, Some(4 * 1024 * 1024));
    }

    #[test]
    fn test_chunk_size_default() {
        let opts = CopyOptions::csv();
        assert_eq!(
            opts.chunk_size.unwrap_or(DEFAULT_IMPORT_CHUNK_SIZE),
            1024 * 1024
        );
    }

    #[test]
    #[should_panic(expected = "chunk size must be > 0")]
    fn test_chunk_size_zero_panics() {
        let _ = CopyOptions::csv().with_chunk_size(0);
    }

    #[test]
    fn test_copy_in_options_csv() {
        let opts = CopyOptions::csv().with_header(true).with_delimiter(b'|');
        let sql = opts.to_copy_in_options();
        assert!(sql.contains("FORMAT csv"));
        assert!(sql.contains("HEADER true"));
        assert!(sql.contains("DELIMITER"));
    }

    #[test]
    fn test_copy_in_options_text() {
        let opts = CopyOptions::text();
        let sql = opts.to_copy_in_options();
        assert!(sql.contains("FORMAT text"));
    }
}
