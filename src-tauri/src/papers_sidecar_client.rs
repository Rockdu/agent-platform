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

/// Locate the papers-plugin binary. Honours `PAPERS_PLUGIN_BIN_OVERRIDE`
/// for tests so the test harness can point at a freshly-built sidecar
/// without depending on the production resolution order.
pub fn resolve_binary_path(workspace_root: &Path) -> Result<PathBuf, SidecarClientError> {
    if let Ok(path) = std::env::var("PAPERS_PLUGIN_BIN_OVERRIDE") {
        let p = PathBuf::from(path);
        if p.exists() {
            return Ok(p);
        }
    }
    let candidates = resolve_expected_paths(workspace_root, SIDECAR_BIN);
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
        let err = resolve_binary_path(tmp.path()).unwrap_err();
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
        let resolved = resolve_binary_path(tmp.path()).unwrap();
        assert_eq!(resolved, fake_bin);
        unsafe {
            std::env::remove_var("PAPERS_PLUGIN_BIN_OVERRIDE");
        }
    }
}
