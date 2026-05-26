.PHONY: clean clean-test-files clean-doc build build-api build-release build-api-release test test-api test-release test-api-release doc examples download-hyperd verify-hyperd-pin npm-pack

# Environment variables for runtime
# HYPERD_PATH points to the Hyper server executable.
# Priority: 1) user-set HYPERD_PATH, 2) .hyperd/current/hyperd (written
# by `make download-hyperd`).
#
# If nothing is found, NEED_AUTO_DOWNLOAD is set and the hyperd-needing
# targets below depend on `download-hyperd`, which fetches the pinned
# release the first time it's needed. Subsequent runs are cache hits.
ifndef HYPERD_PATH
    HYPERD_DOWNLOAD := $(CURDIR)/.hyperd/current/hyperd

    ifeq ($(shell test -f $(HYPERD_DOWNLOAD) && echo yes),yes)
        export HYPERD_PATH := $(HYPERD_DOWNLOAD)
    else
        NEED_AUTO_DOWNLOAD := yes
        export HYPERD_PATH := $(HYPERD_DOWNLOAD)
    endif
else
    export HYPERD_PATH
endif

# When nothing was found, hyperd-needing targets auto-run `download-hyperd`
# so `make test` from a clean checkout Just Works. Targets not listed
# here (help, clean*, download-hyperd itself, verify-hyperd-pin) stay
# free of the dependency.
ifdef NEED_AUTO_DOWNLOAD
build build-api build-release build-api-release test test-api test-release test-api-release test-redirect examples doc: download-hyperd
endif

# Show help
help:
	@echo "Rust Hyper API Makefile"
	@echo ""
	@echo "Targets:"
	@echo "  build          - Build debug binaries (API + MCP)"
	@echo "  build-api      - Build debug binaries (API only, no MCP/Node)"
	@echo "  build-release  - Build release binaries (API + MCP)"
	@echo "  build-api-release - Build release binaries (API only, no MCP/Node)"
	@echo "  test           - Run tests (debug, API + MCP)"
	@echo "  test-api       - Run tests (debug, API only, no MCP/Node)"
	@echo "  test-release   - Run tests (release, API + MCP)"
	@echo "  test-api-release - Run tests (release, API only, no MCP/Node)"
	@echo "  examples       - Run all examples via run_all_examples.sh"
	@echo "  doc            - Generate documentation (only Hyper API crates)"
	@echo "  npm-pack       - Build npm packages locally (.tgz files for sharing)"
	@echo "  download-hyperd- Download hyperd into .hyperd/ (pass flags via ARGS=...)"
	@echo "  verify-hyperd-pin - HEAD each platform URL for the pinned release (network, for CI)"
	@echo "  clean          - Remove build artifacts and test files"
	@echo "  clean-test-files - Remove only test-generated files"
	@echo "  clean-doc      - Remove only documentation"
	@echo "  help           - Show this help"
	@echo ""
	@echo "Environment (auto-configured):"
	@echo "  HYPERD_PATH = $(HYPERD_PATH)"
	@echo "Set the above env var first to directly run cargo"

# Clean everything: cargo artifacts + test files + downloaded hyperd
clean: clean-test-files
	cargo clean
	@echo "Removing .hyperd/ (downloaded hyperd binary)..."
	rm -rf .hyperd
	@echo "Removing Node.js build artifacts..."
	rm -rf hyperdb-api-node/node_modules
	rm -rf hyperdb-api-node/hyperdb-api-node.*.node
	rm -rf hyperdb-api-node/examples/hyper-explorer/node_modules
	rm -rf hyperdb-api-node/examples/hyper-explorer/dist
	@echo "Removing local profiling / benchmarking scratch dirs..."
	rm -rf target-prof bench_ab logs

# Clean only test-generated files (hyper databases and logs)
clean-test-files:
	@echo "Removing test .hyper files and logs..."
	find . -name "*.hyper" -type f -delete 2>/dev/null || true
	find . -name "hyperd*.log" -type f -delete 2>/dev/null || true
	find . -name "hyperd.log" -type f -delete 2>/dev/null || true

# Clean only documentation
clean-doc:
	@echo "Removing documentation..."
	rm -rf target/doc

# Build (debug) - Hyper API library stack + MCP server
build:
	cargo build -p hyperdb-api-core -p hyperdb-api -p hyperdb-mcp

# Build (debug) - Hyper API library stack only (no MCP/Node)
build-api:
	cargo build -p hyperdb-api-core -p hyperdb-api

# Build (release) - Hyper API library stack + MCP server
build-release:
	cargo build --release -p hyperdb-api-core -p hyperdb-api -p hyperdb-mcp

# Build (release) - Hyper API library stack only (no MCP/Node)
build-api-release:
	cargo build --release -p hyperdb-api-core -p hyperdb-api

# Run tests (debug) with proper environment
test:
	@echo "Environment:"
	@echo "  HYPERD_PATH=$(HYPERD_PATH)"
	@echo ""
	cargo test -p hyperdb-api-core -p hyperdb-api -p hyperdb-mcp

# Run tests (debug) - API only (no MCP/Node)
test-api:
	@echo "Environment:"
	@echo "  HYPERD_PATH=$(HYPERD_PATH)"
	@echo ""
	cargo test -p hyperdb-api-core -p hyperdb-api

# Run tests (release) with proper environment
test-release:
	@echo "Environment:"
	@echo "  HYPERD_PATH=$(HYPERD_PATH)"
	@echo ""
	cargo test --release -p hyperdb-api-core -p hyperdb-api -p hyperdb-mcp

# Run tests (release) - API only (no MCP/Node)
test-api-release:
	@echo "Environment:"
	@echo "  HYPERD_PATH=$(HYPERD_PATH)"
	@echo ""
	cargo test --release -p hyperdb-api-core -p hyperdb-api

# Run tests with redirect feature enabled
test-redirect:
	@echo "Running tests with redirect feature enabled..."
	cargo test -p hyperdb-api-core --features redirect
	cargo test -p hyperdb-api --features redirect

# Run all examples
examples:
	./run_all_examples.sh

# Download hyperd from Tableau's Hyper C++ API release into .hyperd/current/
# Forward extra flags via ARGS, e.g. `make download-hyperd ARGS="--latest"`.
download-hyperd:
	cargo run --release -p hyperdb-bootstrap --bin hyperdb-bootstrap -- download $(ARGS)

# Network-only check: HEAD each supported platform URL for the pinned
# release. Intended for CI (nightly + on PRs touching hyperd-version.toml).
verify-hyperd-pin:
	cargo run --release -p hyperdb-bootstrap --bin hyperdb-bootstrap -- verify $(ARGS)

# Build npm packages locally (hyperdb-mcp + hyperdb-api-node with bundled hyperd).
# Produces .tgz files you can share: `npm install ./hyperdb-mcp-0.1.0.tgz`
npm-pack: build-release
	@echo "Assembling npm packages..."
	scripts/assemble-npm.sh
	@echo ""
	@echo "Package files ready. Share with:"
	@echo "  npm install ./hyperdb-mcp/npm/hyperdb-mcp-*.tgz"

# Generate documentation (only Hyper Rust API crates, no dependencies)
# All features are now always-on (no feature flags needed).
# salesforce-auth on hyperdb-api-core is the only remaining optional feature.
doc: clean-doc
	cargo doc --no-deps \
		-p hyperdb-api-core --features hyperdb-api-core/salesforce-auth \
		-p hyperdb-api \
		-p hyperdb-api-salesforce \
		-p hyperdb-mcp \
		-p sea-query-hyperdb
