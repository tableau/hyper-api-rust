// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `hyperdb-bootstrap` — CLI front-end for the library of the same name.
//!
//! Subcommands:
//! - `download` — install `hyperd` under `.hyperd/<version>/` and refresh
//!   `.hyperd/current/`.
//! - `verify`   — probe each platform's wheel URL and cross-check its pinned
//!   digest against PyPI (CI guard against silent yanks and digest drift).
//! - `which`    — print the path of the currently-installed `hyperd`.
//! - `version`  — print the pinned release metadata.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{ArgGroup, Args, Parser, Subcommand};
use hyperdb_bootstrap::{
    DEFAULT_DEST_ROOT, InstallOptions, InstalledHyperd, PinnedRelease, Platform, VersionSource,
    install, verify_release,
};

// CARGO_PKG_VERSION + git short hash captured by build.rs. Both are
// env! literals so concat! collapses them into a &'static str at compile
// time — exactly what clap wants for `version = ...`.
const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), ".r", env!("HYPERDB_GIT_HASH"));

#[derive(Parser)]
#[command(name = "hyperdb-bootstrap", version = VERSION, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Download and install hyperd into `.hyperd/` (or --dest).
    Download(DownloadArgs),
    /// Check every platform's wheel URL is still reachable and that its
    /// pinned digest still matches what PyPI publishes. Useful as a CI
    /// guard against silent yanks and digest drift.
    Verify(VerifyArgs),
    /// Print the path of the currently-installed hyperd (if any).
    Which(WhichArgs),
    /// Print the pinned release metadata.
    Version,
}

#[derive(Args)]
#[command(group(
    ArgGroup::new("version_src")
        .args(["version", "version_file"])
        .required(false)
        .multiple(false)
))]
struct DownloadArgs {
    /// Destination root directory (default: .hyperd in the current dir).
    #[arg(long)]
    dest: Option<PathBuf>,

    /// Re-download and re-extract even if the version is already cached.
    #[arg(long)]
    force: bool,

    /// Explicit version to install (e.g. 0.0.26359). Reuses the pinned wheel
    /// tags and skips sha256 verification, since digests are version-specific.
    #[arg(long)]
    version: Option<String>,

    /// Path to an external pinned-version TOML file.
    #[arg(long, value_name = "PATH")]
    version_file: Option<PathBuf>,
}

#[derive(Args)]
struct WhichArgs {
    /// Destination root directory (default: .hyperd in the current dir).
    #[arg(long)]
    dest: Option<PathBuf>,
}

#[derive(Args)]
struct VerifyArgs {
    /// Load pinned metadata from an external TOML file instead of the
    /// compiled-in default (e.g. CI checking a candidate bump before merge).
    #[arg(long, value_name = "PATH")]
    version_file: Option<PathBuf>,
}

fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    match cli.command {
        Command::Download(args) => run_download(args),
        Command::Verify(args) => run_verify(args),
        Command::Which(args) => run_which(args),
        Command::Version => run_version(),
    }
}

fn run_download(args: DownloadArgs) -> Result<()> {
    let version_source = pick_version_source(&args)?;
    let dest_root = args
        .dest
        .unwrap_or_else(|| PathBuf::from(DEFAULT_DEST_ROOT));

    let opts = InstallOptions {
        dest_root,
        version_source,
        platform: None,
        force: args.force,
    };
    let installed = install(opts).context("installing hyperd failed")?;
    print_installed(&installed);
    Ok(())
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "signature retained for API symmetry / future fallibility; returning Result/Option keeps callers from breaking when the function later grows failure cases"
)]
fn pick_version_source(args: &DownloadArgs) -> Result<VersionSource> {
    // Precedence:
    //   1. --version X           (Explicit)
    //   2. --version-file PATH   (TomlFile)
    //   3. ./hyperd-version.toml (auto-discovered TomlFile)
    //   4. builtin
    if let Some(v) = &args.version {
        // Inherit the builtin pin's wheel tags — they are stable across every
        // release that publishes an arm64 wheel (0.0.19484 onward), so an
        // ad-hoc version override does not also need to restate them. Drop the
        // digests: they are specific to the pinned version's files.
        let release = PinnedRelease {
            version: v.clone(),
            wheel_tag: PinnedRelease::builtin().wheel_tag,
            sha256: std::collections::HashMap::default(),
        };
        return Ok(VersionSource::Explicit(release));
    }
    if let Some(path) = &args.version_file {
        return Ok(VersionSource::TomlFile(path.clone()));
    }
    let cwd_toml = PathBuf::from("hyperd-version.toml");
    if cwd_toml.exists() {
        tracing::info!(path = %cwd_toml.display(), "using hyperd-version.toml from current dir");
        return Ok(VersionSource::TomlFile(cwd_toml));
    }
    Ok(VersionSource::Builtin)
}

fn run_which(args: WhichArgs) -> Result<()> {
    let dest = args
        .dest
        .unwrap_or_else(|| PathBuf::from(DEFAULT_DEST_ROOT));
    let platform = Platform::current().context("detecting current platform")?;
    let binary = dest.join("current").join(platform.executable_name());
    if binary.exists() {
        println!("{}", binary.display());
        Ok(())
    } else {
        anyhow::bail!(
            "no hyperd installed at {} (run `hyperdb-bootstrap download` first)",
            binary.display()
        );
    }
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "signature retained for API symmetry / future fallibility; returning Result/Option keeps callers from breaking when the function later grows failure cases"
)]
fn run_version() -> Result<()> {
    let r = PinnedRelease::builtin();
    println!("pinned version: {}", r.version);
    for platform in [
        Platform::MacosArm64,
        Platform::MacosX86_64,
        Platform::LinuxX86_64,
        Platform::WindowsX86_64,
    ] {
        println!(
            "  {:<16} {}",
            platform.to_string(),
            r.wheel_tag_for(platform).unwrap_or("<no wheel tag pinned>")
        );
    }
    Ok(())
}

fn run_verify(args: VerifyArgs) -> Result<()> {
    let release = match args.version_file {
        Some(path) => PinnedRelease::from_toml_file(&path)
            .with_context(|| format!("loading {}", path.display()))?,
        None => PinnedRelease::builtin(),
    };
    println!("verifying hyperd {}...", release.version);
    let outcomes = verify_release(&release).context("probing platform URLs failed")?;
    let mut all_ok = true;
    for o in &outcomes {
        let label = if o.ok() { "OK  " } else { "FAIL" };
        if !o.ok() {
            all_ok = false;
        }
        let http = match (o.status, &o.error) {
            (Some(status), _) => format!("{status}"),
            (None, Some(err)) => format!("network error: {err}"),
            (None, None) => unreachable!("verify_release always sets status or error"),
        };
        println!(
            "  {label}  {:<16} [{http}] {}",
            o.platform.to_string(),
            o.url
        );
        println!("        {}", o.digest);
    }
    if !all_ok {
        anyhow::bail!("one or more platforms failed URL or digest verification");
    }
    println!("all platforms reachable with matching digests.");
    Ok(())
}

fn print_installed(i: &InstalledHyperd) {
    let status = if i.cache_hit { "cached" } else { "installed" };
    println!(
        "{status}: hyperd {version} ({platform}) -> {path}",
        status = status,
        version = i.version,
        platform = i.platform,
        path = i.binary_path.display(),
    );
}

fn init_tracing() {
    use tracing::Level;
    let _ = tracing::subscriber::set_global_default(
        tracing_subscriber::FmtSubscriber::builder()
            .with_max_level(Level::INFO)
            .with_target(false)
            .finish(),
    );
}
