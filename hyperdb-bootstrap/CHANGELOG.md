# Changelog

All notable changes to the `hyperdb-bootstrap` crate will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Changed

- **BREAKING: `hyperd` is now downloaded from the PyPI `tableauhyperapi`
  wheels instead of Tableau's Hyper Java API zips**, and the pin moves to
  `0.0.26479`. The motivation is that a wheel URL is fully constructible from
  the version plus the platform's wheel tag:

  ```text
  https://files.pythonhosted.org/packages/py3/t/tableauhyperapi/tableauhyperapi-{version}-py3-none-{wheel_tag}.whl
  ```

  whereas the Tableau zip filenames embed an opaque `build_id` (`r07abb490`)
  that cannot be derived from the version — which is why bumping the pin
  used to require scraping an HTML page. And because **PyPI publishes a
  sha256 per file**, the four pinned digests are now read off
  `https://pypi.org/pypi/tableauhyperapi/<version>/json` rather than produced
  by downloading four ~80 MB archives and hashing them by hand. The digests
  are still committed: a hash in git is an attestation independent of the
  host serving the bytes.

  **The bytes are unchanged.** The `hyperd` inside the `macosx_13_0_arm64`
  wheel is bit-identical to the one extracted from the corresponding Java
  zip — sha256
  `aef5c81970bb4d84d06fb9513d5ffd722526fce779632a0c5f63d87b6450e478`,
  277,836,448 bytes. Same build, different envelope; the wheels are ~3.6–4.5%
  smaller. Both binaries report `minos 13.0`, so the `macosx_13_0` wheel tag
  is **not** a raised support floor and no contributor loses support.

  **BREAKING** consequences for the pin format and the public API:

  - `hyperd-version.toml` now holds `version`, a `[wheel_tag]` table, and
    `[sha256]`. **`build_id` is gone.** The wheel tags are pin data rather
    than Rust constants because they are not stable across releases (arm64
    wheels only exist from `0.0.19484`; a future macOS floor bump would change
    `macosx_13_0_arm64`) and a wrong tag yields a *silent 404* — keeping them
    in the pin makes any such change a visible pin edit.
  - `build.rs` now validates `version`, the `[wheel_tag]` entries, and the
    sha256 shapes; it no longer validates `build_id`.
  - The install layout is keyed on the version alone: the versioned cache
    directory is `<dest>/0.0.26479/` (was `<dest>/0.0.26479.r96880f6a/`) and
    `current/VERSION` contains just `0.0.26479`.
  - `hyperd` is extracted from `tableauhyperapi/bin/hyper/` inside the wheel
    rather than `lib/hyper/` inside the zip.
  - New `PinnedRelease::wheel_tag_for(Platform) -> Option<&str>`.
  - `url::build_download_url` is now **fallible**, returning
    `Result<String, Error>` — a platform with no pinned wheel tag is an error
    (`Error::MissingWheelTag`, a new variant).
  - `verify` now additionally cross-checks every pinned sha256 against the
    digest PyPI publishes for that exact wheel filename, on top of HEAD-ing
    the four download URLs. It therefore validates the exact pinned bytes
    rather than merely that the CDN serves *something* at that path.

- **BREAKING:** the minimum supported Rust version is now **1.88**, up from
  1.81, and the crate is compiled with **edition 2024**. 1.88 is the version
  Red Hat Enterprise Linux 9.7 ships as `rust-toolset`.
- **BREAKING:** the TLS crypto provider is now **ring** rather than AWS-LC.
  This crate declares its own `reqwest` dependency rather than inheriting the
  workspace entry, and asked for the `rustls` feature — which forces the
  `aws-lc-rs` provider and, through Cargo's feature unification, imposed it on
  every crate in the workspace. It now uses `rustls-no-provider`.

  That feature links *no* provider at all, and `reqwest` resolves one through
  `CryptoProvider::get_default()`, which has no crate-feature fallback — so
  `scrape_latest` now installs ring as the process-wide default before building
  its HTTP client. **Embedders take note:** if your application installs its own
  `CryptoProvider`, install it before calling into this crate; ours defers to an
  already-installed provider rather than replacing it.

  Guarded by a unit test that builds a `reqwest` client, which needs no network
  because the failure is a panic inside `build()`.

  **This entire workaround is retired by the PyPI migration above**, which
  drops `reqwest` and `rustls` from the crate: there is no in-process HTTP
  client left, so there is no crypto provider to select. `ensure_crypto_provider`,
  the `rustls-no-provider` feature, and the ring pin are all gone, and the
  feature-unification pressure this bullet describes no longer exists for the
  rest of the workspace.

- **Bump the pinned `hyperd` release to `0.0.26359` (`r07abb490`).** This
  supersedes the never-shipped `0.0.26225` bump attempt (PR #219), which was
  held because `0.0.26225` deadlocked on Apple Silicon Macs running macOS
  versions prior to 26 during JIT exception-frame registration. `0.0.26359`
  fixes that engine defect ("Fixed an issue in Hyper on Apple Silicon Macs
  running macOS versions prior to version 26"). Updates the version, build id,
  and all four per-platform sha256s in `hyperd-version.toml`. Performance
  (same-session A/B vs the live `0.0.25080` pin, median of 3 runs at 100M rows,
  single-connection): `full_scan` queries **+67% sync / +33% async** on the
  dominant scan path, filtered queries ~flat; bulk inserts show a small
  regression (Inserter −11%, ChunkSender −6%, async −4%) — soft numbers, as the
  insert path carries cold-start variance and multi-connection deltas throttle
  thermally on a laptop. Verified native arm64 and all 1485 workspace tests pass;
  the macOS-14 CI runner is green (the deadlock is gone).

### Removed

Everything here follows from the move to PyPI wheels described under
**Changed**: the build id no longer exists as a concept, and nothing needs to
be discovered by scraping.

- **BREAKING: the `--build-id` CLI flag is removed.** `--version X` on its own
  is now a complete version source: it inherits the builtin pin's
  `[wheel_tag]` values and carries no digests, so the download is unverified
  and logs a WARN. The four wheel tags are unchanged from `0.0.19484` through
  `0.0.26479`, so this covers any realistic ad-hoc pin or A/B baseline; for a
  release whose tags differ, use `--version-file` with a full pin.
- **BREAKING: the `--latest` CLI flag, the `VersionSource::ScrapeLatest`
  variant, and the whole `scrape` module are removed.** The scraper had been
  broken for three-plus releases without anyone noticing, because its tests
  passed against a synthetic fixture rather than the live page. It had two
  independent defects: the heading regex expected `<h3>VERSION [DATE]</h3>`,
  but Docusaurus renders `0.0.26479 <!-- -->[September 3 2026]`, which `\s*`
  cannot span; and the build-id capture hardcoded `(rc[a-z0-9]+)` while every
  build id since `0.0.24457` has been `r` followed by hex. It is deleted
  rather than fixed — with a constructible URL and published digests, there is
  nothing left for it to do. Version-source precedence is now, highest to
  lowest: `--version X`, `--version-file PATH`, an auto-discovered
  `./hyperd-version.toml`, then the compiled-in default.
- **BREAKING: `PinnedRelease::build_id` and `InstalledHyperd::build_id` are
  removed**, as is `PinnedRelease::version_tag()`. Use `.version`, which is
  now the only release identifier, and `PinnedRelease::wheel_tag_for()` for
  the per-platform tag.
- **BREAKING: the `Error::Http`, `Error::HttpStatus`, and `Error::ScrapeFailed`
  variants are removed.** All three existed only to serve the scraper.
- **BREAKING: the `regex`, `reqwest`, and `rustls` dependencies are dropped
  entirely.** `download.rs` and `verify.rs` already shell out to `curl` —
  deliberately, because Akamai bot protection blocked non-browser TLS stacks
  from GitHub runner IPs — so removing the scraper leaves no in-process HTTP
  client behind.

### Fixed

- **The extracted macOS arm64 `hyperd` is a native arm64 binary.** Earlier
  releases of Tableau's C++ `macos-arm64` zip shipped an **x86_64** `hyperd`
  (an upstream packaging defect), so on Apple Silicon the extracted binary
  only ran under Rosetta; this crate switched to the Java bundle to get a
  native one. Tableau fixed the C++ packaging in `0.0.26225`, and from that
  release the C++ and Java binaries are byte-identical, so the bundle choice
  stopped mattering — and the move to PyPI wheels under **Changed** supersedes
  it entirely. The wheels carry the same native arm64 build.

## [0.1.1] - 2026-05-13

### Added

- `install()` library function and `hyperdb-bootstrap` CLI binary
- `InstallOptions` for configuring an install (destination, version source, sha256 verification)
- `InstalledHyperd` describing a successful install (path, platform, release metadata)
- `VersionSource` enum (pinned compile-time metadata, custom version file, or `--latest` scrape)
- `Platform` enum and detection for macOS (arm64/x86_64), Linux (x86_64), Windows (x86_64)
- `PinnedRelease` for the pinned release metadata baked in at compile time via `hyperd-version.toml`
- `verify_release()` library API and `verify` CLI subcommand that HEADs every supported platform's URL — wired into a GitHub Actions workflow for CI
- `VerifyOutcome` describing the result of a verification
- `Error` type for structured error handling
- `--latest` best-effort scrape of the public Tableau releases page
- `--version-file` override plus auto-discovery of `./hyperd-version.toml`
- sha256 verification when hashes are available
- `build.rs` compile-time validation of `hyperd-version.toml` (shape, hex sha256s, known platform keys; empty sha256s warn)
- `DEFAULT_DEST_ROOT` constant for the default install destination
