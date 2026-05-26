# PowerShell Build Script for Rust Hyper API (Windows)
# This is the Windows PowerShell equivalent of the Makefile
#
# Usage:
#   .\build.ps1 help             - Show help
#   .\build.ps1 build            - Build debug binaries (API + MCP)
#   .\build.ps1 build-api        - Build debug binaries (API only, no MCP/Node)
#   .\build.ps1 build-release    - Build release binaries (API + MCP)
#   .\build.ps1 build-api-release - Build release binaries (API only, no MCP/Node)
#   .\build.ps1 test             - Run tests (debug, API + MCP)
#   .\build.ps1 test-api         - Run tests (debug, API only, no MCP/Node)
#   .\build.ps1 test-release     - Run tests (release, API + MCP)
#   .\build.ps1 test-api-release - Run tests (release, API only, no MCP/Node)
#   .\build.ps1 examples         - Run all examples
#   .\build.ps1 doc              - Generate documentation
#   .\build.ps1 download-hyperd  - Download hyperd into .hyperd\ (extra flags passed through)
#   .\build.ps1 verify-hyperd-pin - HEAD each platform URL for the pinned release (CI guard)
#   .\build.ps1 clean            - Remove build artifacts and test files
#   .\build.ps1 clean-test-files - Remove only test-generated files
#   .\build.ps1 clean-doc        - Remove only documentation

param(
    [Parameter(Position = 0)]
    [string]$Command = "help",
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$RemainingArgs
)

# Environment variables for runtime.
# HYPERD_PATH points to the Hyper server executable.
# Priority: 1) user-set HYPERD_PATH, 2) .hyperd/current/hyperd (written by
# `download-hyperd`).
#
# Commands that actually need hyperd auto-run `download-hyperd` when
# nothing is found (see the $NeedsHyperd gate below).
$HyperdDownload = Join-Path $PSScriptRoot ".hyperd\current\hyperd.exe"

if (-not $env:HYPERD_PATH) {
    if (Test-Path $HyperdDownload) {
        $env:HYPERD_PATH = $HyperdDownload
    } else {
        # Defer the download decision until we know which command runs.
        $env:HYPERD_PATH = $HyperdDownload
    }
}

# Crates built and tested by default. Must stay in sync with the `build` /
# `build-release` / `test` / `test-release` targets in the Makefile so
# Windows contributors building via this script get the same coverage
# (including the MCP server) as Unix contributors running `make`.
$Crates = @("hyperdb-api-core", "hyperdb-api", "hyperdb-mcp")
$ApiCrates = @("hyperdb-api-core", "hyperdb-api")

# Crates documented by the `doc` target. Wider than $Crates because the
# companion crates (hyperdb-api-salesforce, sea-query-hyperdb) ship user-facing
# API docs even though they aren't part of the core build. Kept in sync
# with the `doc` target in the Makefile.
$DocCrates = @(
    "hyperdb-api-core",
    "hyperdb-api",
    "hyperdb-api-salesforce",
    "hyperdb-mcp",
    "sea-query-hyperdb"
)

function Show-Help {
    Write-Host "Rust Hyper API Build Script (PowerShell)" -ForegroundColor Cyan
    Write-Host ""
    Write-Host "Usage: .\build.ps1 <command>"
    Write-Host ""
    Write-Host "Commands:" -ForegroundColor Yellow
    Write-Host "  build          - Build debug binaries (API + MCP)"
    Write-Host "  build-api      - Build debug binaries (API only, no MCP/Node)"
    Write-Host "  build-release  - Build release binaries (API + MCP)"
    Write-Host "  build-api-release - Build release binaries (API only, no MCP/Node)"
    Write-Host "  test           - Run tests (debug, API + MCP)"
    Write-Host "  test-api       - Run tests (debug, API only, no MCP/Node)"
    Write-Host "  test-release   - Run tests (release, API + MCP)"
    Write-Host "  test-api-release - Run tests (release, API only, no MCP/Node)"
    Write-Host "  examples       - Run all examples via run_all_examples.ps1"
    Write-Host "  doc            - Generate documentation (only Hyper API crates)"
    Write-Host "  download-hyperd- Download hyperd into .hyperd\ (flags forwarded after command)"
    Write-Host "  verify-hyperd-pin - HEAD each platform URL for the pinned release (CI guard)"
    Write-Host "  clean          - Remove build artifacts and test files"
    Write-Host "  clean-test-files - Remove only test-generated files"
    Write-Host "  clean-doc      - Remove only documentation"
    Write-Host "  help           - Show this help"
    Write-Host ""
    Write-Host "Environment (auto-configured):" -ForegroundColor Yellow
    Write-Host "  HYPERD_PATH = $env:HYPERD_PATH"
    Write-Host ""
    Write-Host "Note: Set the above env var first to directly run cargo commands"
}

function Clean-TestFiles {
    Write-Host "Removing test .hyper files and logs..." -ForegroundColor Yellow
    
    # Remove .hyper files
    Get-ChildItem -Path . -Recurse -Filter "*.hyper" -File -ErrorAction SilentlyContinue | ForEach-Object {
        Write-Host "  Removing: $($_.FullName)"
        Remove-Item $_.FullName -Force
    }
    
    # Remove hyperd log files
    Get-ChildItem -Path . -Recurse -Filter "hyperd*.log" -File -ErrorAction SilentlyContinue | ForEach-Object {
        Write-Host "  Removing: $($_.FullName)"
        Remove-Item $_.FullName -Force
    }
    
    Get-ChildItem -Path . -Recurse -Filter "hyperd.log" -File -ErrorAction SilentlyContinue | ForEach-Object {
        Write-Host "  Removing: $($_.FullName)"
        Remove-Item $_.FullName -Force
    }
    
    Write-Host "Done cleaning test files." -ForegroundColor Green
}

function Clean-Doc {
    Write-Host "Removing documentation..." -ForegroundColor Yellow
    $DocPath = Join-Path $PSScriptRoot "target\doc"
    if (Test-Path $DocPath) {
        Remove-Item $DocPath -Recurse -Force
        Write-Host "Documentation removed." -ForegroundColor Green
    } else {
        Write-Host "No documentation found to remove." -ForegroundColor Gray
    }
}

function Build-Debug {
    Write-Host "Building debug binaries..." -ForegroundColor Cyan
    $PackageArgs = $Crates | ForEach-Object { "-p", $_ }
    & cargo build @PackageArgs
    if ($LASTEXITCODE -ne 0) {
        Write-Host "Build failed!" -ForegroundColor Red
        exit $LASTEXITCODE
    }
    Write-Host "Build succeeded!" -ForegroundColor Green
}

function Build-ApiDebug {
    Write-Host "Building API debug binaries (no MCP/Node)..." -ForegroundColor Cyan
    $PackageArgs = $ApiCrates | ForEach-Object { "-p", $_ }
    & cargo build @PackageArgs
    if ($LASTEXITCODE -ne 0) {
        Write-Host "Build failed!" -ForegroundColor Red
        exit $LASTEXITCODE
    }
    Write-Host "Build succeeded!" -ForegroundColor Green
}

function Build-Release {
    Write-Host "Building release binaries..." -ForegroundColor Cyan
    $PackageArgs = $Crates | ForEach-Object { "-p", $_ }
    & cargo build --release @PackageArgs
    if ($LASTEXITCODE -ne 0) {
        Write-Host "Build failed!" -ForegroundColor Red
        exit $LASTEXITCODE
    }
    Write-Host "Build succeeded!" -ForegroundColor Green
}

function Build-ApiRelease {
    Write-Host "Building API release binaries (no MCP/Node)..." -ForegroundColor Cyan
    $PackageArgs = $ApiCrates | ForEach-Object { "-p", $_ }
    & cargo build --release @PackageArgs
    if ($LASTEXITCODE -ne 0) {
        Write-Host "Build failed!" -ForegroundColor Red
        exit $LASTEXITCODE
    }
    Write-Host "Build succeeded!" -ForegroundColor Green
}

# On Windows, rustdoc compiles each non-no_run doctest into its own tiny
# .exe in a temp directory and launches it. With cargo test's default
# parallelism (= num CPU cores), dozens of fresh executables hit Defender's
# real-time scanner at once -- some get held with an exclusive scan lock long
# enough that rustdoc's Command::spawn returns ERROR_ACCESS_DENIED (5),
# surfaced as: "Couldn't run the test: Access is denied. (os error 5) -
# maybe your tempdir is mounted with noexec?". We split the cargo invocation
# in two: (1) unit + integration tests with full parallelism, (2) doctests
# with --test-threads=1 so only one doctest exe is in flight at a time,
# letting Defender finish each scan before the next launch. The proper
# fix is a Defender exclusion on the build directory; see
# Show-DoctestRemediation for the one-liner.
#
# This block is kept ASCII-only (no em dashes, no escaped backticks inside
# strings) so that Windows PowerShell 5.1 -- which reads BOM-less .ps1 files
# as Windows-1252 -- parses every brace correctly. An earlier version with
# Unicode em dashes and ``--test-threads=1`` literals caused the parser to
# silently absorb Invoke-CargoTest's body into Show-DoctestRemediation,
# leaving Invoke-CargoTest undefined at call time.
function Show-DoctestRemediation {
    Write-Host ""
    Write-Host "Doctest failed with 'Access is denied. (os error 5)' -- Windows Defender locked" -ForegroundColor Yellow
    Write-Host "a freshly-compiled doctest binary while rustdoc tried to launch it." -ForegroundColor Yellow
    Write-Host ""
    Write-Host "Permanent fix (run once in an admin PowerShell):" -ForegroundColor Cyan
    Write-Host "  Add-MpPreference -ExclusionPath '$PSScriptRoot\target'"
    Write-Host ""
    Write-Host "This script already passes --test-threads=1 to doctests to mitigate the race." -ForegroundColor Gray
    Write-Host ""
}

function Invoke-CargoTest {
    param(
        [string[]]$ProfileArgs,
        [string[]]$PackageArgs
    )

    # 1) Unit + integration tests with default (parallel) test runner.
    & cargo test @ProfileArgs @PackageArgs --tests --lib
    if ($LASTEXITCODE -ne 0) {
        Write-Host "Tests failed!" -ForegroundColor Red
        exit $LASTEXITCODE
    }

    # 2) Doctests serialized to one launch at a time -- see the comment block
    # above Show-DoctestRemediation for the rationale.
    & cargo test @ProfileArgs @PackageArgs --doc -- --test-threads=1
    if ($LASTEXITCODE -ne 0) {
        Write-Host "Doctests failed!" -ForegroundColor Red
        Show-DoctestRemediation
        exit $LASTEXITCODE
    }
}

function Run-Tests {
    Write-Host "Environment:" -ForegroundColor Yellow
    Write-Host "  HYPERD_PATH=$env:HYPERD_PATH"
    Write-Host ""

    $PackageArgs = $Crates | ForEach-Object { "-p", $_ }
    Invoke-CargoTest -ProfileArgs @() -PackageArgs $PackageArgs
    Write-Host "Tests passed!" -ForegroundColor Green
}

function Run-TestsApi {
    Write-Host "Environment:" -ForegroundColor Yellow
    Write-Host "  HYPERD_PATH=$env:HYPERD_PATH"
    Write-Host ""

    $PackageArgs = $ApiCrates | ForEach-Object { "-p", $_ }
    Invoke-CargoTest -ProfileArgs @() -PackageArgs $PackageArgs
    Write-Host "Tests passed!" -ForegroundColor Green
}

function Run-TestsRelease {
    Write-Host "Environment:" -ForegroundColor Yellow
    Write-Host "  HYPERD_PATH=$env:HYPERD_PATH"
    Write-Host ""

    $PackageArgs = $Crates | ForEach-Object { "-p", $_ }
    Invoke-CargoTest -ProfileArgs @("--release") -PackageArgs $PackageArgs
    Write-Host "Tests passed!" -ForegroundColor Green
}

function Run-TestsApiRelease {
    Write-Host "Environment:" -ForegroundColor Yellow
    Write-Host "  HYPERD_PATH=$env:HYPERD_PATH"
    Write-Host ""

    $PackageArgs = $ApiCrates | ForEach-Object { "-p", $_ }
    Invoke-CargoTest -ProfileArgs @("--release") -PackageArgs $PackageArgs
    Write-Host "Tests passed!" -ForegroundColor Green
}

function Run-Examples {
    $ScriptPath = Join-Path $PSScriptRoot "run_all_examples.ps1"
    if (Test-Path $ScriptPath) {
        & $ScriptPath
    } else {
        Write-Host "Error: run_all_examples.ps1 not found!" -ForegroundColor Red
        exit 1
    }
}

function Download-Hyperd {
    Write-Host "Downloading hyperd..." -ForegroundColor Cyan
    $DownloadArgs = @("run", "--release", "-p", "hyperdb-bootstrap", "--bin", "hyperdb-bootstrap", "--", "download")
    if ($RemainingArgs) {
        $DownloadArgs += $RemainingArgs
    }
    & cargo @DownloadArgs
    if ($LASTEXITCODE -ne 0) {
        Write-Host "hyperd download failed!" -ForegroundColor Red
        exit $LASTEXITCODE
    }
    Write-Host "hyperd ready at: $(Join-Path $PSScriptRoot '.hyperd\current\hyperd.exe')" -ForegroundColor Green
}

function Verify-HyperdPin {
    $VerifyArgs = @("run", "--release", "-p", "hyperdb-bootstrap", "--bin", "hyperdb-bootstrap", "--", "verify")
    if ($RemainingArgs) {
        $VerifyArgs += $RemainingArgs
    }
    & cargo @VerifyArgs
    if ($LASTEXITCODE -ne 0) {
        Write-Host "Pinned release verification failed!" -ForegroundColor Red
        exit $LASTEXITCODE
    }
}

function Generate-Doc {
    Clean-Doc
    Write-Host "Generating documentation..." -ForegroundColor Cyan

    # Mirror the Makefile's `doc` target exactly: document every public
    # Hyper crate, and enable `hyperdb-api-core/salesforce-auth` so the docs
    # for that optional code path are also produced. `--no-deps` keeps
    # output scoped to this workspace.
    $DocArgs = @("doc", "--no-deps")
    foreach ($crate in $DocCrates) {
        $DocArgs += "-p"
        $DocArgs += $crate
        if ($crate -eq "hyperdb-api-core") {
            $DocArgs += "--features"
            $DocArgs += "hyperdb-api-core/salesforce-auth"
        }
    }
    & cargo @DocArgs
    if ($LASTEXITCODE -ne 0) {
        Write-Host "Documentation generation failed!" -ForegroundColor Red
        exit $LASTEXITCODE
    }
    Write-Host "Documentation generated successfully!" -ForegroundColor Green
    Write-Host "Opening documentation in browser..." -ForegroundColor Cyan
    & cargo doc --no-deps -p hyperdb-api --open
}

function Clean-All {
    Clean-TestFiles
    Write-Host "Running cargo clean..." -ForegroundColor Yellow
    & cargo clean
    $HyperdDir = Join-Path $PSScriptRoot ".hyperd"
    if (Test-Path $HyperdDir) {
        Write-Host "Removing .hyperd\ (downloaded hyperd binary)..." -ForegroundColor Yellow
        Remove-Item $HyperdDir -Recurse -Force
    }
    Write-Host "Removing Node.js build artifacts..." -ForegroundColor Yellow
    $NodeDirs = @(
        "hyperdb-api-node\node_modules",
        "hyperdb-api-node\examples\hyper-explorer\node_modules",
        "hyperdb-api-node\examples\hyper-explorer\dist"
    )
    foreach ($d in $NodeDirs) {
        $p = Join-Path $PSScriptRoot $d
        if (Test-Path $p) { Remove-Item $p -Recurse -Force }
    }
    Get-ChildItem -Path (Join-Path $PSScriptRoot "hyperdb-api-node") -Filter "hyperdb-api-node.*.node" -File -ErrorAction SilentlyContinue |
        ForEach-Object { Remove-Item $_.FullName -Force }
    Write-Host "Removing local profiling / benchmarking scratch dirs..." -ForegroundColor Yellow
    $ScratchDirs = @("target-prof", "bench_ab", "logs")
    foreach ($d in $ScratchDirs) {
        $p = Join-Path $PSScriptRoot $d
        if (Test-Path $p) { Remove-Item $p -Recurse -Force }
    }
    Write-Host "Clean complete!" -ForegroundColor Green
}

# Commands that need a working hyperd. If none is on disk at the
# configured HYPERD_PATH, run the downloader before the command itself.
$NeedsHyperd = @("build", "build-api", "build-release", "build-api-release", "test", "test-api", "test-release", "test-api-release", "examples", "doc")
if ($NeedsHyperd -contains $Command.ToLower() -and -not (Test-Path $env:HYPERD_PATH)) {
    Write-Host "hyperd not found; running download-hyperd first..." -ForegroundColor Cyan
    $BootstrapArgs = @("run", "--release", "-p", "hyperdb-bootstrap", "--bin", "hyperdb-bootstrap", "--", "download")
    & cargo @BootstrapArgs
    if ($LASTEXITCODE -ne 0) {
        Write-Host "Auto-download of hyperd failed!" -ForegroundColor Red
        exit $LASTEXITCODE
    }
}

# Main script execution
switch ($Command.ToLower()) {
    "help" {
        Show-Help
    }
    "build" {
        Build-Debug
    }
    "build-api" {
        Build-ApiDebug
    }
    "build-release" {
        Build-Release
    }
    "build-api-release" {
        Build-ApiRelease
    }
    "test" {
        Run-Tests
    }
    "test-api" {
        Run-TestsApi
    }
    "test-release" {
        Run-TestsRelease
    }
    "test-api-release" {
        Run-TestsApiRelease
    }
    "examples" {
        Run-Examples
    }
    "doc" {
        Generate-Doc
    }
    "download-hyperd" {
        Download-Hyperd
    }
    "verify-hyperd-pin" {
        Verify-HyperdPin
    }
    "clean" {
        Clean-All
    }
    "clean-test-files" {
        Clean-TestFiles
    }
    "clean-doc" {
        Clean-Doc
    }
    default {
        Write-Host "Unknown command: $Command" -ForegroundColor Red
        Write-Host ""
        Show-Help
        exit 1
    }
}
