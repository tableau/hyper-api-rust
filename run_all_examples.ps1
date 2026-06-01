# PowerShell Script to run all Rust Hyper API examples
#
# This script runs the pure-Rust hyperdb-api examples.
# It sets up HYPERD_PATH to point to the hyperd executable.
# Make sure to use the release version of hyperd otherwise the benchmark runs
# much slower.
#

# Error action preference - continue on errors
$ErrorActionPreference = "Continue"

# Change to script directory
Set-Location $PSScriptRoot

# Set up HYPERD_PATH if not already set
# Priority: 1) Use HYPERD_PATH if already set, 2) Check known locations
if (-not $env:HYPERD_PATH) {
    $HyperdDownload = Join-Path $PSScriptRoot ".hyperd\current\hyperd.exe"

    if (Test-Path $HyperdDownload) {
        $env:HYPERD_PATH = $HyperdDownload
    } else {
        Write-Host "hyperd not found; running download-hyperd first..." -ForegroundColor Cyan
        & cargo run --release -p hyperdb-bootstrap --bin hyperdb-bootstrap -- download
        if ($LASTEXITCODE -ne 0) {
            Write-Host "Auto-download of hyperd failed. Set HYPERD_PATH to an existing hyperd and retry." -ForegroundColor Red
            exit $LASTEXITCODE
        }
        $env:HYPERD_PATH = $HyperdDownload
    }
}

Write-Host "Running all Rust Hyper API examples (pure-Rust)" -ForegroundColor Cyan
Write-Host "================================================" -ForegroundColor Cyan
Write-Host "Environment:"
Write-Host "  HYPERD_PATH=$env:HYPERD_PATH"
Write-Host ""

# List of examples in hyperdb-api
# These match the [[example]] targets defined in hyperdb-api/Cargo.toml
$examples = @(
    # Core canonical examples (matching C++/Python APIs)
    "insert_data_into_single_table",
    "insert_data_into_multiple_tables",
    "create_hyper_file_from_csv",
    "delete_data_in_existing_hyper_file",
    "update_data_in_existing_hyper_file",
    "read_and_print_data_from_existing_hyper_file",
    "insert_data_with_expressions",
    "insert_geospatial_data_to_a_hyper_file",
    # Rust-specific value-add examples
    "arrow",
    "async_usage",
    "threaded_inserter",
    "grpc_query",
    "connection_pool",
    "transactions",
    "async_parity_smoke",
    "prepared_statements",
    "row_mapping_forms"
)

# No feature-gated examples — hyperdb-api has zero feature flags.
# All capabilities (TLS, pooling, transactions, gRPC) are always available.
$featureExamples = @()

$failed = @()
$passed = @()

# Create temp directory for logs
$TempDir = $env:TEMP
if (-not $TempDir) {
    $TempDir = "C:\Temp"
}

foreach ($example in $examples) {
    Write-Host "----------------------------------------" -ForegroundColor Gray
    Write-Host "Running example: $example" -ForegroundColor Yellow
    Write-Host "----------------------------------------" -ForegroundColor Gray

    # Capture start time
    $startTime = Get-Date

    # Log file path
    $logFile = Join-Path $TempDir "hyper_example_$example.log"

    # Run the example with HYPERD_PATH set
    $env:HYPERD_PATH = $env:HYPERD_PATH
    & cargo run --release -p hyperdb-api --example $example > $logFile 2>&1
    $exitCode = $LASTEXITCODE

    # Calculate duration
    $endTime = Get-Date
    $duration = ($endTime - $startTime).TotalSeconds

    if ($exitCode -eq 0) {
        Write-Host "[PASS] $example passed ($([math]::Round($duration, 2))s)" -ForegroundColor Green
        $passed += $example
    } else {
        Write-Host "[FAIL] $example failed after $([math]::Round($duration, 2))s (see $logFile)" -ForegroundColor Red
        # Show last few lines of error log for debugging
        Write-Host "  Last few lines of error:" -ForegroundColor Yellow
        if (Test-Path $logFile) {
            Get-Content $logFile -Tail 10 | ForEach-Object {
                Write-Host "    $_" -ForegroundColor Gray
            }
        }
        $failed += $example
    }
    Write-Host ""
}

# Run examples that require feature flags
foreach ($entry in $featureExamples) {
    $example = $entry.Example
    $features = $entry.Features

    Write-Host "----------------------------------------" -ForegroundColor Gray
    Write-Host "Running example: $example (features: $features)" -ForegroundColor Yellow
    Write-Host "----------------------------------------" -ForegroundColor Gray

    $startTime = Get-Date
    $logFile = Join-Path $TempDir "hyper_example_$example.log"

    # Run the example with features enabled
    & cargo run --release -p hyperdb-api --features $features --example $example > $logFile 2>&1
    $exitCode = $LASTEXITCODE

    $endTime = Get-Date
    $duration = ($endTime - $startTime).TotalSeconds

    if ($exitCode -eq 0) {
        Write-Host "[PASS] $example passed ($([math]::Round($duration, 2))s)" -ForegroundColor Green
        $passed += $example
    } else {
        Write-Host "[FAIL] $example failed after $([math]::Round($duration, 2))s (see $logFile)" -ForegroundColor Red
        Write-Host "  Last few lines of error:" -ForegroundColor Yellow
        if (Test-Path $logFile) {
            Get-Content $logFile -Tail 10 | ForEach-Object {
                Write-Host "    $_" -ForegroundColor Gray
            }
        }
        $failed += $example
    }
    Write-Host ""
}

Write-Host "================================================" -ForegroundColor Cyan
Write-Host "Summary:" -ForegroundColor Cyan
Write-Host "  Passed: $($passed.Count)" -ForegroundColor Green
Write-Host "  Failed: $($failed.Count)" -ForegroundColor Red
Write-Host ""

if ($failed.Count -gt 0) {
    Write-Host "Failed examples:" -ForegroundColor Red
    foreach ($ex in $failed) {
        Write-Host "  - $ex" -ForegroundColor Red
    }
    exit 1
} else {
    Write-Host "All examples passed!" -ForegroundColor Green
    exit 0
}
