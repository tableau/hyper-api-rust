# hyperdb-bootstrap

Download and install the `hyperd` executable from the PyPI `tableauhyperapi`
wheels. Ships as both a CLI binary and a library.

The `hyperd` server isn't on crates.io — it's a prebuilt binary that Tableau
distributes inside its Hyper API packages. This crate takes the PyPI ones:
each `tableauhyperapi` wheel carries `hyperd` at
`tableauhyperapi/bin/hyper/hyperd` (`hyperd.exe` plus `crashdumper.exe` on
Windows). This crate automates the "download the right wheel for your
platform, extract `hyperd` out of it, put it somewhere useful" step so
contributors and CI can bootstrap with a single command.

> **Why the PyPI wheels?** Two properties that no other distribution
> channel offers together. First, **the download URL is constructible** from
> nothing but the version and the platform's wheel tag:
>
> ```text
> https://files.pythonhosted.org/packages/py3/t/tableauhyperapi/tableauhyperapi-{version}-py3-none-{wheel_tag}.whl
> ```
>
> Tableau's own download filenames embed an opaque build id (`r07abb490`)
> that cannot be derived from the version, so every bump previously required
> scraping an HTML page to discover it. Second, **PyPI publishes a sha256 per
> file**, so the pinned digests are now *read off an API* rather than produced
> by downloading four ~80 MB archives and hashing them by hand.
>
> The bytes are the same either way: the `hyperd` inside the
> `macosx_13_0_arm64` wheel is bit-identical (sha256
> `aef5c819…6450e478`, 277,836,448 bytes) to the one this crate used to pull
> from Tableau. Same build, different envelope — and the wheels are ~3.6–4.5%
> smaller. Both report `minos 13.0`, so the `macosx_13_0` wheel tag is not a
> raised support floor and no contributor loses support.
>
> *Historical note, since older revisions of this file argued it at length:*
> Tableau's C++ `macos-arm64` zip did once ship an **x86_64** `hyperd`, which
> is why this crate used to take the Java bundle specifically. Tableau fixed
> that in `0.0.26225`; from that release the C++ and Java binaries are
> byte-identical, so the distinction no longer explains anything.

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

# Install a specific release ad-hoc — just the version, no build id needed
hyperdb-bootstrap download --version 0.0.26359

# Use an external pinned-version TOML instead of the baked-in default
hyperdb-bootstrap download --version-file ./my-hyperd.toml

# Check the pinned release for every supported platform — used by CI to
# catch yanks/renames early. Exits non-zero on any failure.
hyperdb-bootstrap verify

# Print the installed binary's path
hyperdb-bootstrap which

# Print the pinned release metadata
hyperdb-bootstrap version
```

`--version X` on its own inherits the builtin pin's `[wheel_tag]` values and
carries **no** digests, so the download is unverified and a WARN is logged. The
four wheel tags are unchanged from `0.0.19484` through `0.0.26479`, so this works
for any realistic ad-hoc pin or benchmark baseline. For a release whose wheel
tags differ, write a full pin file and pass `--version-file`.

**Version-source precedence (highest → lowest):**

1. `--version X`
2. `--version-file PATH`
3. `./hyperd-version.toml` (auto-discovered in current dir)
4. Compiled-in default shipped with this crate

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
  on every build and fails fast if `version` is missing or malformed, if a
  `[wheel_tag]` entry is missing or empty for a supported platform, if a
  sha256 isn't 64 hex chars, or if an unknown platform key appears in
  either table. Empty sha256 strings are allowed (skip verification for
  that platform) but surface a `cargo:warning` so nobody ships a release
  with missing hashes by accident.
- **Pin verification (`verify` subcommand).** `hyperdb-bootstrap verify`
  does two things for every supported platform: it HEADs the download URL
  for the pinned release, and it cross-checks the pinned sha256 against the
  digest PyPI publishes for that exact wheel filename (via
  `https://pypi.org/pypi/tableauhyperapi/<version>/json`). The second check
  is what makes this meaningful — it validates the exact pinned bytes rather
  than merely that the CDN serves *something* at that path. It's wired into
  CI (see
  [`.github/workflows/verify-hyperd-pin.yml`](../.github/workflows/verify-hyperd-pin.yml))
  so yanked or renamed archives fail a PR instead of the next
  contributor's `make download-hyperd`.

## Supported platforms

| OS      | Arch    | Slug              | Wheel tag                |
|---------|---------|-------------------|--------------------------|
| macOS   | arm64   | `macos-arm64`     | `macosx_13_0_arm64`      |
| macOS   | x86_64  | `macos-x86_64`    | `macosx_10_11_x86_64`    |
| Linux   | x86_64  | `linux-x86_64`    | `manylinux2014_x86_64`   |
| Windows | x86_64  | `windows-x86_64`  | `win_amd64`              |

Any other `(OS, ARCH)` errors out with a clear message.

The wheel tags are pin *data*, not constants in the Rust source: they aren't
guaranteed stable across releases (arm64 wheels only exist from `0.0.19484`,
and a future macOS floor bump would change `macosx_13_0_arm64`), and a wrong
tag produces a **silent 404** rather than a clear error. Keeping them in
`hyperd-version.toml` makes any such change a visible pin edit.

## Install layout

The versioned cache directory and `current/VERSION` are keyed on the version
alone — there is no build id in the path any more.

```text
<dest>/
├── 0.0.26479/                 # versioned cache
│   ├── hyperd                 # hyperd.exe on Windows
│   └── ...                    # other files shipped under the wheel's bin/hyper/
└── current/                   # fresh copy on each successful run
    ├── hyperd
    └── VERSION                # text: "0.0.26479"
```

`current/` is a file copy, not a symlink — this avoids needing admin
rights for `mklink` on Windows and keeps the auto-discovery path stable.

## License

MIT OR Apache-2.0.
