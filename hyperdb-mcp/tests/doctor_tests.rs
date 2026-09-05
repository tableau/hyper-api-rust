// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Native CLI contracts for the side-effect-free `doctor` report.

use std::ffi::{OsStr, OsString};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use hyperdb_mcp::daemon::discovery::DaemonInfo;
use hyperdb_mcp::daemon::health::{self, DaemonState, HealthListener};
use serde_json::{Value, json};
use tempfile::TempDir;

const SECRET_SENTINEL: &str = "UNKNOWN_SECRET_SENTINEL_doctor_7d31e9";
const MAX_REPORTED_STRING_BYTES: usize = 4 * 1024;
const PUBLIC_README: &str = include_str!("../README.md");

/// Canonicalize a path the way the `doctor` binary reports its own paths.
///
/// `std::fs::canonicalize` prepends the `\\?\` verbatim prefix on Windows,
/// but the paths the running binary reports come from `current_dir` /
/// `current_exe`, which are *un-prefixed*. Comparing a prefixed expected
/// path against an un-prefixed reported one fails only on Windows. Strip the
/// prefix (leaving genuine UNC paths, `\\?\UNC\...`, alone) so the expected
/// paths match what the binary emits. On non-Windows this is a plain
/// canonicalize.
fn canonicalize_for_test(path: &Path) -> std::io::Result<PathBuf> {
    let canonical = std::fs::canonicalize(path)?;
    #[cfg(windows)]
    {
        if let Some(s) = canonical.to_str() {
            let stripped = match s.strip_prefix(r"\\?\") {
                Some(rest) if !rest.starts_with("UNC\\") => rest,
                _ => s,
            };
            return Ok(PathBuf::from(stripped));
        }
    }
    Ok(canonical)
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum SnapshotNode {
    Directory,
    File(Vec<u8>),
    Symlink(PathBuf),
}

#[derive(Debug)]
struct DoctorSandbox {
    _temp_dir: TempDir,
    root: PathBuf,
    state_dir: PathBuf,
    persistent_path: PathBuf,
    home_dir: PathBuf,
    runtime_tmp_dir: PathBuf,
    wrapper_package_path: PathBuf,
    platform_package_path: PathBuf,
    launcher_executable_path: PathBuf,
    isolated_daemon_port: u16,
    _isolated_daemon_listener: TcpListener,
}

impl DoctorSandbox {
    fn new() -> Self {
        let temp_dir = TempDir::new().expect("create isolated doctor test root");
        let root =
            canonicalize_for_test(temp_dir.path()).expect("canonicalize isolated doctor test root");
        let isolated_daemon_listener = TcpListener::bind(("127.0.0.1", 0))
            .expect("reserve an OS-assigned foreign daemon-isolation port");
        let isolated_daemon_port = isolated_daemon_listener
            .local_addr()
            .expect("read daemon-isolation listener address")
            .port();
        Self {
            state_dir: root.join("state-must-not-be-created"),
            persistent_path: root
                .join("persistent-parent-must-not-be-created")
                .join("default.hyper"),
            home_dir: root.join("home-must-not-be-created"),
            runtime_tmp_dir: root.join("tmp-must-not-be-created"),
            wrapper_package_path: root.join("npm/wrapper/package.json"),
            platform_package_path: root.join("npm/platform/package.json"),
            launcher_executable_path: root.join("npm/platform/hyperdb-mcp"),
            isolated_daemon_port,
            _isolated_daemon_listener: isolated_daemon_listener,
            _temp_dir: temp_dir,
            root,
        }
    }

    fn launcher_metadata(&self, wrapper_name: &str) -> String {
        json!({
            "wrapper": {
                "name": wrapper_name,
                "version": env!("CARGO_PKG_VERSION"),
                "package_path": self.wrapper_package_path.to_string_lossy()
            },
            "platform": {
                "name": "hyperdb-mcp-test-platform",
                "version": env!("CARGO_PKG_VERSION"),
                "package_path": self.platform_package_path.to_string_lossy()
            },
            "executable_path": self.launcher_executable_path.to_string_lossy(),
            "unknown_secret": SECRET_SENTINEL
        })
        .to_string()
    }

    fn run(&self, json_output: bool, launcher_metadata: &str, hyperd_path: &OsStr) -> Output {
        let args = if json_output {
            &["doctor", "--json", "--read-only", "--no-daemon"][..]
        } else {
            &["doctor", "--read-only", "--no-daemon"][..]
        };
        self.run_with_options(
            args,
            Some(self.persistent_path.as_os_str()),
            launcher_metadata,
            Some(hyperd_path),
            self.isolated_daemon_port,
        )
    }

    fn run_with_options(
        &self,
        args: &[&str],
        persistent_environment: Option<&OsStr>,
        launcher_metadata: &str,
        hyperd_path: Option<&OsStr>,
        daemon_port: u16,
    ) -> Output {
        self.run_with_home_options(
            args,
            persistent_environment,
            launcher_metadata,
            hyperd_path,
            daemon_port,
            self.home_dir.as_os_str(),
        )
    }

    fn run_with_home_options(
        &self,
        args: &[&str],
        persistent_environment: Option<&OsStr>,
        launcher_metadata: &str,
        hyperd_path: Option<&OsStr>,
        daemon_port: u16,
        home_profile: &OsStr,
    ) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_hyperdb-mcp"));
        command.env_clear();
        preserve_child_runtime_environment(&mut command);
        command
            .current_dir(&self.root)
            .env("HOME", home_profile)
            .env("USERPROFILE", home_profile)
            .env(
                "XDG_DATA_HOME",
                self.root.join("xdg-data-must-not-be-created"),
            )
            .env("APPDATA", self.root.join("appdata-must-not-be-created"))
            .env(
                "LOCALAPPDATA",
                self.root.join("localappdata-must-not-be-created"),
            )
            .env("TMPDIR", &self.runtime_tmp_dir)
            .env("TMP", &self.runtime_tmp_dir)
            .env("TEMP", &self.runtime_tmp_dir)
            .env("HYPERDB_STATE_DIR", &self.state_dir)
            .env("HYPERDB_MCP_LAUNCHER_INFO", launcher_metadata)
            .env("HYPERDB_DAEMON_PORT", daemon_port.to_string())
            .env("NO_COLOR", "1")
            .args(args);
        if let Some(path) = persistent_environment {
            command.env("HYPERDB_PERSISTENT_DB", path);
        }
        if let Some(path) = hyperd_path {
            command.env("HYPERD_PATH", path);
        }
        command.output().expect("run isolated hyperdb-mcp doctor")
    }

    fn required_utf8_reported_paths(&self, hyperd_path: &Path) -> Vec<PathBuf> {
        vec![
            self.persistent_path.clone(),
            self.state_dir.clone(),
            self.state_dir.join("daemon.json"),
            self.persistent_path
                .parent()
                .expect("persistent fixture has a parent")
                .join("hyperdb-mcp.log"),
            hyperd_path.to_path_buf(),
            self.wrapper_package_path.clone(),
            self.platform_package_path.clone(),
            self.launcher_executable_path.clone(),
        ]
    }

    fn assert_no_artifacts(&self) {
        for path in [
            &self.state_dir,
            &self.persistent_path,
            &self.home_dir,
            &self.runtime_tmp_dir,
        ] {
            assert!(
                !path.exists(),
                "doctor must not create isolated path {}",
                path.display()
            );
        }
        assert!(
            !self.state_dir.join("daemon.json").exists(),
            "doctor must not create a discovery file"
        );
        assert!(
            !self
                .persistent_path
                .parent()
                .expect("persistent fixture has a parent")
                .join("hyperdb-mcp.log")
                .exists(),
            "doctor must not create a client log"
        );
    }
}

#[derive(Debug)]
struct RunningHealthListener {
    port: u16,
    info: DaemonInfo,
    state: Arc<DaemonState>,
    handle: Option<JoinHandle<()>>,
}

impl RunningHealthListener {
    fn start() -> Self {
        let listener = HealthListener::bind(0).expect("bind OS-assigned health-listener port");
        let port = listener.port;
        let info = DaemonInfo {
            pid: std::process::id(),
            hyperd_endpoint: "127.0.0.1:54321".to_owned(),
            health_port: port,
            started_at: "2026-08-14T12:34:56Z".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        };
        let state = Arc::new(DaemonState::new());
        let shared_info = Arc::new(Mutex::new(info.clone()));
        let run_state = Arc::clone(&state);
        let handle = std::thread::spawn(move || listener.run(run_state, shared_info));
        Self {
            port,
            info,
            state,
            handle: Some(handle),
        }
    }

    fn prime_accept_sleep(&self) {
        let response = health::send_command(self.port, "PING")
            .expect("real health listener must answer the priming PING");
        assert!(
            response.starts_with("PONG hyperdb-mcp "),
            "unexpected health-listener PING response: {response:?}"
        );
        // The response comes from a per-connection worker. Give the accept
        // thread a small scheduling window to re-enter its real 100 ms
        // WouldBlock sleep before launching the already-warm doctor child.
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

impl Drop for RunningHealthListener {
    fn drop(&mut self) {
        self.state.request_shutdown();
        if let Some(handle) = self.handle.take() {
            handle
                .join()
                .expect("health listener must shut down cleanly");
        }
    }
}

fn preserve_child_runtime_environment(command: &mut Command) {
    // The binary path is absolute, but these variables may still be required by
    // the platform loader. No application configuration is inherited.
    for key in [
        "PATH",
        "SystemRoot",
        "WINDIR",
        "COMSPEC",
        "PATHEXT",
        "LD_LIBRARY_PATH",
        "DYLD_LIBRARY_PATH",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
}

fn snapshot_tree(root: &Path) -> Vec<(PathBuf, SnapshotNode)> {
    fn visit(root: &Path, directory: &Path, entries: &mut Vec<(PathBuf, SnapshotNode)>) {
        let mut children: Vec<_> = std::fs::read_dir(directory)
            .unwrap_or_else(|error| {
                panic!("read snapshot directory {}: {error}", directory.display())
            })
            .map(|entry| entry.expect("read snapshot entry").path())
            .collect();
        children.sort_unstable();

        for path in children {
            let relative = path
                .strip_prefix(root)
                .expect("snapshot entry must be below root")
                .to_path_buf();
            let metadata = std::fs::symlink_metadata(&path).unwrap_or_else(|error| {
                panic!("inspect snapshot entry {}: {error}", path.display())
            });
            if metadata.file_type().is_symlink() {
                let target = std::fs::read_link(&path).unwrap_or_else(|error| {
                    panic!("read snapshot symlink {}: {error}", path.display())
                });
                entries.push((relative, SnapshotNode::Symlink(target)));
            } else if metadata.is_dir() {
                entries.push((relative, SnapshotNode::Directory));
                visit(root, &path, entries);
            } else {
                let bytes = std::fs::read(&path).unwrap_or_else(|error| {
                    panic!("read snapshot file {}: {error}", path.display())
                });
                entries.push((relative, SnapshotNode::File(bytes)));
            }
        }
    }

    let mut entries = Vec::new();
    visit(root, root, &mut entries);
    entries
}

fn assert_success(output: &Output, invocation: &str) {
    assert!(
        output.status.success(),
        "`{invocation}` must produce a report and exit zero, even with warnings:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn parse_json_report(output: &Output, invocation: &str) -> Value {
    assert_success(output, invocation);
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("`{invocation}` must emit valid JSON: {error}"))
}

fn assert_exact_top_level_keys(report: &Value) {
    let object = report
        .as_object()
        .unwrap_or_else(|| panic!("doctor JSON must be a top-level object: {report}"));
    let mut actual: Vec<_> = object.keys().map(String::as_str).collect();
    actual.sort_unstable();
    assert_eq!(
        actual,
        [
            "configuration",
            "daemon",
            "installation",
            "status",
            "tool_catalog",
            "warnings",
        ],
        "doctor top-level JSON contract must be exact"
    );
}

fn report_object<'a>(report: &'a Value, section: &str) -> &'a serde_json::Map<String, Value> {
    report
        .get(section)
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("doctor JSON needs typed object section `{section}`: {report}"))
}

fn path_fact_failure(
    configuration: &serde_json::Map<String, Value>,
    key: &str,
    expected_display: &str,
    expected_encoding: &str,
    expected_exists: bool,
    expected_is_file: bool,
    expected_is_directory: bool,
) -> Option<String> {
    let Some(facts) = configuration.get(key).and_then(Value::as_object) else {
        return Some(format!("`configuration.{key}` must be a path-facts object"));
    };
    let Some(path) = facts.get("path").and_then(Value::as_object) else {
        return Some(format!(
            "`configuration.{key}.path` must carry display + encoding"
        ));
    };
    let actual = (
        path.get("display").and_then(Value::as_str),
        path.get("encoding").and_then(Value::as_str),
        facts.get("exists").and_then(Value::as_bool),
        facts.get("is_file").and_then(Value::as_bool),
        facts.get("is_directory").and_then(Value::as_bool),
    );
    let expected = (
        Some(expected_display),
        Some(expected_encoding),
        Some(expected_exists),
        Some(expected_is_file),
        Some(expected_is_directory),
    );
    (actual != expected).then(|| {
        format!("`configuration.{key}` mismatch: actual={actual:?}, expected={expected:?}")
    })
}

fn reported_path_failure(
    configuration: &serde_json::Map<String, Value>,
    key: &str,
    expected_display: &str,
    expected_encoding: &str,
) -> Option<String> {
    let Some(value) = configuration.get(key).and_then(Value::as_object) else {
        return Some(format!("`configuration.{key}` must report a path"));
    };
    let path = value
        .get("path")
        .and_then(Value::as_object)
        .unwrap_or(value);
    let actual = (
        path.get("display").and_then(Value::as_str),
        path.get("encoding").and_then(Value::as_str),
    );
    let expected = (Some(expected_display), Some(expected_encoding));
    (actual != expected).then(|| {
        format!("`configuration.{key}` mismatch: actual={actual:?}, expected={expected:?}")
    })
}

fn warning_text(report: &Value) -> String {
    report
        .get("warnings")
        .cloned()
        .unwrap_or(Value::Null)
        .to_string()
        .to_lowercase()
}

fn has_warning_code(report: &Value, expected: &str) -> bool {
    report
        .get("warnings")
        .and_then(Value::as_array)
        .is_some_and(|warnings| {
            warnings
                .iter()
                .any(|warning| warning.get("code").and_then(Value::as_str) == Some(expected))
        })
}

fn daemon_state(report: &Value) -> Option<&str> {
    report.pointer("/daemon/state").and_then(Value::as_str)
}

fn human_path_parity_failure(
    report: &Value,
    human: &str,
    configuration_key: &str,
    human_label: &str,
) -> Option<String> {
    let Some(configuration) = report.get("configuration").and_then(Value::as_object) else {
        return Some("doctor omitted typed configuration".to_owned());
    };
    let Some(value) = configuration.get(configuration_key) else {
        return Some(format!(
            "configuration omitted `{configuration_key}` needed for JSON/human parity"
        ));
    };
    let expected = if value.is_null() {
        format!("  {human_label}: unavailable")
    } else {
        let Some(object) = value.as_object() else {
            return Some(format!(
                "configuration.{configuration_key} was not a path object: {value}"
            ));
        };
        let path = object
            .get("path")
            .and_then(Value::as_object)
            .unwrap_or(object);
        let Some(display) = path.get("display").and_then(Value::as_str) else {
            return Some(format!(
                "configuration.{configuration_key} omitted path display"
            ));
        };
        let Some(encoding) = path.get("encoding").and_then(Value::as_str) else {
            return Some(format!(
                "configuration.{configuration_key} omitted path encoding"
            ));
        };
        match (
            object.get("exists").and_then(Value::as_bool),
            object.get("is_file").and_then(Value::as_bool),
            object.get("is_directory").and_then(Value::as_bool),
        ) {
            (Some(exists), Some(is_file), Some(is_directory)) => format!(
                "  {human_label}: {display} (encoding: {encoding}; exists: {exists}; file: {is_file}; directory: {is_directory})"
            ),
            (None, None, None) => {
                format!("  {human_label}: {display} (encoding: {encoding})")
            }
            facts => {
                return Some(format!(
                    "configuration.{configuration_key} had incomplete filesystem facts: {facts:?}"
                ));
            }
        }
    };
    (!human.contains(&expected)).then(|| {
        format!(
            "human report disagrees with configuration.{configuration_key}; missing {expected:?}"
        )
    })
}

fn catalog_human_parity_failures(report: &Value, human: &str) -> Vec<String> {
    let Some(catalog) = report.get("tool_catalog").and_then(Value::as_object) else {
        return vec!["doctor omitted typed tool_catalog".to_owned()];
    };
    let metrics = [
        ("tool_count", "Tools"),
        ("canonical_tool_bytes", "Canonical generated tools bytes"),
        (
            "initialization_instructions_bytes",
            "Initialization instructions bytes",
        ),
        ("get_readme_bytes", "get_readme bytes"),
    ];
    let mut failures = Vec::new();
    for (key, label) in metrics {
        let Some(value) = catalog.get(key).and_then(Value::as_u64) else {
            failures.push(format!("tool_catalog.{key} must be an unsigned metric"));
            continue;
        };
        if value == 0 {
            failures.push(format!("tool_catalog.{key} must be nonzero"));
        }
        let expected = format!("  {label}: {value}");
        if !human.contains(&expected) {
            failures.push(format!(
                "human report omitted tool_catalog.{key} parity line {expected:?}"
            ));
        }
    }
    if catalog.get("tool_count").and_then(Value::as_u64) != Some(33) {
        failures.push("generated catalog must contain exactly 33 tools".to_owned());
    }
    failures
}

fn live_daemon_human_parity_failure(report: &Value, human: &str) -> Option<String> {
    let Some(daemon) = report.get("daemon").and_then(Value::as_object) else {
        return Some("doctor omitted typed daemon identity".to_owned());
    };
    let required_text = [
        (
            "state",
            "State",
            daemon.get("state").and_then(Value::as_str),
        ),
        (
            "hyperd_endpoint",
            "Hyperd endpoint",
            daemon.get("hyperd_endpoint").and_then(Value::as_str),
        ),
        (
            "started_at",
            "Started",
            daemon.get("started_at").and_then(Value::as_str),
        ),
        (
            "version",
            "Takeover version",
            daemon.get("version").and_then(Value::as_str),
        ),
        (
            "mcp_version",
            "MCP build",
            daemon.get("mcp_version").and_then(Value::as_str),
        ),
    ];
    let mut missing = Vec::new();
    for (key, label, value) in required_text {
        let Some(value) = value else {
            missing.push(format!("daemon.{key} missing from live identity"));
            continue;
        };
        let expected = format!("  {label}: {value}");
        if !human.contains(&expected) {
            missing.push(format!("human live identity omitted {expected:?}"));
        }
    }
    for (key, label) in [("pid", "PID"), ("health_port", "Health port")] {
        let Some(value) = daemon.get(key).and_then(Value::as_u64) else {
            missing.push(format!("daemon.{key} missing from live identity"));
            continue;
        };
        let expected = format!("  {label}: {value}");
        if !human.contains(&expected) {
            missing.push(format!("human live identity omitted {expected:?}"));
        }
    }
    let executable = daemon.get("executable_path").and_then(Value::as_object);
    match executable {
        Some(executable) => {
            let display = executable.get("display").and_then(Value::as_str);
            let encoding = executable.get("encoding").and_then(Value::as_str);
            match (display, encoding) {
                (Some(display), Some(encoding)) => {
                    let expected = format!("  Daemon executable: {display} (encoding: {encoding})");
                    if !human.contains(&expected) {
                        missing.push(format!("human live identity omitted {expected:?}"));
                    }
                }
                _ => missing.push(
                    "daemon.executable_path must carry display + encoding in live identity"
                        .to_owned(),
                ),
            }
        }
        None => missing.push("daemon.executable_path missing from live identity".to_owned()),
    }
    (!missing.is_empty()).then(|| missing.join("; "))
}

fn live_daemon_fact_failure(
    report: &Value,
    expected_state: &str,
    expected: &DaemonInfo,
) -> Option<String> {
    let Some(daemon) = report.get("daemon").and_then(Value::as_object) else {
        return Some("doctor omitted the typed daemon section".to_owned());
    };
    let actual = (
        daemon.get("state").and_then(Value::as_str),
        daemon.get("pid").and_then(Value::as_u64),
        daemon.get("hyperd_endpoint").and_then(Value::as_str),
        daemon.get("health_port").and_then(Value::as_u64),
        daemon.get("started_at").and_then(Value::as_str),
        daemon.get("version").and_then(Value::as_str),
    );
    let expected_facts = (
        Some(expected_state),
        Some(u64::from(expected.pid)),
        Some(expected.hyperd_endpoint.as_str()),
        Some(u64::from(expected.health_port)),
        Some(expected.started_at.as_str()),
        Some(expected.version.as_str()),
    );
    (actual != expected_facts).then(|| {
        format!("live daemon facts mismatch: actual={actual:?}, expected={expected_facts:?}")
    })
}

fn collect_reported_paths(value: &Value, paths: &mut Vec<(String, String)>) {
    match value {
        Value::Object(object) => {
            if object.contains_key("display") || object.contains_key("encoding") {
                let display = object
                    .get("display")
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| panic!("reported path missing string display: {value}"));
                let encoding = object
                    .get("encoding")
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| panic!("reported path missing string encoding: {value}"));
                assert!(
                    matches!(encoding, "utf8" | "lossy"),
                    "reported path encoding must be utf8 or lossy: {value}"
                );
                assert!(
                    display.len() <= MAX_REPORTED_STRING_BYTES,
                    "reported path exceeds 4 KiB: {} bytes",
                    display.len()
                );
                paths.push((display.to_owned(), encoding.to_owned()));
            }
            for child in object.values() {
                collect_reported_paths(child, paths);
            }
        }
        Value::Array(values) => {
            for child in values {
                collect_reported_paths(child, paths);
            }
        }
        _ => {}
    }
}

fn contains_string(value: &Value, expected: &str) -> bool {
    match value {
        Value::String(actual) => actual == expected,
        Value::Array(values) => values.iter().any(|value| contains_string(value, expected)),
        Value::Object(object) => object
            .values()
            .any(|value| contains_string(value, expected)),
        _ => false,
    }
}

fn contains_number(value: &Value, expected: u64) -> bool {
    match value {
        Value::Number(actual) => actual.as_u64() == Some(expected),
        Value::Array(values) => values.iter().any(|value| contains_number(value, expected)),
        Value::Object(object) => object
            .values()
            .any(|value| contains_number(value, expected)),
        _ => false,
    }
}

fn assert_all_strings_bounded(value: &Value) {
    match value {
        Value::String(string) => assert!(
            string.len() <= MAX_REPORTED_STRING_BYTES,
            "doctor emitted an overlong string ({} bytes)",
            string.len()
        ),
        Value::Array(values) => {
            for child in values {
                assert_all_strings_bounded(child);
            }
        }
        Value::Object(object) => {
            for child in object.values() {
                assert_all_strings_bounded(child);
            }
        }
        _ => {}
    }
}

fn assert_local_path_sharing_warning(text: &str) {
    let lower = text.to_lowercase();
    assert!(
        lower.contains("local paths") && lower.contains("review") && lower.contains("shar"),
        "doctor must explicitly warn users to review local paths before sharing:\n{text}"
    );
}

fn normalized_human_text(text: &str) -> String {
    text.to_lowercase().replace(['_', '-'], " ")
}

fn assert_visible_escape(text: &str, codepoint: u32) {
    let candidates = [
        format!("\\u{{{codepoint:x}}}"),
        format!("\\u{codepoint:04x}"),
        format!("\\x{codepoint:02x}"),
    ];
    assert!(
        candidates.iter().any(|candidate| text.contains(candidate)),
        "human output must preserve U+{codepoint:04X} as a visible escape; accepted forms: {candidates:?}\n{text}"
    );
}

#[derive(Debug)]
struct NonUtf8PathFixture {
    raw: OsString,
    display: String,
}

#[cfg(unix)]
fn non_utf8_path(root: &Path, stem: &str) -> NonUtf8PathFixture {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    let mut bytes = root.as_os_str().as_bytes().to_vec();
    bytes.extend_from_slice(format!("/{stem}-").as_bytes());
    bytes.push(0xff);
    bytes.extend_from_slice(b".hyper");
    NonUtf8PathFixture {
        raw: OsString::from_vec(bytes),
        display: format!("{}/{stem}-\u{fffd}.hyper", root.display()),
    }
}

#[cfg(windows)]
fn non_utf8_path(root: &Path, stem: &str) -> NonUtf8PathFixture {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    let mut wide: Vec<u16> = root.as_os_str().encode_wide().collect();
    wide.push(u16::from(b'\\'));
    wide.extend(stem.encode_utf16());
    wide.push(u16::from(b'-'));
    wide.push(0xD800);
    wide.extend(".hyper".encode_utf16());
    NonUtf8PathFixture {
        raw: OsString::from_wide(&wide),
        display: format!("{}\\{stem}-\u{fffd}.hyper", root.display()),
    }
}

#[cfg(windows)]
const fn platform_hyperd_name() -> &'static str {
    "hyperd.exe"
}

#[cfg(not(windows))]
const fn platform_hyperd_name() -> &'static str {
    "hyperd"
}

fn create_upward_hyperd_candidate(sandbox: &DoctorSandbox) -> PathBuf {
    let candidate = sandbox
        .root
        .join(".hyperd")
        .join("current")
        .join(platform_hyperd_name());
    std::fs::create_dir_all(candidate.parent().expect("candidate has parent"))
        .expect("create upward hyperd fixture directory");
    std::fs::write(&candidate, b"not executed by doctor").expect("write upward hyperd fixture");
    candidate
}

#[cfg(unix)]
fn non_utf8_overlong_path(root: &Path) -> OsString {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    let mut bytes = root.as_os_str().as_bytes().to_vec();
    bytes.extend_from_slice(b"/hyperd-\xff-");
    bytes.extend(std::iter::repeat_n(b'x', 5 * 1024));
    OsString::from_vec(bytes)
}

#[cfg(windows)]
fn non_utf8_overlong_path(root: &Path) -> OsString {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    let mut wide: Vec<u16> = root.as_os_str().encode_wide().collect();
    wide.extend([u16::from(b'\\'), 0xD800, u16::from(b'-')]);
    wide.extend(std::iter::repeat_n(u16::from(b'x'), 5 * 1024));
    OsString::from_wide(&wide)
}

#[cfg(not(any(unix, windows)))]
fn non_utf8_overlong_path(root: &Path) -> OsString {
    root.join(format!("hyperd-{}", "x".repeat(5 * 1024)))
        .into_os_string()
}

#[test]
fn doctor_cli_json_and_human_smoke_is_side_effect_free() {
    let sandbox = DoctorSandbox::new();
    let hyperd_path = sandbox.root.join("missing-hyperd");
    let launcher_metadata = sandbox.launcher_metadata("hyperdb-mcp-test-wrapper");
    let before = snapshot_tree(&sandbox.root);

    let json_output = sandbox.run(true, &launcher_metadata, hyperd_path.as_os_str());
    assert_success(&json_output, "hyperdb-mcp doctor --json");
    assert_eq!(
        snapshot_tree(&sandbox.root),
        before,
        "JSON doctor must be byte-for-byte side-effect-free"
    );
    sandbox.assert_no_artifacts();

    let report: Value = serde_json::from_slice(&json_output.stdout)
        .unwrap_or_else(|error| panic!("doctor --json must emit valid JSON: {error}"));
    assert_exact_top_level_keys(&report);
    let status = report
        .get("status")
        .and_then(Value::as_str)
        .expect("doctor JSON needs typed string section `status`");
    let installation = report_object(&report, "installation");
    let configuration = report_object(&report, "configuration");
    let daemon = report_object(&report, "daemon");
    let tool_catalog = report_object(&report, "tool_catalog");
    let warnings = report
        .get("warnings")
        .and_then(Value::as_array)
        .expect("doctor JSON needs typed array section `warnings`");

    assert!(!status.is_empty(), "doctor status must be meaningful");
    assert!(
        contains_string(&Value::Object(configuration.clone()), "persistent_attached"),
        "environment-provided persistent path must report persistent_attached mode"
    );
    assert!(
        contains_string(&Value::Object(configuration.clone()), "environment"),
        "persistent path source must be reported as environment"
    );
    assert!(
        contains_number(&Value::Object(tool_catalog.clone()), 33),
        "doctor must measure all 33 generated MCP tools"
    );
    assert_eq!(
        configuration.get("read_only").and_then(Value::as_bool),
        Some(true),
        "--read-only must survive in typed JSON"
    );
    assert_eq!(
        configuration.get("no_daemon").and_then(Value::as_bool),
        Some(true),
        "--no-daemon must survive in typed JSON"
    );
    let daemon_state = daemon
        .get("state")
        .and_then(Value::as_str)
        .expect("daemon section must contain one typed discovery state");
    assert_eq!(
        daemon_state, "missing",
        "OS-assigned pinned isolation port must prevent a resident developer daemon from changing this smoke test"
    );

    let mut reported_paths = Vec::new();
    collect_reported_paths(&report, &mut reported_paths);
    for expected in sandbox.required_utf8_reported_paths(&hyperd_path) {
        let expected = expected.to_string_lossy();
        assert!(
            reported_paths
                .iter()
                .any(|(display, encoding)| display == expected.as_ref() && encoding == "utf8"),
            "required path must carry display + utf8 semantics: {expected}; paths={reported_paths:?}"
        );
    }
    let current_exe = canonicalize_for_test(Path::new(env!("CARGO_BIN_EXE_hyperdb-mcp")))
        .expect("canonicalize test binary");
    assert!(
        reported_paths.iter().any(|(display, encoding)| {
            display == current_exe.to_string_lossy().as_ref() && encoding == "utf8"
        }),
        "installation must report the actual native executable path"
    );
    assert_local_path_sharing_warning(&report.to_string());
    assert!(
        !report.to_string().contains(SECRET_SENTINEL),
        "unknown launcher keys must never appear in JSON"
    );

    let human_output = sandbox.run(false, &launcher_metadata, hyperd_path.as_os_str());
    assert_success(&human_output, "hyperdb-mcp doctor");
    assert_eq!(
        snapshot_tree(&sandbox.root),
        before,
        "human doctor must be byte-for-byte side-effect-free"
    );
    sandbox.assert_no_artifacts();

    let human =
        std::str::from_utf8(&human_output.stdout).expect("human doctor report must be valid UTF-8");
    let normalized = normalized_human_text(human);
    for heading in [
        "status",
        "installation",
        "configuration",
        "daemon",
        "tool catalog",
        "warnings",
    ] {
        assert!(
            normalized.contains(heading),
            "human report missing `{heading}` section:\n{human}"
        );
    }
    for core_fact in [
        status,
        "persistent_attached",
        "environment",
        daemon_state,
        env!("CARGO_PKG_VERSION"),
    ] {
        // Normalize the needle exactly as the haystack was. The `-` matters:
        // a prerelease version like `1.0.0-rc.1` renders as `1.0.0 rc.1` after
        // normalization, so replacing only `_` here would never match it.
        let normalized_fact = normalized_human_text(core_fact);
        assert!(
            normalized.contains(&normalized_fact),
            "JSON/human reports disagree about core fact `{core_fact}`:\n{human}"
        );
    }
    assert!(
        normalized.contains("33") && normalized.contains("tool"),
        "human report must carry the generated 33-tool count:\n{human}"
    );
    assert!(
        human.contains("  Read only: true") && human.contains("  No daemon: true"),
        "human report must agree with both true configuration flags:\n{human}"
    );
    let path_parity_contracts = [
        ("observed_persistent_path", "Observed persistent path"),
        ("resolved_persistent_path", "Resolved persistent path"),
        ("resolved_persistent_parent", "Resolved persistent parent"),
        ("daemon_state_directory", "Daemon state directory"),
        ("daemon_discovery_file", "Daemon discovery file"),
        ("client_log", "Client log"),
        ("observed_hyperd_path", "Observed HYPERD_PATH"),
        ("effective_hyperd_path", "Effective hyperd path"),
        (
            "upward_hyperd_candidate",
            "Upward .hyperd/current candidate",
        ),
    ];
    for (key, label) in path_parity_contracts {
        if let Some(failure) = human_path_parity_failure(&report, human, key, label) {
            panic!("JSON/human path parity failed for {key}: {failure}");
        }
    }
    let catalog_failures = catalog_human_parity_failures(&report, human);
    assert!(
        catalog_failures.is_empty(),
        "JSON/human catalog parity failures:\n{}",
        catalog_failures.join("\n")
    );
    for expected in sandbox.required_utf8_reported_paths(&hyperd_path) {
        assert!(
            human.contains(expected.to_string_lossy().as_ref()),
            "human report missing path {}",
            expected.display()
        );
    }
    assert_local_path_sharing_warning(human);
    assert!(
        !human.contains(SECRET_SENTINEL),
        "unknown launcher keys must never appear in human output"
    );
    assert!(
        installation.contains_key("native_executable"),
        "installation section must identify the native executable"
    );
    assert!(warnings.iter().all(Value::is_object) || warnings.iter().all(Value::is_string));
}

#[test]
fn doctor_human_output_escapes_and_bounds_reported_paths() {
    let sandbox = DoctorSandbox::new();
    let controlled_wrapper_name = "hyperdb-mcp\0\u{1}\u{1b}\u{7f}";
    let launcher_metadata = sandbox.launcher_metadata(controlled_wrapper_name);
    let hyperd_path = non_utf8_overlong_path(&sandbox.root);
    let before = snapshot_tree(&sandbox.root);

    let json_output = sandbox.run(true, &launcher_metadata, &hyperd_path);
    assert_success(&json_output, "hyperdb-mcp doctor --json");
    let report: Value = serde_json::from_slice(&json_output.stdout)
        .unwrap_or_else(|error| panic!("doctor --json must emit valid JSON: {error}"));
    assert_eq!(
        daemon_state(&report),
        Some("missing"),
        "OS-assigned pinned isolation port must keep the edge-case report daemon-independent"
    );
    assert_eq!(snapshot_tree(&sandbox.root), before);
    sandbox.assert_no_artifacts();

    assert!(
        contains_string(&report, controlled_wrapper_name),
        "known launcher field must survive in typed JSON"
    );
    assert_all_strings_bounded(&report);
    let mut reported_paths = Vec::new();
    collect_reported_paths(&report, &mut reported_paths);
    #[cfg(any(unix, windows))]
    assert!(
        reported_paths
            .iter()
            .any(|(display, encoding)| { encoding == "lossy" && display.contains('\u{fffd}') }),
        "OS-supported non-UTF-8 paths must be marked lossy: {reported_paths:?}"
    );
    assert!(
        !report.to_string().contains(SECRET_SENTINEL),
        "unknown secret sentinel must not be re-emitted"
    );
    assert_local_path_sharing_warning(&report.to_string());

    let human_output = sandbox.run(false, &launcher_metadata, &hyperd_path);
    assert_success(&human_output, "hyperdb-mcp doctor");
    assert_eq!(snapshot_tree(&sandbox.root), before);
    sandbox.assert_no_artifacts();

    let human =
        std::str::from_utf8(&human_output.stdout).expect("human doctor report must be valid UTF-8");
    let stderr = String::from_utf8_lossy(&human_output.stderr);
    assert!(
        human.contains("hyperdb-mcp"),
        "known field must be rendered"
    );
    for raw_control in ['\0', '\u{1}', '\u{1b}', '\u{7f}'] {
        assert!(
            !human.contains(raw_control),
            "human report leaked raw control U+{:04X}",
            u32::from(raw_control)
        );
    }
    for codepoint in [0, 1, 0x1b, 0x7f] {
        assert_visible_escape(human, codepoint);
    }
    assert!(
        normalized_human_text(human).contains("lossy"),
        "human paths must expose lossy encoding semantics:\n{human}"
    );
    assert!(
        !human.contains(&"x".repeat(MAX_REPORTED_STRING_BYTES + 1)),
        "human output contains an unbounded reported path"
    );
    assert!(
        !human.contains(SECRET_SENTINEL) && !stderr.contains(SECRET_SENTINEL),
        "unknown secret sentinel must be absent from all human-mode output"
    );
    assert_local_path_sharing_warning(human);
}

#[test]
fn doctor_persistent_tilde_sources_match_runtime_and_preserve_cli_semantics() {
    struct Case {
        label: &'static str,
        args: &'static [&'static str],
        persistent_environment: Option<&'static str>,
        expected_source: &'static str,
    }

    let sandbox = DoctorSandbox::new();
    let launcher_metadata = sandbox.launcher_metadata("hyperdb-mcp-test-wrapper");
    let missing_hyperd = sandbox.root.join("missing-hyperd");
    let expected_effective = sandbox.home_dir.join("data.hyper");
    let expected_parent = sandbox.home_dir.clone();
    let expected_log = sandbox.home_dir.join("hyperdb-mcp.log");
    let before = snapshot_tree(&sandbox.root);
    let cases = [
        Case {
            label: "preferred CLI",
            args: &["doctor", "--json", "--persistent-db", "~/data.hyper"],
            persistent_environment: Some("~/ignored-environment.hyper"),
            expected_source: "cli",
        },
        Case {
            label: "environment",
            args: &["doctor", "--json"],
            persistent_environment: Some("~/data.hyper"),
            expected_source: "environment",
        },
        Case {
            label: "deprecated alias",
            args: &["doctor", "--json", "--workspace", "~/data.hyper"],
            persistent_environment: Some("~/ignored-environment.hyper"),
            expected_source: "deprecated_alias",
        },
    ];
    let mut failures = Vec::new();

    for case in cases {
        let output = sandbox.run_with_options(
            case.args,
            case.persistent_environment.map(OsStr::new),
            &launcher_metadata,
            Some(missing_hyperd.as_os_str()),
            sandbox.isolated_daemon_port,
        );
        if !output.status.success() {
            failures.push(format!(
                "{} did not exit zero: stdout={:?}, stderr={:?}",
                case.label,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ));
            continue;
        }
        let report = match serde_json::from_slice::<Value>(&output.stdout) {
            Ok(report) => report,
            Err(error) => {
                failures.push(format!("{} did not emit JSON: {error}", case.label));
                continue;
            }
        };
        let Some(configuration) = report.get("configuration").and_then(Value::as_object) else {
            failures.push(format!("{} omitted typed configuration", case.label));
            continue;
        };

        if configuration
            .get("persistent_path_source")
            .and_then(Value::as_str)
            != Some(case.expected_source)
        {
            failures.push(format!(
                "{} source was not {}: {}",
                case.label, case.expected_source, report
            ));
        }
        if let Some(failure) = reported_path_failure(
            configuration,
            "observed_persistent_path",
            "~/data.hyper",
            "utf8",
        ) {
            failures.push(format!("{}: {failure}", case.label));
        }
        for failure in [
            path_fact_failure(
                configuration,
                "resolved_persistent_path",
                expected_effective.to_string_lossy().as_ref(),
                "utf8",
                false,
                false,
                false,
            ),
            path_fact_failure(
                configuration,
                "resolved_persistent_parent",
                expected_parent.to_string_lossy().as_ref(),
                "utf8",
                false,
                false,
                false,
            ),
            path_fact_failure(
                configuration,
                "client_log",
                expected_log.to_string_lossy().as_ref(),
                "utf8",
                false,
                false,
                false,
            ),
        ]
        .into_iter()
        .flatten()
        {
            failures.push(format!("{}: {failure}", case.label));
        }
        if daemon_state(&report) != Some("missing") {
            failures.push(format!(
                "{} escaped the pinned missing-daemon sandbox: {:?}",
                case.label,
                daemon_state(&report)
            ));
        }
        if case.expected_source == "deprecated_alias"
            && !has_warning_code(&report, "deprecated_persistent_alias")
        {
            failures.push("deprecated alias omitted its typed warning".to_owned());
        }
        if snapshot_tree(&sandbox.root) != before {
            failures.push(format!("{} changed the isolated filesystem", case.label));
        }
    }

    let disabled = sandbox.run_with_options(
        &["doctor", "--json", "--ephemeral-only"],
        Some(OsStr::new("~/ignored-while-disabled.hyper")),
        &launcher_metadata,
        Some(missing_hyperd.as_os_str()),
        sandbox.isolated_daemon_port,
    );
    if disabled.status.success() {
        match serde_json::from_slice::<Value>(&disabled.stdout) {
            Ok(report) => {
                let configuration = report_object(&report, "configuration");
                if configuration.get("persistent_mode").and_then(Value::as_str)
                    != Some("ephemeral_only")
                    || configuration
                        .get("persistent_path_source")
                        .and_then(Value::as_str)
                        != Some("disabled")
                    || configuration
                        .get("observed_persistent_path")
                        .is_some_and(|value| !value.is_null())
                    || configuration
                        .get("resolved_persistent_path")
                        .is_some_and(|value| !value.is_null())
                    || configuration
                        .get("resolved_persistent_parent")
                        .is_some_and(|value| !value.is_null())
                {
                    failures.push(format!(
                        "ephemeral-only did not preserve exact disabled semantics: {report}"
                    ));
                }
                let log_display = report
                    .pointer("/configuration/client_log/path/display")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !Path::new(log_display).starts_with(&sandbox.runtime_tmp_dir)
                    || !Path::new(log_display).ends_with("hyperdb-mcp.log")
                {
                    failures.push(format!(
                        "ephemeral-only client log was not under isolated runtime temp: {log_display:?}"
                    ));
                }
                if daemon_state(&report) != Some("missing") {
                    failures.push("ephemeral-only daemon state was not exactly missing".to_owned());
                }
            }
            Err(error) => failures.push(format!("ephemeral-only did not emit JSON: {error}")),
        }
    } else {
        failures.push(format!(
            "ephemeral-only doctor failed: {}",
            String::from_utf8_lossy(&disabled.stderr)
        ));
    }

    let conflicts = [
        (
            "ephemeral/path conflict",
            &[
                "doctor",
                "--json",
                "--ephemeral-only",
                "--persistent-db",
                "~/data.hyper",
            ][..],
            "error: --ephemeral-only is incompatible with --persistent-db / --workspace\n",
        ),
        (
            "preferred/deprecated conflict",
            &[
                "doctor",
                "--json",
                "--persistent-db",
                "~/data.hyper",
                "--workspace",
                "~/other.hyper",
            ][..],
            "error: Both --persistent-db and --workspace were supplied. --workspace is a deprecated alias; pass only --persistent-db.\n",
        ),
    ];
    for (label, args, expected_stderr) in conflicts {
        let output = sandbox.run_with_options(
            args,
            None,
            &launcher_metadata,
            Some(missing_hyperd.as_os_str()),
            sandbox.isolated_daemon_port,
        );
        if output.status.code() != Some(2)
            || !output.stdout.is_empty()
            || output.stderr != expected_stderr.as_bytes()
        {
            failures.push(format!(
                "{label} contract mismatch: status={:?}, stdout={:?}, stderr={:?}",
                output.status.code(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
    }

    if snapshot_tree(&sandbox.root) != before {
        failures.push("persistent resolution cases changed isolated filesystem bytes".to_owned());
    }
    assert!(
        failures.is_empty(),
        "persistent path doctor contract failures:\n{}",
        failures.join("\n")
    );
}

#[test]
fn doctor_relative_persistent_path_normalizes_parent_and_matches_runtime() {
    let sandbox = DoctorSandbox::new();
    let launcher_metadata = sandbox.launcher_metadata("hyperdb-mcp-test-wrapper");
    let missing_hyperd = sandbox.root.join("missing-hyperd");
    let before = snapshot_tree(&sandbox.root);
    let mut failures = Vec::new();

    let json_output = sandbox.run_with_options(
        &["doctor", "--json", "--persistent-db", "foo.hyper"],
        Some(OsStr::new("ignored-environment.hyper")),
        &launcher_metadata,
        Some(missing_hyperd.as_os_str()),
        sandbox.isolated_daemon_port,
    );
    let report = parse_json_report(
        &json_output,
        "hyperdb-mcp doctor --json --persistent-db foo.hyper",
    );
    assert_exact_top_level_keys(&report);
    let configuration = report_object(&report, "configuration");
    if configuration
        .get("persistent_path_source")
        .and_then(Value::as_str)
        != Some("cli")
    {
        failures.push("relative preferred CLI path lost its source".to_owned());
    }
    if let Some(failure) = reported_path_failure(
        configuration,
        "observed_persistent_path",
        "foo.hyper",
        "utf8",
    ) {
        failures.push(failure);
    }
    for failure in [
        path_fact_failure(
            configuration,
            "resolved_persistent_path",
            "foo.hyper",
            "utf8",
            false,
            false,
            false,
        ),
        path_fact_failure(
            configuration,
            "resolved_persistent_parent",
            ".",
            "utf8",
            true,
            false,
            true,
        ),
        path_fact_failure(
            configuration,
            "client_log",
            "hyperdb-mcp.log",
            "utf8",
            false,
            false,
            false,
        ),
    ]
    .into_iter()
    .flatten()
    {
        failures.push(failure);
    }
    if daemon_state(&report) != Some("missing") {
        failures.push("relative path case escaped pinned missing-daemon isolation".to_owned());
    }

    let human_output = sandbox.run_with_options(
        &["doctor", "--persistent-db", "foo.hyper"],
        Some(OsStr::new("ignored-environment.hyper")),
        &launcher_metadata,
        Some(missing_hyperd.as_os_str()),
        sandbox.isolated_daemon_port,
    );
    assert_success(
        &human_output,
        "hyperdb-mcp doctor --persistent-db foo.hyper",
    );
    let human = std::str::from_utf8(&human_output.stdout)
        .expect("relative-path human doctor report must be UTF-8");
    for (key, label) in [
        ("observed_persistent_path", "Observed persistent path"),
        ("resolved_persistent_path", "Resolved persistent path"),
        ("resolved_persistent_parent", "Resolved persistent parent"),
        ("client_log", "Client log"),
    ] {
        if let Some(failure) = human_path_parity_failure(&report, human, key, label) {
            failures.push(failure);
        }
    }
    let expected_parent = "  Resolved persistent parent: . (encoding: utf8; exists: true; file: false; directory: true)";
    if !human.contains(expected_parent) {
        failures.push(format!(
            "human report did not normalize the relative parent to an existing directory: {human}"
        ));
    }
    if snapshot_tree(&sandbox.root) != before {
        failures.push("relative-path doctor changed isolated filesystem bytes".to_owned());
    }
    sandbox.assert_no_artifacts();
    assert!(
        failures.is_empty(),
        "relative persistent path contract failures:\n{}",
        failures.join("\n")
    );
}

#[test]
fn doctor_nested_tilde_home_expands_once_and_preserves_raw_input() {
    let sandbox = DoctorSandbox::new();
    let launcher_metadata = sandbox.launcher_metadata("hyperdb-mcp-test-wrapper");
    let missing_hyperd = sandbox.root.join("missing-hyperd");
    let nested_home = PathBuf::from("~").join("outer");
    let nested_home_fixture = sandbox.root.join(&nested_home);
    std::fs::create_dir_all(&nested_home_fixture)
        .expect("create literal nested-tilde HOME fixture");
    let expected_effective = nested_home.join("data.hyper");
    let expected_log = nested_home.join("hyperdb-mcp.log");
    let before = snapshot_tree(&sandbox.root);
    let mut failures = Vec::new();

    let json_output = sandbox.run_with_home_options(
        &["doctor", "--json", "--persistent-db", "~/data.hyper"],
        Some(OsStr::new("~/ignored-environment.hyper")),
        &launcher_metadata,
        Some(missing_hyperd.as_os_str()),
        sandbox.isolated_daemon_port,
        nested_home.as_os_str(),
    );
    let report = parse_json_report(&json_output, "hyperdb-mcp doctor --json with HOME=~/outer");
    assert_exact_top_level_keys(&report);
    let configuration = report_object(&report, "configuration");
    if configuration
        .get("persistent_path_source")
        .and_then(Value::as_str)
        != Some("cli")
    {
        failures.push("nested-tilde preferred CLI path lost its source".to_owned());
    }
    if let Some(failure) = reported_path_failure(
        configuration,
        "observed_persistent_path",
        "~/data.hyper",
        "utf8",
    ) {
        failures.push(failure);
    }
    for failure in [
        path_fact_failure(
            configuration,
            "resolved_persistent_path",
            expected_effective.to_string_lossy().as_ref(),
            "utf8",
            false,
            false,
            false,
        ),
        path_fact_failure(
            configuration,
            "resolved_persistent_parent",
            nested_home.to_string_lossy().as_ref(),
            "utf8",
            true,
            false,
            true,
        ),
        path_fact_failure(
            configuration,
            "client_log",
            expected_log.to_string_lossy().as_ref(),
            "utf8",
            false,
            false,
            false,
        ),
    ]
    .into_iter()
    .flatten()
    {
        failures.push(failure);
    }
    if daemon_state(&report) != Some("missing") {
        failures.push("nested-tilde case escaped pinned missing-daemon isolation".to_owned());
    }

    let human_output = sandbox.run_with_home_options(
        &["doctor", "--persistent-db", "~/data.hyper"],
        Some(OsStr::new("~/ignored-environment.hyper")),
        &launcher_metadata,
        Some(missing_hyperd.as_os_str()),
        sandbox.isolated_daemon_port,
        nested_home.as_os_str(),
    );
    assert_success(&human_output, "hyperdb-mcp doctor with HOME=~/outer");
    let human = std::str::from_utf8(&human_output.stdout)
        .expect("nested-tilde human doctor report must be UTF-8");
    for (key, label) in [
        ("observed_persistent_path", "Observed persistent path"),
        ("resolved_persistent_path", "Resolved persistent path"),
        ("resolved_persistent_parent", "Resolved persistent parent"),
        ("client_log", "Client log"),
    ] {
        if let Some(failure) = human_path_parity_failure(&report, human, key, label) {
            failures.push(failure);
        }
    }
    for expected in [
        "  Observed persistent path: ~/data.hyper (encoding: utf8)".to_owned(),
        format!(
            "  Resolved persistent path: {} (encoding: utf8; exists: false; file: false; directory: false)",
            expected_effective.display()
        ),
        format!(
            "  Client log: {} (encoding: utf8; exists: false; file: false; directory: false)",
            expected_log.display()
        ),
    ] {
        if !human.contains(&expected) {
            failures.push(format!(
                "nested-tilde JSON/human single-expansion fact missing: {expected:?}"
            ));
        }
    }
    if snapshot_tree(&sandbox.root) != before {
        failures.push("nested-tilde doctor changed isolated filesystem bytes".to_owned());
    }
    assert!(
        !sandbox.persistent_path.exists()
            && !sandbox.root.join(&expected_effective).exists()
            && !sandbox.root.join(&expected_log).exists()
            && !sandbox.state_dir.join("daemon.json").exists(),
        "nested-tilde doctor must not create a database, log, or discovery file"
    );
    assert!(
        failures.is_empty(),
        "nested-tilde single-expansion contract failures:\n{}",
        failures.join("\n")
    );
}

#[cfg(any(unix, windows))]
#[test]
fn doctor_non_utf8_persistent_environment_reports_observed_and_effective_paths() {
    let sandbox = DoctorSandbox::new();
    let launcher_metadata = sandbox.launcher_metadata("hyperdb-mcp-test-wrapper");
    let missing_hyperd = sandbox.root.join("missing-hyperd");
    let configured = non_utf8_path(&sandbox.root, "persistent");
    let effective = PathBuf::from(&configured.display);
    let effective_parent = effective.parent().expect("effective path has a parent");
    let expected_log = effective_parent.join("hyperdb-mcp.log");
    let before = snapshot_tree(&sandbox.root);

    let output = sandbox.run_with_options(
        &["doctor", "--json"],
        Some(configured.raw.as_os_str()),
        &launcher_metadata,
        Some(missing_hyperd.as_os_str()),
        sandbox.isolated_daemon_port,
    );
    let report = parse_json_report(
        &output,
        "hyperdb-mcp doctor --json with non-UTF-8 HYPERDB_PERSISTENT_DB",
    );
    let configuration = report_object(&report, "configuration");
    let mut failures = Vec::new();

    if configuration
        .get("persistent_path_source")
        .and_then(Value::as_str)
        != Some("environment")
    {
        failures.push(format!(
            "non-UTF-8 environment path lost its source: {report}"
        ));
    }
    if let Some(failure) = reported_path_failure(
        configuration,
        "observed_persistent_path",
        &configured.display,
        "lossy",
    ) {
        failures.push(failure);
    }
    for failure in [
        path_fact_failure(
            configuration,
            "resolved_persistent_path",
            effective.to_string_lossy().as_ref(),
            "utf8",
            false,
            false,
            false,
        ),
        path_fact_failure(
            configuration,
            "resolved_persistent_parent",
            effective_parent.to_string_lossy().as_ref(),
            "utf8",
            true,
            false,
            true,
        ),
        path_fact_failure(
            configuration,
            "client_log",
            expected_log.to_string_lossy().as_ref(),
            "utf8",
            false,
            false,
            false,
        ),
    ]
    .into_iter()
    .flatten()
    {
        failures.push(failure);
    }
    if daemon_state(&report) != Some("missing") {
        failures.push(format!(
            "pinned daemon state was not missing: {:?}",
            daemon_state(&report)
        ));
    }
    if snapshot_tree(&sandbox.root) != before {
        failures.push("doctor changed filesystem bytes for non-UTF-8 input".to_owned());
    }
    sandbox.assert_no_artifacts();
    assert_all_strings_bounded(&report);
    assert!(
        failures.is_empty(),
        "non-UTF-8 observed/effective persistent path failures:\n{}",
        failures.join("\n")
    );
}

#[test]
fn doctor_hyperd_path_diagnostics_match_runtime_resolution() {
    let mut failures = Vec::new();

    #[cfg(any(unix, windows))]
    {
        let sandbox = DoctorSandbox::new();
        let launcher_metadata = sandbox.launcher_metadata("hyperdb-mcp-test-wrapper");
        let configured = non_utf8_path(&sandbox.root, "ignored-hyperd");
        let upward = create_upward_hyperd_candidate(&sandbox);
        let before = snapshot_tree(&sandbox.root);
        let output = sandbox.run_with_options(
            &["doctor", "--json"],
            Some(sandbox.persistent_path.as_os_str()),
            &launcher_metadata,
            Some(configured.raw.as_os_str()),
            sandbox.isolated_daemon_port,
        );
        let report = parse_json_report(
            &output,
            "hyperdb-mcp doctor --json with non-UTF-8 HYPERD_PATH",
        );
        let configuration = report_object(&report, "configuration");
        for failure in [
            path_fact_failure(
                configuration,
                "observed_hyperd_path",
                &configured.display,
                "lossy",
                false,
                false,
                false,
            ),
            path_fact_failure(
                configuration,
                "upward_hyperd_candidate",
                upward.to_string_lossy().as_ref(),
                "utf8",
                true,
                true,
                false,
            ),
        ]
        .into_iter()
        .flatten()
        {
            failures.push(format!("non-UTF-8 HYPERD_PATH: {failure}"));
        }
        let warnings = normalized_human_text(&warning_text(&report));
        if !warnings.contains("non utf 8")
            || !(warnings.contains("ignored") || warnings.contains("upward"))
        {
            failures.push(format!(
                "non-UTF-8 HYPERD_PATH needs a bounded warning that runtime ignores it and uses upward resolution: {}",
                report.get("warnings").unwrap_or(&Value::Null)
            ));
        }
        if has_warning_code(&report, "observed_hyperd_path_missing") {
            failures.push(
                "non-UTF-8 HYPERD_PATH was misleadingly diagnosed as the effective missing path"
                    .to_owned(),
            );
        }
        if daemon_state(&report) != Some("missing") {
            failures.push("non-UTF-8 case escaped the pinned daemon port".to_owned());
        }
        if snapshot_tree(&sandbox.root) != before {
            failures.push("non-UTF-8 HYPERD_PATH case changed filesystem bytes".to_owned());
        }
        assert_all_strings_bounded(&report);
        sandbox.assert_no_artifacts();
    }

    {
        let sandbox = DoctorSandbox::new();
        let launcher_metadata = sandbox.launcher_metadata("hyperdb-mcp-test-wrapper");
        let configured_directory = sandbox.root.join("configured-hyperd-directory");
        std::fs::create_dir(&configured_directory).expect("create empty HYPERD_PATH directory");
        let _unused_upward = create_upward_hyperd_candidate(&sandbox);
        let before = snapshot_tree(&sandbox.root);
        let output = sandbox.run_with_options(
            &["doctor", "--json"],
            Some(sandbox.persistent_path.as_os_str()),
            &launcher_metadata,
            Some(configured_directory.as_os_str()),
            sandbox.isolated_daemon_port,
        );
        let report = parse_json_report(
            &output,
            "hyperdb-mcp doctor --json with an empty HYPERD_PATH directory",
        );
        let configuration = report_object(&report, "configuration");
        if let Some(failure) = path_fact_failure(
            configuration,
            "observed_hyperd_path",
            configured_directory.to_string_lossy().as_ref(),
            "utf8",
            true,
            false,
            true,
        ) {
            failures.push(format!("empty HYPERD_PATH directory: {failure}"));
        }
        if configuration
            .get("upward_hyperd_candidate")
            .is_some_and(|value| !value.is_null())
        {
            failures
                .push("valid UTF-8 directory HYPERD_PATH must suppress upward fallback".to_owned());
        }
        let warnings = warning_text(&report);
        if !warnings.contains("directory")
            || !warnings.contains("hyperd")
            || !(warnings.contains("not found") || warnings.contains("missing"))
        {
            failures.push(format!(
                "existing HYPERD_PATH directory without {} needs an actionable bounded diagnostic: {}",
                platform_hyperd_name(),
                report.get("warnings").unwrap_or(&Value::Null)
            ));
        }
        if daemon_state(&report) != Some("missing") {
            failures.push("empty-directory case escaped the pinned daemon port".to_owned());
        }
        if snapshot_tree(&sandbox.root) != before {
            failures.push("empty-directory HYPERD_PATH case changed filesystem bytes".to_owned());
        }
        assert_all_strings_bounded(&report);
        sandbox.assert_no_artifacts();
    }

    {
        let sandbox = DoctorSandbox::new();
        let launcher_metadata = sandbox.launcher_metadata("hyperdb-mcp-test-wrapper");
        let _unused_upward = create_upward_hyperd_candidate(&sandbox);
        let before = snapshot_tree(&sandbox.root);
        let output = sandbox.run_with_options(
            &["doctor", "--json"],
            Some(sandbox.persistent_path.as_os_str()),
            &launcher_metadata,
            Some(OsStr::new("")),
            sandbox.isolated_daemon_port,
        );
        let report = parse_json_report(
            &output,
            "hyperdb-mcp doctor --json with empty UTF-8 HYPERD_PATH",
        );
        let configuration = report_object(&report, "configuration");
        if let Some(failure) = path_fact_failure(
            configuration,
            "observed_hyperd_path",
            "",
            "utf8",
            false,
            false,
            false,
        ) {
            failures.push(format!("empty UTF-8 HYPERD_PATH: {failure}"));
        }
        if configuration
            .get("upward_hyperd_candidate")
            .is_some_and(|value| !value.is_null())
        {
            failures.push(
                "empty UTF-8 HYPERD_PATH must match runtime by suppressing upward fallback"
                    .to_owned(),
            );
        }
        if !has_warning_code(&report, "observed_hyperd_path_missing") {
            failures.push(
                "empty UTF-8 HYPERD_PATH needs the bounded missing-path diagnostic runtime would produce"
                    .to_owned(),
            );
        }
        if daemon_state(&report) != Some("missing") {
            failures.push("empty UTF-8 case escaped the pinned daemon port".to_owned());
        }
        if snapshot_tree(&sandbox.root) != before {
            failures.push("empty UTF-8 HYPERD_PATH case changed filesystem bytes".to_owned());
        }
        assert_all_strings_bounded(&report);
        sandbox.assert_no_artifacts();
    }

    #[cfg(windows)]
    {
        let sandbox = DoctorSandbox::new();
        let launcher_metadata = sandbox.launcher_metadata("hyperdb-mcp-test-wrapper");
        let configured_stem = sandbox.root.join("configured-hyperd");
        let accepted_executable = PathBuf::from(format!("{}.exe", configured_stem.display()));
        std::fs::write(&accepted_executable, b"not executed by doctor")
            .expect("create Windows .exe fallback fixture");
        let _unused_upward = create_upward_hyperd_candidate(&sandbox);
        let before = snapshot_tree(&sandbox.root);
        let output = sandbox.run_with_options(
            &["doctor", "--json"],
            Some(sandbox.persistent_path.as_os_str()),
            &launcher_metadata,
            Some(configured_stem.as_os_str()),
            sandbox.isolated_daemon_port,
        );
        let report = parse_json_report(
            &output,
            "hyperdb-mcp doctor --json with Windows HYPERD_PATH stem",
        );
        let configuration = report_object(&report, "configuration");
        if let Some(failure) = path_fact_failure(
            configuration,
            "effective_hyperd_path",
            accepted_executable.to_string_lossy().as_ref(),
            "utf8",
            true,
            true,
            false,
        ) {
            failures.push(format!("Windows .exe fallback: {failure}"));
        }
        if has_warning_code(&report, "observed_hyperd_path_missing") {
            failures.push(
                "Windows HYPERD_PATH stem accepted through .exe must not be diagnosed as missing"
                    .to_owned(),
            );
        }
        if configuration
            .get("upward_hyperd_candidate")
            .is_some_and(|value| !value.is_null())
        {
            failures.push("accepted Windows HYPERD_PATH must suppress upward fallback".to_owned());
        }
        if daemon_state(&report) != Some("missing") {
            failures.push("Windows .exe case escaped the pinned daemon port".to_owned());
        }
        if snapshot_tree(&sandbox.root) != before {
            failures.push("Windows .exe HYPERD_PATH case changed filesystem bytes".to_owned());
        }
        assert_all_strings_bounded(&report);
        sandbox.assert_no_artifacts();
    }

    assert!(
        failures.is_empty(),
        "HYPERD_PATH doctor/runtime resolution mismatches:\n{}",
        failures.join("\n")
    );
}

#[test]
fn doctor_cli_reports_live_from_discovery_via_real_health_listener() {
    const ATTEMPTS: usize = 4;
    let mut failures = Vec::new();

    for attempt in 1..=ATTEMPTS {
        let sandbox = DoctorSandbox::new();
        let launcher_metadata = sandbox.launcher_metadata("hyperdb-mcp-test-wrapper");
        let missing_hyperd = sandbox.root.join("missing-hyperd");

        // Warm the already-built child before aligning the real listener's
        // nonblocking accept loop. This keeps process-loader latency from
        // determining whether the child lands inside the 100 ms sleep cadence.
        let warm_before = snapshot_tree(&sandbox.root);
        let warm = sandbox.run_with_options(
            &["doctor", "--json"],
            Some(sandbox.persistent_path.as_os_str()),
            &launcher_metadata,
            Some(missing_hyperd.as_os_str()),
            sandbox.isolated_daemon_port,
        );
        let warm_report = parse_json_report(&warm, "warm hyperdb-mcp doctor --json");
        if daemon_state(&warm_report) != Some("missing") {
            failures.push(format!(
                "attempt {attempt}: warmup escaped pinned missing-daemon isolation"
            ));
        }
        if snapshot_tree(&sandbox.root) != warm_before {
            failures.push(format!(
                "attempt {attempt}: warmup changed filesystem bytes"
            ));
        }

        let listener = RunningHealthListener::start();
        std::fs::create_dir_all(&sandbox.state_dir).expect("create discovery fixture directory");
        let discovery_path = sandbox.state_dir.join("daemon.json");
        let discovery_bytes =
            serde_json::to_vec_pretty(&listener.info).expect("serialize discovery fixture");
        std::fs::write(&discovery_path, &discovery_bytes).expect("write discovery fixture");
        let before = snapshot_tree(&sandbox.root);

        listener.prime_accept_sleep();
        let output = sandbox.run_with_options(
            &["doctor", "--json"],
            Some(sandbox.persistent_path.as_os_str()),
            &launcher_metadata,
            Some(missing_hyperd.as_os_str()),
            listener.port,
        );
        let report = parse_json_report(
            &output,
            "hyperdb-mcp doctor --json against discovery HealthListener",
        );
        assert_exact_top_level_keys(&report);
        if let Some(failure) =
            live_daemon_fact_failure(&report, "live_from_discovery", &listener.info)
        {
            failures.push(format!("attempt {attempt}: {failure}"));
        }
        if attempt == 1 {
            listener.prime_accept_sleep();
            let human_output = sandbox.run_with_options(
                &["doctor"],
                Some(sandbox.persistent_path.as_os_str()),
                &launcher_metadata,
                Some(missing_hyperd.as_os_str()),
                listener.port,
            );
            assert_success(
                &human_output,
                "hyperdb-mcp doctor against discovery HealthListener",
            );
            let human = std::str::from_utf8(&human_output.stdout)
                .expect("live-daemon human report must be UTF-8");
            if let Some(failure) = live_daemon_human_parity_failure(&report, human) {
                failures.push(format!(
                    "attempt {attempt}: live JSON/human identity parity: {failure}"
                ));
            }
        }
        if snapshot_tree(&sandbox.root) != before {
            failures.push(format!(
                "attempt {attempt}: discovery doctor changed filesystem bytes"
            ));
        }
        match std::fs::read(&discovery_path) {
            Ok(after) if after == discovery_bytes => {}
            Ok(after) => failures.push(format!(
                "attempt {attempt}: discovery bytes changed: before={discovery_bytes:?}, after={after:?}"
            )),
            Err(error) => failures.push(format!(
                "attempt {attempt}: discovery fixture disappeared: {error}"
            )),
        }
        assert_all_strings_bounded(&report);
        drop(listener);
    }

    assert!(
        failures.is_empty(),
        "real discovery HealthListener regressions:\n{}",
        failures.join("\n")
    );
}

#[test]
fn doctor_cli_reports_live_from_scan_via_real_health_listener() {
    const ATTEMPTS: usize = 4;
    let mut failures = Vec::new();

    for attempt in 1..=ATTEMPTS {
        let sandbox = DoctorSandbox::new();
        let launcher_metadata = sandbox.launcher_metadata("hyperdb-mcp-test-wrapper");
        let missing_hyperd = sandbox.root.join("missing-hyperd");

        let warm_before = snapshot_tree(&sandbox.root);
        let warm = sandbox.run_with_options(
            &["doctor", "--json"],
            Some(sandbox.persistent_path.as_os_str()),
            &launcher_metadata,
            Some(missing_hyperd.as_os_str()),
            sandbox.isolated_daemon_port,
        );
        let warm_report = parse_json_report(&warm, "warm hyperdb-mcp doctor --json");
        if daemon_state(&warm_report) != Some("missing") {
            failures.push(format!(
                "attempt {attempt}: warmup escaped pinned missing-daemon isolation"
            ));
        }
        if snapshot_tree(&sandbox.root) != warm_before {
            failures.push(format!(
                "attempt {attempt}: warmup changed filesystem bytes"
            ));
        }

        let listener = RunningHealthListener::start();
        let before = snapshot_tree(&sandbox.root);
        listener.prime_accept_sleep();
        let output = sandbox.run_with_options(
            &["doctor", "--json"],
            Some(sandbox.persistent_path.as_os_str()),
            &launcher_metadata,
            Some(missing_hyperd.as_os_str()),
            listener.port,
        );
        let report = parse_json_report(
            &output,
            "hyperdb-mcp doctor --json against scanned HealthListener",
        );
        if let Some(failure) = live_daemon_fact_failure(&report, "live_from_scan", &listener.info) {
            failures.push(format!("attempt {attempt}: {failure}"));
        }
        if snapshot_tree(&sandbox.root) != before {
            failures.push(format!(
                "attempt {attempt}: scan doctor changed filesystem bytes"
            ));
        }
        if sandbox.state_dir.exists() {
            failures.push(format!(
                "attempt {attempt}: scan doctor created a daemon state directory"
            ));
        }
        sandbox.assert_no_artifacts();
        assert_all_strings_bounded(&report);
        drop(listener);
    }

    assert!(
        failures.is_empty(),
        "real scanned HealthListener regressions:\n{}",
        failures.join("\n")
    );
}

/// `--help` is the recovery surface available even when neither MCP nor
/// `hyperd` can start, so its resolution and read-only claims must be exact.
/// This catches mutations to Clap help or its checked-in static README mirror,
/// especially cross-option token matches that conceal a false export claim.
#[test]
fn cli_help_matches_hyperd_and_read_only_contract() {
    fn run_help(args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_hyperdb-mcp"))
            .args(args)
            .output()
            .expect("run hyperdb-mcp help without starting the engine")
    }

    fn normalized_output(output: &Output) -> String {
        let mut bytes = output.stdout.clone();
        bytes.extend_from_slice(&output.stderr);
        String::from_utf8_lossy(&bytes)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    }

    fn option_scope<'a>(help: &'a str, flag: &str, next_flag: &str) -> &'a str {
        let Some((_, after_flag)) = help.split_once(flag) else {
            return "";
        };
        let Some((scope, _)) = after_flag.split_once(next_flag) else {
            return after_flag;
        };
        scope
    }

    const GUARDED_TOOLS: &[&str] = &[
        "execute",
        "load_data",
        "load_file",
        "load_files",
        "load_iceberg",
        "watch_directory",
        "save_query",
        "delete_query",
        "set_table_metadata",
        "copy_query",
        "kv_set",
        "kv_set_many",
        "kv_delete",
        "kv_pop",
        "kv_clear",
    ];

    let root_output = run_help(&["--help"]);
    let daemon_output = run_help(&["daemon", "--help"]);
    let root_help = normalized_output(&root_output);
    let daemon_help = normalized_output(&daemon_output);
    let read_only_help = option_scope(&root_help, "--read-only", "--no-daemon");
    let static_cli = PUBLIC_README
        .to_lowercase()
        .split_once("## cli reference")
        .and_then(|(_, tail)| tail.split_once("\n---"))
        .map(|(section, _)| section.to_owned())
        .unwrap_or_default();
    let mut failures = Vec::new();

    if !root_output.status.success() {
        failures.push(format!(
            "hyperdb-mcp --help exited with {}: {root_help}",
            root_output.status
        ));
    }
    if !daemon_output.status.success() {
        failures.push(format!(
            "hyperdb-mcp daemon --help exited with {}: {daemon_help}",
            daemon_output.status
        ));
    }

    if !(root_help.contains("hyperd_path")
        && root_help.contains("executable")
        && root_help.contains("directory")
        && root_help.contains(".hyperd/current")
        && ["walk upward", "search upward", "ancestor"]
            .iter()
            .any(|phrase| root_help.contains(phrase)))
    {
        failures.push(
            "root help must describe HYPERD_PATH as an executable or containing directory and the upward .hyperd/current fallback"
                .to_owned(),
        );
    }
    if ["searches path", "path fallback", "or on path"]
        .iter()
        .any(|phrase| root_help.contains(phrase))
    {
        failures.push("root help must not claim the runtime searches PATH".to_owned());
    }

    for tool in GUARDED_TOOLS {
        if !read_only_help.contains(tool) {
            failures.push(format!("--read-only help is missing guarded tool {tool}"));
        }
    }
    if !(read_only_help.contains("attach_database") && read_only_help.contains("writable")) {
        failures.push(
            "--read-only help must distinguish writable attach_database from allowed read-only attachment"
                .to_owned(),
        );
    }
    let export_availability = if let Some((_, after_export)) = read_only_help.split_once("export") {
        after_export.contains("hyper")
            && [
                "allowed",
                "remain available",
                "stays available",
                "stay available",
            ]
            .iter()
            .any(|phrase| after_export.contains(phrase))
    } else {
        false
    };
    if !(read_only_help.contains("unwatch_directory") && export_availability) {
        failures.push(
            "--read-only help must explicitly keep unwatch_directory and Hyper-format export available"
                .to_owned(),
        );
    }
    for false_claim in [
        "disables export",
        "export is disabled",
        "hyper-format export is disabled",
        "disables hyper-format export",
    ] {
        if read_only_help.contains(false_claim) {
            failures.push(format!(
                "--read-only help still makes the associated false claim {false_claim:?}"
            ));
        }
    }

    if !(daemon_help.contains("auto-spawn")
        && daemon_help.contains("scan")
        && daemon_help.contains("foreground")
        && daemon_help.contains("exact"))
    {
        failures.push(
            "daemon help must distinguish auto-spawn port scanning from the foreground daemon's exact/base-port bind"
                .to_owned(),
        );
    }
    if daemon_help.contains("daemon scans from the base port to find a free port") {
        failures.push(
            "foreground daemon help must not promise startup scanning that it does not perform"
                .to_owned(),
        );
    }

    let static_daemon_command = static_cli
        .lines()
        .find(|line| line.trim_start().starts_with("daemon "))
        .unwrap_or("");
    if !static_daemon_command.contains("foreground") || static_daemon_command.contains("background")
    {
        failures.push(
            "static README CLI command summary must describe `daemon` as foreground, not background"
                .to_owned(),
        );
    }
    if !(static_cli.contains("hyperdb_daemon_port")
        && static_cli.contains("auto-spawn")
        && static_cli.contains("configured/base")
        && static_cli.contains("exact"))
    {
        failures.push(
            "static README CLI reference must distinguish HYPERDB_DAEMON_PORT auto-spawn discovery from foreground configured/base binding"
                .to_owned(),
        );
    }

    assert!(
        failures.is_empty(),
        "CLI help contract failures:\n- {}",
        failures.join("\n- ")
    );
}
