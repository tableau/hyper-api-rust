// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `hyperd-bootstrap` — CLI front-end for the library of the same name.
//!
//! Subcommands:
//! - `download` — install `hyperd` under `.hyperd/<version>/` and refresh
//!   `.hyperd/current/`.
//! - `verify`   — HEAD each platform URL for the pinned release to confirm
//!   the CDN is still serving it (CI guard against silent yanks).
//! - `which`    — print the path of the currently-installed `hyperd`.
//! - `version`  — print the pinned release metadata.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{ArgGroup, Args, Parser, Subcommand};
use hyperdb_bootstrap::{
    install, verify_release, InstallOptions, InstalledHyperd, PinnedRelease, Platform,
    VersionSource, DEFAULT_DEST_ROOT,
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
    /// HEAD every platform URL for the pinned release to check they're
    /// still reachable. Useful as a CI guard against silent yanks.
    Verify(VerifyArgs),
    /// Print the path of the currently-installed hyperd (if any).
    Which(WhichArgs),
    /// Print the pinned release metadata.
    Version,
}

#[derive(Args)]
#[command(group(
    ArgGroup::new("version_src")
        .args(["latest", "version", "version_file"])
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

    /// Scrape the latest release from the Tableau releases page.
    /// Best-effort; skips sha256 verification.
    #[arg(long)]
    latest: bool,

    /// Explicit version to install (e.g. 0.0.24457). Requires --build-id.
    #[arg(long, requires = "build_id")]
    version: Option<String>,

    /// Build id for --version (e.g. rc36858b6).
    #[arg(long, requires = "version")]
    build_id: Option<String>,

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
    //   1. --version + --build-id  (Explicit)
    //   2. --latest                (ScrapeLatest)
    //   3. --version-file PATH     (TomlFile)
    //   4. ./hyperd-version.toml   (auto-discovered TomlFile)
    //   5. builtin
    if let (Some(v), Some(b)) = (&args.version, &args.build_id) {
        let release = PinnedRelease {
            version: v.clone(),
            build_id: b.clone(),
            sha256: std::collections::HashMap::default(),
        };
        return Ok(VersionSource::Explicit(release));
    }
    if args.latest {
        return Ok(VersionSource::ScrapeLatest);
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
            "no hyperd installed at {} (run `hyperd-bootstrap download` first)",
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
    println!("pinned version:  {}", r.version);
    println!("pinned build_id: {}", r.build_id);
    println!("version tag:     {}", r.version_tag());
    Ok(())
}

fn run_verify(args: VerifyArgs) -> Result<()> {
    let release = match args.version_file {
        Some(path) => PinnedRelease::from_toml_file(&path)
            .with_context(|| format!("loading {}", path.display()))?,
        None => PinnedRelease::builtin(),
    };
    println!("verifying hyperd {}...", release.version_tag());
    let outcomes = verify_release(&release).context("HEAD requests failed")?;
    let mut all_ok = true;
    for o in &outcomes {
        match (o.status, &o.error) {
            (Some(status), _) if o.ok() => {
                println!(
                    "  OK    {:<16} [{status}] {}",
                    o.platform.to_string(),
                    o.url
                );
            }
            (Some(status), _) => {
                all_ok = false;
                println!(
                    "  FAIL  {:<16} [{status}] {}",
                    o.platform.to_string(),
                    o.url
                );
            }
            (None, Some(err)) => {
                all_ok = false;
                println!(
                    "  FAIL  {:<16} [network error] {} ({err})",
                    o.platform.to_string(),
                    o.url
                );
            }
            (None, None) => unreachable!("verify_release always sets status or error"),
        }
    }
    if !all_ok {
        anyhow::bail!("one or more platform URLs failed to resolve");
    }
    println!("all platforms reachable.");
    Ok(())
}

fn print_installed(i: &InstalledHyperd) {
    let status = if i.cache_hit { "cached" } else { "installed" };
    println!(
        "{status}: hyperd {version}.{build_id} ({platform}) -> {path}",
        status = status,
        version = i.version,
        build_id = i.build_id,
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
