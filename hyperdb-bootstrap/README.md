# hyperdb-bootstrap

Download and install the `hyperd` executable from Tableau's Hyper Java API
release packages. Ships as both a CLI binary and a library.

The `hyperd` server isn't on crates.io — it's a prebuilt binary distributed
inside Tableau's Hyper API zips at
<https://tableau.github.io/hyper-db/docs/releases>. This crate automates
the "download the right zip for your platform, extract `hyperd` out of
`lib/hyper/`, put it somewhere useful" step so contributors and CI can
bootstrap with a single command.

> **Why the Java bundle, not C++?** Tableau publishes `hyperd` inside both
> the C++ and Java API zips. The C++ `macos-arm64` zip currently ships an
> **x86_64** `hyperd` (an upstream packaging defect), so on Apple Silicon it
> would only run under Rosetta. The Java `macos-arm64` zip carries a native
> arm64 `hyperd`. The two bundles are otherwise identical for our purposes
> (same URL template, same `lib/hyper/hyperd` layout), so this crate pulls
> from the Java bundle on every platform for consistency.

## Install

```bash
cargo install hyperdb-bootstrap
```

## CLI

```bash
# Install the pinned release into ./.hyperd/current/hyperd
hyperdb-bootstrap download

# Install into a custom location
hyperdb-bootstrap download --dest /opt/hyperd

# Force a re-download even if the version is already cached
hyperdb-bootstrap download --force

# Scrape the latest release off the public releases page (best-effort,
# skips sha256 verification).
hyperdb-bootstrap download --latest

# Install a specific release by version + build id
hyperdb-bootstrap download --version 0.0.24457 --build-id rc36858b6

# Use an external pinned-version TOML instead of the baked-in default
hyperdb-bootstrap download --version-file ./my-hyperd.toml

# HEAD each supported platform's URL for the pinned release — used by CI
# to catch Tableau yanks/renames early. Exits non-zero on any failure.
hyperdb-bootstrap verify

# Print the installed binary's path
hyperdb-bootstrap which

# Print the pinned release metadata
hyperdb-bootstrap version
```

**Version-source precedence (highest → lowest):**

1. `--version X --build-id Y`
2. `--latest`
3. `--version-file PATH`
4. `./hyperd-version.toml` (auto-discovered in current dir)
5. Compiled-in default shipped with this crate

## Library

```rust
use hyperd_bootstrap::{install, InstallOptions, VersionSource};

let installed = install(InstallOptions {
    dest_root: "/opt/hyperd".into(),
    version_source: VersionSource::Builtin,
    platform: None, // auto-detect
    force: false,
})?;

println!("hyperd: {}", installed.binary_path.display());
# Ok::<(), hyperd_bootstrap::Error>(())
```

The library is blocking (no async runtime) and has no `tokio` dependency,
so it can be dropped into build scripts, `postinstall` hooks, or
synchronous applications.

## Build-time guarantees

- **Compile-time pin validation.** `build.rs` parses `hyperd-version.toml`
  on every build and fails fast if `version`/`build_id` are missing or
  malformed, if a sha256 isn't 64 hex chars, or if an unknown platform
  key appears. Empty sha256 strings are allowed (skip verification for
  that platform) but surface a `cargo:warning` so nobody ships a release
  with missing hashes by accident.
- **URL reachability (`verify` subcommand).** `hyperdb-bootstrap verify`
  HEADs every supported platform's download URL for the pinned release.
  It's wired into CI (see
  [`.github/workflows/verify-hyperd-pin.yml`](../.github/workflows/verify-hyperd-pin.yml))
  so yanked or renamed archives fail a PR instead of the next
  contributor's `make download-hyperd`.

## Supported platforms

| OS      | Arch    | Slug              |
|---------|---------|-------------------|
| macOS   | arm64   | `macos-arm64`     |
| macOS   | x86_64  | `macos-x86_64`    |
| Linux   | x86_64  | `linux-x86_64`    |
| Windows | x86_64  | `windows-x86_64`  |

Any other `(OS, ARCH)` errors out with a clear message.

## Install layout

```text
<dest>/
├── 0.0.24457.rc36858b6/      # versioned cache
│   ├── hyperd                 # hyperd.exe on Windows
│   └── ...                    # other files shipped under lib/hyper/
└── current/                   # fresh copy on each successful run
    ├── hyperd
    └── VERSION                # text: "0.0.24457.rc36858b6"
```

`current/` is a file copy, not a symlink — this avoids needing admin
rights for `mklink` on Windows and keeps the auto-discovery path stable.

## License

MIT OR Apache-2.0.
