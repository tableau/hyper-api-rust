#!/bin/bash
# Script to run all Rust Hyper API examples
#
# This script runs the pure-Rust hyperdb-api examples.
# It sets up HYPERD_PATH to point to the hyperd executable.
# Make sure to build the release version of hyperd otherwise the benchmark runs
# much slower
#

# Don't exit on error - we want to continue running other examples even if one fails
set +e

cd "$(dirname "$0")"

# Set up HYPERD_PATH if not already set.
# Priority: 1) user-set HYPERD_PATH, 2) .hyperd/current/hyperd (written by
# `make download-hyperd`). If neither is present, auto-run the downloader
# — this script only exists to run the examples, so `hyperd` is always
# required.
if [ -z "$HYPERD_PATH" ]; then
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

echo "Running all Rust Hyper API examples (pure-Rust)"
echo "================================================"
echo "Environment:"
echo "  HYPERD_PATH=$HYPERD_PATH"
echo ""

# List of examples in hyperdb-api
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
    "row_mapping_forms"
)

# Examples that require additional feature flags
# Format: "example_name:feature1,feature2"
feature_examples=(
)

failed=()
passed=()

for example in "${examples[@]}"; do
    echo "----------------------------------------"
    echo "Running example: $example"
    echo "----------------------------------------"

    # Capture start time using bash's built-in SECONDS variable
    start_seconds=$SECONDS

    # Run the example with HYPERD_PATH set
    env HYPERD_PATH="$HYPERD_PATH" \
        cargo run --release -p hyperdb-api --example "$example" > /tmp/hyper_example_${example}.log 2>&1
    exit_code=$?

    # Calculate duration
    duration=$((SECONDS - start_seconds))

    if [ $exit_code -eq 0 ]; then
        echo "✓ $example passed (${duration}s)"
        passed+=("$example")
    else
        echo "✗ $example failed after ${duration}s (see /tmp/hyper_example_${example}.log)"
        # Show last few lines of error log for debugging
        echo "  Last few lines of error:"
        tail -10 /tmp/hyper_example_${example}.log | sed 's/^/    /'
        failed+=("$example")
    fi
    echo ""
done

# Run examples that require feature flags
for entry in "${feature_examples[@]}"; do
    # Parse "example_name:features" format
    example="${entry%%:*}"
    features="${entry#*:}"

    echo "----------------------------------------"
    echo "Running example: $example (features: $features)"
    echo "----------------------------------------"

    start_seconds=$SECONDS

    # Run the example with features enabled
    env HYPERD_PATH="$HYPERD_PATH" \
        cargo run --release -p hyperdb-api --features "$features" --example "$example" > /tmp/hyper_example_${example}.log 2>&1
    exit_code=$?

    duration=$((SECONDS - start_seconds))

    if [ $exit_code -eq 0 ]; then
        echo "✓ $example passed (${duration}s)"
        passed+=("$example")
    else
        echo "✗ $example failed after ${duration}s (see /tmp/hyper_example_${example}.log)"
        echo "  Last few lines of error:"
        tail -10 /tmp/hyper_example_${example}.log | sed 's/^/    /'
        failed+=("$example")
    fi
    echo ""
done

echo "================================================"
echo "Summary:"
echo "  Passed: ${#passed[@]}"
echo "  Failed: ${#failed[@]}"
echo ""

if [ ${#failed[@]} -gt 0 ]; then
    echo "Failed examples:"
    for ex in "${failed[@]}"; do
        echo "  - $ex"
    done
    exit 1
else
    echo "All examples passed!"
    exit 0
fi
