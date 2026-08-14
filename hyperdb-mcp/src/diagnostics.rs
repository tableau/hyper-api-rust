// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Installation and launcher identity contracts.

use std::ffi::OsStr;

use semver::Version;
use serde::{Deserialize, Serialize};

const MAX_LAUNCHER_INFO_BYTES: usize = 16 * 1024;
const MAX_REPORTED_STRING_BYTES: usize = 4 * 1024;

/// How an operating-system path was converted to its bounded display form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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
