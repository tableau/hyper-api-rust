#!/bin/bash
# Run all Rust API examples in WSL

set -e

cd "$(dirname "$0")"

export PATH="$HOME/.cargo/bin:/usr/bin:$PATH"

# Set up HYPERD_PATH if not already set
# Priority: 1) Use HYPERD_PATH if already set, 2) Check known locations
if [ -z "$HYPERD_PATH" ]; then
    # Priority: user-set HYPERD_PATH, else .hyperd/current/hyperd (the
    # repo-local downloader output). If neither is present, auto-run the
    # downloader — this script exists only to run examples.
    HYPERD_DOWNLOAD="$(pwd)/.hyperd/current/hyperd"

    if [ -f "$HYPERD_DOWNLOAD" ] && [ -x "$HYPERD_DOWNLOAD" ]; then
        HYPERD_PATH="$HYPERD_DOWNLOAD"
    else
        echo "hyperd not found; running download-hyperd first..."
        if ! cargo run --release -p hyperdb-bootstrap --bin hyperdb-bootstrap -- download; then
            echo "Auto-download of hyperd failed. Set HYPERD_PATH to an existing hyperd and retry." >&2
            exit 1
        fi
        HYPERD_PATH="$HYPERD_DOWNLOAD"
    fi
    export HYPERD_PATH
fi
export CC="/usr/bin/gcc"

echo "=== Running All Rust API Examples ==="
echo "HYPERD_PATH=$HYPERD_PATH"
echo ""

examples=(
    "insert_data_into_single_table"
    "insert_data_into_multiple_tables"
    "create_hyper_file_from_csv"
    "read_and_print_data_from_existing_hyper_file"
    "insert_data_with_expressions"
    "insert_geospatial_data_to_a_hyper_file"
    "delete_data_in_existing_hyper_file"
    "update_data_in_existing_hyper_file"
    # Consolidated examples
    "reading_data"              # Was: read_data + result_types
    "inserter"
    "threaded_inserter"
    "arrow"
    "catalog_and_schema"        # Was: catalog_operations + schema_introspection
    "multiple_databases"
    "name_types"
    "type_system"
    "geography"
    "logging"
    "notice_receiver"
    "parameterized_queries"
    "struct_mapping"
    "row_mapping_forms"         # All five FromRow mapping forms (sync + async stream_as)
    "async_usage"               # Was: async_connection + async_integration
    "sharding_cluster"
    "query_builder"             # Was: query_builder_demo + advanced_query_builder
    "future_improvements"
    "grpc_query"                # Now includes grpc_query_builder_demo content
    "transactions"
    "grpc_benchmark_tests"
    "grpc_compilation_check"
)

passed=0
failed=0
failed_list=()

for ex in "${examples[@]}"; do
    echo "----------------------------------------"
    echo "Running: $ex"
    echo "----------------------------------------"

    if cargo run --release -p hyperdb-api --example "$ex" > "/tmp/rust_ex_${ex}.log" 2>&1; then
        echo "✓ PASSED: $ex"
        passed=$((passed + 1))
    else
        echo "✗ FAILED: $ex"
        echo "Last few lines:"
        tail -5 "/tmp/rust_ex_${ex}.log" | sed 's/^/  /'
        failed=$((failed + 1))
        failed_list+=("$ex")
    fi
    echo ""
done

# Examples that require additional feature flags
# Format: "example_name:feature1,feature2"
feature_examples=(
    "sea_query:sea-query"
    "connection_pool:pool"
)

for entry in "${feature_examples[@]}"; do
    # Parse "example_name:features" format
    ex="${entry%%:*}"
    features="${entry#*:}"

    echo "----------------------------------------"
    echo "Running: $ex (features: $features)"
    echo "----------------------------------------"

    if cargo run --release -p hyperdb-api --features "$features" --example "$ex" > "/tmp/rust_ex_${ex}.log" 2>&1; then
        echo "✓ PASSED: $ex"
        passed=$((passed + 1))
    else
        echo "✗ FAILED: $ex"
        echo "Last few lines:"
        tail -5 "/tmp/rust_ex_${ex}.log" | sed 's/^/  /'
        failed=$((failed + 1))
        failed_list+=("$ex")
    fi
    echo ""
done

echo "========================================"
echo "Summary:"
echo "  Passed: $passed"
echo "  Failed: $failed"
echo ""

if [ $failed -gt 0 ]; then
    echo "Failed examples:"
    for ex in "${failed_list[@]}"; do
        echo "  - $ex"
    done
    exit 1
else
    echo "All examples passed!"
    exit 0
fi
