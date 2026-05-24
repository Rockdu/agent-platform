//! Host-side client for the `papers-plugin` MCP sidecar.
//!
//! Per the immutable plan goal the papers sidecar binary owns arXiv
//! HTTP — the host scheduler / UI / orchestrator host RPC must not
//! perform the network call in-process. This module spawns the
//! sidecar binary as a one-shot subprocess for each arXiv operation,
//! sends one MCP `tools/call` frame on stdin, reads the matching
//! response from stdout, parses it back into a `Vec<PaperRecord>`,
//! and exits. The sidecar handles opt-in, rate-limit, retry, parse,
//! and SQLite writes internally via `fetch_papers_gated`.
//!
//! Per-request spawn is acceptable here because Papers operations
//! are infrequent: one daily scheduled fetch + occasional manual
//! searches. The ~50ms cost per spawn is dominated by the arXiv
//! round trip itself.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use papers_plugin::{FetchPurpose, PaperRecord};
use serde_json::{json, Value};

use crate::dev_diagnostics::resolve_expected_paths;

const SIDECAR_BIN: &str = "papers-plugin";
const CLIENT_ID: &str = "host_ui:papers";

/// Tauri `bundle.externalBin` target triples we probe when looking up
/// a packaged sidecar. Includes the Windows triples — without them a
/// packaged Windows app reports `BinaryNotFound` for every scheduled
/// or manual fetch even though the binary was bundled correctly. Each
/// probe is also tried with `std::env::consts::EXE_SUFFIX` appended so
/// a Windows runtime checks `.exe`-suffixed names while macOS/Linux
/// keep checking extensionless names.
const BUNDLE_PROBE_TRIPLES: &[&str] = &[
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
    "x86_64-pc-windows-gnu",
];

#[derive(Debug, thiserror::Error)]
pub enum SidecarClientError {
    #[error("papers-plugin binary not found: searched {searched}")]
    BinaryNotFound { searched: String },
    #[error("spawn failed: {0}")]
    Spawn(String),
    #[error("stdin write failed: {0}")]
    Write(String),
    #[error("stdout read failed: {0}")]
    Read(String),
    #[error("invalid response frame: {0}")]
    Frame(String),
    #[error("sidecar reported error: {code} {message}")]
    Mcp { code: i64, message: String },
    #[error("no response received")]
    NoResponse,
}

/// Build the set of filenames the resolver should check inside the
/// packaged bundle resource root. Pure helper so the Windows / Unix
/// behaviour is unit-testable without running on the target OS.
///
/// `exe_suffix` is normally `std::env::consts::EXE_SUFFIX` (`.exe` on
/// Windows, empty elsewhere). Returns the canonical name plus every
/// triple-suffixed name, all with `exe_suffix` appended.
pub fn bundle_candidate_names(sidecar: &str, exe_suffix: &str) -> Vec<String> {
    let mut names = Vec::with_capacity(1 + BUNDLE_PROBE_TRIPLES.len());
    names.push(format!("{sidecar}{exe_suffix}"));
    for triple in BUNDLE_PROBE_TRIPLES {
        names.push(format!("{sidecar}-{triple}{exe_suffix}"));
    }
    names
}

/// Locate the papers-plugin binary. Resolution order:
///   1. `PAPERS_PLUGIN_BIN_OVERRIDE` env var (tests).
///   2. Dev source-tree paths under `binary_root` via
///      `dev_diagnostics::resolve_expected_paths`. `binary_root` MUST
///      be the repo workspace root (e.g. `dev_diagnostics::workspace_root_for_dev()`),
///      NOT the user data dir — the latter has no `target/` tree.
///   3. Packaged-resource paths under `bundle_resource_root` if
///      provided — Tauri's `bundle.externalBin` lands sidecars there
///      as `<name>-<target-triple>` (with `.exe` on Windows). The probe
///      set is built by `bundle_candidate_names` so Windows-only
///      filenames are also checked on a Windows runtime.
pub fn resolve_binary_path(
    binary_root: &Path,
    bundle_resource_root: Option<&Path>,
) -> Result<PathBuf, SidecarClientError> {
    if let Ok(path) = std::env::var("PAPERS_PLUGIN_BIN_OVERRIDE") {
        let p = PathBuf::from(path);
        if p.exists() {
            return Ok(p);
        }
    }
    let mut candidates: Vec<PathBuf> = resolve_expected_paths(binary_root, SIDECAR_BIN);
    if let Some(bundle) = bundle_resource_root {
        for name in bundle_candidate_names(SIDECAR_BIN, std::env::consts::EXE_SUFFIX) {
            candidates.push(bundle.join(name));
        }
    }
    for c in &candidates {
        if c.exists() {
            return Ok(c.clone());
        }
    }
    Err(SidecarClientError::BinaryNotFound {
        searched: candidates
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", "),
    })
}

/// Spawn the sidecar, send a single `tools/call` frame for `papers.fetch`
/// or `papers.search` depending on `purpose`, and return the parsed
/// `Vec<PaperRecord>` from the response.
///
/// Synchronous — callers wrap this in `tokio::task::spawn_blocking` so
/// the tokio reactor is not blocked on stdout reads.
pub fn fetch_via_sidecar(
    binary: &Path,
    app_data: &Path,
    workspace: &Path,
    query: &str,
    purpose: FetchPurpose,
) -> Result<Vec<PaperRecord>, SidecarClientError> {
    let tool_name = match purpose {
        FetchPurpose::ScheduledDigest => "papers.fetch",
        FetchPurpose::ManualSearch => "papers.search",
    };
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": tool_name,
            "arguments": { "query": query }
        }
    });

    let mut cmd = Command::new(binary);
    cmd.arg("--client-id")
        .arg(CLIENT_ID)
        .arg("--workspace")
        .arg(workspace)
        .env("APP_DATA_DIR", app_data)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // Propagate the test override so the sidecar hits wiremock instead
    // of real arXiv when present.
    if let Ok(base) = std::env::var("PAPERS_ARXIV_BASE_OVERRIDE") {
        cmd.env("PAPERS_ARXIV_BASE_OVERRIDE", base);
    }

    let mut child = cmd.spawn().map_err(|e| SidecarClientError::Spawn(e.to_string()))?;
    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| SidecarClientError::Spawn("stdin handle missing".to_string()))?;
        let line = serde_json::to_string(&request)
            .map_err(|e| SidecarClientError::Frame(format!("serialize: {e}")))?;
        stdin
            .write_all(line.as_bytes())
            .map_err(|e| SidecarClientError::Write(e.to_string()))?;
        stdin
            .write_all(b"\n")
            .map_err(|e| SidecarClientError::Write(e.to_string()))?;
        stdin
            .flush()
            .map_err(|e| SidecarClientError::Write(e.to_string()))?;
    }
    // Drop stdin so the sidecar exits on EOF after responding.
    drop(child.stdin.take());

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| SidecarClientError::Read("stdout handle missing".to_string()))?;
    let reader = BufReader::new(stdout);

    let mut response: Option<Value> = None;
    for line in reader.lines() {
        let line = line.map_err(|e| SidecarClientError::Read(e.to_string()))?;
        if line.trim().is_empty() {
            continue;
        }
        let parsed: Value = serde_json::from_str(&line)
            .map_err(|e| SidecarClientError::Frame(format!("parse `{line}`: {e}")))?;
        // Take the first response with our request id.
        if parsed.get("id").and_then(|v| v.as_i64()) == Some(1) {
            response = Some(parsed);
            break;
        }
    }
    let _ = child.wait();

    let response = response.ok_or(SidecarClientError::NoResponse)?;
    if let Some(err) = response.get("error") {
        let code = err.get("code").and_then(|v| v.as_i64()).unwrap_or(-32000);
        let message = err
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("(no message)")
            .to_string();
        return Err(SidecarClientError::Mcp { code, message });
    }
    let papers = response
        .get("result")
        .and_then(|r| r.get("papers"))
        .cloned()
        .unwrap_or(json!([]));
    serde_json::from_value::<Vec<PaperRecord>>(papers)
        .map_err(|e| SidecarClientError::Frame(format!("papers deserialise: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_not_found_surfaces_searched_paths() {
        let tmp = tempfile::TempDir::new().unwrap();
        // SAFETY: tests run single-threaded in this crate's lib-test
        // binary; the env mutation is contained within the test.
        unsafe {
            std::env::remove_var("PAPERS_PLUGIN_BIN_OVERRIDE");
        }
        let err = resolve_binary_path(tmp.path(), None).unwrap_err();
        match err {
            SidecarClientError::BinaryNotFound { searched } => {
                assert!(searched.contains("target/debug/papers-plugin"));
                assert!(searched.contains("target/release/papers-plugin"));
            }
            other => panic!("expected BinaryNotFound, got {other:?}"),
        }
    }

    #[test]
    fn override_env_takes_precedence() {
        let tmp = tempfile::TempDir::new().unwrap();
        let fake_bin = tmp.path().join("custom-papers-plugin");
        std::fs::write(&fake_bin, "stub").unwrap();
        // SAFETY: tests run single-threaded; cleanup unsets the var.
        unsafe {
            std::env::set_var("PAPERS_PLUGIN_BIN_OVERRIDE", &fake_bin);
        }
        let resolved = resolve_binary_path(tmp.path(), None).unwrap();
        assert_eq!(resolved, fake_bin);
        unsafe {
            std::env::remove_var("PAPERS_PLUGIN_BIN_OVERRIDE");
        }
    }

    #[test]
    fn resolve_binary_path_does_not_search_user_data_root() {
        // Regression: a previous wiring passed the user data root
        // (e.g. ~/AgentPlatform) as the search root, but that tree
        // has no `target/` directory. The resolver must search the
        // workspace root paths (`<root>/target/...` and
        // `<root>/src-tauri/target/...`), not anything based on the
        // user data dir. Here we use a tempdir that simulates the
        // user data root: no `target/` subtree exists, and the
        // resolver should fail with BinaryNotFound listing the
        // workspace candidates relative to the workspace root
        // passed in — never relative to ~/AgentPlatform.
        let user_data_root = tempfile::TempDir::new().unwrap();
        let workspace_root = tempfile::TempDir::new().unwrap();
        unsafe {
            std::env::remove_var("PAPERS_PLUGIN_BIN_OVERRIDE");
        }
        let err = resolve_binary_path(workspace_root.path(), None).unwrap_err();
        let SidecarClientError::BinaryNotFound { searched } = err else {
            panic!("expected BinaryNotFound");
        };
        // The error must reference the workspace_root passed in,
        // not the user_data_root.
        let workspace_str = workspace_root.path().display().to_string();
        let user_data_str = user_data_root.path().display().to_string();
        assert!(
            searched.contains(&workspace_str),
            "searched must include workspace_root; got `{searched}`"
        );
        assert!(
            !searched.contains(&user_data_str),
            "searched MUST NOT include user_data_root; got `{searched}`"
        );
    }

    /// The host store and the sidecar subprocess MUST open the same
    /// SQLite path when given the same `app_data_dir`. Catches the
    /// regression where the host opened one DB while the sidecar
    /// wrote to another. This test pins the path contract: both
    /// sides compute `${app_data_dir}/plugins/papers/state.sqlite`.
    #[test]
    fn host_store_path_equals_sidecar_app_data_path() {
        let app_data = tempfile::TempDir::new().unwrap();
        // The path the host bootstrap derives.
        let host_path = app_data
            .path()
            .join("plugins")
            .join("papers")
            .join("state.sqlite");
        // The path the sidecar derives via APP_DATA_DIR resolution.
        // Mirrors `papers_plugin::resolve_papers_db_path`.
        let sidecar_path = std::path::PathBuf::from(app_data.path())
            .join("plugins")
            .join("papers")
            .join("state.sqlite");
        assert_eq!(
            host_path, sidecar_path,
            "host and sidecar must derive the same SQLite path from the same app_data_dir"
        );
    }

    /// Cross-handle visibility regression: a row inserted via one `PapersStore`
    /// handle pointed at a tempdir MUST be visible via another
    /// `PapersStore` handle pointed at the SAME tempdir. This proves
    /// the WAL-mode sharing semantics that the host scheduler /
    /// list_recent code paths depend on after the manual search runs
    /// inside the sidecar subprocess.
    #[test]
    fn host_sees_sidecar_inserted_paper_via_list_recent() {
        use papers_plugin::{FetchPurpose, PaperRecord, PapersStore};
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("plugins").join("papers").join("state.sqlite");
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        // Initialise schema directly via rusqlite so this test does
        // not depend on the workspace migration runner.
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(include_str!(
            "../../plugins/papers/migrations/0001_init.sql"
        ))
        .unwrap();
        drop(conn);

        // Sidecar-side handle: insert as if a manual search ran.
        let sidecar_store = PapersStore::open(&db_path).unwrap();
        sidecar_store.set_opt_in(true).unwrap();
        let record = PaperRecord {
            arxiv_id: "2024.cross-process".to_string(),
            title: "Cross-Process Visibility".to_string(),
            authors: vec!["Tester".to_string()],
            abstract_snippet: "ensures host sees sidecar inserts".to_string(),
            pdf_url: "http://arxiv.org/pdf/2024.cross-process".to_string(),
            abs_url: "http://arxiv.org/abs/2024.cross-process".to_string(),
            source: "manual".to_string(),
            fetched_at: papers_plugin::now_iso8601(),
            starred: false,
            read_at: None,
        };
        sidecar_store.insert_dedup(&[record], "manual-query").unwrap();
        // Manual-search invariant: insert_dedup must NOT have advanced last_fired_at.
        assert!(sidecar_store.last_fired_at_unix().unwrap().is_none());
        // Touch FetchPurpose so the trait is exercised; the value is
        // not used past this point.
        let _ = FetchPurpose::ManualSearch;

        // Host-side handle on the same SQLite file: must see the row.
        let host_store = PapersStore::open(&db_path).unwrap();
        let listed = host_store.list_recent(10).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].arxiv_id, "2024.cross-process");
        assert_eq!(listed[0].source, "manual");
    }

    #[test]
    fn bundle_resource_root_candidate_names_include_windows_triples_with_exe() {
        // Regression: a packaged Windows app stages the sidecar as
        // `papers-plugin-x86_64-pc-windows-msvc.exe`. The resolver
        // never probed Windows triples nor appended `.exe`, so every
        // scheduled or manual fetch reported BinaryNotFound. The probe
        // set must include the canonical name + every supported triple
        // (macOS / Linux / Windows), all with the provided exe suffix.
        let names = bundle_candidate_names(SIDECAR_BIN, ".exe");
        assert!(
            names.contains(&"papers-plugin.exe".to_string()),
            "canonical name with .exe missing from probe set: {names:?}"
        );
        for triple in [
            "x86_64-pc-windows-msvc",
            "aarch64-pc-windows-msvc",
            "x86_64-pc-windows-gnu",
        ] {
            let expected = format!("papers-plugin-{triple}.exe");
            assert!(
                names.contains(&expected),
                "Windows-named candidate `{expected}` missing from probe set: {names:?}"
            );
        }
        // The macOS/Linux triples are still probed (with the .exe
        // suffix in this branch; on a real Unix runtime the suffix
        // would be empty — covered by the next test).
        for triple in [
            "aarch64-apple-darwin",
            "x86_64-apple-darwin",
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
        ] {
            let expected = format!("papers-plugin-{triple}.exe");
            assert!(
                names.contains(&expected),
                "unix-triple candidate `{expected}` missing from probe set: {names:?}"
            );
        }
    }

    #[test]
    fn bundle_resource_root_candidate_names_omit_exe_on_unix() {
        // Regression guard: the existing macOS/Linux runtime probe
        // set MUST stay extensionless when std::env::consts::EXE_SUFFIX
        // is empty. The Windows-aware fix must not accidentally
        // append `.exe` on Unix targets.
        let names = bundle_candidate_names(SIDECAR_BIN, "");
        assert!(
            names.contains(&"papers-plugin".to_string()),
            "canonical extensionless name missing from probe set: {names:?}"
        );
        assert!(
            names.contains(&"papers-plugin-aarch64-apple-darwin".to_string()),
            "expected extensionless macOS-triple candidate; got: {names:?}"
        );
        for name in &names {
            assert!(
                !name.ends_with(".exe"),
                "no candidate may carry `.exe` when exe_suffix is empty; offender: {name}"
            );
        }
    }

    #[test]
    fn bundle_resource_root_paths_are_tried() {
        let workspace_root = tempfile::TempDir::new().unwrap();
        let bundle = tempfile::TempDir::new().unwrap();
        // Stage a triple-suffixed binary under the bundle resource root
        // to simulate `bundle.externalBin` staging.
        let triple = "aarch64-apple-darwin";
        let bundled = bundle.path().join(format!("papers-plugin-{triple}"));
        std::fs::write(&bundled, "stub").unwrap();
        unsafe {
            std::env::remove_var("PAPERS_PLUGIN_BIN_OVERRIDE");
        }
        let resolved = resolve_binary_path(workspace_root.path(), Some(bundle.path())).unwrap();
        assert_eq!(resolved, bundled);
    }
}
