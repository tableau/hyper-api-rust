// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

#![allow(
    missing_docs,
    reason = "internal crate; not published to crates.io. Docs are contributor-facing, covered in DEVELOPMENT.md."
)]

//! Client library for Hyper database.
//!
//! This crate provides both synchronous and asynchronous PostgreSQL-wire-protocol clients
//! specifically for Hyper database servers, with support for `HyperBinary`
//! data format, gRPC transport, and Salesforce Data Cloud authentication.
//!
//! # Features
//!
//! ## Core Client Features
//! - **Dual architecture**: `AsyncClient` for async applications, `Client` for sync
//! - Thread-safe clients (can be shared between threads/tasks)
//! - Multiple authentication methods: cleartext, MD5, SCRAM-SHA-256
//! - Simple query protocol for ad-hoc queries
//! - Extended query protocol for prepared statements
//! - COPY protocol for high-performance bulk insertion
//! - Optional TLS support (rustls)
//!
//! ## Advanced Transport Features
//! - **gRPC transport**: Query-only access with Arrow IPC format
//! - **Salesforce authentication**: OAuth 2.0 and JWT Bearer Token flows
//! - **Connection pooling**: Async connection pooling via deadpool
//!
//! # Quick Start
//!
//! ## Synchronous Client
//!
//! ```no_run
//! use hyperdb_api_core::client::{Client, Config};
//!
//! fn main() -> hyperdb_api_core::client::Result<()> {
//!     let config = Config::new()
//!         .with_host("localhost")
//!         .with_port(7483)
//!         .with_database("test.hyper");
//!
//!     let client = Client::connect(&config)?;
//!
//!     let rows = client.query("SELECT 1 as value")?;
//!     for row in rows {
//!         println!("value: {:?}", row.get_i32(0));
//!     }
//!
//!     client.close()?;
//!     Ok(())
//! }
//! ```
//!
//! ## Asynchronous Client
//!
//! ```no_run
//! use hyperdb_api_core::client::{AsyncClient, Config};
//!
//! #[tokio::main]
//! async fn main() -> hyperdb_api_core::client::Result<()> {
//!     let config = Config::new()
//!         .with_host("localhost")
//!         .with_port(7483)
//!         .with_database("test.hyper");
//!
//!     let client = AsyncClient::connect(&config).await?;
//!     let rows = client.query("SELECT 1").await?;
//!     client.close().await?;
//!     Ok(())
//! }
//! ```
//!
//! ## gRPC Client
//!
//! ```no_run
//! use hyperdb_api_core::client::grpc::{GrpcClient, GrpcConfig};
//!
//! #[tokio::main]
//! async fn main() -> hyperdb_api_core::client::Result<()> {
//!     let config = GrpcConfig::new("http://localhost:7484");
//!     let mut client = GrpcClient::connect(config).await?;
//!
//!     let result = client.execute_query("SELECT 1").await?;
//!     println!("Query complete: {}", result.is_complete());
//!     Ok(())
//! }
//! ```
//!
//! ## Salesforce Authentication
//!
//! ```ignore
//! use hyperdb_api_salesforce::{SalesforceAuthConfig, AuthMode, DataCloudTokenProvider};
//! use hyperdb_api_core::client::grpc::{GrpcClient, GrpcConfig};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let auth_config = SalesforceAuthConfig::new(
//!         "https://login.salesforce.com",
//!         "your-client-id",
//!     )?.auth_mode(AuthMode::password("user@example.com", "password"));
//!
//!     let mut token_provider = DataCloudTokenProvider::new(auth_config)?;
//!     let token = token_provider.get_token().await?;
//!
//!     let grpc_config = GrpcConfig::new("https://hyper.data.salesforce.com")
//!         .header("Authorization", token.bearer_token())
//!         .header("audience", token.tenant_url_str());
//!
//!     let mut client = GrpcClient::connect(grpc_config).await?;
//!     let result = client.execute_query("SELECT 1").await?;
//!     Ok(())
//! }
//! ```
//!
//! # Authentication Methods
//!
//! ## Basic Authentication
//!
//! ```no_run
//! use hyperdb_api_core::client::Config;
//!
//! let config = Config::new()
//!     .with_host("localhost")
//!     .with_port(7483)
//!     .with_user("myuser")
//!     .with_password("mypassword")
//!     .with_database("test.hyper");
//! ```
//!
//! Supported methods:
//! - Trust (no password required)
//! - Cleartext password
//! - MD5 password hash
//! - SCRAM-SHA-256 (most secure)
//!
//! ## Salesforce Data Cloud Authentication
//!
//! Three authentication modes are supported:
//!
//! - **Password**: Username + password + client secret (OAuth password grant)
//! - **`PrivateKey`**: JWT Bearer Token Flow using RSA private key (recommended for server-to-server)
//! - **`RefreshToken`**: OAuth refresh token + client secret
//!
//! See the Salesforce authentication section above for a complete example.
//!
//! # Bulk Insertion with COPY
//!
//! ## Synchronous COPY
//!
//! ```no_run
//! use hyperdb_api_core::client::{Client, Config};
//! use hyperdb_api_core::protocol::copy;
//!
//! # fn example() -> hyperdb_api_core::client::Result<()> {
//! let config = Config::new().with_host("localhost").with_port(7483);
//! let client = Client::connect(&config)?;
//!
//! let mut writer = client.copy_in("\"my_table\"", &["col1", "col2"])?;
//!
//! // Build binary data
//! let mut buf = bytes::BytesMut::new();
//! copy::write_header(&mut buf);
//! copy::write_i32(&mut buf, 42);
//! copy::write_varbinary(&mut buf, b"hello");
//!
//! writer.send(&buf)?;
//! let rows = writer.finish()?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Asynchronous COPY
//!
//! ```no_run
//! use hyperdb_api_core::client::{AsyncClient, Config};
//! use hyperdb_api_core::protocol::copy;
//!
//! #[tokio::main]
//! async fn example() -> hyperdb_api_core::client::Result<()> {
//!     let config = Config::new().with_host("localhost").with_port(7483);
//!     let client = AsyncClient::connect(&config).await?;
//!
//!     let mut writer = client.copy_in("\"my_table\"", &["col1", "col2"]).await?;
//!
//!     // Build binary data
//!     let mut buf = bytes::BytesMut::new();
//!     copy::write_header(&mut buf);
//!     copy::write_i32(&mut buf, 42);
//!     copy::write_varbinary(&mut buf, b"hello");
//!
//!     writer.send(&buf).await?;
//!     let rows = writer.finish().await?;
//!     Ok(())
//! }
//! ```
//!
//! # gRPC Transport Details
//!
//! The gRPC transport provides read-only access to Hyper databases with the following benefits:
//!
//! - Better support for load balancing
//! - Built-in streaming for large result sets  
//! - HTTP/2 multiplexing
//! - Easier integration with service meshes
//! - Arrow IPC format for efficient data transfer
//!
//! ## gRPC Limitations
//!
//! The gRPC interface is **read-only**:
//! - Only SELECT queries are supported
//! - No INSERT, UPDATE, DELETE, or DDL operations
//! - No COPY protocol for bulk data insertion
//!
//! For write operations, use the standard TCP connection.
//!
//! ## gRPC Parameterized Queries
//!
//! ```no_run
//! use hyperdb_api_core::client::grpc::{GrpcClient, GrpcConfig, QueryParameters, ParameterStyle};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let config = GrpcConfig::new("http://localhost:7484");
//!     let mut client = GrpcClient::connect(config).await?;
//!
//!     // Dollar-numbered parameters ($1, $2, ...) - use serde_json::json! for mixed types
//!     let params = QueryParameters::json_positional(&[
//!         &serde_json::json!(42),
//!         &serde_json::json!("Alice"),
//!     ])?;
//!     let result = client.execute_query_with_params(
//!         "SELECT * FROM users WHERE id = $1 AND name = $2",
//!         params,
//!         ParameterStyle::DollarNumbered,
//!     ).await?;
//!
//!     // Named parameters using builder pattern
//!     let params = QueryParameters::json_named()
//!         .add("id", &42i64)?
//!         .add("user_name", &"Alice")?
//!         .build();
//!     let result = client.execute_query_with_params(
//!         "SELECT * FROM users WHERE id = :id AND name = :user_name",
//!         params,
//!         ParameterStyle::Named,
//!     ).await?;
//!
//!     Ok(())
//! }
//! ```
//!
//! # Feature Flags
//!
//! - **`salesforce-auth`**: Salesforce Data Cloud OAuth authentication (via `hyperdb-api-salesforce` crate)
//!
//! **Always Available (no feature flags required):**
//! - TLS support (rustls)
//! - gRPC transport with Arrow IPC format
//! - Async client (`AsyncClient`)
//!
//! # Attribution
//!
//! The `hyper-client` crate code was inspired by the design patterns and API
//! structure of the [`libpq`](https://crates.io/crates/libpq) Rust crate (MIT License).
//! While `hyper-client` does not depend on the `libpq` crate, its connection
//! management patterns served as valuable inspiration during development.
//!
//! **libpq crate:**
//! - Repository: <https://crates.io/crates/libpq>
//! - License: MIT License
//! - Note: The `libpq` crate is not a dependency of this project.

#![warn(missing_docs, rust_2018_idioms, clippy::all)]

pub mod auth;
pub mod cancel;
#[allow(
    clippy::module_inception,
    reason = "preserved submodule name after collapsing the old hyper-client crate into hyperdb-api-core/src/client/"
)]
pub mod client;
pub mod config;
pub mod connection;
pub mod endpoint;
pub mod error;
pub mod notice;
pub mod prepare;
pub mod prepared_stream;
pub mod row;
pub mod statement;
pub mod sync_stream;

// Async modules
pub mod async_client;
pub mod async_connection;
pub mod async_prepared_stream;
pub mod async_stream;
pub mod async_stream_query;

pub mod tls;

pub mod grpc;

// Re-exports - Sync client
pub use async_client::AsyncPreparedStatement;
pub use cancel::Cancellable;
pub use client::{Client, CopyInWriter, QueryStream};
pub use config::Config;
pub use endpoint::ConnectionEndpoint;
pub use error::{Error, Result};
pub use notice::{Notice, NoticeReceiver};
pub use prepare::{OwnedPreparedStatement, PreparedStatement, SqlParam};
pub use row::{BatchRow, FromBinaryValue, Row, StreamRow};
pub use statement::{Column, ColumnFormat, ParamFormat};

// Re-exports - Async client
pub use async_client::{AsyncClient, AsyncCopyInWriter, AsyncCopyInWriterOwned};
pub use async_connection::AsyncRawConnection;
pub use async_prepared_stream::AsyncPreparedQueryStream;
pub use async_stream::AsyncStream;
pub use async_stream_query::AsyncQueryStream;
pub use prepared_stream::PreparedQueryStream;
pub use sync_stream::SyncStream;

// gRPC types (always available)
pub use grpc::{GrpcClient, GrpcConfig, GrpcError, GrpcQueryResult, GrpcResultChunk};

// Re-exports of the sibling submodules, so existing `use crate::client::{protocol, types}`
// paths (from when these were separate crates) still resolve.
pub use crate::{protocol, types};
