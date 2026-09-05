.PHONY: clean clean-test-files clean-doc build build-api build-release build-api-release test test-api test-release test-api-release doc examples download-hyperd verify-hyperd-pin npm-pack check-rhel

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
build build-api build-release build-api-release test test-api test-release test-api-release examples doc: download-hyperd
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
	@echo "  check-rhel     - Build in a UBI9 container with RHEL rust-toolset, no rustup"
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

# Run all examples
examples:
	./run_all_examples.sh

# Download hyperd from the PyPI tableauhyperapi wheel into .hyperd/current/
# Forward extra flags via ARGS, e.g. `make download-hyperd ARGS="--force"`.
download-hyperd:
	cargo run --release -p hyperdb-bootstrap --bin hyperdb-bootstrap -- download $(ARGS)

# Network-only check: probe each supported platform's wheel URL for the pinned
# release and cross-check its digest against PyPI. Intended for CI (nightly +
# on PRs touching hyperd-version.toml).
verify-hyperd-pin:
	cargo run --release -p hyperdb-bootstrap --bin hyperdb-bootstrap -- verify $(ARGS)

# Local mirror of .github/workflows/rhel-compatibility.yml: check that the
# workspace builds with RHEL's system-native rust-toolset and no rustup.
#
# This is fast feedback, not the authoritative gate. CI is authoritative
# because GitHub's runners are x86_64, so the container runs native amd64 and
# therefore exercises the clang+mold linker override in .cargo/config.toml that
# is scoped to x86_64-unknown-linux-gnu. This target defaults to the host
# architecture instead, because forcing --platform linux/amd64 on Apple Silicon
# means qemu/Rosetta emulation and a workspace check slow enough that nobody
# would run it. Pass RHEL_PLATFORM to opt into that fidelity:
#
#   make check-rhel                              # native, fast
#   make check-rhel RHEL_PLATFORM=linux/amd64    # matches CI, slow under emulation
#
# protoc is fetched rather than dnf-installed: it is not in any UBI repository.
#
# The repo is mounted read-only and built in place rather than copied. The two
# developer-convenience overrides are neutralized by environment instead:
# RUSTFLAGS= replaces target.<triple>.rustflags (dropping -fuse-ld=mold), and
# CARGO_TARGET_<TRIPLE>_LINKER=cc replaces the clang linker. Note that the
# `--config target.<triple>.rustflags=[]` form does NOT work -- cargo joins
# rustflags across config sources -- whereas the env var replaces them.
# rust-toolchain.toml needs no handling: a distro cargo is not a rustup shim
# and ignores it, which the rustup assertion below makes explicit.
CONTAINER_ENGINE ?= $(shell command -v podman 2>/dev/null || command -v docker 2>/dev/null)
RHEL_IMAGE       ?= registry.access.redhat.com/ubi9/ubi:latest
PROTOC_VERSION   ?= 35.1
RHEL_PLATFORM    ?=

check-rhel:
	@test -n "$(CONTAINER_ENGINE)" || \
		{ echo "ERROR: neither podman nor docker found on PATH"; exit 1; }
	@$(CONTAINER_ENGINE) info >/dev/null 2>&1 || \
		{ echo "ERROR: container daemon not reachable (on macOS try 'colima start')"; exit 1; }
	@echo "==> RHEL rust-toolset check via $(notdir $(CONTAINER_ENGINE))$(if $(RHEL_PLATFORM), [$(RHEL_PLATFORM)], [native])"
	$(CONTAINER_ENGINE) run --rm -t \
		$(if $(RHEL_PLATFORM),--platform $(RHEL_PLATFORM),) \
		-v "$(CURDIR)":/src:ro \
		-e PROTOC_VERSION=$(PROTOC_VERSION) \
		-e CARGO_TARGET_DIR=/tmp/rhel-target \
		-e RUSTFLAGS= \
		$(RHEL_IMAGE) \
		bash -euo pipefail -c '\
			dnf install -y -q rust-toolset fontconfig-devel unzip gcc gcc-c++; \
			arch=$$(uname -m); \
			case "$$arch" in \
				x86_64)  pa=linux-x86_64;   triple=X86_64_UNKNOWN_LINUX_GNU ;; \
				aarch64) pa=linux-aarch_64; triple=AARCH64_UNKNOWN_LINUX_GNU ;; \
				*) echo "unsupported arch $$arch" >&2; exit 1 ;; \
			esac; \
			curl -fsSLO "https://github.com/protocolbuffers/protobuf/releases/download/v$$PROTOC_VERSION/protoc-$$PROTOC_VERSION-$$pa.zip"; \
			unzip -q "protoc-$$PROTOC_VERSION-$$pa.zip" -d /usr/local; \
			command -v rustup >/dev/null 2>&1 && { echo "rustup unexpectedly present" >&2; exit 1; } || true; \
			rustc --version; cargo --version; protoc --version; \
			export CARGO_TARGET_$${triple}_LINKER=cc; \
			cd /src && cargo check --workspace --locked --all-targets'

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
# RUSTDOCFLAGS="-D warnings" matches what CONTRIBUTING.md lists as a gate.
# Covers all 8 workspace members, matching the CI `doc` job so a local pass
# means a CI pass. hyperdb-api-derive, hyperdb-bootstrap and hyperdb-api-node
# were previously omitted, so their rustdoc was never checked.
doc: clean-doc
	RUSTDOCFLAGS="-D warnings" cargo doc --no-deps \
		-p hyperdb-api-core --features hyperdb-api-core/salesforce-auth \
		-p hyperdb-api \
		-p hyperdb-api-derive \
		-p hyperdb-api-node \
		-p hyperdb-api-salesforce \
		-p hyperdb-bootstrap \
		-p hyperdb-mcp \
		-p sea-query-hyperdb
