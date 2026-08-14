// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Contract tests for bounded, launcher-reported installation identity.

use std::ffi::{OsStr, OsString};

use hyperdb_mcp::diagnostics::{
    installation_identity_from_parts, parse_launcher_identity, IdentityWarning, PathEncoding,
    ReportedPath,
};
use serde_json::{json, Value};

fn launcher_json(
    wrapper_name: &str,
    wrapper_version: Option<&str>,
    platform_name: &str,
    platform_version: Option<&str>,
) -> String {
    json!({
        "wrapper": {
            "name": wrapper_name,
            "version": wrapper_version,
            "package_path": "/opt/hyperdb/node_modules/hyperdb-mcp/package.json"
        },
        "platform": {
            "name": platform_name,
            "version": platform_version,
            "package_path": "/opt/hyperdb/node_modules/hyperdb-mcp-linux-x64-gnu/package.json"
        },
        "executable_path": "/opt/hyperdb/node_modules/hyperdb-mcp-linux-x64-gnu/hyperdb-mcp"
    })
    .to_string()
}

#[cfg(unix)]
fn non_utf8_os_string() -> OsString {
    use std::os::unix::ffi::OsStringExt;

    OsString::from_vec(b"/tmp/hyperdb-\xff-mcp".to_vec())
}

#[cfg(windows)]
fn non_utf8_os_string() -> OsString {
    use std::os::windows::ffi::OsStringExt;

    OsString::from_wide(&[
        u16::from(b'C'),
        u16::from(b':'),
        u16::from(b'\\'),
        0xD800,
        u16::from(b'x'),
    ])
}

#[test]
fn launcher_identity_parsing_contract() {
    let absent = parse_launcher_identity(None);
    assert_eq!(absent.identity, None);
    assert!(absent.warnings.is_empty());

    let secret = "UNKNOWN_SECRET_SENTINEL_4c4c08";
    let valid = json!({
        "wrapper": {
            "name": "hyperdb-mcp",
            "version": "1.2.3",
            "package_path": "/opt/hyperdb/node_modules/hyperdb-mcp/package.json",
            "unknown_secret": secret
        },
        "platform": {
            "name": "hyperdb-mcp-linux-x64-gnu",
            "version": "1.2.3",
            "package_path": "/opt/hyperdb/node_modules/hyperdb-mcp-linux-x64-gnu/package.json",
            "credentials": { "token": secret }
        },
        "executable_path": "/opt/hyperdb/node_modules/hyperdb-mcp-linux-x64-gnu/hyperdb-mcp",
        "unknown_root": secret
    })
    .to_string();

    let parsed = parse_launcher_identity(Some(OsStr::new(&valid)));
    assert!(parsed.warnings.is_empty());
    let identity = parsed.identity.expect("valid launcher metadata must parse");
    assert_eq!(
        serde_json::to_value(&identity).expect("launcher identity must serialize"),
        json!({
            "wrapper": {
                "name": "hyperdb-mcp",
                "version": "1.2.3",
                "package_path": { "display": "/opt/hyperdb/node_modules/hyperdb-mcp/package.json", "encoding": "utf8" }
            },
            "platform": {
                "name": "hyperdb-mcp-linux-x64-gnu",
                "version": "1.2.3",
                "package_path": { "display": "/opt/hyperdb/node_modules/hyperdb-mcp-linux-x64-gnu/package.json", "encoding": "utf8" }
            },
            "executable_path": { "display": "/opt/hyperdb/node_modules/hyperdb-mcp-linux-x64-gnu/hyperdb-mcp", "encoding": "utf8" }
        })
    );
    assert!(
        !serde_json::to_string(&identity)
            .expect("launcher identity must serialize")
            .contains(secret),
        "unknown launcher keys must never be re-emitted"
    );

    let malformed =
        parse_launcher_identity(Some(OsStr::new(r#"{"wrapper":{"name":"hyperdb-mcp"}"#)));
    assert_eq!(malformed.identity, None);
    assert_eq!(
        malformed.warnings,
        vec![IdentityWarning::MalformedLauncherInfo]
    );

    let utf8_path = ReportedPath::from_os_str(OsStr::new("/tmp/HyperDB/über.hyper"));
    assert_eq!(utf8_path.display, "/tmp/HyperDB/über.hyper");
    assert_eq!(utf8_path.encoding, PathEncoding::Utf8);

    let non_utf8 = non_utf8_os_string();
    let lossy_path = ReportedPath::from_os_str(&non_utf8);
    assert_eq!(lossy_path.encoding, PathEncoding::Lossy);
    assert!(lossy_path.display.contains('\u{fffd}'));
    assert!(lossy_path.display.len() <= 4 * 1024);

    let long_path = format!("/tmp/{}", "x".repeat(5 * 1024));
    let bounded_path = ReportedPath::from_os_str(OsStr::new(&long_path));
    assert_eq!(bounded_path.encoding, PathEncoding::Utf8);
    assert!(bounded_path.display.len() <= 4 * 1024);
}

#[test]
fn installation_identity_version_warning_contract() {
    let matching_launcher = launcher_json(
        "hyperdb-mcp",
        Some("1.2.3"),
        "hyperdb-mcp-linux-x64-gnu",
        Some("1.2.3"),
    );
    let matching = installation_identity_from_parts(
        OsStr::new("/opt/hyperdb/hyperdb-mcp"),
        "1.2.3.rdeadbeef-dirty-20260814T120000Z",
        "0.7.0.rdeadbeef-dirty-20260814T120000Z",
        Some(OsStr::new(&matching_launcher)),
    );

    assert_eq!(
        matching.mcp.source,
        "1.2.3.rdeadbeef-dirty-20260814T120000Z"
    );
    assert_eq!(matching.mcp.version.as_deref(), Some("1.2.3"));
    assert_eq!(
        matching.mcp.build.as_deref(),
        Some("deadbeef-dirty-20260814T120000Z")
    );
    assert_eq!(
        matching.hyper_rust_api.source,
        "0.7.0.rdeadbeef-dirty-20260814T120000Z"
    );
    assert_eq!(matching.hyper_rust_api.version.as_deref(), Some("0.7.0"));
    assert_eq!(
        matching.hyper_rust_api.build.as_deref(),
        Some("deadbeef-dirty-20260814T120000Z")
    );
    assert!(
        matching.warnings.is_empty(),
        "the native .r<build> suffix is not part of npm semver: {:?}",
        matching.warnings
    );

    let mismatched_launcher = launcher_json(
        "hyperdb-mcp",
        Some("2.0.0"),
        "hyperdb-mcp-linux-x64-gnu",
        Some("3.0.0"),
    );
    let mismatched = installation_identity_from_parts(
        OsStr::new("/opt/hyperdb/hyperdb-mcp"),
        "1.2.3.rdeadbeef",
        "0.7.0.rdeadbeef",
        Some(OsStr::new(&mismatched_launcher)),
    );
    assert!(mismatched.warnings.iter().any(|warning| {
        matches!(
            warning,
            IdentityWarning::VersionMismatch {
                native,
                wrapper: Some(wrapper),
                platform: Some(platform),
            } if native == "1.2.3" && wrapper == "2.0.0" && platform == "3.0.0"
        )
    }));

    let malformed_launcher = launcher_json(
        "hyperdb-mcp",
        Some("1.2.3.rlauncher-hash"),
        "hyperdb-mcp-linux-x64-gnu",
        Some("1.2.3"),
    );
    let malformed = installation_identity_from_parts(
        OsStr::new("/opt/hyperdb/hyperdb-mcp"),
        "1.2.3.rdeadbeef",
        "0.7.0.rdeadbeef",
        Some(OsStr::new(&malformed_launcher)),
    );
    assert!(malformed.warnings.iter().any(|warning| {
        matches!(
            warning,
            IdentityWarning::MalformedVersion { component }
                if component == "wrapper.version"
        )
    }));
}

#[test]
fn launcher_identity_rejects_oversize_without_secret_leakage() {
    const WHOLE_LIMIT: usize = 16 * 1024;
    const STRING_LIMIT: usize = 4 * 1024;
    const SECRET: &str = "OVERSIZE_SECRET_SENTINEL_7637fb";

    let base = launcher_json(
        "hyperdb-mcp",
        Some("1.2.3"),
        "hyperdb-mcp-linux-x64-gnu",
        Some("1.2.3"),
    );
    let mut at_whole_limit = base.clone();
    at_whole_limit.push_str(&" ".repeat(WHOLE_LIMIT - at_whole_limit.len()));
    assert_eq!(at_whole_limit.len(), WHOLE_LIMIT);
    assert!(
        parse_launcher_identity(Some(OsStr::new(&at_whole_limit)))
            .identity
            .is_some(),
        "the 16 KiB boundary itself must remain accepted"
    );

    let mut over_whole_limit = json!({
        "wrapper": { "name": "hyperdb-mcp", "version": "1.2.3", "package_path": "/wrapper" },
        "platform": { "name": "hyperdb-mcp-linux-x64-gnu", "version": "1.2.3", "package_path": "/platform" },
        "executable_path": "/platform/hyperdb-mcp",
        "unknown_secret": SECRET
    })
    .to_string();
    over_whole_limit.push_str(&" ".repeat(WHOLE_LIMIT + 1 - over_whole_limit.len()));
    let over_whole = parse_launcher_identity(Some(OsStr::new(&over_whole_limit)));
    assert_eq!(over_whole.identity, None);
    assert_eq!(
        over_whole.warnings,
        vec![IdentityWarning::LauncherInfoTooLarge]
    );
    assert!(!serde_json::to_string(&over_whole).unwrap().contains(SECRET));

    let boundary_cases = [
        ("/wrapper/name", "n".repeat(STRING_LIMIT)),
        (
            "/wrapper/version",
            format!("1.0.0+{}", "a".repeat(STRING_LIMIT - 6)),
        ),
        (
            "/wrapper/package_path",
            format!("/{}", "w".repeat(STRING_LIMIT - 1)),
        ),
        ("/platform/name", "p".repeat(STRING_LIMIT)),
        (
            "/platform/version",
            format!("1.0.0+{}", "b".repeat(STRING_LIMIT - 6)),
        ),
        (
            "/platform/package_path",
            format!("/{}", "q".repeat(STRING_LIMIT - 1)),
        ),
        (
            "/executable_path",
            format!("/{}", "e".repeat(STRING_LIMIT - 1)),
        ),
    ];
    for (pointer, boundary_value) in boundary_cases {
        assert_eq!(boundary_value.len(), STRING_LIMIT);
        let mut metadata = json!({
            "wrapper": { "name": "hyperdb-mcp", "version": "1.2.3", "package_path": "/wrapper" },
            "platform": { "name": "hyperdb-mcp-linux-x64-gnu", "version": "1.2.3", "package_path": "/platform" },
            "executable_path": "/platform/hyperdb-mcp"
        });
        *metadata
            .pointer_mut(pointer)
            .unwrap_or_else(|| panic!("test fixture pointer {pointer} must exist")) =
            Value::String(boundary_value);
        let raw = metadata.to_string();
        assert!(
            parse_launcher_identity(Some(OsStr::new(&raw)))
                .identity
                .is_some(),
            "the 4 KiB boundary itself must remain accepted for {pointer}"
        );
    }

    let over_limit_fields = [
        ("/wrapper/name", "wrapper.name"),
        ("/wrapper/version", "wrapper.version"),
        ("/wrapper/package_path", "wrapper.package_path"),
        ("/platform/name", "platform.name"),
        ("/platform/version", "platform.version"),
        ("/platform/package_path", "platform.package_path"),
        ("/executable_path", "executable_path"),
    ];
    for (pointer, field) in over_limit_fields {
        let mut metadata = json!({
            "wrapper": { "name": "hyperdb-mcp", "version": "1.2.3", "package_path": "/wrapper" },
            "platform": { "name": "hyperdb-mcp-linux-x64-gnu", "version": "1.2.3", "package_path": "/platform" },
            "executable_path": "/platform/hyperdb-mcp",
            "unknown_secret": SECRET
        });
        *metadata
            .pointer_mut(pointer)
            .unwrap_or_else(|| panic!("test fixture pointer {pointer} must exist")) =
            Value::String("x".repeat(STRING_LIMIT + 1));

        let raw = metadata.to_string();
        let over_string = parse_launcher_identity(Some(OsStr::new(&raw)));
        assert_eq!(over_string.identity, None, "overlong {field} was accepted");
        assert_eq!(
            over_string.warnings,
            vec![IdentityWarning::LauncherFieldTooLarge {
                field: field.to_owned()
            }]
        );
        let serialized = serde_json::to_value(&over_string).unwrap_or(Value::Null);
        assert!(!serialized.to_string().contains(SECRET));
    }
}
