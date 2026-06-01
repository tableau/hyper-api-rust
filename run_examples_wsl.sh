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

# Keep this list in sync with run_all_examples.sh — both run the same set of
# registered hyperdb-api examples (see the [[example]] targets in
# hyperdb-api/Cargo.toml). Benchmarks and the feature-gated
# compile_time_validation example are intentionally excluded.
examples=(
    # Core canonical examples (matching C++/Python APIs)
    "insert_data_into_single_table"
    "insert_data_into_multiple_tables"
    "create_hyper_file_from_csv"
    "delete_data_in_existing_hyper_file"
    "update_data_in_existing_hyper_file"
    "read_and_print_data_from_existing_hyper_file"
    "insert_data_with_expressions"
    "insert_geospatial_data_to_a_hyper_file"
    # Rust-specific value-add examples
    "arrow"
    "async_usage"
    "threaded_inserter"
    "grpc_query"
    "connection_pool"
    "transactions"
    "async_parity_smoke"
    "prepared_statements"
    "row_mapping_forms"         # All five FromRow mapping forms (sync + async stream_as)
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
