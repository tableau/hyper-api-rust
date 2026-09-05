// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Installation and launcher identity contracts.

use std::ffi::OsStr;
use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use semver::Version;
use serde::{Deserialize, Serialize};

use crate::daemon::discovery::{DaemonRecord, PortScan, RawDiscoveryRead};

const MAX_LAUNCHER_INFO_BYTES: usize = 16 * 1024;
const MAX_REPORTED_STRING_BYTES: usize = 4 * 1024;
const MAX_STATUS_RESPONSE_BYTES: usize = 64 * 1024;
/// Global wall-clock ceiling for the whole daemon-discovery phase of `doctor`.
///
/// A `doctor` run that finds a live daemon returns as soon as it has the
/// verified STATUS — this bound only caps how long discovery *waits* before
/// giving up and reporting the daemon missing. It must stay comfortably below
/// the 650ms watchdog the real-network tests assert (see
/// `real_doctor_collector_enforces_global_deadline_against_slow_drip`), which
/// is why this is 500ms and not higher.
const DOCTOR_DAEMON_TIMEOUT: Duration = Duration::from_millis(500);
/// Per-socket-operation ceiling (connect / write / read), also clamped to the
/// remaining global budget. A single read is the binding constraint when a
/// daemon is slow to *start accepting*: the OS completes the connection into
/// the listen backlog immediately, but the PONG/STATUS reply doesn't arrive
/// until the daemon's accept loop services it. On CPU-saturated CI runners
/// (macOS-14 has ~3 cores) that startup latency routinely exceeded the old
/// 150ms window, so `doctor` timed the read out and wrongly reported the
/// daemon missing. 300ms gives that read ~2x the observed slack while still
/// leaving room under the global deadline for the follow-up STATUS round-trip.
const DOCTOR_NETWORK_PHASE_TIMEOUT: Duration = Duration::from_millis(300);

/// How an operating-system path was converted to its bounded display form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PathEncoding {
    /// The original path was valid UTF-8.
    Utf8,
    /// The display form required a lossy operating-system string conversion.
    Lossy,
}

/// A bounded path display that never assumes operating-system paths are UTF-8.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReportedPath {
    /// Bounded display form.
    pub display: String,
    /// Whether display conversion was exact or lossy.
    pub encoding: PathEncoding,
}

impl<'de> Deserialize<'de> for ReportedPath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireReportedPath {
            display: String,
            encoding: PathEncoding,
        }

        let mut wire = WireReportedPath::deserialize(deserializer)?;
        truncate_utf8(&mut wire.display, MAX_REPORTED_STRING_BYTES);
        Ok(Self {
            display: wire.display,
            encoding: wire.encoding,
        })
    }
}

impl ReportedPath {
    /// Build a bounded display representation from an operating-system string.
    #[must_use]
    pub fn from_os_str(path: &OsStr) -> Self {
        let (mut display, encoding) = match path.to_str() {
            Some(path) => (path.to_owned(), PathEncoding::Utf8),
            None => (path.to_string_lossy().into_owned(), PathEncoding::Lossy),
        };
        truncate_utf8(&mut display, MAX_REPORTED_STRING_BYTES);

        Self { display, encoding }
    }
}

/// Launcher-reported identity for one npm package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LauncherPackageIdentity {
    /// Package name.
    pub name: String,
    /// Package version, absent in source manifests.
    pub version: Option<String>,
    /// Path to the package manifest.
    pub package_path: ReportedPath,
}

/// Allowlisted identity reported by the npm launcher.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LauncherIdentity {
    /// Umbrella npm package.
    pub wrapper: LauncherPackageIdentity,
    /// Selected platform-specific npm package.
    pub platform: LauncherPackageIdentity,
    /// Selected native executable.
    pub executable_path: ReportedPath,
}

/// A bounded, typed warning produced while collecting installation identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum IdentityWarning {
    /// Launcher metadata was not valid JSON with the expected shape.
    MalformedLauncherInfo,
    /// The complete launcher value exceeded its fixed input limit.
    LauncherInfoTooLarge,
    /// One allowlisted string exceeded its fixed input limit.
    LauncherFieldTooLarge {
        /// Stable dotted field name; never the rejected field value.
        field: String,
    },
    /// A reported or compiled version could not be parsed.
    MalformedVersion {
        /// Stable component name; never the malformed value.
        component: String,
    },
    /// Launcher package bases disagree with the authoritative native base.
    VersionMismatch {
        /// Native MCP semantic-version base.
        native: String,
        /// Wrapper npm version, when present and valid.
        wrapper: Option<String>,
        /// Platform npm version, when present and valid.
        platform: Option<String>,
    },
}

/// Result of pure launcher metadata parsing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ParsedLauncherIdentity {
    /// Validated launcher identity, or none when absent/rejected.
    pub identity: Option<LauncherIdentity>,
    /// Bounded warnings explaining rejected metadata.
    pub warnings: Vec<IdentityWarning>,
}

/// A compiled source version split into its semantic base and build suffix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceVersionIdentity {
    /// Full compiled source string.
    pub source: String,
    /// Parsed semantic-version base.
    pub version: Option<String>,
    /// Build suffix following `.r`, without the `r` marker.
    pub build: Option<String>,
}

/// Authoritative native identity plus optional launcher-reported metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstallationIdentity {
    /// Actual native executable path.
    pub native_executable: ReportedPath,
    /// MCP source version and build identity.
    pub mcp: SourceVersionIdentity,
    /// Rust Hyper API source version and build identity.
    pub hyper_rust_api: SourceVersionIdentity,
    /// Optional, validated launcher report.
    pub launcher: Option<LauncherIdentity>,
    /// Bounded parse and comparison warnings.
    pub warnings: Vec<IdentityWarning>,
}

/// Global CLI inputs that influence a doctor report.
#[derive(Debug, Clone, Copy)]
pub struct DoctorOptions<'a> {
    /// Preferred persistent-database CLI path.
    pub persistent_db: Option<&'a str>,
    /// Deprecated persistent-database CLI alias.
    pub deprecated_workspace: Option<&'a str>,
    /// Disable the reserved persistent attachment.
    pub ephemeral_only: bool,
    /// Effective MCP read-only mode.
    pub read_only: bool,
    /// Effective private-hyperd mode.
    pub no_daemon: bool,
}

/// Failures that prevent serializable doctor facts from being assembled.
#[derive(Debug, thiserror::Error)]
pub enum DoctorReportError {
    /// The operating system could not identify the running native executable.
    #[error("could not identify the current hyperdb-mcp executable: {0}")]
    CurrentExecutable(#[source] io::Error),
    /// The generated MCP tool catalog could not be serialized canonically.
    #[error("could not serialize the generated MCP tool catalog: {0}")]
    Catalog(#[from] serde_json::Error),
}

/// Collect the installation identity shared by `doctor` and MCP `status`.
///
/// This only inspects process metadata and the bounded launcher environment
/// value. It deliberately performs no daemon, filesystem, or database probe.
///
/// # Errors
///
/// Returns an error if the operating system cannot resolve the current executable path.
pub fn current_installation_identity() -> Result<InstallationIdentity, io::Error> {
    let current_executable = std::env::current_exe()?;
    let launcher_info = std::env::var_os("HYPERDB_MCP_LAUNCHER_INFO");
    Ok(installation_identity_from_parts(
        current_executable.as_os_str(),
        &crate::version::mcp_version_string(),
        &crate::version::hyper_api_version_string(),
        launcher_info.as_deref(),
    ))
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum DoctorStatus {
    Ok,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum PersistentMode {
    PersistentAttached,
    EphemeralOnly,
}

#[derive(Debug, Clone, Serialize)]
struct DoctorPathFacts {
    path: ReportedPath,
    exists: bool,
    is_file: bool,
    is_directory: bool,
}

#[derive(Debug, Clone, Serialize)]
struct DoctorInstallationReport {
    native_executable: ReportedPath,
    mcp_version: SourceVersionIdentity,
    hyper_rust_api_version: SourceVersionIdentity,
    launcher: Option<LauncherIdentity>,
}

#[derive(Debug, Clone, Serialize)]
struct DoctorConfigurationReport {
    persistent_mode: PersistentMode,
    persistent_path_source: crate::paths::PersistentDbPathSource,
    observed_persistent_path: Option<ReportedPath>,
    resolved_persistent_path: Option<DoctorPathFacts>,
    resolved_persistent_parent: Option<DoctorPathFacts>,
    daemon_state_directory: Option<DoctorPathFacts>,
    daemon_discovery_file: Option<DoctorPathFacts>,
    client_log: DoctorPathFacts,
    observed_hyperd_path: Option<DoctorPathFacts>,
    effective_hyperd_path: Option<DoctorPathFacts>,
    upward_hyperd_candidate: Option<DoctorPathFacts>,
    read_only: bool,
    no_daemon: bool,
}

#[derive(Debug, Clone, Serialize)]
struct DoctorDaemonSection {
    state: DoctorDaemonState,
    pid: Option<u32>,
    hyperd_endpoint: Option<String>,
    health_port: Option<u16>,
    started_at: Option<String>,
    version: Option<String>,
    mcp_version: Option<String>,
    executable_path: Option<ReportedPath>,
}

#[derive(Debug, Clone, Serialize)]
struct DoctorToolCatalogReport {
    tool_count: usize,
    canonical_tool_bytes: usize,
    initialization_instructions_bytes: usize,
    get_readme_bytes: usize,
}

#[derive(Debug, Clone, Serialize)]
struct DoctorWarning {
    code: String,
    message: String,
}

/// Stable, typed native doctor report.
#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    status: DoctorStatus,
    installation: DoctorInstallationReport,
    configuration: DoctorConfigurationReport,
    daemon: DoctorDaemonSection,
    tool_catalog: DoctorToolCatalogReport,
    warnings: Vec<DoctorWarning>,
}

/// Monotonic instant supplied to the pure doctor collector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DoctorMoment(pub(crate) u64);

/// Finite monotonic deadline shared by doctor scans and probes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DoctorDeadline(pub(crate) u64);

/// Bounded candidate-scan request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DoctorScanRequest {
    pub(crate) ports: PortScan,
    pub(crate) deadline: DoctorDeadline,
}

/// A candidate location returned by the bounded scanner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DoctorScanCandidate {
    pub(crate) responding_port: u16,
}

/// Raw outcome from fetching enriched `STATUS` at one candidate port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DoctorStatusProbe {
    Unreachable,
    Response(String),
}

/// Finite collection policy supplied independently of process globals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DoctorCollectRequest {
    pub(crate) ports: PortScan,
    pub(crate) timeout: Duration,
}

/// The stable daemon discovery state exposed by doctor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DoctorDaemonState {
    Missing,
    Unreadable,
    Malformed,
    ParsedUnreachable,
    LiveFromDiscovery,
    LiveFromScan,
}

/// One recorded discovery fact that disagrees with fresh enriched `STATUS`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DiscoveryFactMismatch {
    Pid {
        recorded: u32,
        fresh: u32,
    },
    McpVersion {
        recorded: String,
        fresh: String,
    },
    ExecutablePath {
        recorded: ReportedPath,
        fresh: ReportedPath,
    },
}

/// Typed warnings produced while verifying daemon candidates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DoctorDaemonWarning {
    DiscoveryUnreadable {
        kind: io::ErrorKind,
    },
    MalformedDiscovery,
    DiscoveryCandidateUnreachable {
        responding_port: u16,
    },
    StaleOrReplacedDiscovery {
        mismatches: Vec<DiscoveryFactMismatch>,
    },
    StatusHealthPortMismatch {
        responding_port: u16,
        reported_port: u16,
    },
    MalformedStatus {
        responding_port: u16,
    },
}

/// Fresh daemon facts accepted only after candidate-port verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VerifiedDoctorDaemon {
    pub(crate) responding_port: u16,
    pub(crate) record: DaemonRecord,
}

/// Pure daemon portion of the doctor report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DoctorDaemonReport {
    pub(crate) state: DoctorDaemonState,
    pub(crate) verified: Option<VerifiedDoctorDaemon>,
    pub(crate) warnings: Vec<DoctorDaemonWarning>,
}

/// The complete external capability set reachable by daemon collection.
///
/// Deliberately absent: discovery writers, cleanup, process control, filesystem
/// mutation, and unbounded network operations.
pub(crate) struct DoctorCollectorDependencies<'a> {
    pub(crate) read_raw_discovery: &'a dyn Fn() -> RawDiscoveryRead,
    pub(crate) probe_enriched_status: &'a dyn Fn(u16, DoctorDeadline) -> DoctorStatusProbe,
    pub(crate) scan_candidates: &'a dyn Fn(DoctorScanRequest) -> Vec<DoctorScanCandidate>,
    pub(crate) now: &'a dyn Fn() -> DoctorMoment,
    pub(crate) deadline_after: &'a dyn Fn(DoctorMoment, Duration) -> DoctorDeadline,
}

/// Collect daemon doctor facts without granting mutation capabilities.
pub(crate) fn collect_doctor_daemon(
    dependencies: &DoctorCollectorDependencies<'_>,
    request: DoctorCollectRequest,
) -> DoctorDaemonReport {
    let deadline = (dependencies.deadline_after)((dependencies.now)(), request.timeout);
    let raw = (dependencies.read_raw_discovery)();
    let mut warnings = Vec::new();

    let (fallback_state, discovery_record) = match raw {
        RawDiscoveryRead::Missing { .. } => (DoctorDaemonState::Missing, None),
        RawDiscoveryRead::Unreadable { kind, .. } => {
            warnings.push(DoctorDaemonWarning::DiscoveryUnreadable { kind });
            (DoctorDaemonState::Unreadable, None)
        }
        RawDiscoveryRead::Malformed { .. } => {
            warnings.push(DoctorDaemonWarning::MalformedDiscovery);
            (DoctorDaemonState::Malformed, None)
        }
        RawDiscoveryRead::Parsed { record, .. } => {
            (DoctorDaemonState::ParsedUnreachable, Some(record))
        }
    };

    if let Some(recorded) = discovery_record.as_ref() {
        let responding_port = recorded.info().health_port;
        match verify_status_candidate(dependencies, responding_port, deadline, &mut warnings) {
            Some(fresh) => {
                let mismatches = discovery_fact_mismatches(recorded, &fresh);
                if !mismatches.is_empty() {
                    warnings.push(DoctorDaemonWarning::StaleOrReplacedDiscovery { mismatches });
                }
                return DoctorDaemonReport {
                    state: DoctorDaemonState::LiveFromDiscovery,
                    verified: Some(VerifiedDoctorDaemon {
                        responding_port,
                        record: fresh,
                    }),
                    warnings,
                };
            }
            None if warnings.is_empty() => {
                warnings
                    .push(DoctorDaemonWarning::DiscoveryCandidateUnreachable { responding_port });
            }
            None => {}
        }
    }

    for candidate in (dependencies.scan_candidates)(DoctorScanRequest {
        ports: request.ports,
        deadline,
    }) {
        if let Some(record) = verify_status_candidate(
            dependencies,
            candidate.responding_port,
            deadline,
            &mut warnings,
        ) {
            return DoctorDaemonReport {
                state: DoctorDaemonState::LiveFromScan,
                verified: Some(VerifiedDoctorDaemon {
                    responding_port: candidate.responding_port,
                    record,
                }),
                warnings,
            };
        }
    }

    DoctorDaemonReport {
        state: fallback_state,
        verified: None,
        warnings,
    }
}

fn verify_status_candidate(
    dependencies: &DoctorCollectorDependencies<'_>,
    responding_port: u16,
    deadline: DoctorDeadline,
    warnings: &mut Vec<DoctorDaemonWarning>,
) -> Option<DaemonRecord> {
    let DoctorStatusProbe::Response(response) =
        (dependencies.probe_enriched_status)(responding_port, deadline)
    else {
        return None;
    };
    let Ok(record) = serde_json::from_str::<DaemonRecord>(&response) else {
        warnings.push(DoctorDaemonWarning::MalformedStatus { responding_port });
        return None;
    };
    if record.identity().is_none() {
        warnings.push(DoctorDaemonWarning::MalformedStatus { responding_port });
        return None;
    }
    if record.info().health_port != responding_port {
        warnings.push(DoctorDaemonWarning::StatusHealthPortMismatch {
            responding_port,
            reported_port: record.info().health_port,
        });
        return None;
    }
    Some(record)
}

fn discovery_fact_mismatches(
    recorded: &DaemonRecord,
    fresh: &DaemonRecord,
) -> Vec<DiscoveryFactMismatch> {
    let mut mismatches = Vec::new();
    if recorded.info().pid != fresh.info().pid {
        mismatches.push(DiscoveryFactMismatch::Pid {
            recorded: recorded.info().pid,
            fresh: fresh.info().pid,
        });
    }
    if let (Some(recorded_identity), Some(fresh_identity)) = (recorded.identity(), fresh.identity())
    {
        if recorded_identity.mcp_version() != fresh_identity.mcp_version() {
            mismatches.push(DiscoveryFactMismatch::McpVersion {
                recorded: recorded_identity.mcp_version().to_owned(),
                fresh: fresh_identity.mcp_version().to_owned(),
            });
        }
        if recorded_identity.executable_path() != fresh_identity.executable_path() {
            mismatches.push(DiscoveryFactMismatch::ExecutablePath {
                recorded: recorded_identity.executable_path().clone(),
                fresh: fresh_identity.executable_path().clone(),
            });
        }
    }
    mismatches
}

/// Collect the complete side-effect-free native doctor report.
///
/// This reads process configuration, filesystem metadata, the non-mutating raw
/// discovery record, and bounded loopback health responses. It never creates a
/// directory or file, starts a daemon or `hyperd`, or opens a database.
///
/// # Errors
///
/// Returns [`DoctorReportError::CurrentExecutable`] when the operating system
/// cannot identify this process's executable, or [`DoctorReportError::Catalog`]
/// when the generated typed tool catalog cannot be serialized.
pub fn collect_doctor_report(
    options: DoctorOptions<'_>,
) -> Result<DoctorReport, DoctorReportError> {
    let installation =
        current_installation_identity().map_err(DoctorReportError::CurrentExecutable)?;

    let resolved = crate::paths::resolve_persistent_db_path_with_source(
        options.persistent_db,
        options.deprecated_workspace,
        options.ephemeral_only,
    );
    let persistent_mode = if resolved.path.is_some() {
        PersistentMode::PersistentAttached
    } else {
        PersistentMode::EphemeralOnly
    };
    let observed_persistent_path = resolved
        .observed_path
        .as_deref()
        .map(|path| ReportedPath::from_os_str(path.as_os_str()));
    let resolved_persistent_path = resolved.path.as_deref().map(doctor_path_facts);
    let resolved_persistent_parent = resolved
        .path
        .as_deref()
        .map(normalized_persistent_parent)
        .map(|path| doctor_path_facts(&path));

    let state_dir_result = crate::daemon::discovery::state_dir();
    let state_error_kind = state_dir_result.as_ref().err().map(io::Error::kind);
    let state_dir = state_dir_result.ok();
    let discovery_path = state_dir.as_ref().map(|path| path.join("daemon.json"));
    let daemon_state_directory = state_dir.as_deref().map(doctor_path_facts);
    let daemon_discovery_file = discovery_path.as_deref().map(doctor_path_facts);

    let client_log_path = doctor_client_log_path(resolved.path.as_deref());
    let hyperd_resolution = resolve_doctor_hyperd();

    let daemon_report = collect_real_doctor_daemon(
        discovery_path.as_deref(),
        state_error_kind,
        crate::daemon::discovery::resolve_port_scan(),
    );
    let daemon = doctor_daemon_section(&daemon_report);
    let catalog = crate::server::HyperMcpServer::doctor_catalog_snapshot(options.read_only)?;

    let mut warnings = installation
        .warnings
        .iter()
        .map(identity_doctor_warning)
        .collect::<Vec<_>>();
    warnings.extend(daemon_report.warnings.iter().map(daemon_doctor_warning));
    if let Some(kind) = state_error_kind {
        warnings.push(doctor_warning(
            "daemon_state_path_unavailable",
            format!("The daemon state path could not be resolved ({kind:?})."),
        ));
    }
    if resolved.source == crate::paths::PersistentDbPathSource::DeprecatedAlias {
        warnings.push(doctor_warning(
            "deprecated_persistent_alias",
            "The persistent path came from deprecated --workspace; use --persistent-db.",
        ));
    }
    if resolved.path.is_none() {
        // No persistent path => `doctor_client_log_path(None)` falls back to
        // `resolve_log_dir(None)`, which keys the log directory to *this*
        // doctor process's PID. A separate running MCP server logs under its
        // own per-process directory, so the reported path cannot correspond
        // to any real session — surface that instead of presenting it as fact.
        warnings.push(doctor_warning(
            "ephemeral_client_log_path_illustrative",
            "No persistent database is configured (ephemeral-only mode), so the reported client log path is derived from this doctor invocation's own temporary directory and process id. It is illustrative only: a running MCP server logs under its own per-process directory, which a separate doctor run cannot identify.",
        ));
    }
    if let Some(warning) = hyperd_resolution.warning {
        warnings.push(warning);
    }
    if let Some(verified) = daemon_report.verified.as_ref()
        && let Some(identity) = verified.record.identity()
    {
        if identity.mcp_version() != installation.mcp.source {
            warnings.push(doctor_warning(
                "daemon_client_build_mismatch",
                format!(
                    "The live daemon MCP build '{}' differs from this client build '{}'.",
                    identity.mcp_version(),
                    installation.mcp.source
                ),
            ));
        }
        if identity.executable_path() != &installation.native_executable {
            warnings.push(doctor_warning(
                "daemon_client_executable_mismatch",
                format!(
                    "The live daemon executable '{}' differs from this client executable '{}'.",
                    identity.executable_path().display,
                    installation.native_executable.display
                ),
            ));
        }
    }
    warnings.push(doctor_warning(
        "local_paths_review",
        "This report contains local paths; review it before sharing.",
    ));

    Ok(DoctorReport {
        status: DoctorStatus::Ok,
        installation: DoctorInstallationReport {
            native_executable: installation.native_executable,
            mcp_version: bounded_source_version(installation.mcp),
            hyper_rust_api_version: bounded_source_version(installation.hyper_rust_api),
            launcher: installation.launcher,
        },
        configuration: DoctorConfigurationReport {
            persistent_mode,
            persistent_path_source: resolved.source,
            observed_persistent_path,
            resolved_persistent_path,
            resolved_persistent_parent,
            daemon_state_directory,
            daemon_discovery_file,
            client_log: doctor_path_facts(&client_log_path),
            observed_hyperd_path: hyperd_resolution.observed.as_deref().map(doctor_path_facts),
            effective_hyperd_path: hyperd_resolution
                .effective
                .as_deref()
                .map(doctor_path_facts),
            upward_hyperd_candidate: hyperd_resolution
                .upward_candidate
                .as_deref()
                .map(doctor_path_facts),
            read_only: options.read_only,
            no_daemon: options.no_daemon,
        },
        daemon,
        tool_catalog: DoctorToolCatalogReport {
            tool_count: catalog.tool_count,
            canonical_tool_bytes: catalog.canonical_tool_bytes,
            initialization_instructions_bytes: catalog.initialization_instructions_bytes,
            get_readme_bytes: catalog.get_readme_bytes,
        },
        warnings,
    })
}

/// Render the typed doctor report as terminal-safe human-readable text.
#[must_use]
pub fn render_doctor_human(report: &DoctorReport) -> String {
    let mut output = String::new();
    let _ = writeln!(output, "Status");
    let _ = writeln!(output, "  Overall: ok");

    let _ = writeln!(output, "\nInstallation");
    push_human_path(
        &mut output,
        "Native executable",
        &report.installation.native_executable,
        None,
    );
    let _ = writeln!(
        output,
        "  MCP version: {}",
        escape_human(&report.installation.mcp_version.source)
    );
    let _ = writeln!(
        output,
        "  Hyper Rust API version: {}",
        escape_human(&report.installation.hyper_rust_api_version.source)
    );
    match report.installation.launcher.as_ref() {
        Some(launcher) => {
            let _ = writeln!(output, "  Launcher-reported wrapper:");
            let _ = writeln!(output, "    Name: {}", escape_human(&launcher.wrapper.name));
            let _ = writeln!(
                output,
                "    Version: {}",
                escape_human(launcher.wrapper.version.as_deref().unwrap_or("unavailable"))
            );
            push_human_path(
                &mut output,
                "    Package path",
                &launcher.wrapper.package_path,
                None,
            );
            let _ = writeln!(output, "  Launcher-reported platform:");
            let _ = writeln!(
                output,
                "    Name: {}",
                escape_human(&launcher.platform.name)
            );
            let _ = writeln!(
                output,
                "    Version: {}",
                escape_human(
                    launcher
                        .platform
                        .version
                        .as_deref()
                        .unwrap_or("unavailable")
                )
            );
            push_human_path(
                &mut output,
                "    Package path",
                &launcher.platform.package_path,
                None,
            );
            push_human_path(
                &mut output,
                "  Launcher executable",
                &launcher.executable_path,
                None,
            );
        }
        None => {
            let _ = writeln!(output, "  Launcher-reported metadata: absent");
        }
    }

    let _ = writeln!(output, "\nConfiguration");
    let _ = writeln!(
        output,
        "  Persistent mode: {}",
        persistent_mode_label(report.configuration.persistent_mode)
    );
    let _ = writeln!(
        output,
        "  Persistent path source: {}",
        persistent_source_label(report.configuration.persistent_path_source)
    );
    match report.configuration.observed_persistent_path.as_ref() {
        Some(path) => push_human_path(&mut output, "Observed persistent path", path, None),
        None => {
            let _ = writeln!(output, "  Observed persistent path: unavailable");
        }
    }
    push_optional_human_path_facts(
        &mut output,
        "Resolved persistent path",
        report.configuration.resolved_persistent_path.as_ref(),
    );
    push_optional_human_path_facts(
        &mut output,
        "Resolved persistent parent",
        report.configuration.resolved_persistent_parent.as_ref(),
    );
    push_optional_human_path_facts(
        &mut output,
        "Daemon state directory",
        report.configuration.daemon_state_directory.as_ref(),
    );
    push_optional_human_path_facts(
        &mut output,
        "Daemon discovery file",
        report.configuration.daemon_discovery_file.as_ref(),
    );
    push_human_path_facts(&mut output, "Client log", &report.configuration.client_log);
    push_optional_human_path_facts(
        &mut output,
        "Observed HYPERD_PATH",
        report.configuration.observed_hyperd_path.as_ref(),
    );
    push_optional_human_path_facts(
        &mut output,
        "Effective hyperd path",
        report.configuration.effective_hyperd_path.as_ref(),
    );
    push_optional_human_path_facts(
        &mut output,
        "Upward .hyperd/current candidate",
        report.configuration.upward_hyperd_candidate.as_ref(),
    );
    let _ = writeln!(output, "  Read only: {}", report.configuration.read_only);
    let _ = writeln!(output, "  No daemon: {}", report.configuration.no_daemon);

    let _ = writeln!(output, "\nDaemon");
    let _ = writeln!(
        output,
        "  State: {}",
        daemon_state_label(report.daemon.state)
    );
    if let Some(pid) = report.daemon.pid {
        let _ = writeln!(output, "  PID: {pid}");
    }
    if let Some(endpoint) = report.daemon.hyperd_endpoint.as_deref() {
        let _ = writeln!(output, "  Hyperd endpoint: {}", escape_human(endpoint));
    }
    if let Some(port) = report.daemon.health_port {
        let _ = writeln!(output, "  Health port: {port}");
    }
    if let Some(started_at) = report.daemon.started_at.as_deref() {
        let _ = writeln!(output, "  Started: {}", escape_human(started_at));
    }
    if let Some(version) = report.daemon.version.as_deref() {
        let _ = writeln!(output, "  Takeover version: {}", escape_human(version));
    }
    if let Some(version) = report.daemon.mcp_version.as_deref() {
        let _ = writeln!(output, "  MCP build: {}", escape_human(version));
    }
    if let Some(path) = report.daemon.executable_path.as_ref() {
        push_human_path(&mut output, "Daemon executable", path, None);
    }

    let _ = writeln!(output, "\nTool catalog");
    let _ = writeln!(output, "  Tools: {}", report.tool_catalog.tool_count);
    let _ = writeln!(
        output,
        "  Canonical generated tools bytes: {}",
        report.tool_catalog.canonical_tool_bytes
    );
    let _ = writeln!(
        output,
        "  Initialization instructions bytes: {}",
        report.tool_catalog.initialization_instructions_bytes
    );
    let _ = writeln!(
        output,
        "  get_readme bytes: {}",
        report.tool_catalog.get_readme_bytes
    );

    let _ = writeln!(output, "\nWarnings");
    if report.warnings.is_empty() {
        let _ = writeln!(output, "  None");
    } else {
        for warning in &report.warnings {
            let _ = writeln!(
                output,
                "  [{}] {}",
                escape_human(&warning.code),
                escape_human(&warning.message)
            );
        }
    }
    output
}

fn doctor_path_facts(path: &Path) -> DoctorPathFacts {
    let metadata = std::fs::metadata(path).ok();
    DoctorPathFacts {
        path: ReportedPath::from_os_str(path.as_os_str()),
        exists: metadata.is_some(),
        is_file: metadata.as_ref().is_some_and(std::fs::Metadata::is_file),
        is_directory: metadata.as_ref().is_some_and(std::fs::Metadata::is_dir),
    }
}

fn normalized_persistent_parent(path: &Path) -> PathBuf {
    match path.parent() {
        Some(parent) if parent.as_os_str().is_empty() => PathBuf::from("."),
        Some(parent) => parent.to_path_buf(),
        None => PathBuf::from("."),
    }
}

fn doctor_client_log_path(persistent_path: Option<&Path>) -> PathBuf {
    let log_dir = match persistent_path {
        // `persistent_path` is already the effective runtime path. Derive the
        // sibling log directly so a literal `~/` in HOME is not expanded a
        // second time.
        Some(path) => path
            .parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf),
        None => crate::engine::resolve_log_dir(None),
    };
    log_dir.join(crate::engine::CLIENT_LOG_FILE_NAME)
}

fn find_upward_hyperd_candidate() -> Option<PathBuf> {
    #[cfg(windows)]
    const HYPERD_EXE: &str = "hyperd.exe";
    #[cfg(not(windows))]
    const HYPERD_EXE: &str = "hyperd";

    let current_dir = std::env::current_dir().ok()?;
    current_dir
        .ancestors()
        .map(|directory| directory.join(".hyperd").join("current").join(HYPERD_EXE))
        .find(|candidate| candidate.exists())
}

struct DoctorHyperdResolution {
    observed: Option<PathBuf>,
    effective: Option<PathBuf>,
    upward_candidate: Option<PathBuf>,
    warning: Option<DoctorWarning>,
}

fn resolve_doctor_hyperd() -> DoctorHyperdResolution {
    match std::env::var("HYPERD_PATH") {
        Ok(configured) => {
            let observed = PathBuf::from(&configured);
            let (effective, warning) = resolve_configured_hyperd(&observed, &configured);
            DoctorHyperdResolution {
                observed: Some(observed),
                effective,
                upward_candidate: None,
                warning,
            }
        }
        Err(std::env::VarError::NotUnicode(configured)) => {
            let upward_candidate = find_upward_hyperd_candidate();
            DoctorHyperdResolution {
                observed: Some(PathBuf::from(configured)),
                effective: upward_candidate.clone(),
                upward_candidate,
                warning: Some(doctor_warning(
                    "non_utf8_hyperd_path_ignored",
                    "HYPERD_PATH is non-UTF-8; runtime ignores that override and uses upward .hyperd/current resolution when available.",
                )),
            }
        }
        Err(std::env::VarError::NotPresent) => {
            let upward_candidate = find_upward_hyperd_candidate();
            DoctorHyperdResolution {
                observed: None,
                effective: upward_candidate.clone(),
                upward_candidate,
                warning: None,
            }
        }
    }
}

fn resolve_configured_hyperd(
    configured: &Path,
    configured_text: &str,
) -> (Option<PathBuf>, Option<DoctorWarning>) {
    // Only the Windows `.exe` fallback below reads the raw text form.
    #[cfg(not(windows))]
    let _ = configured_text; // silence the unused-variable lint off Windows

    #[cfg(windows)]
    const HYPERD_EXE: &str = "hyperd.exe";
    #[cfg(not(windows))]
    const HYPERD_EXE: &str = "hyperd";

    if configured.is_dir() {
        let executable = configured.join(HYPERD_EXE);
        if executable.exists() {
            return (Some(executable), None);
        }
        #[cfg(windows)]
        {
            let executable_without_extension = configured.join("hyperd");
            if executable_without_extension.exists() {
                return (Some(executable_without_extension), None);
            }
        }
        return (
            None,
            Some(doctor_warning(
                "observed_hyperd_directory_missing_executable",
                format!(
                    "HYPERD_PATH is a directory, but {HYPERD_EXE} was not found in that directory."
                ),
            )),
        );
    }
    if configured.exists() {
        return (Some(configured.to_path_buf()), None);
    }
    #[cfg(windows)]
    {
        let executable = PathBuf::from(format!("{configured_text}.exe"));
        if executable.exists() {
            return (Some(executable), None);
        }
    }
    (
        None,
        Some(doctor_warning(
            "observed_hyperd_path_missing",
            "HYPERD_PATH was observed, but the configured hyperd executable was not found.",
        )),
    )
}

fn collect_real_doctor_daemon(
    discovery_path: Option<&Path>,
    state_error_kind: Option<io::ErrorKind>,
    ports: PortScan,
) -> DoctorDaemonReport {
    let origin = Instant::now();
    let probing_scan = std::cell::Cell::new(false);
    let read_raw_discovery = || match discovery_path {
        Some(path) => crate::daemon::discovery::read_discovery_file_raw(path),
        None => RawDiscoveryRead::Unreadable {
            path: ReportedPath::from_os_str(OsStr::new("")),
            kind: state_error_kind.unwrap_or(io::ErrorKind::NotFound),
        },
    };
    let probe_enriched_status = |port: u16, deadline: DoctorDeadline| {
        // Preserve the established scan handshake, but verify each identified
        // port immediately instead of gathering PONGs across the whole range.
        // Discovery candidates already have a recorded identity and go
        // straight to the stronger fresh STATUS verification.
        if probing_scan.get() {
            match send_doctor_command(port, "PING", origin, deadline) {
                Ok(response) if is_identified_doctor_pong(&response) => {}
                _ => return DoctorStatusProbe::Unreachable,
            }
        }

        match send_doctor_command(port, "STATUS", origin, deadline) {
            Ok(response) => DoctorStatusProbe::Response(response),
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                DoctorStatusProbe::Response(String::new())
            }
            Err(_) => DoctorStatusProbe::Unreachable,
        }
    };
    let scan_candidates = |request: DoctorScanRequest| {
        probing_scan.set(true);
        let mut candidates = Vec::new();
        for offset in 0..request.ports.span {
            let Some(port) = request.ports.base.checked_add(offset) else {
                break;
            };
            candidates.push(DoctorScanCandidate {
                responding_port: port,
            });
        }
        candidates
    };
    let now = || DoctorMoment(elapsed_millis(origin));
    let deadline_after = |now: DoctorMoment, timeout: Duration| {
        DoctorDeadline(now.0.saturating_add(duration_millis(timeout)))
    };
    let dependencies = DoctorCollectorDependencies {
        read_raw_discovery: &read_raw_discovery,
        probe_enriched_status: &probe_enriched_status,
        scan_candidates: &scan_candidates,
        now: &now,
        deadline_after: &deadline_after,
    };
    collect_doctor_daemon(
        &dependencies,
        DoctorCollectRequest {
            ports,
            timeout: DOCTOR_DAEMON_TIMEOUT,
        },
    )
}

fn doctor_network_timeout(origin: Instant, deadline: DoctorDeadline) -> io::Result<Duration> {
    let Some(remaining_millis) = deadline.0.checked_sub(elapsed_millis(origin)) else {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "doctor daemon deadline elapsed",
        ));
    };
    if remaining_millis == 0 {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "doctor daemon deadline elapsed",
        ));
    }
    Ok(Duration::from_millis(remaining_millis).min(DOCTOR_NETWORK_PHASE_TIMEOUT))
}

fn elapsed_millis(origin: Instant) -> u64 {
    duration_millis(origin.elapsed())
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn send_doctor_command(
    port: u16,
    command: &str,
    origin: Instant,
    deadline: DoctorDeadline,
) -> io::Result<String> {
    use std::io::{Read, Write};

    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let connect_timeout = doctor_network_timeout(origin, deadline)?;
    let mut stream = std::net::TcpStream::connect_timeout(&address, connect_timeout)?;
    let message = format!("{command}\n");
    let mut written = 0;
    while written < message.len() {
        let timeout = doctor_network_timeout(origin, deadline)?;
        stream.set_write_timeout(Some(timeout))?;
        match stream.write(&message.as_bytes()[written..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "doctor health peer stopped accepting the request",
                ));
            }
            Ok(count) => written += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }

    let mut response = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let timeout = doctor_network_timeout(origin, deadline)?;
        stream.set_read_timeout(Some(timeout))?;
        let remaining_capacity = MAX_STATUS_RESPONSE_BYTES
            .saturating_add(1)
            .saturating_sub(response.len());
        if remaining_capacity == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "doctor health response exceeded its fixed limit",
            ));
        }
        let read_capacity = remaining_capacity.min(chunk.len());
        match stream.read(&mut chunk[..read_capacity]) {
            Ok(0) => break,
            Ok(count) => {
                let line_end = chunk[..count]
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .map_or(count, |index| index + 1);
                response.extend_from_slice(&chunk[..line_end]);
                if response.len() > MAX_STATUS_RESPONSE_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "doctor health response exceeded its fixed limit",
                    ));
                }
                if response.last() == Some(&b'\n') {
                    break;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    String::from_utf8(response)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))
}

fn is_identified_doctor_pong(response: &str) -> bool {
    let mut tokens = response.split_whitespace();
    tokens.next() == Some("PONG") && tokens.next() == Some(crate::daemon::health::PONG_TOKEN)
}

fn doctor_daemon_section(report: &DoctorDaemonReport) -> DoctorDaemonSection {
    let Some(verified) = report.verified.as_ref() else {
        return DoctorDaemonSection {
            state: report.state,
            pid: None,
            hyperd_endpoint: None,
            health_port: None,
            started_at: None,
            version: None,
            mcp_version: None,
            executable_path: None,
        };
    };
    let identity = verified.record.identity();
    DoctorDaemonSection {
        state: report.state,
        pid: Some(verified.record.info().pid),
        hyperd_endpoint: Some(bounded_string(&verified.record.info().hyperd_endpoint)),
        health_port: Some(verified.record.info().health_port),
        started_at: Some(bounded_string(&verified.record.info().started_at)),
        version: Some(bounded_string(&verified.record.info().version)),
        mcp_version: identity.map(|identity| bounded_string(identity.mcp_version())),
        executable_path: identity.map(|identity| identity.executable_path().clone()),
    }
}

fn bounded_source_version(mut version: SourceVersionIdentity) -> SourceVersionIdentity {
    truncate_utf8(&mut version.source, MAX_REPORTED_STRING_BYTES);
    if let Some(value) = version.version.as_mut() {
        truncate_utf8(value, MAX_REPORTED_STRING_BYTES);
    }
    if let Some(value) = version.build.as_mut() {
        truncate_utf8(value, MAX_REPORTED_STRING_BYTES);
    }
    version
}

fn bounded_string(value: &str) -> String {
    let mut bounded = value.to_owned();
    truncate_utf8(&mut bounded, MAX_REPORTED_STRING_BYTES);
    bounded
}

fn doctor_warning(code: impl Into<String>, message: impl Into<String>) -> DoctorWarning {
    let mut code = code.into();
    let mut message = message.into();
    truncate_utf8(&mut code, MAX_REPORTED_STRING_BYTES);
    truncate_utf8(&mut message, MAX_REPORTED_STRING_BYTES);
    DoctorWarning { code, message }
}

fn identity_doctor_warning(warning: &IdentityWarning) -> DoctorWarning {
    match warning {
        IdentityWarning::MalformedLauncherInfo => doctor_warning(
            "malformed_launcher_info",
            "HYPERDB_MCP_LAUNCHER_INFO was malformed and was ignored.",
        ),
        IdentityWarning::LauncherInfoTooLarge => doctor_warning(
            "launcher_info_too_large",
            "HYPERDB_MCP_LAUNCHER_INFO exceeded 16 KiB and was ignored.",
        ),
        IdentityWarning::LauncherFieldTooLarge { field } => doctor_warning(
            "launcher_field_too_large",
            format!(
                "Launcher field '{field}' exceeded 4 KiB and all launcher metadata was ignored."
            ),
        ),
        IdentityWarning::MalformedVersion { component } => doctor_warning(
            "malformed_version",
            format!("The {component} value was not valid semantic version identity."),
        ),
        IdentityWarning::VersionMismatch {
            native,
            wrapper,
            platform,
        } => doctor_warning(
            "launcher_native_version_mismatch",
            format!(
                "Launcher package versions differ from native {native}: wrapper={}, platform={}.",
                wrapper.as_deref().unwrap_or("unavailable"),
                platform.as_deref().unwrap_or("unavailable")
            ),
        ),
    }
}

fn daemon_doctor_warning(warning: &DoctorDaemonWarning) -> DoctorWarning {
    match warning {
        DoctorDaemonWarning::DiscoveryUnreadable { kind } => doctor_warning(
            "daemon_discovery_unreadable",
            format!("The daemon discovery file was unreadable ({kind:?})."),
        ),
        DoctorDaemonWarning::MalformedDiscovery => doctor_warning(
            "daemon_discovery_malformed",
            "The daemon discovery file was malformed; it was left unchanged.",
        ),
        DoctorDaemonWarning::DiscoveryCandidateUnreachable { responding_port } => doctor_warning(
            "daemon_discovery_candidate_unreachable",
            format!(
                "The recorded daemon candidate on port {responding_port} did not return fresh enriched STATUS."
            ),
        ),
        DoctorDaemonWarning::StaleOrReplacedDiscovery { mismatches } => {
            let facts = mismatches
                .iter()
                .map(discovery_mismatch_message)
                .collect::<Vec<_>>()
                .join("; ");
            doctor_warning(
                "daemon_discovery_stale_or_replaced",
                format!("Fresh daemon STATUS disagreed with the discovery record: {facts}."),
            )
        }
        DoctorDaemonWarning::StatusHealthPortMismatch {
            responding_port,
            reported_port,
        } => doctor_warning(
            "daemon_status_health_port_mismatch",
            format!(
                "STATUS from port {responding_port} reported health port {reported_port}; the candidate was rejected."
            ),
        ),
        DoctorDaemonWarning::MalformedStatus { responding_port } => doctor_warning(
            "daemon_status_malformed",
            format!(
                "Port {responding_port} returned malformed or unenriched STATUS; the candidate was rejected."
            ),
        ),
    }
}

fn discovery_mismatch_message(mismatch: &DiscoveryFactMismatch) -> String {
    match mismatch {
        DiscoveryFactMismatch::Pid { recorded, fresh } => {
            format!("PID recorded={recorded} fresh={fresh}")
        }
        DiscoveryFactMismatch::McpVersion { recorded, fresh } => {
            format!("MCP build recorded='{recorded}' fresh='{fresh}'")
        }
        DiscoveryFactMismatch::ExecutablePath { recorded, fresh } => format!(
            "executable recorded='{}' fresh='{}'",
            recorded.display, fresh.display
        ),
    }
}

fn escape_human(value: &str) -> String {
    let mut escaped = String::new();
    for character in value.chars() {
        if character <= '\u{1f}' || character == '\u{7f}' {
            let _ = write!(escaped, "\\u{{{:x}}}", u32::from(character));
        } else {
            escaped.push(character);
        }
    }
    truncate_utf8(&mut escaped, MAX_REPORTED_STRING_BYTES);
    escaped
}

fn push_optional_human_path_facts(
    output: &mut String,
    label: &str,
    facts: Option<&DoctorPathFacts>,
) {
    match facts {
        Some(facts) => push_human_path_facts(output, label, facts),
        None => {
            let _ = writeln!(output, "  {label}: unavailable");
        }
    }
}

fn push_human_path_facts(output: &mut String, label: &str, facts: &DoctorPathFacts) {
    push_human_path(
        output,
        label,
        &facts.path,
        Some((facts.exists, facts.is_file, facts.is_directory)),
    );
}

fn push_human_path(
    output: &mut String,
    label: &str,
    path: &ReportedPath,
    facts: Option<(bool, bool, bool)>,
) {
    let encoding = match path.encoding {
        PathEncoding::Utf8 => "utf8",
        PathEncoding::Lossy => "lossy",
    };
    match facts {
        Some((exists, is_file, is_directory)) => {
            let _ = writeln!(
                output,
                "  {label}: {} (encoding: {encoding}; exists: {exists}; file: {is_file}; directory: {is_directory})",
                escape_human(&path.display)
            );
        }
        None => {
            let _ = writeln!(
                output,
                "  {label}: {} (encoding: {encoding})",
                escape_human(&path.display)
            );
        }
    }
}

const fn persistent_mode_label(mode: PersistentMode) -> &'static str {
    match mode {
        PersistentMode::PersistentAttached => "persistent_attached",
        PersistentMode::EphemeralOnly => "ephemeral_only",
    }
}

const fn persistent_source_label(source: crate::paths::PersistentDbPathSource) -> &'static str {
    match source {
        crate::paths::PersistentDbPathSource::Cli => "cli",
        crate::paths::PersistentDbPathSource::DeprecatedAlias => "deprecated_alias",
        crate::paths::PersistentDbPathSource::Environment => "environment",
        crate::paths::PersistentDbPathSource::PlatformDefault => "platform_default",
        crate::paths::PersistentDbPathSource::Disabled => "disabled",
    }
}

const fn daemon_state_label(state: DoctorDaemonState) -> &'static str {
    match state {
        DoctorDaemonState::Missing => "missing",
        DoctorDaemonState::Unreadable => "unreadable",
        DoctorDaemonState::Malformed => "malformed",
        DoctorDaemonState::ParsedUnreachable => "parsed_unreachable",
        DoctorDaemonState::LiveFromDiscovery => "live_from_discovery",
        DoctorDaemonState::LiveFromScan => "live_from_scan",
    }
}

#[derive(Deserialize)]
struct RawLauncherPackageIdentity {
    name: String,
    version: Option<String>,
    package_path: String,
}

#[derive(Deserialize)]
struct RawLauncherIdentity {
    wrapper: RawLauncherPackageIdentity,
    platform: RawLauncherPackageIdentity,
    executable_path: String,
}

/// Parse launcher metadata without reading or mutating process environment.
#[must_use]
pub fn parse_launcher_identity(value: Option<&OsStr>) -> ParsedLauncherIdentity {
    let Some(value) = value else {
        return ParsedLauncherIdentity {
            identity: None,
            warnings: Vec::new(),
        };
    };

    if value.as_encoded_bytes().len() > MAX_LAUNCHER_INFO_BYTES {
        return rejected_launcher(IdentityWarning::LauncherInfoTooLarge);
    }

    let Some(value) = value.to_str() else {
        return rejected_launcher(IdentityWarning::MalformedLauncherInfo);
    };
    let Ok(raw) = serde_json::from_str::<RawLauncherIdentity>(value) else {
        return rejected_launcher(IdentityWarning::MalformedLauncherInfo);
    };

    for (field, value) in raw_launcher_fields(&raw) {
        if value.len() > MAX_REPORTED_STRING_BYTES {
            return rejected_launcher(IdentityWarning::LauncherFieldTooLarge {
                field: field.to_owned(),
            });
        }
    }

    ParsedLauncherIdentity {
        identity: Some(LauncherIdentity {
            wrapper: launcher_package_identity(raw.wrapper),
            platform: launcher_package_identity(raw.platform),
            executable_path: ReportedPath::from_os_str(OsStr::new(&raw.executable_path)),
        }),
        warnings: Vec::new(),
    }
}

/// Build installation identity from injected authoritative facts.
#[must_use]
pub fn installation_identity_from_parts(
    native_executable: &OsStr,
    mcp_version: &str,
    hyper_rust_api_version: &str,
    launcher_info: Option<&OsStr>,
) -> InstallationIdentity {
    let parsed_launcher = parse_launcher_identity(launcher_info);
    let mut warnings = parsed_launcher.warnings;

    let (mcp, native_version) = parse_source_version(mcp_version);
    if native_version.is_none() {
        warnings.push(IdentityWarning::MalformedVersion {
            component: "mcp.version".to_owned(),
        });
    }

    let (hyper_rust_api, hyper_version) = parse_source_version(hyper_rust_api_version);
    if hyper_version.is_none() {
        warnings.push(IdentityWarning::MalformedVersion {
            component: "hyper_rust_api.version".to_owned(),
        });
    }

    if let Some(launcher) = parsed_launcher.identity.as_ref() {
        let wrapper_version = parse_launcher_version(
            launcher.wrapper.version.as_deref(),
            "wrapper.version",
            &mut warnings,
        );
        let platform_version = parse_launcher_version(
            launcher.platform.version.as_deref(),
            "platform.version",
            &mut warnings,
        );

        if let Some(native_version) = native_version.as_ref() {
            let wrapper_mismatch = wrapper_version
                .as_ref()
                .is_some_and(|version| version != native_version);
            let platform_mismatch = platform_version
                .as_ref()
                .is_some_and(|version| version != native_version);
            if wrapper_mismatch || platform_mismatch {
                warnings.push(IdentityWarning::VersionMismatch {
                    native: native_version.to_string(),
                    wrapper: wrapper_version.map(|version| version.to_string()),
                    platform: platform_version.map(|version| version.to_string()),
                });
            }
        }
    }

    InstallationIdentity {
        native_executable: ReportedPath::from_os_str(native_executable),
        mcp,
        hyper_rust_api,
        launcher: parsed_launcher.identity,
        warnings,
    }
}

fn truncate_utf8(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }

    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
}

fn rejected_launcher(warning: IdentityWarning) -> ParsedLauncherIdentity {
    ParsedLauncherIdentity {
        identity: None,
        warnings: vec![warning],
    }
}

fn raw_launcher_fields(raw: &RawLauncherIdentity) -> [(&'static str, &str); 7] {
    [
        ("wrapper.name", raw.wrapper.name.as_str()),
        (
            "wrapper.version",
            raw.wrapper.version.as_deref().unwrap_or_default(),
        ),
        ("wrapper.package_path", raw.wrapper.package_path.as_str()),
        ("platform.name", raw.platform.name.as_str()),
        (
            "platform.version",
            raw.platform.version.as_deref().unwrap_or_default(),
        ),
        ("platform.package_path", raw.platform.package_path.as_str()),
        ("executable_path", raw.executable_path.as_str()),
    ]
}

fn launcher_package_identity(raw: RawLauncherPackageIdentity) -> LauncherPackageIdentity {
    LauncherPackageIdentity {
        name: raw.name,
        version: raw.version,
        package_path: ReportedPath::from_os_str(OsStr::new(&raw.package_path)),
    }
}

fn parse_source_version(source: &str) -> (SourceVersionIdentity, Option<Version>) {
    let (version, build, suffix_is_valid) = match source.rsplit_once(".r") {
        Some((version, build)) => (
            version,
            (!build.is_empty()).then(|| build.to_owned()),
            !build.is_empty(),
        ),
        None => (source, None, true),
    };
    let parsed = suffix_is_valid
        .then(|| Version::parse(version).ok())
        .flatten();

    (
        SourceVersionIdentity {
            source: source.to_owned(),
            version: parsed.as_ref().map(ToString::to_string),
            build,
        },
        parsed,
    )
}

fn parse_launcher_version(
    version: Option<&str>,
    component: &'static str,
    warnings: &mut Vec<IdentityWarning>,
) -> Option<Version> {
    let version = version?;
    if let Ok(version) = Version::parse(version) {
        Some(version)
    } else {
        warnings.push(IdentityWarning::MalformedVersion {
            component: component.to_owned(),
        });
        None
    }
}

#[cfg(test)]
pub(crate) fn real_network_test_guard() -> std::sync::MutexGuard<'static, ()> {
    static REAL_NETWORK_TEST_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> =
        std::sync::OnceLock::new();
    REAL_NETWORK_TEST_LOCK
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::ffi::OsStr;
    use std::io;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use serde_json::{Value, json};
    use tempfile::TempDir;

    use crate::daemon::discovery::{
        DaemonBuildIdentity, DaemonInfo, DaemonRecord, PortScan, RawDiscoveryRead,
        read_discovery_file_raw,
    };
    use crate::daemon::health::{DaemonState, HealthListener};

    use super::{
        DiscoveryFactMismatch, DoctorCollectRequest, DoctorCollectorDependencies,
        DoctorDaemonState, DoctorDaemonWarning, DoctorDeadline, DoctorMoment, DoctorScanCandidate,
        DoctorScanRequest, DoctorStatusProbe, ReportedPath, collect_doctor_daemon,
        collect_real_doctor_daemon, real_network_test_guard,
    };

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum RawFixture {
        Missing,
        Unreadable(io::ErrorKind),
        Malformed,
        Parsed(Value),
    }

    impl RawFixture {
        fn read(&self) -> RawDiscoveryRead {
            let path = ReportedPath::from_os_str(OsStr::new("/virtual/state/daemon.json"));
            match self {
                Self::Missing => RawDiscoveryRead::Missing { path },
                Self::Unreadable(kind) => RawDiscoveryRead::Unreadable { path, kind: *kind },
                Self::Malformed => RawDiscoveryRead::Malformed { path },
                Self::Parsed(value) => RawDiscoveryRead::Parsed {
                    path,
                    record: serde_json::from_value(value.clone()).unwrap(),
                },
            }
        }
    }

    fn enriched_status(pid: u32, health_port: u16, build: &str, executable: &str) -> Value {
        json!({
            "pid": pid,
            "hyperd_endpoint": "127.0.0.1:54321",
            "health_port": health_port,
            "started_at": "2026-08-13T12:34:56Z",
            "version": "0.7.0",
            "identity": {
                "mcp_version": build,
                "executable_path": ReportedPath::from_os_str(OsStr::new(executable))
            }
        })
    }

    fn listener_daemon_info(pid: u32, health_port: u16) -> DaemonInfo {
        DaemonInfo {
            pid,
            hyperd_endpoint: "127.0.0.1:54321".to_string(),
            health_port,
            started_at: "2026-08-13T12:34:56Z".to_string(),
            version: "0.7.0".to_string(),
        }
    }

    fn adjacent_fake_and_health_listeners() -> (std::net::TcpListener, HealthListener) {
        for _ in 0..128 {
            let fake = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let fake_port = fake.local_addr().unwrap().port();
            let Some(health_port) = fake_port.checked_add(1) else {
                continue;
            };
            if let Ok(health) = HealthListener::bind(health_port) {
                return (fake, health);
            }
        }
        panic!("could not reserve adjacent OS-selected loopback listeners after 128 attempts");
    }

    fn health_listener_with_adjacent_followers(
        follower_count: u16,
    ) -> (HealthListener, Vec<std::net::TcpListener>) {
        for _ in 0..128 {
            let health = HealthListener::bind(0).unwrap();
            let Some(last_port) = health.port.checked_add(follower_count) else {
                continue;
            };
            let mut followers = Vec::with_capacity(usize::from(follower_count));
            for port in health.port + 1..=last_port {
                let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", port)) else {
                    break;
                };
                followers.push(listener);
            }
            if followers.len() == usize::from(follower_count) {
                return (health, followers);
            }
        }
        panic!("could not reserve a contiguous OS-selected loopback range");
    }

    fn run_invalid_status_then_later_daemon_attempt() -> Vec<String> {
        use std::io::{BufRead as _, BufReader, Write as _};

        let tmp = TempDir::new().unwrap();
        let (fake_listener, health_listener) = adjacent_fake_and_health_listeners();
        let fake_port = fake_listener.local_addr().unwrap().port();
        let health_port = health_listener.port;
        fake_listener.set_nonblocking(true).unwrap();

        let stop_fake = Arc::new(AtomicBool::new(false));
        let fake_stop = Arc::clone(&stop_fake);
        let served_ping = Arc::new(AtomicUsize::new(0));
        let fake_served_ping = Arc::clone(&served_ping);
        let served_status = Arc::new(AtomicUsize::new(0));
        let fake_served_status = Arc::clone(&served_status);
        let (fake_ready_sender, fake_ready_receiver) = mpsc::sync_channel(1);
        let wrong_port_status = enriched_status(
            9_090,
            health_port,
            "0.7.0.rwrong-port",
            "/opt/hyperdb/wrong-port-daemon",
        )
        .to_string();
        let fake_server = std::thread::spawn(move || -> Result<(), String> {
            fake_ready_sender
                .send(())
                .map_err(|_| "fake candidate readiness receiver closed".to_string())?;
            let deadline = Instant::now() + Duration::from_secs(2);
            while !fake_stop.load(Ordering::Acquire) && Instant::now() < deadline {
                let mut stream = match fake_listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => return Err(format!("fake candidate accept failed: {error}")),
                };
                stream
                    .set_read_timeout(Some(Duration::from_millis(200)))
                    .map_err(|error| error.to_string())?;
                let mut command = String::new();
                let read_result =
                    BufReader::new(stream.try_clone().map_err(|error| error.to_string())?)
                        .read_line(&mut command);
                match read_result {
                    Ok(0) if command.is_empty() => continue,
                    Err(error)
                        if command.is_empty()
                            && matches!(
                                error.kind(),
                                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                            ) =>
                    {
                        continue;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        return Err(format!(
                            "fake candidate command read failed after {} bytes: {error}",
                            command.len()
                        ));
                    }
                }
                let (response, served) = match command.trim() {
                    "PING" => ("PONG hyperdb-mcp 0.7.0\n".to_string(), &fake_served_ping),
                    "STATUS" => (format!("{wrong_port_status}\n"), &fake_served_status),
                    other => return Err(format!("fake candidate received {other:?}")),
                };
                stream
                    .write_all(response.as_bytes())
                    .map_err(|error| error.to_string())?;
                served.fetch_add(1, Ordering::AcqRel);
            }
            Ok(())
        });

        let health_state = Arc::new(DaemonState::new());
        let health_info = Arc::new(Mutex::new(listener_daemon_info(9_191, health_port)));
        let run_state = Arc::clone(&health_state);
        let run_info = Arc::clone(&health_info);
        let (health_ready_sender, health_ready_receiver) = mpsc::sync_channel(1);
        let health_server = std::thread::spawn(move || {
            let _ = health_ready_sender.send(());
            health_listener.run(run_state, run_info);
        });

        let mut failures = Vec::new();
        let fake_ready = fake_ready_receiver
            .recv_timeout(Duration::from_millis(500))
            .is_ok();
        if !fake_ready {
            failures.push("fake candidate did not signal readiness within 500ms".to_string());
        }
        let health_ready = health_ready_receiver
            .recv_timeout(Duration::from_millis(500))
            .is_ok();
        if !health_ready {
            failures.push("later HealthListener did not signal readiness within 500ms".to_string());
        }

        let report = if fake_ready && health_ready {
            if let Ok(report) = catch_unwind(AssertUnwindSafe(|| {
                collect_real_doctor_daemon(
                    Some(&tmp.path().join("missing-daemon.json")),
                    None,
                    PortScan {
                        base: fake_port,
                        span: 2,
                    },
                )
            })) {
                Some(report)
            } else {
                failures.push("doctor collector panicked during adjacent scan".to_string());
                None
            }
        } else {
            None
        };

        stop_fake.store(true, Ordering::Release);
        health_state.request_shutdown();
        match fake_server.join() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => failures.push(error),
            Err(_) => failures.push("fake candidate server panicked".to_string()),
        }
        if health_server.join().is_err() {
            failures.push("later HealthListener server panicked".to_string());
        }

        let ping_count = served_ping.load(Ordering::Acquire);
        let status_count = served_status.load(Ordering::Acquire);
        if ping_count == 0 || status_count == 0 {
            failures.push(format!(
                "fake candidate served PING {ping_count} time(s) and STATUS {status_count} time(s); both must be demonstrated"
            ));
        }
        if let Some(report) = report {
            if report.state != DoctorDaemonState::LiveFromScan {
                failures.push(format!(
                    "adjacent scan state was {:?}, expected LiveFromScan",
                    report.state
                ));
            }
            match report.verified.as_ref() {
                Some(verified)
                    if verified.responding_port == health_port
                        && verified.record.info().health_port == health_port
                        && verified.record.info().pid == 9_191 => {}
                other => failures.push(format!(
                    "later real HealthListener did not supply exact fresh facts: {other:?}"
                )),
            }
            if !report.warnings.iter().any(|warning| {
                matches!(
                    warning,
                    DoctorDaemonWarning::StatusHealthPortMismatch {
                        responding_port,
                        reported_port,
                    } if *responding_port == fake_port && *reported_port == health_port
                )
            }) {
                failures.push(format!(
                    "first candidate's wrong-port STATUS was not retained as a warning: {:?}",
                    report.warnings
                ));
            }
        }

        failures
    }

    #[test]
    fn real_scan_skips_invalid_status_candidate_and_finds_later_daemon() {
        const MAX_SCENARIO_ATTEMPTS: usize = 3;

        let _network_guard = real_network_test_guard();
        let mut attempt_failures = Vec::new();
        for attempt in 1..=MAX_SCENARIO_ATTEMPTS {
            let failures = run_invalid_status_then_later_daemon_attempt();
            if failures.is_empty() {
                return;
            }
            attempt_failures.push(format!("attempt {attempt}:\n{}", failures.join("\n")));
        }

        panic!(
            "adjacent candidate scan failed all {MAX_SCENARIO_ATTEMPTS} bounded attempts:\n{}",
            attempt_failures.join("\n")
        );
    }

    #[test]
    fn real_scan_verifies_early_daemon_before_slow_later_ports() {
        use std::io::{BufRead as _, BufReader, Write as _};

        const FOLLOWER_COUNT: u16 = 4;

        let _network_guard = real_network_test_guard();
        let (health_listener, slow_listeners) =
            health_listener_with_adjacent_followers(FOLLOWER_COUNT);
        let health_port = health_listener.port;

        let health_state = Arc::new(DaemonState::new());
        let health_info = Arc::new(Mutex::new(listener_daemon_info(7_171, health_port)));
        let run_state = Arc::clone(&health_state);
        let run_info = Arc::clone(&health_info);
        let health_server = std::thread::spawn(move || health_listener.run(run_state, run_info));

        let stop_slow_peers = Arc::new(AtomicBool::new(false));
        let slow_peer_commands = Arc::new(AtomicUsize::new(0));
        let mut slow_servers = Vec::new();
        for listener in slow_listeners {
            listener.set_nonblocking(true).unwrap();
            let stop = Arc::clone(&stop_slow_peers);
            let command_count = Arc::clone(&slow_peer_commands);
            slow_servers.push(std::thread::spawn(move || -> Result<(), String> {
                let deadline = Instant::now() + Duration::from_secs(2);
                while !stop.load(Ordering::Acquire) && Instant::now() < deadline {
                    let mut stream = match listener.accept() {
                        Ok((stream, _)) => stream,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(2));
                            continue;
                        }
                        Err(error) => {
                            return Err(format!("slow follower accept failed: {error}"));
                        }
                    };
                    stream
                        .set_read_timeout(Some(Duration::from_millis(200)))
                        .map_err(|error| error.to_string())?;
                    let mut command = String::new();
                    let read_result =
                        BufReader::new(stream.try_clone().map_err(|error| error.to_string())?)
                            .read_line(&mut command);
                    match read_result {
                        Ok(0) if command.is_empty() => continue,
                        Err(error)
                            if command.is_empty()
                                && matches!(
                                    error.kind(),
                                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                                ) =>
                        {
                            continue;
                        }
                        Ok(_) => {}
                        Err(error) => {
                            return Err(format!(
                                "slow follower command read failed after {} bytes: {error}",
                                command.len()
                            ));
                        }
                    }
                    if command.trim() != "PING" {
                        return Err(format!(
                            "slow follower received unexpected command {command:?}"
                        ));
                    }
                    command_count.fetch_add(1, Ordering::AcqRel);

                    let response_at = Instant::now() + Duration::from_millis(110);
                    while !stop.load(Ordering::Acquire) && Instant::now() < response_at {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    match stream.write_all(b"PONG hyperdb-mcp 0.7.0\n") {
                        Ok(()) => {}
                        Err(error)
                            if matches!(
                                error.kind(),
                                io::ErrorKind::BrokenPipe
                                    | io::ErrorKind::ConnectionReset
                                    | io::ErrorKind::NotConnected
                            ) => {}
                        Err(error) => {
                            return Err(format!("slow follower PONG failed: {error}"));
                        }
                    }
                }
                Ok(())
            }));
        }

        let tmp = TempDir::new().unwrap();
        let missing_discovery = tmp.path().join("missing-daemon.json");
        let (result_sender, result_receiver) = mpsc::channel();
        let collector = std::thread::spawn(move || {
            let started = Instant::now();
            let report = collect_real_doctor_daemon(
                Some(&missing_discovery),
                None,
                PortScan {
                    base: health_port,
                    span: FOLLOWER_COUNT + 1,
                },
            );
            let _ = result_sender.send((report, started.elapsed()));
        });

        let bounded_result = result_receiver.recv_timeout(Duration::from_millis(650));
        stop_slow_peers.store(true, Ordering::Release);
        health_state.request_shutdown();
        let slow_results = slow_servers
            .into_iter()
            .map(|server| server.join().unwrap())
            .collect::<Vec<_>>();
        health_server.join().unwrap();
        collector.join().unwrap();

        let mut failures = slow_results
            .into_iter()
            .filter_map(Result::err)
            .collect::<Vec<_>>();
        match bounded_result {
            Ok((report, elapsed)) => {
                if elapsed > Duration::from_millis(650) {
                    failures.push(format!(
                        "early-daemon scan completed after its 650ms watchdog: {elapsed:?}"
                    ));
                }
                if report.state != DoctorDaemonState::LiveFromScan {
                    failures.push(format!(
                        "early-daemon scan state was {:?}, expected LiveFromScan",
                        report.state
                    ));
                }
                match report.verified {
                    Some(verified)
                        if verified.responding_port == health_port
                            && verified.record.info().health_port == health_port
                            && verified.record.info().pid == 7_171 => {}
                    other => failures.push(format!(
                        "healthy first port did not supply exact fresh daemon facts: {other:?}"
                    )),
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => failures.push(
                "later identified peers starved the healthy first port past 650ms".to_string(),
            ),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                failures.push("early-daemon collector disconnected without a report".to_string());
            }
        }
        let later_commands = slow_peer_commands.load(Ordering::Acquire);
        if later_commands != 0 {
            failures.push(format!(
                "{later_commands} later peer command(s) ran after the healthy first port"
            ));
        }

        assert!(
            failures.is_empty(),
            "early daemon scan failures:\n{}",
            failures.join("\n")
        );
    }

    #[test]
    fn real_health_listener_accept_cadence_fits_doctor_budget() {
        let _network_guard = real_network_test_guard();
        let listener = HealthListener::bind(0).unwrap();
        let port = listener.port;
        let state = Arc::new(DaemonState::new());
        let info = Arc::new(Mutex::new(listener_daemon_info(8_181, port)));
        let run_state = Arc::clone(&state);
        let run_info = Arc::clone(&info);
        let listener_thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(75));
            listener.run(run_state, run_info);
        });

        let tmp = TempDir::new().unwrap();
        let missing_discovery = tmp.path().join("missing-daemon.json");
        let started = Instant::now();
        let report = collect_real_doctor_daemon(
            Some(&missing_discovery),
            None,
            PortScan {
                base: port,
                span: 1,
            },
        );
        let elapsed = started.elapsed();

        state.request_shutdown();
        listener_thread.join().unwrap();

        let mut failures = Vec::new();
        if report.state != DoctorDaemonState::LiveFromScan {
            failures.push(format!(
                "real HealthListener state was {:?}, expected LiveFromScan",
                report.state
            ));
        }
        match report.verified {
            Some(verified)
                if verified.responding_port == port
                    && verified.record.info().health_port == port
                    && verified.record.info().pid == 8_181 => {}
            other => failures.push(format!(
                "real HealthListener did not yield exact fresh daemon facts: {other:?}"
            )),
        }
        if elapsed > Duration::from_millis(650) {
            failures.push(format!(
                "real HealthListener collection exceeded the 650ms watchdog: {elapsed:?}"
            ));
        }

        assert!(
            failures.is_empty(),
            "real HealthListener budget failures:\n{}",
            failures.join("\n")
        );
    }

    #[test]
    fn real_doctor_collector_enforces_global_deadline_against_slow_drip() {
        use std::io::{BufRead as _, BufReader, Write as _};
        use std::net::{Shutdown, TcpListener};

        let _network_guard = real_network_test_guard();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let stop_writer = Arc::new(AtomicBool::new(false));
        let server_stop = Arc::clone(&stop_writer);
        let server = std::thread::spawn(move || -> Result<(), String> {
            let accept_deadline = Instant::now() + Duration::from_secs(2);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        if server_stop.load(Ordering::Acquire) || Instant::now() >= accept_deadline
                        {
                            return Err("slow-drip peer never received a connection".to_string());
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => return Err(format!("slow-drip accept failed: {error}")),
                }
            };
            stream
                .set_nodelay(true)
                .map_err(|error| error.to_string())?;
            stream
                .set_read_timeout(Some(Duration::from_millis(200)))
                .map_err(|error| error.to_string())?;
            let mut command = String::new();
            BufReader::new(stream.try_clone().map_err(|error| error.to_string())?)
                .read_line(&mut command)
                .map_err(|error| error.to_string())?;
            if command.trim() != "PING" {
                return Err(format!(
                    "slow-drip peer received unexpected command {command:?}"
                ));
            }

            let write_deadline = Instant::now() + Duration::from_secs(2);
            while !server_stop.load(Ordering::Acquire) && Instant::now() < write_deadline {
                if stream.write_all(b"x").is_err() || stream.flush().is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = stream.shutdown(Shutdown::Both);
            Ok(())
        });

        let tmp = TempDir::new().unwrap();
        let missing_discovery = tmp.path().join("missing-daemon.json");
        let (result_sender, result_receiver) = mpsc::channel();
        let collector = std::thread::spawn(move || {
            let started = Instant::now();
            let report = collect_real_doctor_daemon(
                Some(&missing_discovery),
                None,
                PortScan {
                    base: port,
                    span: 1,
                },
            );
            let _ = result_sender.send((report, started.elapsed()));
        });

        let bounded_result = result_receiver.recv_timeout(Duration::from_millis(650));
        stop_writer.store(true, Ordering::Release);
        let server_result = server.join().unwrap();
        collector.join().unwrap();

        let mut failures = Vec::new();
        match bounded_result {
            Ok((report, elapsed)) => {
                if elapsed > Duration::from_millis(650) {
                    failures.push(format!(
                        "collector reported completion after the 650ms watchdog: {elapsed:?}"
                    ));
                }
                if report.verified.is_some() {
                    failures.push("slow-drip foreign peer was accepted as a daemon".to_string());
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => failures.push(
                "real collector exceeded 650ms because each drip reset its socket read timeout"
                    .to_string(),
            ),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                failures.push("real collector worker disconnected without a report".to_string());
            }
        }
        if let Err(error) = server_result {
            failures.push(error);
        }

        assert!(
            failures.is_empty(),
            "slow-drip deadline failures:\n{}",
            failures.join("\n")
        );
    }

    #[test]
    fn collect_doctor_state_matrix_is_pure() {
        struct Case {
            name: &'static str,
            raw: RawFixture,
            scan_ports: Vec<u16>,
            status_responses: Vec<(u16, Value)>,
            expected_state: DoctorDaemonState,
            expected_live: Option<(u32, u16, &'static str)>,
            expected_warnings: Vec<DoctorDaemonWarning>,
        }

        let discovery_live = enriched_status(
            4_242,
            7_486,
            "0.7.0.rdiscovery",
            "/opt/hyperdb/discovery-daemon",
        );
        let scan_live = enriched_status(5_151, 7_487, "0.7.0.rscan", "/opt/hyperdb/scanned-daemon");
        let cases = vec![
            Case {
                name: "missing",
                raw: RawFixture::Missing,
                scan_ports: vec![],
                status_responses: vec![],
                expected_state: DoctorDaemonState::Missing,
                expected_live: None,
                expected_warnings: vec![],
            },
            Case {
                name: "unreadable",
                raw: RawFixture::Unreadable(io::ErrorKind::PermissionDenied),
                scan_ports: vec![],
                status_responses: vec![],
                expected_state: DoctorDaemonState::Unreadable,
                expected_live: None,
                expected_warnings: vec![DoctorDaemonWarning::DiscoveryUnreadable {
                    kind: io::ErrorKind::PermissionDenied,
                }],
            },
            Case {
                name: "malformed",
                raw: RawFixture::Malformed,
                scan_ports: vec![],
                status_responses: vec![],
                expected_state: DoctorDaemonState::Malformed,
                expected_live: None,
                expected_warnings: vec![DoctorDaemonWarning::MalformedDiscovery],
            },
            Case {
                name: "parsed-unreachable",
                raw: RawFixture::Parsed(enriched_status(
                    4_040,
                    7_485,
                    "0.7.0.rstale",
                    "/opt/hyperdb/stale-daemon",
                )),
                scan_ports: vec![],
                status_responses: vec![],
                expected_state: DoctorDaemonState::ParsedUnreachable,
                expected_live: None,
                expected_warnings: vec![DoctorDaemonWarning::DiscoveryCandidateUnreachable {
                    responding_port: 7_485,
                }],
            },
            Case {
                name: "live-from-discovery",
                raw: RawFixture::Parsed(discovery_live.clone()),
                scan_ports: vec![],
                status_responses: vec![(7_486, discovery_live)],
                expected_state: DoctorDaemonState::LiveFromDiscovery,
                expected_live: Some((4_242, 7_486, "0.7.0.rdiscovery")),
                expected_warnings: vec![],
            },
            Case {
                name: "live-from-scan",
                raw: RawFixture::Missing,
                scan_ports: vec![7_487],
                status_responses: vec![(7_487, scan_live)],
                expected_state: DoctorDaemonState::LiveFromScan,
                expected_live: Some((5_151, 7_487, "0.7.0.rscan")),
                expected_warnings: vec![],
            },
        ];

        let mut failures = Vec::new();
        for case in cases {
            let raw_before = case.raw.clone();
            let operations = RefCell::new(Vec::new());
            let read_raw_discovery = || {
                operations.borrow_mut().push("raw-reader".to_string());
                case.raw.read()
            };
            let probe_enriched_status = |port: u16, deadline: DoctorDeadline| {
                operations
                    .borrow_mut()
                    .push(format!("status-prober:{port}:{}", deadline.0));
                case.status_responses
                    .iter()
                    .find(|(candidate, _)| *candidate == port)
                    .map_or(DoctorStatusProbe::Unreachable, |(_, response)| {
                        DoctorStatusProbe::Response(response.to_string())
                    })
            };
            let scan_candidates = |request: DoctorScanRequest| {
                operations.borrow_mut().push(format!(
                    "bounded-scanner:{}:{}:{}",
                    request.ports.base, request.ports.span, request.deadline.0
                ));
                case.scan_ports
                    .iter()
                    .copied()
                    .map(|responding_port| DoctorScanCandidate { responding_port })
                    .collect()
            };
            let now = || {
                operations.borrow_mut().push("clock".to_string());
                DoctorMoment(10_000)
            };
            let deadline_after = |now: DoctorMoment, timeout: Duration| {
                operations
                    .borrow_mut()
                    .push(format!("deadline:{}:{}", now.0, timeout.as_millis()));
                DoctorDeadline(
                    now.0
                        + u64::try_from(timeout.as_millis())
                            .expect("the test timeout fits in u64 milliseconds"),
                )
            };
            let dependencies = DoctorCollectorDependencies {
                read_raw_discovery: &read_raw_discovery,
                probe_enriched_status: &probe_enriched_status,
                scan_candidates: &scan_candidates,
                now: &now,
                deadline_after: &deadline_after,
            };
            let request = DoctorCollectRequest {
                ports: PortScan {
                    base: 7_485,
                    span: 4,
                },
                timeout: Duration::from_millis(275),
            };

            match catch_unwind(AssertUnwindSafe(|| {
                collect_doctor_daemon(&dependencies, request)
            })) {
                Ok(report) => {
                    if report.state != case.expected_state {
                        failures.push(format!(
                            "{}: state was {:?}, expected {:?}",
                            case.name, report.state, case.expected_state
                        ));
                    }
                    match (report.verified.as_ref(), case.expected_live) {
                        (None, None) => {}
                        (Some(verified), Some((pid, port, build))) => {
                            if verified.responding_port != port
                                || verified.record.info().pid != pid
                                || verified.record.info().health_port != port
                                || verified
                                    .record
                                    .identity()
                                    .map(DaemonBuildIdentity::mcp_version)
                                    != Some(build)
                            {
                                failures.push(format!(
                                    "{}: collector did not report the fresh verified STATUS facts",
                                    case.name
                                ));
                            }
                        }
                        (actual, expected) => failures.push(format!(
                            "{}: verified daemon was {actual:?}, expected {expected:?}",
                            case.name
                        )),
                    }
                    if report.warnings != case.expected_warnings {
                        failures.push(format!(
                            "{}: warnings were {:?}, expected {:?}",
                            case.name, report.warnings, case.expected_warnings
                        ));
                    }
                }
                Err(_) => failures.push(format!(
                    "{}: pure doctor collector remains unimplemented",
                    case.name
                )),
            }

            if case.raw != raw_before {
                failures.push(format!(
                    "{}: raw discovery fixture was conceptually mutated",
                    case.name
                ));
            }
            let unexpected = operations
                .borrow()
                .iter()
                .filter(|operation| {
                    !operation.starts_with("raw-reader")
                        && !operation.starts_with("status-prober")
                        && !operation.starts_with("bounded-scanner")
                        && !operation.starts_with("clock")
                        && !operation.starts_with("deadline")
                })
                .cloned()
                .collect::<Vec<_>>();
            if !unexpected.is_empty() {
                failures.push(format!(
                    "{}: collector reached non-read dependencies: {unexpected:?}",
                    case.name
                ));
            }
        }

        assert!(
            failures.is_empty(),
            "doctor state matrix failures:\n{}",
            failures.join("\n")
        );
    }

    #[test]
    fn candidates_refetch_and_verify_enriched_status() {
        #[derive(Clone)]
        enum ProbeFixture {
            Malformed,
            Response(Value),
        }

        struct Case {
            name: &'static str,
            raw: RawFixture,
            scan_ports: Vec<u16>,
            probes: Vec<(u16, ProbeFixture)>,
            expected_state: DoctorDaemonState,
            expected_fresh_pid: Option<u32>,
            expected_probed_ports: Vec<u16>,
            expected_scan: bool,
            expected_warnings: Vec<DoctorDaemonWarning>,
        }

        let recorded_executable =
            ReportedPath::from_os_str(OsStr::new("/opt/hyperdb/recorded-daemon"));
        let fresh_executable = ReportedPath::from_os_str(OsStr::new("/opt/hyperdb/fresh-daemon"));
        let recorded = enriched_status(101, 8_000, "0.7.0.rrecorded", &recorded_executable.display);
        let fresh = enriched_status(202, 8_000, "0.7.0.rfresh", &fresh_executable.display);
        let cases = vec![
            Case {
                name: "discovery-is-refetched-and-fresh-facts-win",
                raw: RawFixture::Parsed(recorded),
                scan_ports: vec![],
                probes: vec![(8_000, ProbeFixture::Response(fresh))],
                expected_state: DoctorDaemonState::LiveFromDiscovery,
                expected_fresh_pid: Some(202),
                expected_probed_ports: vec![8_000],
                expected_scan: false,
                expected_warnings: vec![DoctorDaemonWarning::StaleOrReplacedDiscovery {
                    mismatches: vec![
                        DiscoveryFactMismatch::Pid {
                            recorded: 101,
                            fresh: 202,
                        },
                        DiscoveryFactMismatch::McpVersion {
                            recorded: "0.7.0.rrecorded".to_string(),
                            fresh: "0.7.0.rfresh".to_string(),
                        },
                        DiscoveryFactMismatch::ExecutablePath {
                            recorded: recorded_executable,
                            fresh: fresh_executable,
                        },
                    ],
                }],
            },
            Case {
                name: "discovery-status-health-port-must-match-responder",
                raw: RawFixture::Parsed(enriched_status(
                    303,
                    8_001,
                    "0.7.0.rrecorded",
                    "/opt/hyperdb/discovery-candidate",
                )),
                scan_ports: vec![],
                probes: vec![(
                    8_001,
                    ProbeFixture::Response(enriched_status(
                        404,
                        9_001,
                        "0.7.0.rfresh",
                        "/opt/hyperdb/other-daemon",
                    )),
                )],
                expected_state: DoctorDaemonState::ParsedUnreachable,
                expected_fresh_pid: None,
                expected_probed_ports: vec![8_001],
                expected_scan: true,
                expected_warnings: vec![DoctorDaemonWarning::StatusHealthPortMismatch {
                    responding_port: 8_001,
                    reported_port: 9_001,
                }],
            },
            Case {
                name: "malformed-discovery-status-is-not-live-evidence",
                raw: RawFixture::Parsed(enriched_status(
                    505,
                    8_002,
                    "0.7.0.rrecorded",
                    "/opt/hyperdb/discovery-candidate",
                )),
                scan_ports: vec![],
                probes: vec![(8_002, ProbeFixture::Malformed)],
                expected_state: DoctorDaemonState::ParsedUnreachable,
                expected_fresh_pid: None,
                expected_probed_ports: vec![8_002],
                expected_scan: true,
                expected_warnings: vec![DoctorDaemonWarning::MalformedStatus {
                    responding_port: 8_002,
                }],
            },
            Case {
                name: "scan-hit-is-refetched-before-becoming-live",
                raw: RawFixture::Missing,
                scan_ports: vec![8_003],
                probes: vec![(
                    8_003,
                    ProbeFixture::Response(enriched_status(
                        606,
                        8_003,
                        "0.7.0.rscan-fresh",
                        "/opt/hyperdb/scan-fresh-daemon",
                    )),
                )],
                expected_state: DoctorDaemonState::LiveFromScan,
                expected_fresh_pid: Some(606),
                expected_probed_ports: vec![8_003],
                expected_scan: true,
                expected_warnings: vec![],
            },
            Case {
                name: "scan-status-health-port-must-match-responder",
                raw: RawFixture::Missing,
                scan_ports: vec![8_004],
                probes: vec![(
                    8_004,
                    ProbeFixture::Response(enriched_status(
                        707,
                        9_004,
                        "0.7.0.rwrong-port",
                        "/opt/hyperdb/wrong-port-daemon",
                    )),
                )],
                expected_state: DoctorDaemonState::Missing,
                expected_fresh_pid: None,
                expected_probed_ports: vec![8_004],
                expected_scan: true,
                expected_warnings: vec![DoctorDaemonWarning::StatusHealthPortMismatch {
                    responding_port: 8_004,
                    reported_port: 9_004,
                }],
            },
        ];

        let mut failures = Vec::new();
        for case in cases {
            let raw_before = case.raw.clone();
            let probed_ports = RefCell::new(Vec::new());
            let scan_requests = RefCell::new(Vec::new());
            let read_raw_discovery = || case.raw.read();
            let probe_enriched_status = |port: u16, deadline: DoctorDeadline| {
                probed_ports.borrow_mut().push((port, deadline));
                match case
                    .probes
                    .iter()
                    .find(|(candidate, _)| *candidate == port)
                    .map(|(_, response)| response)
                {
                    Some(ProbeFixture::Response(response)) => {
                        DoctorStatusProbe::Response(response.to_string())
                    }
                    Some(ProbeFixture::Malformed) => {
                        DoctorStatusProbe::Response("{not-valid-json".to_string())
                    }
                    None => DoctorStatusProbe::Unreachable,
                }
            };
            let scan_candidates = |request: DoctorScanRequest| {
                scan_requests.borrow_mut().push(request);
                case.scan_ports
                    .iter()
                    .copied()
                    .map(|responding_port| DoctorScanCandidate { responding_port })
                    .collect()
            };
            let now = || DoctorMoment(60_000);
            let deadline_after = |now: DoctorMoment, timeout: Duration| {
                DoctorDeadline(
                    now.0
                        + u64::try_from(timeout.as_millis())
                            .expect("the test timeout fits in u64 milliseconds"),
                )
            };
            let dependencies = DoctorCollectorDependencies {
                read_raw_discovery: &read_raw_discovery,
                probe_enriched_status: &probe_enriched_status,
                scan_candidates: &scan_candidates,
                now: &now,
                deadline_after: &deadline_after,
            };
            let request = DoctorCollectRequest {
                ports: PortScan {
                    base: 8_000,
                    span: 5,
                },
                timeout: Duration::from_millis(125),
            };

            match catch_unwind(AssertUnwindSafe(|| {
                collect_doctor_daemon(&dependencies, request)
            })) {
                Ok(report) => {
                    if report.state != case.expected_state {
                        failures.push(format!(
                            "{}: state was {:?}, expected {:?}",
                            case.name, report.state, case.expected_state
                        ));
                    }
                    let fresh_pid = report
                        .verified
                        .as_ref()
                        .map(|verified| verified.record.info().pid);
                    if fresh_pid != case.expected_fresh_pid {
                        failures.push(format!(
                            "{}: fresh verified PID was {fresh_pid:?}, expected {:?}",
                            case.name, case.expected_fresh_pid
                        ));
                    }
                    if report.warnings != case.expected_warnings {
                        failures.push(format!(
                            "{}: warnings were {:?}, expected {:?}",
                            case.name, report.warnings, case.expected_warnings
                        ));
                    }
                }
                Err(_) => failures.push(format!(
                    "{}: candidate verification collector remains unimplemented",
                    case.name
                )),
            }

            let actual_ports = probed_ports
                .borrow()
                .iter()
                .map(|(port, _)| *port)
                .collect::<Vec<_>>();
            if actual_ports != case.expected_probed_ports {
                failures.push(format!(
                    "{}: STATUS probes were {actual_ports:?}, expected {:?}",
                    case.name, case.expected_probed_ports
                ));
            }
            if probed_ports
                .borrow()
                .iter()
                .any(|(_, deadline)| *deadline != DoctorDeadline(60_125))
            {
                failures.push(format!(
                    "{}: STATUS probe did not receive the finite shared deadline",
                    case.name
                ));
            }

            let expected_scan_requests = usize::from(case.expected_scan);
            if scan_requests.borrow().len() != expected_scan_requests {
                failures.push(format!(
                    "{}: bounded scanner call count was {}, expected {expected_scan_requests}",
                    case.name,
                    scan_requests.borrow().len()
                ));
            }
            for scan_request in scan_requests.borrow().iter() {
                if scan_request.ports
                    != (PortScan {
                        base: 8_000,
                        span: 5,
                    })
                    || scan_request.deadline != DoctorDeadline(60_125)
                {
                    failures.push(format!(
                        "{}: scan request was not bounded to five ports and 125ms: {scan_request:?}",
                        case.name
                    ));
                }
            }
            if case.raw != raw_before {
                failures.push(format!(
                    "{}: candidate verification mutated the raw record",
                    case.name
                ));
            }
        }

        assert!(
            failures.is_empty(),
            "candidate verification failures:\n{}",
            failures.join("\n")
        );
    }

    fn assert_identity_accessors(
        identity: &DaemonBuildIdentity,
        expected_version: &str,
        expected_path: &ReportedPath,
    ) {
        assert_eq!(identity.mcp_version(), expected_version);
        assert_eq!(identity.executable_path(), expected_path);
    }

    fn assert_record_accessors(record: &DaemonRecord) {
        let _ = record.info();
        let _ = record.identity();
    }

    fn assert_record_contract(
        path: &Path,
        expected_wire: &Value,
        expected_identity: Option<(&str, &ReportedPath)>,
    ) {
        std::fs::write(path, serde_json::to_vec(expected_wire).unwrap()).unwrap();

        let record = match read_discovery_file_raw(path) {
            RawDiscoveryRead::Parsed { record, .. } => record,
            other => panic!("expected parsed raw daemon record, got {other:?}"),
        };
        assert_record_accessors(&record);

        let round_trip = serde_json::to_value(&record).unwrap();
        assert_eq!(round_trip, *expected_wire);
        assert!(
            round_trip.get("info").is_none(),
            "legacy daemon fields must remain at the top level"
        );

        let info = record.info();
        assert_eq!(info.pid, 4242);
        assert_eq!(info.hyperd_endpoint, "127.0.0.1:54321");
        assert_eq!(info.health_port, 7485);
        assert_eq!(info.started_at, "2026-08-13T12:34:56Z");
        assert_eq!(info.version, "0.7.0");

        match (record.identity(), expected_identity) {
            (None, None) => {}
            (Some(identity), Some((expected_version, expected_path))) => {
                assert_identity_accessors(identity, expected_version, expected_path);
            }
            (actual, expected) => {
                panic!("identity mismatch: actual={actual:?}, expected={expected:?}")
            }
        }
    }

    #[test]
    fn doctor_can_inspect_raw_daemon_record() {
        let tmp = TempDir::new().unwrap();
        let old_wire = json!({
            "pid": 4242,
            "hyperd_endpoint": "127.0.0.1:54321",
            "health_port": 7485,
            "started_at": "2026-08-13T12:34:56Z",
            "version": "0.7.0"
        });
        assert_record_contract(&tmp.path().join("old.json"), &old_wire, None);

        let executable_path =
            ReportedPath::from_os_str(std::ffi::OsStr::new("/opt/hyperdb/bin/hyperdb-mcp"));
        let enriched_wire = json!({
            "pid": 4242,
            "hyperd_endpoint": "127.0.0.1:54321",
            "health_port": 7485,
            "started_at": "2026-08-13T12:34:56Z",
            "version": "0.7.0",
            "identity": {
                "mcp_version": "0.7.0.rabc123",
                "executable_path": executable_path
            }
        });
        assert_record_contract(
            &tmp.path().join("enriched.json"),
            &enriched_wire,
            Some(("0.7.0.rabc123", &executable_path)),
        );
    }
}
