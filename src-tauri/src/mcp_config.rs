//! Per-claude-tab MCP config generator (AC-2.2).
//!
//! Each Terminal Mesh tab that launches `claude` gets a dedicated config
//! file at `${APP_DATA}/claude-mcp-configs/<tab_id>.json`. Atomic
//! temp+rename writes; owner-only permissions (0600); deterministic
//! key ordering via BTreeMap; never contains secrets. See
//! `docs/specs/claude-launch.md` §"MCP Config Generation".

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;

use crate::dev_diagnostics::resolve_expected_paths;
use crate::generated::plugin_registry::PLUGINS;

const CONFIGS_DIRNAME: &str = "claude-mcp-configs";
const ORCHESTRATOR_CROSS_TAB_PLUGIN: &str = "terminal-mesh";
const TAB_ID_PATTERN: &str = r"^[A-Za-z0-9._-]{1,128}$";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpConfigKind {
    Standard,
    Orchestrator,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct McpServerEntry {
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpConfigDocument {
    pub mcp_servers: BTreeMap<String, McpServerEntry>,
}

#[derive(Debug, thiserror::Error)]
pub enum McpConfigError {
    #[error("workspace path `{path}` is not a usable directory: {message}")]
    WorkspaceUnavailable { path: PathBuf, message: String },

    #[error("invalid tab_id `{raw}`; must match {pattern}")]
    InvalidTabId { raw: String, pattern: &'static str },

    #[error("no plugin produced a usable MCP server entry for tab `{tab_id}`")]
    NoUsablePlugins { tab_id: String },

    #[error("io error in `{context}`: {message}")]
    Io { context: String, message: String },

    #[error("serialization failure: {message}")]
    Serialization { message: String },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum McpConfigErrorDto {
    WorkspaceUnavailable { path: String, message: String },
    InvalidTabId { raw: String, pattern: String },
    NoUsablePlugins { tab_id: String },
    Io { context: String, message: String },
    Serialization { message: String },
}

impl From<&McpConfigError> for McpConfigErrorDto {
    fn from(err: &McpConfigError) -> Self {
        match err {
            McpConfigError::WorkspaceUnavailable { path, message } => Self::WorkspaceUnavailable {
                path: path.display().to_string(),
                message: message.clone(),
            },
            McpConfigError::InvalidTabId { raw, pattern } => Self::InvalidTabId {
                raw: raw.clone(),
                pattern: (*pattern).to_string(),
            },
            McpConfigError::NoUsablePlugins { tab_id } => Self::NoUsablePlugins {
                tab_id: tab_id.clone(),
            },
            McpConfigError::Io { context, message } => Self::Io {
                context: context.clone(),
                message: message.clone(),
            },
            McpConfigError::Serialization { message } => Self::Serialization {
                message: message.clone(),
            },
        }
    }
}

/// In-memory tab_id → config_path map. Tauri-managed; cleared on tab
/// close (`delete_config`) or startup GC.
pub struct McpConfigRegistry {
    inner: Mutex<HashMap<String, PathBuf>>,
}

impl Default for McpConfigRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl McpConfigRegistry {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    pub fn record(&self, tab_id: &str, path: PathBuf) {
        let mut guard = self.inner.lock().expect("McpConfigRegistry poisoned");
        guard.insert(tab_id.to_string(), path);
    }

    pub fn forget(&self, tab_id: &str) -> Option<PathBuf> {
        let mut guard = self.inner.lock().expect("McpConfigRegistry poisoned");
        guard.remove(tab_id)
    }

    #[allow(dead_code)]
    pub fn lookup(&self, tab_id: &str) -> Option<PathBuf> {
        let guard = self.inner.lock().expect("McpConfigRegistry poisoned");
        guard.get(tab_id).cloned()
    }

    #[allow(dead_code)]
    pub fn snapshot(&self) -> HashMap<String, PathBuf> {
        let guard = self.inner.lock().expect("McpConfigRegistry poisoned");
        guard.clone()
    }
}

fn validate_tab_id(tab_id: &str) -> Result<(), McpConfigError> {
    static TAB_ID_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = TAB_ID_RE.get_or_init(|| regex::Regex::new(TAB_ID_PATTERN).unwrap());
    if re.is_match(tab_id) {
        Ok(())
    } else {
        Err(McpConfigError::InvalidTabId {
            raw: tab_id.to_string(),
            pattern: TAB_ID_PATTERN,
        })
    }
}

fn configs_dir(app_data: &Path) -> PathBuf {
    app_data.join(CONFIGS_DIRNAME)
}

/// Build the in-memory `McpConfigDocument`. Pure function: only touches
/// the filesystem to resolve plugin binary paths. The atomic disk write
/// happens in [`write_atomic`].
///
/// Round 39 (task21 remediation per Codex round-38 review): retained
/// as a back-compat shim only — every production caller now goes
/// through [`generate_config_with_host_rpc_sock`] so the terminal-mesh
/// sidecar receives `--host-rpc-sock`. Tests still drive the no-sock
/// path through this entry point. `#[allow(dead_code)]` because
/// production-time dead-code detection sees only the tests' usage.
#[allow(dead_code)]
pub fn generate_config(
    tab_id: &str,
    workspace: &Path,
    kind: McpConfigKind,
    app_data: &Path,
    workspace_root_for_dev: &Path,
) -> Result<McpConfigDocument, McpConfigError> {
    generate_config_with_host_rpc_sock(
        tab_id,
        workspace,
        kind,
        app_data,
        workspace_root_for_dev,
        None,
    )
}

/// task21 / AC-3.3 — Round 38 variant that also threads
/// `--host-rpc-sock <path>` into every sidecar's argv. The built-in
/// `terminal-mesh` MCP sidecar entry is included whenever its binary
/// can be resolved on the dev paths (production resource bundling
/// will land with task22+).
pub fn generate_config_with_host_rpc_sock(
    tab_id: &str,
    workspace: &Path,
    kind: McpConfigKind,
    app_data: &Path,
    workspace_root_for_dev: &Path,
    host_rpc_sock: Option<&Path>,
) -> Result<McpConfigDocument, McpConfigError> {
    validate_tab_id(tab_id)?;

    let workspace_meta = std::fs::metadata(workspace).map_err(|e| McpConfigError::WorkspaceUnavailable {
        path: workspace.to_path_buf(),
        message: e.to_string(),
    })?;
    if !workspace_meta.is_dir() {
        return Err(McpConfigError::WorkspaceUnavailable {
            path: workspace.to_path_buf(),
            message: "not a directory".to_string(),
        });
    }

    let mut mcp_servers: BTreeMap<String, McpServerEntry> = BTreeMap::new();
    // Iterate the codegen `PLUGINS` AND the built-in `BUILTIN_PLUGINS`
    // (task21 / AC-3.3). The built-in path covers host-internal MCP
    // sidecars that don't have a frontend plugin manifest (today:
    // `terminal-mesh`).
    let plugin_iter = PLUGINS
        .iter()
        .map(|p| (p.plugin_id, p.command_bin))
        .chain(
            crate::builtin_plugins::BUILTIN_PLUGINS
                .iter()
                .map(|p| (p.plugin_id, p.command_bin)),
        );
    for (plugin_id, command_bin) in plugin_iter {
        let candidates = resolve_expected_paths(workspace_root_for_dev, command_bin);
        let Some(command_path) = candidates.iter().find(|p| p.exists()) else {
            tracing::warn!(
                plugin_id = plugin_id,
                command_bin = command_bin,
                "mcp_config: skipping plugin without a resolvable sidecar binary"
            );
            continue;
        };

        let mut args = vec![
            "--client-id".to_string(),
            format!("claude:{tab_id}:{plugin_id}"),
            "--workspace".to_string(),
            workspace.display().to_string(),
        ];
        if let Some(sock) = host_rpc_sock {
            args.push("--host-rpc-sock".to_string());
            args.push(sock.display().to_string());
        }
        if kind == McpConfigKind::Orchestrator && plugin_id == ORCHESTRATOR_CROSS_TAB_PLUGIN {
            args.push("--cross-tab-read".to_string());
        }

        let mut env: BTreeMap<String, String> = BTreeMap::new();
        env.insert("APP_DATA_DIR".to_string(), app_data.display().to_string());

        // Defensive: refuse to ship any env key that looks like a secret.
        for key in env.keys() {
            let lower = key.to_ascii_lowercase();
            if lower.contains("token")
                || lower.contains("password")
                || lower.contains("secret")
                || lower.contains("bearer")
            {
                return Err(McpConfigError::Serialization {
                    message: format!("env key `{key}` looks secret-bearing; refusing to embed"),
                });
            }
        }

        mcp_servers.insert(
            plugin_id.to_string(),
            McpServerEntry {
                command: command_path.display().to_string(),
                args,
                env,
            },
        );
    }

    if mcp_servers.is_empty() {
        return Err(McpConfigError::NoUsablePlugins {
            tab_id: tab_id.to_string(),
        });
    }

    Ok(McpConfigDocument { mcp_servers })
}

/// Write `<tab_id>.json` atomically under `${app_data}/claude-mcp-configs/`.
/// Returns the resolved final path on success. Creates the parent dir
/// if missing. Sets mode 0600 on the destination file.
pub fn write_atomic(
    app_data: &Path,
    tab_id: &str,
    doc: &McpConfigDocument,
) -> Result<PathBuf, McpConfigError> {
    validate_tab_id(tab_id)?;
    let dir = configs_dir(app_data);
    std::fs::create_dir_all(&dir).map_err(|e| McpConfigError::Io {
        context: format!("create_dir_all {}", dir.display()),
        message: e.to_string(),
    })?;
    let final_path = dir.join(format!("{tab_id}.json"));
    let tmp_path = dir.join(format!("{tab_id}.json.tmp"));

    let body = serde_json::to_vec_pretty(doc).map_err(|e| McpConfigError::Serialization {
        message: e.to_string(),
    })?;

    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp_path)
            .map_err(|e| McpConfigError::Io {
                context: format!("open {}", tmp_path.display()),
                message: e.to_string(),
            })?;
        f.write_all(&body).map_err(|e| McpConfigError::Io {
            context: format!("write {}", tmp_path.display()),
            message: e.to_string(),
        })?;
        // fsync best-effort: ignore EINVAL on filesystems that don't
        // support it (e.g. some sandboxed test FS).
        let _ = f.sync_all();
    }

    std::fs::rename(&tmp_path, &final_path).map_err(|e| McpConfigError::Io {
        context: format!("rename {} -> {}", tmp_path.display(), final_path.display()),
        message: e.to_string(),
    })?;

    // Re-assert mode 0600 on the final path because some filesystems
    // reset perms across rename.
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&final_path, std::fs::Permissions::from_mode(0o600));

    Ok(final_path)
}

/// Delete `<tab_id>.json` if it exists. Missing file is not an error.
pub fn delete_config(app_data: &Path, tab_id: &str) {
    // Validate even on delete — refuse path traversal regardless of
    // call path.
    if validate_tab_id(tab_id).is_err() {
        tracing::warn!(tab_id, "mcp_config: refusing to delete invalid tab_id");
        return;
    }
    let path = configs_dir(app_data).join(format!("{tab_id}.json"));
    match std::fs::remove_file(&path) {
        Ok(()) => tracing::info!(path = %path.display(), "mcp_config: deleted"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(path = %path.display(), %e, "mcp_config: delete failed"),
    }
}

/// Startup GC: conservatively unlink every `*.json` inside
/// `${app_data}/claude-mcp-configs/`. Refuses to follow symlinks that
/// escape the directory.
pub fn startup_gc(app_data: &Path) {
    let dir = configs_dir(app_data);
    let Ok(canonical_dir) = std::fs::canonicalize(&dir) else {
        // Directory doesn't exist yet (first launch) — nothing to GC.
        return;
    };
    let read = match std::fs::read_dir(&canonical_dir) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(dir = %canonical_dir.display(), %e, "mcp_config: GC read_dir failed");
            return;
        }
    };
    let mut removed: usize = 0;
    for entry in read.flatten() {
        let raw_path = entry.path();
        if raw_path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        // symlink_metadata avoids following the link; canonicalize the
        // resolved target before deciding whether to unlink.
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let Ok(canonical_target) = std::fs::canonicalize(&raw_path) else {
            tracing::warn!(path = %raw_path.display(), "mcp_config: GC skipped (canonicalize failed)");
            continue;
        };
        let Some(parent) = canonical_target.parent() else {
            continue;
        };
        if parent != canonical_dir {
            tracing::warn!(
                path = %raw_path.display(),
                resolved = %canonical_target.display(),
                "mcp_config: GC refusing to delete escape symlink target"
            );
            continue;
        }
        match std::fs::remove_file(&raw_path) {
            Ok(()) => {
                removed += 1;
                tracing::info!(path = %raw_path.display(), "mcp_config: GC removed");
            }
            Err(e) => {
                tracing::warn!(path = %raw_path.display(), %e, "mcp_config: GC remove failed");
            }
        }
    }
    if removed > 0 {
        tracing::info!(removed, dir = %canonical_dir.display(), "mcp_config: GC complete");
    }
}

// ---------------------------------------------------------------------------
// Tauri command surface
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum McpConfigKindDto {
    Standard,
    Orchestrator,
}

impl From<McpConfigKindDto> for McpConfigKind {
    fn from(d: McpConfigKindDto) -> Self {
        match d {
            McpConfigKindDto::Standard => McpConfigKind::Standard,
            McpConfigKindDto::Orchestrator => McpConfigKind::Orchestrator,
        }
    }
}

#[tauri::command]
pub async fn generate_mcp_config(
    tab_id: String,
    workspace: String,
    kind: McpConfigKindDto,
    registry: tauri::State<'_, McpConfigRegistry>,
    bootstrap: tauri::State<'_, crate::orchestrator::OrchestratorBootstrap>,
) -> Result<String, McpConfigErrorDto> {
    let Some((_, app_data)) = resolve_dirs() else {
        return Err(McpConfigErrorDto::Io {
            context: "resolve app_data".into(),
            message: "bootstrap dirs unavailable".into(),
        });
    };
    let workspace_path = PathBuf::from(workspace);
    let workspace_root = crate::dev_diagnostics::workspace_root_for_dev();
    // task21 / AC-3.3 Round 39: every per-tab config (orchestrator AND
    // standard) must carry `--host-rpc-sock` because the terminal-mesh
    // sidecar binary requires it at startup. Without it, a regular
    // tab's terminal-mesh sidecar exits before it can route its
    // PermissionDenied for cross-tab reads through the bridge.
    let doc = generate_config_with_host_rpc_sock(
        &tab_id,
        &workspace_path,
        kind.into(),
        &app_data,
        &workspace_root,
        bootstrap.host_rpc_sock.as_deref(),
    )
    .map_err(|e| McpConfigErrorDto::from(&e))?;
    let final_path = write_atomic(&app_data, &tab_id, &doc).map_err(|e| McpConfigErrorDto::from(&e))?;
    registry.record(&tab_id, final_path.clone());
    Ok(final_path.display().to_string())
}

#[tauri::command]
pub async fn delete_mcp_config(tab_id: String, registry: tauri::State<'_, McpConfigRegistry>) -> Result<(), ()> {
    if let Some((_, app_data)) = resolve_dirs() {
        delete_config(&app_data, &tab_id);
    }
    let _ = registry.forget(&tab_id);
    Ok(())
}

fn resolve_dirs() -> Option<(PathBuf, PathBuf)> {
    let base = directories::BaseDirs::new()?;
    let project = directories::ProjectDirs::from("com", "agentplatform", "app")?;
    Some((
        base.home_dir().to_path_buf(),
        project.data_dir().to_path_buf(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn make_workspace_dir() -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();
        dir
    }

    /// Materialize a fake plugin sidecar binary at the standard dev
    /// candidate path so `resolve_expected_paths` picks it up.
    fn stub_sidecar_for(workspace_root: &Path, command_bin: &str) -> PathBuf {
        let dir = workspace_root.join("src-tauri").join("target").join("debug");
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join(command_bin);
        std::fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    #[test]
    fn write_atomic_emits_specced_path_and_perms() {
        let app_data = tempfile::TempDir::new().unwrap();
        let workspace_root = tempfile::TempDir::new().unwrap();
        let workspace = make_workspace_dir();
        stub_sidecar_for(workspace_root.path(), "notes-plugin");
        let doc = generate_config(
            "tab-abc",
            workspace.path(),
            McpConfigKind::Standard,
            app_data.path(),
            workspace_root.path(),
        )
        .expect("generate");
        let out = write_atomic(app_data.path(), "tab-abc", &doc).expect("write");
        assert!(out.is_file());
        assert_eq!(out.parent().unwrap().file_name().unwrap(), CONFIGS_DIRNAME);
        let mode = std::fs::metadata(&out).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn write_atomic_leaves_no_tmp_file_after_success() {
        let app_data = tempfile::TempDir::new().unwrap();
        let workspace_root = tempfile::TempDir::new().unwrap();
        let workspace = make_workspace_dir();
        stub_sidecar_for(workspace_root.path(), "notes-plugin");
        let doc = generate_config(
            "tab-1",
            workspace.path(),
            McpConfigKind::Standard,
            app_data.path(),
            workspace_root.path(),
        )
        .expect("generate");
        write_atomic(app_data.path(), "tab-1", &doc).unwrap();
        write_atomic(app_data.path(), "tab-1", &doc).unwrap();
        let dir = configs_dir(app_data.path());
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["tab-1.json".to_string()]);
    }

    #[test]
    fn write_atomic_rejects_path_traversal_tab_id() {
        let app_data = tempfile::TempDir::new().unwrap();
        let workspace_root = tempfile::TempDir::new().unwrap();
        let workspace = make_workspace_dir();
        stub_sidecar_for(workspace_root.path(), "notes-plugin");
        let doc = generate_config(
            "tab-x",
            workspace.path(),
            McpConfigKind::Standard,
            app_data.path(),
            workspace_root.path(),
        )
        .expect("generate doc for fixture");
        let err = write_atomic(app_data.path(), "../etc/passwd", &doc).unwrap_err();
        match err {
            McpConfigError::InvalidTabId { raw, .. } => assert_eq!(raw, "../etc/passwd"),
            other => panic!("expected InvalidTabId; got {other:?}"),
        }
    }

    #[test]
    fn generate_config_includes_workspace_arg_and_app_data_env() {
        let app_data = tempfile::TempDir::new().unwrap();
        let workspace_root = tempfile::TempDir::new().unwrap();
        let workspace = make_workspace_dir();
        stub_sidecar_for(workspace_root.path(), "notes-plugin");
        let doc = generate_config(
            "tab-z",
            workspace.path(),
            McpConfigKind::Standard,
            app_data.path(),
            workspace_root.path(),
        )
        .expect("generate");
        let entry = doc.mcp_servers.get("example-notes").expect("notes entry");
        assert!(entry.args.iter().any(|a| a == "--workspace"));
        assert!(entry.args.iter().any(|a| a == &workspace.path().display().to_string()));
        assert_eq!(
            entry.env.get("APP_DATA_DIR").map(String::as_str),
            Some(app_data.path().display().to_string()).as_deref()
        );
        // Standard config: no cross-tab-read.
        assert!(!entry.args.iter().any(|a| a == "--cross-tab-read"));
    }

    #[test]
    fn generate_orchestrator_config_adds_cross_tab_read_only_for_terminal_mesh() {
        // MVP plugin set: only example-notes is registered. The
        // contract is "only `terminal-mesh` gains --cross-tab-read";
        // since terminal-mesh isn't a plugin yet, we assert the inverse
        // — example-notes does NOT get the flag even in orchestrator
        // mode. When terminal-mesh lands this assertion still passes
        // for example-notes; a future test will pin the positive case.
        let app_data = tempfile::TempDir::new().unwrap();
        let workspace_root = tempfile::TempDir::new().unwrap();
        let workspace = make_workspace_dir();
        stub_sidecar_for(workspace_root.path(), "notes-plugin");
        let doc = generate_config(
            "tab-orch",
            workspace.path(),
            McpConfigKind::Orchestrator,
            app_data.path(),
            workspace_root.path(),
        )
        .expect("generate");
        let entry = doc.mcp_servers.get("example-notes").expect("notes entry");
        assert!(
            !entry.args.iter().any(|a| a == "--cross-tab-read"),
            "non-terminal-mesh plugins must NOT receive --cross-tab-read; args={:?}",
            entry.args
        );
    }

    // ----- task21 Round 38: terminal-mesh-sidecar entry + --host-rpc-sock -----

    #[test]
    fn generate_orchestrator_config_includes_terminal_mesh_sidecar_with_cross_tab_read() {
        let app_data = tempfile::TempDir::new().unwrap();
        let workspace_root = tempfile::TempDir::new().unwrap();
        let workspace = make_workspace_dir();
        // Stub BOTH binaries so the iteration finds them.
        stub_sidecar_for(workspace_root.path(), "notes-plugin");
        stub_sidecar_for(workspace_root.path(), "terminal-mesh-sidecar");
        let doc = generate_config_with_host_rpc_sock(
            "tab-orch",
            workspace.path(),
            McpConfigKind::Orchestrator,
            app_data.path(),
            workspace_root.path(),
            Some(std::path::Path::new("/tmp/host.sock")),
        )
        .expect("generate");
        let entry = doc
            .mcp_servers
            .get("terminal-mesh")
            .expect("terminal-mesh entry in orchestrator config");
        assert!(
            entry.args.iter().any(|a| a == "--cross-tab-read"),
            "orchestrator terminal-mesh entry MUST carry --cross-tab-read; args={:?}",
            entry.args
        );
        // --host-rpc-sock argv threaded through with the right path.
        let mut iter = entry.args.iter();
        let has_sock = loop {
            match iter.next() {
                Some(a) if a == "--host-rpc-sock" => {
                    break iter.next().map(|s| s.as_str()) == Some("/tmp/host.sock");
                }
                Some(_) => continue,
                None => break false,
            }
        };
        assert!(has_sock, "--host-rpc-sock /tmp/host.sock missing; args={:?}", entry.args);
    }

    #[test]
    fn generate_standard_config_includes_terminal_mesh_sidecar_with_host_rpc_sock_without_cross_tab_read() {
        let app_data = tempfile::TempDir::new().unwrap();
        let workspace_root = tempfile::TempDir::new().unwrap();
        let workspace = make_workspace_dir();
        stub_sidecar_for(workspace_root.path(), "notes-plugin");
        stub_sidecar_for(workspace_root.path(), "terminal-mesh-sidecar");
        // Round 39: standard tabs ALSO need --host-rpc-sock so the
        // terminal-mesh sidecar can boot and route denials through
        // the bridge (it requires the arg at startup). Generating
        // standard config WITHOUT the sock is a misconfig.
        let doc = generate_config_with_host_rpc_sock(
            "tab-std",
            workspace.path(),
            McpConfigKind::Standard,
            app_data.path(),
            workspace_root.path(),
            Some(std::path::Path::new("/tmp/host-std.sock")),
        )
        .expect("generate");
        let entry = doc
            .mcp_servers
            .get("terminal-mesh")
            .expect("terminal-mesh entry in standard config too");
        assert!(
            !entry.args.iter().any(|a| a == "--cross-tab-read"),
            "non-orchestrator terminal-mesh entry MUST NOT carry --cross-tab-read; args={:?}",
            entry.args
        );
        // --host-rpc-sock /tmp/host-std.sock present.
        let mut iter = entry.args.iter();
        let has_sock = loop {
            match iter.next() {
                Some(a) if a == "--host-rpc-sock" => {
                    break iter.next().map(|s| s.as_str()) == Some("/tmp/host-std.sock");
                }
                Some(_) => continue,
                None => break false,
            }
        };
        assert!(
            has_sock,
            "--host-rpc-sock /tmp/host-std.sock MUST be present even in standard config; args={:?}",
            entry.args
        );
    }

    #[test]
    fn generate_config_rejects_missing_workspace_dir() {
        let app_data = tempfile::TempDir::new().unwrap();
        let workspace_root = tempfile::TempDir::new().unwrap();
        stub_sidecar_for(workspace_root.path(), "notes-plugin");
        let err = generate_config(
            "tab-w",
            Path::new("/nonexistent-workspace-for-test-22ef"),
            McpConfigKind::Standard,
            app_data.path(),
            workspace_root.path(),
        )
        .unwrap_err();
        match err {
            McpConfigError::WorkspaceUnavailable { .. } => {}
            other => panic!("expected WorkspaceUnavailable; got {other:?}"),
        }
    }

    #[test]
    fn generate_config_rejects_when_no_plugin_binary_resolves() {
        let app_data = tempfile::TempDir::new().unwrap();
        let workspace_root = tempfile::TempDir::new().unwrap();
        let workspace = make_workspace_dir();
        // Note: no sidecar stub created. The lone registered plugin
        // (example-notes) has no resolvable binary, so the generator
        // must surface NoUsablePlugins instead of writing an empty doc.
        let err = generate_config(
            "tab-empty",
            workspace.path(),
            McpConfigKind::Standard,
            app_data.path(),
            workspace_root.path(),
        )
        .unwrap_err();
        match err {
            McpConfigError::NoUsablePlugins { tab_id } => assert_eq!(tab_id, "tab-empty"),
            other => panic!("expected NoUsablePlugins; got {other:?}"),
        }
    }

    #[test]
    fn delete_config_is_idempotent_for_missing_tab_id() {
        let app_data = tempfile::TempDir::new().unwrap();
        delete_config(app_data.path(), "tab-never-existed");
        // No panic, no error surfaced — success.
    }

    #[test]
    fn delete_config_refuses_invalid_tab_id() {
        let app_data = tempfile::TempDir::new().unwrap();
        // create a config that COULD be deleted if validation were
        // bypassed; we want to assert the malicious tab_id doesn't
        // touch it.
        let dir = configs_dir(app_data.path());
        std::fs::create_dir_all(&dir).unwrap();
        let canary = dir.join("legitimate.json");
        std::fs::write(&canary, b"{}\n").unwrap();
        delete_config(app_data.path(), "../legitimate");
        assert!(canary.is_file(), "canary must survive bogus delete attempt");
    }

    #[test]
    fn startup_gc_removes_only_inside_configs_dir() {
        let app_data = tempfile::TempDir::new().unwrap();
        let dir = configs_dir(app_data.path());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.json"), b"{}\n").unwrap();
        std::fs::write(dir.join("b.json"), b"{}\n").unwrap();
        let outside = app_data.path().join("other");
        std::fs::create_dir_all(&outside).unwrap();
        let canary = outside.join("c.json");
        std::fs::write(&canary, b"{}\n").unwrap();
        startup_gc(app_data.path());
        assert!(!dir.join("a.json").exists());
        assert!(!dir.join("b.json").exists());
        assert!(canary.is_file(), "files outside the configs dir must survive GC");
    }

    #[test]
    fn startup_gc_ignores_non_json_files() {
        let app_data = tempfile::TempDir::new().unwrap();
        let dir = configs_dir(app_data.path());
        std::fs::create_dir_all(&dir).unwrap();
        let keep_tmp = dir.join("a.json.tmp");
        let keep_bak = dir.join("b.bak");
        std::fs::write(&keep_tmp, b"{}\n").unwrap();
        std::fs::write(&keep_bak, b"{}\n").unwrap();
        startup_gc(app_data.path());
        assert!(keep_tmp.is_file());
        assert!(keep_bak.is_file());
    }

    #[test]
    fn startup_gc_refuses_to_follow_symlink_out_of_dir() {
        use std::os::unix::fs::symlink;
        let app_data = tempfile::TempDir::new().unwrap();
        let dir = configs_dir(app_data.path());
        std::fs::create_dir_all(&dir).unwrap();
        // Decoy outside-the-dir target.
        let outside = tempfile::TempDir::new().unwrap();
        let target = outside.path().join("victim.json");
        std::fs::write(&target, b"victim\n").unwrap();
        let link = dir.join("escape.json");
        symlink(&target, &link).unwrap();
        // Also seed an honest file alongside so we can confirm the GC
        // ran at all and didn't bail out at the first weirdness.
        std::fs::write(dir.join("honest.json"), b"{}\n").unwrap();

        startup_gc(app_data.path());
        assert!(target.is_file(), "outside-dir file must NOT be deleted");
        assert!(!dir.join("honest.json").exists(), "honest file should be GC'd");
        // The escape symlink itself may or may not be removed depending
        // on canonicalize semantics — that's irrelevant; what matters
        // is that the target survives.
    }

    #[test]
    fn registry_round_trip() {
        let r = McpConfigRegistry::new();
        r.record("tab-1", PathBuf::from("/tmp/a.json"));
        r.record("tab-2", PathBuf::from("/tmp/b.json"));
        assert_eq!(r.lookup("tab-1"), Some(PathBuf::from("/tmp/a.json")));
        assert_eq!(r.snapshot().len(), 2);
        assert_eq!(r.forget("tab-1"), Some(PathBuf::from("/tmp/a.json")));
        assert_eq!(r.lookup("tab-1"), None);
    }

    #[test]
    fn document_serializes_to_spec_shape() {
        let app_data = tempfile::TempDir::new().unwrap();
        let workspace_root = tempfile::TempDir::new().unwrap();
        let workspace = make_workspace_dir();
        stub_sidecar_for(workspace_root.path(), "notes-plugin");
        let doc = generate_config(
            "tab-pin",
            workspace.path(),
            McpConfigKind::Standard,
            app_data.path(),
            workspace_root.path(),
        )
        .unwrap();
        let json = serde_json::to_value(&doc).unwrap();
        assert!(json.get("mcpServers").is_some(), "field name must be camelCase mcpServers");
        let server = json
            .get("mcpServers")
            .and_then(|v| v.get("example-notes"))
            .expect("plugin entry present");
        assert!(server.get("command").is_some());
        assert!(server.get("args").is_some());
        assert!(server.get("env").is_some());
    }

    #[test]
    fn error_dto_serializes_with_kind_discriminant() {
        let dto = McpConfigErrorDto::from(&McpConfigError::WorkspaceUnavailable {
            path: PathBuf::from("/nope"),
            message: "missing".into(),
        });
        let v: serde_json::Value = serde_json::to_value(&dto).unwrap();
        assert_eq!(v.get("kind").and_then(|x| x.as_str()), Some("workspaceUnavailable"));
        assert_eq!(v.get("path").and_then(|x| x.as_str()), Some("/nope"));
    }
}
