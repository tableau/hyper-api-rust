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

use crate::daemon::control::HealthEndpoint;
use crate::daemon::discovery::{DaemonRecord, RawDiscoveryRead};

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
    health_endpoint: Option<String>,
    /// Whether a daemon currently holds the single-instance lock.
    lock: DoctorLockState,
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

/// Whether the daemon lock is held, observed without taking it for long or
/// creating it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DoctorLockState {
    /// The lock file does not exist, so no daemon has ever run here.
    Absent,
    /// A daemon holds the lock (running, starting or stopping).
    Held,
    /// The lock file exists and nothing holds it.
    Free,
    /// The lock could not be inspected.
    Unknown,
}

/// Finite monotonic deadline shared by doctor probes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DoctorDeadline(pub(crate) u64);

/// Raw outcome from fetching enriched `STATUS` at the recorded health endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DoctorStatusProbe {
    Unreachable,
    Response(String),
}

/// Finite collection policy supplied independently of process globals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DoctorCollectRequest {
    pub(crate) timeout: Duration,
}

/// The stable daemon discovery state exposed by doctor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DoctorDaemonState {
    Missing,
    Unreadable,
    Malformed,
    /// The discovery file is well-formed but larger than any legitimate
    /// record should be; distinct from `Malformed` so the user isn't told to
    /// fix "invalid JSON" that parses just fine.
    Oversized,
    ParsedUnreachable,
    LiveFromDiscovery,
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
    OversizedDiscovery,
    DiscoveryCandidateUnreachable {
        health_endpoint: String,
    },
    StaleOrReplacedDiscovery {
        mismatches: Vec<DiscoveryFactMismatch>,
    },
    StatusHealthEndpointMismatch {
        recorded_endpoint: String,
        reported_endpoint: String,
    },
    MalformedStatus {
        health_endpoint: String,
    },
}

/// Fresh daemon facts accepted only after verification against the recorded health endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VerifiedDoctorDaemon {
    pub(crate) health_endpoint: String,
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
    pub(crate) probe_enriched_status: &'a dyn Fn(&str, DoctorDeadline) -> DoctorStatusProbe,
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
        RawDiscoveryRead::Oversized { .. } => {
            warnings.push(DoctorDaemonWarning::OversizedDiscovery);
            (DoctorDaemonState::Oversized, None)
        }
        RawDiscoveryRead::Parsed { record, .. } => {
            (DoctorDaemonState::ParsedUnreachable, Some(record))
        }
    };

    if let Some(recorded) = discovery_record.as_ref() {
        let health_endpoint = recorded.info().health_endpoint.clone();
        match verify_status_candidate(dependencies, &health_endpoint, deadline, &mut warnings) {
            Some(fresh) => {
                let mismatches = discovery_fact_mismatches(recorded, &fresh);
                if !mismatches.is_empty() {
                    warnings.push(DoctorDaemonWarning::StaleOrReplacedDiscovery { mismatches });
                }
                return DoctorDaemonReport {
                    state: DoctorDaemonState::LiveFromDiscovery,
                    verified: Some(VerifiedDoctorDaemon {
                        health_endpoint,
                        record: fresh,
                    }),
                    warnings,
                };
            }
            None if warnings.is_empty() => {
                warnings
                    .push(DoctorDaemonWarning::DiscoveryCandidateUnreachable { health_endpoint });
            }
            None => {}
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
    health_endpoint: &str,
    deadline: DoctorDeadline,
    warnings: &mut Vec<DoctorDaemonWarning>,
) -> Option<DaemonRecord> {
    let DoctorStatusProbe::Response(response) =
        (dependencies.probe_enriched_status)(health_endpoint, deadline)
    else {
        return None;
    };
    let Ok(record) = serde_json::from_str::<DaemonRecord>(&response) else {
        warnings.push(DoctorDaemonWarning::MalformedStatus {
            health_endpoint: health_endpoint.to_owned(),
        });
        return None;
    };
    if record.identity().is_none() {
        warnings.push(DoctorDaemonWarning::MalformedStatus {
            health_endpoint: health_endpoint.to_owned(),
        });
        return None;
    }
    if record.info().health_endpoint != health_endpoint {
        warnings.push(DoctorDaemonWarning::StatusHealthEndpointMismatch {
            recorded_endpoint: health_endpoint.to_owned(),
            reported_endpoint: record.info().health_endpoint.clone(),
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
        state_dir.as_deref(),
    );
    let mut daemon = doctor_daemon_section(&daemon_report);
    daemon.lock = state_dir
        .as_deref()
        .map_or(DoctorLockState::Unknown, probe_daemon_lock);
    let catalog = crate::server::HyperMcpServer::doctor_catalog_snapshot(options.read_only)?;

    let mut warnings = installation
        .warnings
        .iter()
        .map(identity_doctor_warning)
        .collect::<Vec<_>>();
    warnings.extend(daemon_report.warnings.iter().map(daemon_doctor_warning));
    if let Some(error) = state_dir.as_deref().and_then(untrusted_state_dir_error) {
        warnings.push(doctor_warning(
            "untrusted_state_dir",
            format!(
                "The daemon state directory is not trusted, so doctor did not read its record, connect to its socket or inspect its lock: {error}"
            ),
        ));
    }
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
    if let Some(endpoint) = report.daemon.health_endpoint.as_deref() {
        let _ = writeln!(output, "  Health endpoint: {}", escape_human(endpoint));
    }
    let _ = writeln!(
        output,
        "  Daemon lock: {}",
        lock_state_label(report.daemon.lock)
    );
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

/// Look at the daemon lock without creating it. Taking it for an instant is
/// the only way to learn whether it is held; a daemon that starts in that
/// instant simply retries.
fn probe_daemon_lock(state_dir: &Path) -> DoctorLockState {
    if untrusted_state_dir_error(state_dir).is_some() {
        return DoctorLockState::Unknown;
    }
    let lock_path = state_dir.join(crate::daemon::lock::LOCK_FILE_NAME);
    match std::fs::symlink_metadata(&lock_path) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => return DoctorLockState::Unknown,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return DoctorLockState::Absent,
        Err(_) => return DoctorLockState::Unknown,
    }
    match crate::daemon::lock::DaemonLock::try_acquire(state_dir) {
        Ok(Some(_lock)) => DoctorLockState::Free,
        Ok(None) => DoctorLockState::Held,
        Err(_) => DoctorLockState::Unknown,
    }
}

/// Why `state_dir` must not be trusted (owned by someone else, or writable by
/// group or others), or `None` when it is trusted or does not exist. Doctor
/// neither reads a record from, connects through, nor locks in such a
/// directory: anything in it could have been planted by another account.
fn untrusted_state_dir_error(state_dir: &Path) -> Option<io::Error> {
    crate::daemon::state_perms::verify_state_dir_trusted(state_dir)
        .err()
        .filter(|error| error.kind() != io::ErrorKind::NotFound)
}

fn collect_real_doctor_daemon(
    discovery_path: Option<&Path>,
    state_error_kind: Option<io::ErrorKind>,
    state_dir: Option<&Path>,
) -> DoctorDaemonReport {
    let origin = Instant::now();
    let untrusted = state_dir.and_then(untrusted_state_dir_error);
    let (discovery_path, state_error_kind, state_dir) = match &untrusted {
        Some(error) => (None, Some(error.kind()), None),
        None => (discovery_path, state_error_kind, state_dir),
    };
    let read_raw_discovery = || match discovery_path {
        Some(path) => crate::daemon::discovery::read_discovery_file_raw(path),
        None => RawDiscoveryRead::Unreadable {
            path: ReportedPath::from_os_str(OsStr::new("")),
            kind: state_error_kind.unwrap_or(io::ErrorKind::NotFound),
        },
    };
    let probe_enriched_status = |recorded: &str, deadline: DoctorDeadline| {
        // The record is untrusted input: only the endpoint this state
        // directory implies is ever connected to, so a tampered record cannot
        // point doctor at an arbitrary socket.
        let Some(endpoint) = state_dir.and_then(|dir| HealthEndpoint::from_record(recorded, dir))
        else {
            return DoctorStatusProbe::Unreachable;
        };
        match send_doctor_command(&endpoint, "STATUS", origin, deadline) {
            Ok(response) => DoctorStatusProbe::Response(response),
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                DoctorStatusProbe::Response(String::new())
            }
            Err(_) => DoctorStatusProbe::Unreachable,
        }
    };
    let now = || DoctorMoment(elapsed_millis(origin));
    let deadline_after = |now: DoctorMoment, timeout: Duration| {
        DoctorDeadline(now.0.saturating_add(duration_millis(timeout)))
    };
    let dependencies = DoctorCollectorDependencies {
        read_raw_discovery: &read_raw_discovery,
        probe_enriched_status: &probe_enriched_status,
        now: &now,
        deadline_after: &deadline_after,
    };
    collect_doctor_daemon(
        &dependencies,
        DoctorCollectRequest {
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
    endpoint: &HealthEndpoint,
    command: &str,
    origin: Instant,
    deadline: DoctorDeadline,
) -> io::Result<String> {
    use std::io::{Read, Write};

    let connect_timeout = doctor_network_timeout(origin, deadline)?;
    let mut stream = crate::daemon::control::connect(endpoint, connect_timeout)?;
    let message = format!("{command}\n");
    let mut written = 0;
    while written < message.len() {
        let timeout = doctor_network_timeout(origin, deadline)?;
        stream.set_io_timeout(timeout)?;
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
        crate::daemon::health::rearm_io_timeout(&mut stream, timeout)?;
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
            // Classify a silent close exactly as `send_command_with_timeout`
            // does. Returning `Ok("")` here would reach `verify_status_candidate`
            // as an unparseable STATUS body and be reported as
            // `MalformedStatus` — but a peer that never wrote a byte did not
            // send a malformed response, it failed to answer at all.
            Ok(0) if response.is_empty() => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "doctor health peer closed the connection before any response was sent",
                ));
            }
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

fn doctor_daemon_section(report: &DoctorDaemonReport) -> DoctorDaemonSection {
    let Some(verified) = report.verified.as_ref() else {
        return DoctorDaemonSection {
            state: report.state,
            pid: None,
            hyperd_endpoint: None,
            health_endpoint: None,
            lock: DoctorLockState::Unknown,
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
        health_endpoint: Some(bounded_string(&verified.record.info().health_endpoint)),
        lock: DoctorLockState::Unknown,
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
        DoctorDaemonWarning::OversizedDiscovery => doctor_warning(
            "daemon_discovery_oversized",
            "The daemon discovery file is valid but larger than any legitimate record should be; it was left unchanged.",
        ),
        DoctorDaemonWarning::DiscoveryCandidateUnreachable { health_endpoint } => doctor_warning(
            "daemon_discovery_candidate_unreachable",
            format!(
                "The recorded daemon candidate at {} did not return fresh enriched STATUS.",
                bounded_string(health_endpoint)
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
        DoctorDaemonWarning::StatusHealthEndpointMismatch {
            recorded_endpoint,
            reported_endpoint,
        } => doctor_warning(
            "daemon_status_health_endpoint_mismatch",
            format!(
                "STATUS from {} reported health endpoint {}; the candidate was rejected.",
                bounded_string(recorded_endpoint),
                bounded_string(reported_endpoint)
            ),
        ),
        DoctorDaemonWarning::MalformedStatus { health_endpoint } => doctor_warning(
            "daemon_status_malformed",
            format!(
                "{} returned malformed or unenriched STATUS; the candidate was rejected.",
                bounded_string(health_endpoint)
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
        DoctorDaemonState::Oversized => "oversized",
        DoctorDaemonState::ParsedUnreachable => "parsed_unreachable",
        DoctorDaemonState::LiveFromDiscovery => "live_from_discovery",
    }
}

const fn lock_state_label(state: DoctorLockState) -> &'static str {
    match state {
        DoctorLockState::Absent => "absent",
        DoctorLockState::Held => "held",
        DoctorLockState::Free => "free",
        DoctorLockState::Unknown => "unknown",
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
    use std::io::{self, Read as _, Write as _};
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use serde_json::{Value, json};
    use tempfile::TempDir;

    use crate::daemon::control::{ControlListener, ControlStream, HealthEndpoint};
    use crate::daemon::discovery::{
        DaemonBuildIdentity, DaemonInfo, DaemonRecord, DiscoveryBytes, RawDiscoveryRead,
        read_discovery_file_raw,
    };
    use crate::daemon::health::{DaemonState, HealthListener};
    use crate::daemon::lock::{DaemonLock, LOCK_FILE_NAME};

    use super::{
        DiscoveryFactMismatch, DoctorCollectRequest, DoctorCollectorDependencies,
        DoctorDaemonState, DoctorDaemonWarning, DoctorDeadline, DoctorLockState, DoctorMoment,
        DoctorStatusProbe, MAX_STATUS_RESPONSE_BYTES, ReportedPath, collect_doctor_daemon,
        collect_real_doctor_daemon, probe_daemon_lock, real_network_test_guard,
        send_doctor_command,
    };

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum RawFixture {
        Missing,
        Unreadable(io::ErrorKind),
        Malformed,
        Oversized,
        Parsed(Value),
    }

    impl RawFixture {
        fn read(&self) -> RawDiscoveryRead {
            let path = ReportedPath::from_os_str(OsStr::new("/virtual/state/daemon.json"));
            match self {
                Self::Missing => RawDiscoveryRead::Missing { path },
                Self::Unreadable(kind) => RawDiscoveryRead::Unreadable { path, kind: *kind },
                // The doctor never reads the carried bytes — they exist for
                // `discover()`'s tolerant fallback — so any non-record payload
                // is a faithful fixture here.
                Self::Malformed => RawDiscoveryRead::Malformed {
                    path,
                    contents: DiscoveryBytes::new(b"{ unterminated".to_vec()),
                },
                Self::Oversized => RawDiscoveryRead::Oversized {
                    path,
                    contents: DiscoveryBytes::new(b"{ well-formed but enormous }".to_vec()),
                },
                Self::Parsed(value) => RawDiscoveryRead::Parsed {
                    path,
                    record: serde_json::from_value(value.clone()).unwrap(),
                },
            }
        }
    }

    fn enriched_status(pid: u32, health_endpoint: &str, build: &str, executable: &str) -> Value {
        json!({
            "pid": pid,
            "hyperd_endpoint": "127.0.0.1:54321",
            "health_endpoint": health_endpoint,
            "started_at": "2026-08-13T12:34:56Z",
            "version": "0.7.0",
            "identity": {
                "mcp_version": build,
                "executable_path": ReportedPath::from_os_str(OsStr::new(executable))
            }
        })
    }

    fn listener_daemon_info(pid: u32, health_endpoint: &HealthEndpoint) -> DaemonInfo {
        DaemonInfo {
            pid,
            hyperd_endpoint: "127.0.0.1:54321".to_string(),
            health_endpoint: health_endpoint.as_str().to_owned(),
            started_at: "2026-08-13T12:34:56Z".to_string(),
            version: "0.7.0".to_string(),
        }
    }

    /// The endpoint a daemon with state directory `dir` would serve.
    fn endpoint_in(dir: &TempDir) -> HealthEndpoint {
        HealthEndpoint::for_new_daemon(dir.path()).expect("health endpoint for the test state dir")
    }

    /// Write `status` as the state directory's `daemon.json` and return its path.
    fn write_discovery(dir: &TempDir, status: &Value) -> std::path::PathBuf {
        let path = dir.path().join("daemon.json");
        std::fs::write(&path, serde_json::to_vec(status).unwrap()).unwrap();
        path
    }

    /// Accept one client on a fake peer, giving up after `window` or once
    /// `stop` is set.
    fn accept_client(
        listener: &mut ControlListener,
        window: Duration,
        stop: &AtomicBool,
    ) -> Result<Option<ControlStream>, String> {
        let deadline = Instant::now() + window;
        while !stop.load(Ordering::Acquire) && Instant::now() < deadline {
            match listener.accept_timeout(Duration::from_millis(20)) {
                Ok(Some(stream)) => return Ok(Some(stream)),
                Ok(None) => {}
                Err(error) => return Err(format!("fake peer accept failed: {error}")),
            }
        }
        Ok(None)
    }

    /// Read the one command line a doctor probe sends.
    fn read_command(stream: &mut ControlStream) -> Result<String, String> {
        stream
            .set_io_timeout(Duration::from_secs(2))
            .map_err(|error| format!("fake peer set timeout failed: {error}"))?;
        let mut command = Vec::new();
        let mut byte = [0_u8; 1];
        loop {
            match stream.read(&mut byte) {
                Ok(0) => break,
                Ok(_) if byte[0] == b'\n' => break,
                Ok(_) => command.push(byte[0]),
                Err(error) => return Err(format!("fake peer command read failed: {error}")),
            }
        }
        Ok(String::from_utf8_lossy(&command).into_owned())
    }

    /// Keep a fake peer's end open until the client hangs up (or a bounded
    /// wait elapses). Closing first can make a later `set_io_timeout` on the
    /// client fail on macOS, which would mask the behavior under test.
    fn hold_until_client_closes(stream: &mut ControlStream) {
        if stream.set_io_timeout(Duration::from_secs(1)).is_err() {
            return;
        }
        let mut scratch = [0_u8; 256];
        while matches!(stream.read(&mut scratch), Ok(count) if count > 0) {}
    }

    /// A fake STATUS peer: accepts one client, records its command, and
    /// answers with `reply` followed by a newline.
    fn spawn_status_peer(
        mut listener: ControlListener,
        reply: String,
    ) -> std::thread::JoinHandle<Result<String, String>> {
        std::thread::spawn(move || {
            let stop = AtomicBool::new(false);
            let Some(mut stream) = accept_client(&mut listener, Duration::from_secs(3), &stop)?
            else {
                return Err("status peer never received a connection".to_string());
            };
            let command = read_command(&mut stream)?;
            stream
                .write_all(format!("{reply}\n").as_bytes())
                .map_err(|error| format!("status peer write failed: {error}"))?;
            hold_until_client_closes(&mut stream);
            Ok(command)
        })
    }

    #[test]
    fn real_health_listener_answers_doctor_within_budget() {
        let _network_guard = real_network_test_guard();
        let dir = TempDir::new().unwrap();
        let endpoint = endpoint_in(&dir);
        let listener = HealthListener::bind(&endpoint).unwrap();
        let state = Arc::new(DaemonState::new(endpoint.clone()));
        let info = Arc::new(Mutex::new(listener_daemon_info(8_181, &endpoint)));
        let run_state = Arc::clone(&state);
        let run_info = Arc::clone(&info);
        // Start serving late so the doctor's connection waits in the accept
        // backlog, pinning that the listener's accept cadence fits the budget.
        let listener_thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(75));
            listener.run(run_state, run_info);
        });

        let discovery = write_discovery(
            &dir,
            &enriched_status(8_181, endpoint.as_str(), "0.7.0.rrecorded", "/opt/recorded"),
        );
        let started = Instant::now();
        let report = collect_real_doctor_daemon(Some(&discovery), None, Some(dir.path()));
        let elapsed = started.elapsed();

        state.request_shutdown();
        listener_thread.join().unwrap();

        let mut failures = Vec::new();
        if report.state != DoctorDaemonState::LiveFromDiscovery {
            failures.push(format!(
                "real HealthListener state was {:?}, expected LiveFromDiscovery",
                report.state
            ));
        }
        match report.verified {
            Some(verified)
                if verified.health_endpoint == endpoint.as_str()
                    && verified.record.info().health_endpoint == endpoint.as_str()
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
    fn real_doctor_reports_a_mismatched_status_endpoint_and_discards_the_peer() {
        let _network_guard = real_network_test_guard();
        let dir = TempDir::new().unwrap();
        let endpoint = endpoint_in(&dir);
        let listener = ControlListener::bind(&endpoint).unwrap();
        let reported = "/somewhere/else/daemon.sock";
        let peer = spawn_status_peer(
            listener,
            enriched_status(707, reported, "0.7.0.rwrong", "/opt/hyperdb/wrong-daemon").to_string(),
        );
        let discovery = write_discovery(
            &dir,
            &enriched_status(
                707,
                endpoint.as_str(),
                "0.7.0.rwrong",
                "/opt/hyperdb/wrong-daemon",
            ),
        );

        let report = collect_real_doctor_daemon(Some(&discovery), None, Some(dir.path()));
        let observed_command = peer.join().unwrap();

        assert_eq!(observed_command.as_deref(), Ok("STATUS"));
        assert!(
            report.verified.is_none(),
            "a mismatched peer must not verify"
        );
        assert_eq!(report.state, DoctorDaemonState::ParsedUnreachable);
        assert_eq!(
            report.warnings,
            vec![DoctorDaemonWarning::StatusHealthEndpointMismatch {
                recorded_endpoint: endpoint.as_str().to_owned(),
                reported_endpoint: reported.to_owned(),
            }]
        );
    }

    #[test]
    fn real_doctor_reports_an_oversized_status_as_malformed() {
        let _network_guard = real_network_test_guard();
        let dir = TempDir::new().unwrap();
        let endpoint = endpoint_in(&dir);
        let mut listener = ControlListener::bind(&endpoint).unwrap();
        let server = std::thread::spawn(move || -> Result<(), String> {
            let stop = AtomicBool::new(false);
            let Some(mut stream) = accept_client(&mut listener, Duration::from_secs(3), &stop)?
            else {
                return Err("oversized peer never received a connection".to_string());
            };
            read_command(&mut stream)?;
            // More than the fixed cap and never a newline; the client closing
            // early surfaces as a write error, which is the expected outcome.
            let flood = vec![b'a'; MAX_STATUS_RESPONSE_BYTES + 4096];
            let _ = stream.write_all(&flood);
            hold_until_client_closes(&mut stream);
            Ok(())
        });
        let discovery = write_discovery(
            &dir,
            &enriched_status(
                909,
                endpoint.as_str(),
                "0.7.0.rbig",
                "/opt/hyperdb/big-daemon",
            ),
        );

        let report = collect_real_doctor_daemon(Some(&discovery), None, Some(dir.path()));
        server.join().unwrap().unwrap();

        assert!(report.verified.is_none());
        assert_eq!(
            report.warnings,
            vec![DoctorDaemonWarning::MalformedStatus {
                health_endpoint: endpoint.as_str().to_owned(),
            }]
        );
    }

    #[test]
    fn send_doctor_command_rejects_a_response_past_the_size_cap() {
        let _network_guard = real_network_test_guard();
        let dir = TempDir::new().unwrap();
        let endpoint = endpoint_in(&dir);
        let mut listener = ControlListener::bind(&endpoint).unwrap();
        let server = std::thread::spawn(move || -> Result<(), String> {
            let stop = AtomicBool::new(false);
            let Some(mut stream) = accept_client(&mut listener, Duration::from_secs(3), &stop)?
            else {
                return Err("size-cap peer never received a connection".to_string());
            };
            read_command(&mut stream)?;
            let flood = vec![b'a'; MAX_STATUS_RESPONSE_BYTES + 4096];
            let _ = stream.write_all(&flood);
            hold_until_client_closes(&mut stream);
            Ok(())
        });

        let origin = Instant::now();
        let error = send_doctor_command(&endpoint, "STATUS", origin, DoctorDeadline(2_000))
            .expect_err("a response past the cap must be rejected");
        server.join().unwrap().unwrap();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn real_doctor_collector_enforces_global_deadline_against_slow_drip() {
        let _network_guard = real_network_test_guard();
        let dir = TempDir::new().unwrap();
        let endpoint = endpoint_in(&dir);
        let mut listener = ControlListener::bind(&endpoint).unwrap();
        let stop_writer = Arc::new(AtomicBool::new(false));
        let server_stop = Arc::clone(&stop_writer);
        let server = std::thread::spawn(move || -> Result<(), String> {
            let Some(mut stream) =
                accept_client(&mut listener, Duration::from_secs(2), &server_stop)?
            else {
                return Err("slow-drip peer never received a connection".to_string());
            };
            let command = read_command(&mut stream)?;
            if command != "STATUS" {
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
            let _ = stream.finish();
            Ok(())
        });

        let discovery = write_discovery(
            &dir,
            &enriched_status(
                616,
                endpoint.as_str(),
                "0.7.0.rdrip",
                "/opt/hyperdb/drip-daemon",
            ),
        );
        let state_dir = dir.path().to_path_buf();
        let (result_sender, result_receiver) = mpsc::channel();
        let collector = std::thread::spawn(move || {
            let started = Instant::now();
            let report = collect_real_doctor_daemon(Some(&discovery), None, Some(&state_dir));
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
                "real collector exceeded 650ms because each drip reset its read timeout"
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

    /// A recorded daemon candidate that accepts the connection and then closes
    /// without writing a byte never sent a response, so it cannot have sent a
    /// malformed one. `send_doctor_command` used to return `Ok("")` there, which
    /// reached `verify_status_candidate` as an unparseable STATUS body and was
    /// reported to the operator as `daemon_status_malformed`. It must classify
    /// the silent close the way `send_command_with_timeout` does — as an EOF —
    /// so the candidate is reported unreachable instead.
    #[test]
    fn real_doctor_reports_a_silent_peer_close_as_unreachable_not_malformed() {
        let _network_guard = real_network_test_guard();
        let dir = TempDir::new().unwrap();
        let endpoint = endpoint_in(&dir);
        let mut listener = ControlListener::bind(&endpoint).unwrap();
        let server = std::thread::spawn(move || -> Result<String, String> {
            let stop = AtomicBool::new(false);
            let Some(mut stream) = accept_client(&mut listener, Duration::from_secs(3), &stop)?
            else {
                return Err("silent-close peer never received a connection".to_string());
            };
            let command = read_command(&mut stream)?;
            // Close having written nothing at all.
            drop(stream);
            Ok(command)
        });

        let discovery = write_discovery(
            &dir,
            &enriched_status(
                4_242,
                endpoint.as_str(),
                "0.7.0.rsilent",
                "/opt/hyperdb/bin/hyperdb-mcp",
            ),
        );
        let report = collect_real_doctor_daemon(Some(&discovery), None, Some(dir.path()));
        let observed_command = server.join().unwrap();

        let mut failures = Vec::new();
        match observed_command {
            Ok(command) if command == "STATUS" => {}
            Ok(command) => failures.push(format!(
                "discovery candidate was probed with {command:?}, expected a direct STATUS"
            )),
            Err(error) => failures.push(error),
        }
        if report.verified.is_some() {
            failures.push("a peer that answered nothing was accepted as a daemon".to_string());
        }
        if report.state != DoctorDaemonState::ParsedUnreachable {
            failures.push(format!(
                "expected state parsed_unreachable, got {:?}",
                report.state
            ));
        }
        if report.warnings
            != vec![DoctorDaemonWarning::DiscoveryCandidateUnreachable {
                health_endpoint: endpoint.as_str().to_owned(),
            }]
        {
            failures.push(format!(
                "a silent close must be reported as an unreachable candidate, not as malformed \
                 STATUS; got {:?}",
                report.warnings
            ));
        }

        assert!(
            failures.is_empty(),
            "silent-close doctor classification failures:\n{}",
            failures.join("\n")
        );
    }

    /// The record is untrusted input: an endpoint other than the one the state
    /// directory implies must never be connected to, even when a live peer is
    /// listening there.
    #[cfg(unix)]
    #[test]
    fn real_doctor_never_connects_to_a_recorded_endpoint_outside_the_state_dir() {
        let _network_guard = real_network_test_guard();
        let state = TempDir::new().unwrap();
        let elsewhere = TempDir::new().unwrap();
        let foreign_endpoint = endpoint_in(&elsewhere);
        let mut listener = ControlListener::bind(&foreign_endpoint).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let peer_stop = Arc::clone(&stop);
        let connections = Arc::new(AtomicUsize::new(0));
        let peer_connections = Arc::clone(&connections);
        let peer = std::thread::spawn(move || -> Result<(), String> {
            while let Some(stream) =
                accept_client(&mut listener, Duration::from_secs(2), &peer_stop)?
            {
                peer_connections.fetch_add(1, Ordering::AcqRel);
                drop(stream);
            }
            Ok(())
        });

        let discovery = write_discovery(
            &state,
            &enriched_status(
                1_313,
                foreign_endpoint.as_str(),
                "0.7.0.rforeign",
                "/opt/hyperdb/foreign-daemon",
            ),
        );
        let report = collect_real_doctor_daemon(Some(&discovery), None, Some(state.path()));
        // Give a wrongly-issued connection time to arrive before stopping.
        std::thread::sleep(Duration::from_millis(100));
        stop.store(true, Ordering::Release);
        peer.join().unwrap().unwrap();

        assert_eq!(connections.load(Ordering::Acquire), 0);
        assert!(report.verified.is_none());
        assert_eq!(report.state, DoctorDaemonState::ParsedUnreachable);
        assert_eq!(
            report.warnings,
            vec![DoctorDaemonWarning::DiscoveryCandidateUnreachable {
                health_endpoint: foreign_endpoint.as_str().to_owned(),
            }]
        );
    }

    #[cfg(unix)]
    #[test]
    fn real_doctor_rejects_a_recorded_path_that_is_not_the_state_dirs_socket() {
        let state = TempDir::new().unwrap();
        let record = state.path().join("other.sock");
        let discovery = write_discovery(
            &state,
            &enriched_status(
                1_414,
                &record.to_string_lossy(),
                "0.7.0.rother",
                "/opt/hyperdb/other-daemon",
            ),
        );
        let report = collect_real_doctor_daemon(Some(&discovery), None, Some(state.path()));
        assert_eq!(report.state, DoctorDaemonState::ParsedUnreachable);
        assert!(report.verified.is_none());
        assert_eq!(
            report.warnings,
            vec![DoctorDaemonWarning::DiscoveryCandidateUnreachable {
                health_endpoint: record.to_string_lossy().into_owned(),
            }]
        );

        // Without a resolvable state directory nothing is ever connected to.
        let report = collect_real_doctor_daemon(Some(&discovery), None, None);
        assert_eq!(report.state, DoctorDaemonState::ParsedUnreachable);
        assert!(report.verified.is_none());
    }

    /// A state directory another account can write to is never read from,
    /// connected through or locked: a planted socket sees no connection and
    /// no lock file appears.
    #[cfg(unix)]
    #[test]
    fn real_doctor_ignores_an_untrusted_state_dir() {
        use std::os::unix::fs::PermissionsExt as _;

        let _network_guard = real_network_test_guard();
        let state = TempDir::new().unwrap();
        let endpoint = endpoint_in(&state);
        let mut listener = ControlListener::bind(&endpoint).unwrap();
        let discovery = write_discovery(
            &state,
            &enriched_status(1_515, endpoint.as_str(), "0.7.0.rplanted", "/opt/planted"),
        );
        std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o770)).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let peer_stop = Arc::clone(&stop);
        let connections = Arc::new(AtomicUsize::new(0));
        let peer_connections = Arc::clone(&connections);
        let peer = std::thread::spawn(move || -> Result<(), String> {
            while let Some(stream) =
                accept_client(&mut listener, Duration::from_secs(2), &peer_stop)?
            {
                peer_connections.fetch_add(1, Ordering::AcqRel);
                drop(stream);
            }
            Ok(())
        });

        let report = collect_real_doctor_daemon(Some(&discovery), None, Some(state.path()));
        let lock = probe_daemon_lock(state.path());
        std::thread::sleep(Duration::from_millis(100));
        stop.store(true, Ordering::Release);
        peer.join().unwrap().unwrap();

        assert_eq!(connections.load(Ordering::Acquire), 0);
        assert!(report.verified.is_none());
        assert_eq!(report.state, DoctorDaemonState::Unreadable);
        assert_eq!(lock, DoctorLockState::Unknown);
        assert!(
            !state
                .path()
                .join(crate::daemon::lock::LOCK_FILE_NAME)
                .exists(),
            "doctor must not create the lock file"
        );
        assert!(crate::diagnostics::untrusted_state_dir_error(state.path()).is_some());
    }

    #[test]
    fn lock_probe_reports_an_absent_lock_without_creating_it() {
        let dir = TempDir::new().unwrap();
        assert_eq!(probe_daemon_lock(dir.path()), DoctorLockState::Absent);
        assert!(
            !dir.path().join(LOCK_FILE_NAME).exists(),
            "probing must not create the lock file"
        );
    }

    #[test]
    fn lock_probe_reports_a_free_lock_once_its_holder_is_gone() {
        let dir = TempDir::new().unwrap();
        drop(
            DaemonLock::try_acquire(dir.path())
                .unwrap()
                .expect("first acquire"),
        );
        assert_eq!(probe_daemon_lock(dir.path()), DoctorLockState::Free);
        // The probe took the lock for an instant and must have released it.
        assert!(
            DaemonLock::try_acquire(dir.path()).unwrap().is_some(),
            "the probe must not leave the lock held"
        );
    }

    #[test]
    fn lock_probe_reports_a_held_lock() {
        let dir = TempDir::new().unwrap();
        let _held = DaemonLock::try_acquire(dir.path())
            .unwrap()
            .expect("acquire");
        assert_eq!(probe_daemon_lock(dir.path()), DoctorLockState::Held);
    }

    #[test]
    fn lock_probe_reports_a_non_file_lock_path_as_unknown() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join(LOCK_FILE_NAME)).unwrap();
        assert_eq!(probe_daemon_lock(dir.path()), DoctorLockState::Unknown);
    }

    #[test]
    fn collect_doctor_state_matrix_is_pure() {
        struct Case {
            name: &'static str,
            raw: RawFixture,
            status_responses: Vec<(&'static str, Value)>,
            expected_state: DoctorDaemonState,
            expected_live: Option<(u32, &'static str, &'static str)>,
            expected_warnings: Vec<DoctorDaemonWarning>,
        }

        let discovery_live = enriched_status(
            4_242,
            "/virtual/live/daemon.sock",
            "0.7.0.rdiscovery",
            "/opt/hyperdb/discovery-daemon",
        );
        let cases = vec![
            Case {
                name: "missing",
                raw: RawFixture::Missing,
                status_responses: vec![],
                expected_state: DoctorDaemonState::Missing,
                expected_live: None,
                expected_warnings: vec![],
            },
            Case {
                name: "unreadable",
                raw: RawFixture::Unreadable(io::ErrorKind::PermissionDenied),
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
                status_responses: vec![],
                expected_state: DoctorDaemonState::Malformed,
                expected_live: None,
                expected_warnings: vec![DoctorDaemonWarning::MalformedDiscovery],
            },
            Case {
                name: "oversized",
                raw: RawFixture::Oversized,
                status_responses: vec![],
                expected_state: DoctorDaemonState::Oversized,
                expected_live: None,
                expected_warnings: vec![DoctorDaemonWarning::OversizedDiscovery],
            },
            Case {
                name: "parsed-unreachable",
                raw: RawFixture::Parsed(enriched_status(
                    4_040,
                    "/virtual/stale/daemon.sock",
                    "0.7.0.rstale",
                    "/opt/hyperdb/stale-daemon",
                )),
                status_responses: vec![],
                expected_state: DoctorDaemonState::ParsedUnreachable,
                expected_live: None,
                expected_warnings: vec![DoctorDaemonWarning::DiscoveryCandidateUnreachable {
                    health_endpoint: "/virtual/stale/daemon.sock".to_string(),
                }],
            },
            Case {
                name: "live-from-discovery",
                raw: RawFixture::Parsed(discovery_live.clone()),
                status_responses: vec![("/virtual/live/daemon.sock", discovery_live)],
                expected_state: DoctorDaemonState::LiveFromDiscovery,
                expected_live: Some((4_242, "/virtual/live/daemon.sock", "0.7.0.rdiscovery")),
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
            let probe_enriched_status = |endpoint: &str, deadline: DoctorDeadline| {
                operations
                    .borrow_mut()
                    .push(format!("status-prober:{endpoint}:{}", deadline.0));
                case.status_responses
                    .iter()
                    .find(|(candidate, _)| *candidate == endpoint)
                    .map_or(DoctorStatusProbe::Unreachable, |(_, response)| {
                        DoctorStatusProbe::Response(response.to_string())
                    })
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
                now: &now,
                deadline_after: &deadline_after,
            };
            let request = DoctorCollectRequest {
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
                        (Some(verified), Some((pid, endpoint, build))) => {
                            if verified.health_endpoint != endpoint
                                || verified.record.info().pid != pid
                                || verified.record.info().health_endpoint != endpoint
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
            probes: Vec<(&'static str, ProbeFixture)>,
            expected_state: DoctorDaemonState,
            expected_fresh_pid: Option<u32>,
            expected_probed: Vec<&'static str>,
            expected_warnings: Vec<DoctorDaemonWarning>,
        }

        let recorded_executable =
            ReportedPath::from_os_str(OsStr::new("/opt/hyperdb/recorded-daemon"));
        let fresh_executable = ReportedPath::from_os_str(OsStr::new("/opt/hyperdb/fresh-daemon"));
        let recorded = enriched_status(
            101,
            "/virtual/a/daemon.sock",
            "0.7.0.rrecorded",
            &recorded_executable.display,
        );
        let fresh = enriched_status(
            202,
            "/virtual/a/daemon.sock",
            "0.7.0.rfresh",
            &fresh_executable.display,
        );
        let cases = vec![
            Case {
                name: "discovery-is-refetched-and-fresh-facts-win",
                raw: RawFixture::Parsed(recorded),
                probes: vec![("/virtual/a/daemon.sock", ProbeFixture::Response(fresh))],
                expected_state: DoctorDaemonState::LiveFromDiscovery,
                expected_fresh_pid: Some(202),
                expected_probed: vec!["/virtual/a/daemon.sock"],
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
                name: "discovery-status-health-endpoint-must-match-responder",
                raw: RawFixture::Parsed(enriched_status(
                    303,
                    "/virtual/b/daemon.sock",
                    "0.7.0.rrecorded",
                    "/opt/hyperdb/discovery-candidate",
                )),
                probes: vec![(
                    "/virtual/b/daemon.sock",
                    ProbeFixture::Response(enriched_status(
                        404,
                        "/virtual/other/daemon.sock",
                        "0.7.0.rfresh",
                        "/opt/hyperdb/other-daemon",
                    )),
                )],
                expected_state: DoctorDaemonState::ParsedUnreachable,
                expected_fresh_pid: None,
                expected_probed: vec!["/virtual/b/daemon.sock"],
                expected_warnings: vec![DoctorDaemonWarning::StatusHealthEndpointMismatch {
                    recorded_endpoint: "/virtual/b/daemon.sock".to_string(),
                    reported_endpoint: "/virtual/other/daemon.sock".to_string(),
                }],
            },
            Case {
                name: "malformed-discovery-status-is-not-live-evidence",
                raw: RawFixture::Parsed(enriched_status(
                    505,
                    "/virtual/c/daemon.sock",
                    "0.7.0.rrecorded",
                    "/opt/hyperdb/discovery-candidate",
                )),
                probes: vec![("/virtual/c/daemon.sock", ProbeFixture::Malformed)],
                expected_state: DoctorDaemonState::ParsedUnreachable,
                expected_fresh_pid: None,
                expected_probed: vec!["/virtual/c/daemon.sock"],
                expected_warnings: vec![DoctorDaemonWarning::MalformedStatus {
                    health_endpoint: "/virtual/c/daemon.sock".to_string(),
                }],
            },
            Case {
                name: "missing-discovery-probes-nothing",
                raw: RawFixture::Missing,
                probes: vec![],
                expected_state: DoctorDaemonState::Missing,
                expected_fresh_pid: None,
                expected_probed: vec![],
                expected_warnings: vec![],
            },
        ];

        let mut failures = Vec::new();
        for case in cases {
            let raw_before = case.raw.clone();
            let probed = RefCell::new(Vec::new());
            let read_raw_discovery = || case.raw.read();
            let probe_enriched_status = |endpoint: &str, deadline: DoctorDeadline| {
                probed.borrow_mut().push((endpoint.to_owned(), deadline));
                match case
                    .probes
                    .iter()
                    .find(|(candidate, _)| *candidate == endpoint)
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
                now: &now,
                deadline_after: &deadline_after,
            };
            let request = DoctorCollectRequest {
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

            let actual_probed = probed
                .borrow()
                .iter()
                .map(|(endpoint, _)| endpoint.clone())
                .collect::<Vec<_>>();
            if actual_probed != case.expected_probed {
                failures.push(format!(
                    "{}: STATUS probes were {actual_probed:?}, expected {:?}",
                    case.name, case.expected_probed
                ));
            }
            if probed
                .borrow()
                .iter()
                .any(|(_, deadline)| *deadline != DoctorDeadline(60_125))
            {
                failures.push(format!(
                    "{}: STATUS probe did not receive the finite shared deadline",
                    case.name
                ));
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
        assert_eq!(info.health_endpoint, "/virtual/state/daemon.sock");
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
            "health_endpoint": "/virtual/state/daemon.sock",
            "started_at": "2026-08-13T12:34:56Z",
            "version": "0.7.0"
        });
        assert_record_contract(&tmp.path().join("old.json"), &old_wire, None);

        let executable_path =
            ReportedPath::from_os_str(std::ffi::OsStr::new("/opt/hyperdb/bin/hyperdb-mcp"));
        let enriched_wire = json!({
            "pid": 4242,
            "hyperd_endpoint": "127.0.0.1:54321",
            "health_endpoint": "/virtual/state/daemon.sock",
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
