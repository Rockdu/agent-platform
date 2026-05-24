//! Tauri command surface for the Papers tab frontend.
//!
//! The Papers tab calls these commands via Tauri invoke. Every
//! command requires a `capability` argument — the opaque mount
//! handle (`cap_v1.<mount-uuid>.<nonce>`) the host minted when the
//! Papers plugin was mounted via `PluginRoot` / `mountPlugin`. The
//! handle is verified against `MountRegistry` on every call via
//! `dispatcher::authorize_capability_handle`, so other plugins or
//! arbitrary frontend code in the shared WebView cannot invoke
//! these commands and bypass the user-confirmed opt-in flow or
//! trigger network calls without holding a valid Papers mount.
//!
//! All write paths funnel through `PapersStore`; the same store the
//! orchestrator's papers-plugin sidecar opens, so the data is
//! consistent regardless of which side mutates.

use std::path::PathBuf;
use std::sync::Arc;

use papers_plugin::{FetchPurpose, PaperRecord, PapersStore, RATE_LIMIT_SECONDS};
use serde::Serialize;
use tauri::State;

use crate::dispatcher::{authorize_capability_handle, MountRegistry};
use crate::papers_scheduler::PapersScheduler;
use crate::papers_sidecar_client::{fetch_via_sidecar, resolve_binary_path};

/// Plugin id the capability must belong to.
const PAPERS_PLUGIN_ID: &str = "papers";

/// Verify the caller presented a valid `cap_v1.*` mount handle whose
/// `plugin_id` is `"papers"`. Returns the standard dispatcher
/// `DispatchErrorDto` stringified so the React UI surfaces an
/// actionable error.
fn require_papers_capability(
    registry: &MountRegistry,
    capability: &str,
) -> Result<(), String> {
    if capability.is_empty() {
        return Err("capability_missing: empty capability handle".to_string());
    }
    authorize_capability_handle(registry, capability, PAPERS_PLUGIN_ID)
        .map(|_entry| ())
        .map_err(|err| format!("{err:?}"))
}

/// Tauri-managed handle exposed by `lib.rs::run()`.
pub struct PapersHandle {
    pub store: Arc<PapersStore>,
    pub scheduler: PapersScheduler,
    /// Directory the sidecar binary resolver walks (repo root in dev).
    pub binary_root: PathBuf,
    pub bundle_resource_root: Option<PathBuf>,
    /// Sidecar APP_DATA_DIR — also where the host opens `PapersStore`.
    pub app_data_dir: PathBuf,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OptInStateDto {
    pub enabled: Option<bool>,
}

#[tauri::command]
pub fn papers_list_recent(
    handle: State<'_, PapersHandle>,
    registry: State<'_, MountRegistry>,
    capability: String,
    limit: Option<i64>,
    source: Option<String>,
) -> Result<Vec<PaperRecord>, String> {
    require_papers_capability(&registry, &capability)?;
    handle
        .store
        .list_recent_by_source(source.as_deref(), limit.unwrap_or(20))
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn papers_get_opt_in(
    handle: State<'_, PapersHandle>,
    registry: State<'_, MountRegistry>,
    capability: String,
) -> Result<OptInStateDto, String> {
    require_papers_capability(&registry, &capability)?;
    handle
        .store
        .get_opt_in()
        .map(|enabled| OptInStateDto { enabled })
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn papers_set_opt_in(
    handle: State<'_, PapersHandle>,
    registry: State<'_, MountRegistry>,
    capability: String,
    enabled: bool,
) -> Result<(), String> {
    require_papers_capability(&registry, &capability)?;
    handle.store.set_opt_in(enabled).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn papers_toggle_star(
    handle: State<'_, PapersHandle>,
    registry: State<'_, MountRegistry>,
    capability: String,
    arxiv_id: String,
    starred: bool,
) -> Result<(), String> {
    require_papers_capability(&registry, &capability)?;
    handle
        .store
        .toggle_star(&arxiv_id, starred)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn papers_mark_read(
    handle: State<'_, PapersHandle>,
    registry: State<'_, MountRegistry>,
    capability: String,
    arxiv_id: String,
) -> Result<(), String> {
    require_papers_capability(&registry, &capability)?;
    handle
        .store
        .mark_read(&arxiv_id)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn papers_search(
    handle: State<'_, PapersHandle>,
    registry: State<'_, MountRegistry>,
    capability: String,
    query: String,
) -> Result<Vec<PaperRecord>, String> {
    require_papers_capability(&registry, &capability)?;
    // arXiv HTTP runs inside the papers-plugin sidecar process, not
    // here. The host owns timing (none for manual) and the SQLite
    // reads that the UI consumes afterwards. The sidecar opens the
    // same SQLite via APP_DATA_DIR; manual fetches stamp `source =
    // "manual"` and do NOT advance `last_fired_at`.
    let binary_root = handle.binary_root.clone();
    let bundle_resource_root = handle.bundle_resource_root.clone();
    let app_data_dir = handle.app_data_dir.clone();
    tokio::task::spawn_blocking(move || {
        let binary = resolve_binary_path(&binary_root, bundle_resource_root.as_deref())
            .map_err(|e| e.to_string())?;
        fetch_via_sidecar(
            &binary,
            &app_data_dir,
            &app_data_dir,
            &query,
            FetchPurpose::ManualSearch,
        )
        .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CooldownStateDto {
    pub last_fetched_at: Option<String>,
    pub last_error: Option<String>,
    pub seconds_until_ready: u64,
    pub rate_limit_seconds: u64,
}

#[tauri::command]
pub fn papers_get_cooldown_state(
    handle: State<'_, PapersHandle>,
    registry: State<'_, MountRegistry>,
    capability: String,
) -> Result<CooldownStateDto, String> {
    require_papers_capability(&registry, &capability)?;
    let store = &handle.store;
    let wait = store.rate_limit_wait().map_err(|e| e.to_string())?;
    let last_error = store.last_error().map_err(|e| e.to_string())?;
    // Convert last_fired_at to ISO 8601 for display via list_recent's
    // existing wire shape; the UI just renders it textually.
    let last_fetched_unix = store.last_fired_at_unix().map_err(|e| e.to_string())?;
    let last_fetched_at = last_fetched_unix.map(papers_plugin::iso8601_from_unix);
    Ok(CooldownStateDto {
        last_fetched_at,
        last_error,
        seconds_until_ready: wait.unwrap_or(0),
        rate_limit_seconds: RATE_LIMIT_SECONDS,
    })
}

/// Trigger an immediate scheduler fire (the "Refresh now" button).
/// Respects the rate-limit gate; returns an error string with the
/// remaining wait if the gate fires.
#[tauri::command]
pub async fn papers_refresh_now(
    handle: State<'_, PapersHandle>,
    registry: State<'_, MountRegistry>,
    capability: String,
) -> Result<usize, String> {
    require_papers_capability(&registry, &capability)?;
    let sched = handle.scheduler.clone();
    sched.fire().await.map_err(|e| e.to_string())
}

/// Open an arXiv URL in the system browser. Tauri intercepts
/// `target="_blank"` anchors so a plain link does nothing; this
/// command shells out to the platform opener like the IDE/Finder
/// handoff does. Only `http://` / `https://` URLs are accepted —
/// anything else (file://, custom schemes, shell metacharacters) is
/// rejected so the command can't be abused to launch arbitrary
/// handlers.
#[tauri::command]
pub fn papers_open_url(
    registry: State<'_, MountRegistry>,
    capability: String,
    url: String,
) -> Result<(), String> {
    require_papers_capability(&registry, &capability)?;
    let parsed = url.trim();
    if !is_openable_http_url(parsed) {
        return Err(format!("refusing to open non-http(s) or malformed url: {parsed}"));
    }
    open_external_url(parsed).map_err(|e| e.to_string())
}

/// True only for plain `http(s)://` URLs with no whitespace or control
/// characters. Used to gate `papers_open_url` so the command can't
/// shell-launch `file://`, custom schemes, or anything containing
/// shell-confusing whitespace; arXiv abstract URLs always satisfy it.
fn is_openable_http_url(url: &str) -> bool {
    (url.starts_with("https://") || url.starts_with("http://"))
        && !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

fn open_external_url(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    let (cmd, args): (&str, Vec<&str>) = ("/usr/bin/open", vec![url]);
    #[cfg(target_os = "linux")]
    let (cmd, args): (&str, Vec<&str>) = ("xdg-open", vec![url]);
    #[cfg(target_os = "windows")]
    let (cmd, args): (&str, Vec<&str>) = ("cmd", vec!["/C", "start", "", url]);
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let (cmd, args): (&str, Vec<&str>) = ("xdg-open", vec![url]);

    std::process::Command::new(cmd)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatcher::{mount_inner, MountRegistry};

    /// Mint a valid papers mount and return the handle string.
    fn make_valid_papers_capability() -> (MountRegistry, String) {
        let registry = MountRegistry::new();
        let resp = mount_inner(&registry, PAPERS_PLUGIN_ID, Some("tab-test"))
            .expect("papers is a known plugin (codegen registers it)");
        (registry, resp.handle)
    }

    #[test]
    fn require_papers_capability_rejects_empty_string() {
        let registry = MountRegistry::new();
        let err = require_papers_capability(&registry, "").unwrap_err();
        assert!(err.contains("empty"), "got: {err}");
    }

    #[test]
    fn require_papers_capability_rejects_malformed_handle() {
        let registry = MountRegistry::new();
        let err = require_papers_capability(&registry, "not-a-cap-handle").unwrap_err();
        // dispatcher returns capability_invalid for malformed envelopes.
        assert!(
            err.contains("capability_invalid"),
            "got: {err}"
        );
    }

    #[test]
    fn require_papers_capability_rejects_capability_for_another_plugin() {
        // Mount a NON-papers plugin and try to use its handle for papers.
        let registry = MountRegistry::new();
        let resp =
            mount_inner(&registry, "example-notes", Some("tab-other")).expect("mount notes");
        let err = require_papers_capability(&registry, &resp.handle).unwrap_err();
        // dispatcher returns capability_mismatched or capability_expired
        // depending on whether the mount owner index has the row; both
        // are rejections.
        assert!(
            err.contains("capability_mismatched") || err.contains("capability_expired"),
            "expected capability rejection; got: {err}"
        );
    }

    #[test]
    fn require_papers_capability_accepts_valid_papers_handle() {
        let (registry, handle) = make_valid_papers_capability();
        require_papers_capability(&registry, &handle).expect("valid papers handle must pass");
    }

    #[test]
    fn require_papers_capability_rejects_after_unmount() {
        let (registry, handle) = make_valid_papers_capability();
        require_papers_capability(&registry, &handle).expect("first call must pass");
        crate::dispatcher::unmount_inner(&registry, &handle, Some("tab-test"))
            .expect("unmount must succeed");
        // After unmount the capability is no longer valid.
        let err = require_papers_capability(&registry, &handle).unwrap_err();
        assert!(err.contains("capability_expired"), "got: {err}");
    }

    #[test]
    fn is_openable_http_url_accepts_arxiv_and_rejects_other_schemes() {
        assert!(is_openable_http_url("https://arxiv.org/abs/2401.00001"));
        assert!(is_openable_http_url("http://export.arxiv.org/abs/2401.00001"));
        // Non-http(s) schemes and shell-confusing inputs are rejected.
        assert!(!is_openable_http_url("file:///etc/passwd"));
        assert!(!is_openable_http_url("javascript:alert(1)"));
        assert!(!is_openable_http_url("ftp://example.com"));
        assert!(!is_openable_http_url("https://arxiv.org/abs/2401 foo"));
        assert!(!is_openable_http_url("https://arxiv.org/abs/\n2401"));
        assert!(!is_openable_http_url(""));
    }
}
