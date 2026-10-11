// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Build script for hyperdb-api-core.
//!
//! This generates Rust code from the `HyperService` protobuf definitions.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Path to proto files (relative to this crate's root)
    let proto_root = "protos";

    // Compile the HyperService and ErrorInfo protos.
    //
    // `.bytes(".")` tells prost to generate `bytes::Bytes` for every `bytes`
    // field in the protos (instead of the default `Vec<u8>`). This lets prost
    // decode directly from the HTTP/2 frame buffer with a refcount bump
    // instead of an allocation + copy, which is critical on the gRPC result
    // path where payloads are full Arrow IPC streams.
    // Proto compilation uses `tonic-prost-build`.
    let mut config = tonic_prost_build::configure()
        .build_server(false) // We only need the client
        .build_client(true)
        .bytes(".");

    // Suppress warnings for generated types that may not be used
    // These types are part of the proto definition but may not be used in all scenarios
    // Both types are in the same package: salesforce.hyperdb.grpc.v1
    config = config.type_attribute(
        "salesforce.hyperdb.grpc.v1.TextPosition",
        "#[allow(dead_code)]",
    );
    config = config.type_attribute(
        "salesforce.hyperdb.grpc.v1.ErrorInfo",
        "#[allow(dead_code)]",
    );

    config.compile_protos(
        &[
            format!("{proto_root}/salesforce/hyperdb/grpc/v1/hyper_service.proto"),
            format!("{proto_root}/salesforce/hyperdb/grpc/v1/error_details.proto"),
        ],
        &[proto_root.to_string()],
    )?;

    Ok(())
}
