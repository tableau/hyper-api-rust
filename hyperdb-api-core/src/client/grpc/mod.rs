// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! gRPC transport for Hyper database.
//!
//! This module provides gRPC-based connectivity to Hyper servers as an
//! alternative to the `PostgreSQL` wire protocol over TCP.
//!
//! gRPC transport is always available - no feature flags required.
//!
//! # Architecture
//!
//! The gRPC transport uses Protocol Buffers for message serialization and
//! returns query results in Apache Arrow IPC format. This provides:
//!
//! - Better support for load balancing
//! - Built-in streaming for large result sets
//! - HTTP/2 multiplexing
//! - Easier integration with service meshes
//!
//! # Limitations
//!
//! The gRPC interface is **read-only**:
//! - Only SELECT queries are supported
//! - No INSERT, UPDATE, DELETE, or DDL operations
//! - No COPY protocol for bulk data insertion
//!
//! For write operations, use the standard TCP connection.
//!
//! # Message Size Limits
//!
//! By default, the client uses a 64 MB message size limit ([`DEFAULT_MAX_MESSAGE_SIZE`]).
//! This is important when using [`TransferMode::Sync`], which returns all results in
//! a single gRPC response. For [`TransferMode::Adaptive`] (default) or [`TransferMode::Async`],
//! results are streamed in smaller chunks, making large message sizes less critical.
//!
//! ```
//! use hyperdb_api_core::client::grpc::{GrpcConfig, TransferMode};
//!
//! // For SYNC mode with large results, increase the limit
//! let config = GrpcConfig::new("http://localhost:7484")
//!     .database("large_data.hyper")
//!     .transfer_mode(TransferMode::Sync)
//!     .max_message_size(256 * 1024 * 1024); // 256 MB
//! ```
//!
//! # Parameterized Queries
//!
//! gRPC supports parameterized queries using [`QueryParameters`] and [`ParameterStyle`]:
//!
//! ```no_run
//! use hyperdb_api_core::client::grpc::{GrpcClient, GrpcConfig, QueryParameters, ParameterStyle};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! # let config = GrpcConfig::new("http://localhost:7484");
//! let mut client = GrpcClient::connect(config).await?;
//!
//! // Dollar-numbered parameters ($1, $2, ...) - use serde_json::json! for mixed types
//! let params = QueryParameters::json_positional(&[
//!     &serde_json::json!(42),
//!     &serde_json::json!("Alice"),
//! ])?;
//! let result = client.execute_query_with_params(
//!     "SELECT * FROM users WHERE id = $1 AND name = $2",
//!     params,
//!     ParameterStyle::DollarNumbered,
//! ).await?;
//!
//! // Named parameters (:id, :user_name, ...). A keyword such as `name`
//! // must be quoted (`:"name"`) to be used as a parameter name.
//! let params = QueryParameters::json_named()
//!     .add("id", &42i64)?
//!     .add("user_name", &"Alice")?
//!     .build();
//! let result = client.execute_query_with_params(
//!     "SELECT * FROM users WHERE id = :id AND name = :user_name",
//!     params,
//!     ParameterStyle::Named,
//! ).await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Example
//!
//! ```no_run
//! use hyperdb_api_core::client::grpc::{GrpcClient, GrpcConfig};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let config = GrpcConfig::new("http://localhost:7484")
//!         .database("my_database.hyper");
//!
//!     let mut client = GrpcClient::connect(config).await?;
//!
//!     // Execute a query and get Arrow IPC bytes
//!     let arrow_data = client.execute_query_to_arrow("SELECT * FROM users").await?;
//!
//!     // Or get parsed results
//!     let result = client.execute_query("SELECT id, name FROM users").await?;
//!     // Process Arrow data...
//!
//!     Ok(())
//! }
//! ```

mod client;
mod config;
mod error;
mod executor;
mod params;
mod proto;
mod result;

#[cfg(feature = "salesforce-auth")]
mod authenticated_client;

pub use client::{GrpcChunkStreamSync, GrpcClient, GrpcClientSync};
pub use config::{DEFAULT_MAX_MESSAGE_SIZE, GrpcConfig};
pub use error::GrpcError;
pub use executor::GrpcChunkStream;
pub use params::{JsonNamedParamsBuilder, ParameterStyle, QueryParameters};
pub use result::{GrpcColumnInfo, GrpcQueryResult, GrpcResultChunk};

#[cfg(feature = "salesforce-auth")]
pub use authenticated_client::{AuthenticatedGrpcClient, AuthenticatedGrpcClientSync, TableInfo};

// Re-export transfer mode for users who want to specify it
pub use config::TransferMode;

// Re-export output format for advanced users
pub use proto::hyper_service::OutputFormat;
