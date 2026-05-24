//! Tauri command surface for the Papers tab frontend.
//!
//! The Papers tab calls these commands via Tauri invoke (the plugin
//! capability surface routes paper commands through here instead of
//! through the per-plugin `commands.<name>(args, capability)`
//! wrappers — papers is special-cased because the host owns the
//! scheduler and shared SQLite handle).
//!
//! All write paths funnel through `PapersStore`; the same store the
//! orchestrator's papers-plugin sidecar opens, so the data is
//! consistent regardless of which side mutates.

use std::path::PathBuf;
use std::sync::Arc;

use papers_plugin::{FetchPurpose, PaperRecord, PapersStore, RATE_LIMIT_SECONDS};
use serde::Serialize;
use tauri::State;

use crate::papers_scheduler::PapersScheduler;
use crate::papers_sidecar_client::{fetch_via_sidecar, resolve_binary_path};

/// Tauri-managed handle exposed by `lib.rs::run()`.
pub struct PapersHandle {
    pub store: Arc<PapersStore>,
    pub scheduler: PapersScheduler,
    pub workspace_root: PathBuf,
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
    limit: Option<i64>,
) -> Result<Vec<PaperRecord>, String> {
    handle
        .store
        .list_recent(limit.unwrap_or(20))
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn papers_get_opt_in(handle: State<'_, PapersHandle>) -> Result<OptInStateDto, String> {
    handle
        .store
        .get_opt_in()
        .map(|enabled| OptInStateDto { enabled })
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn papers_set_opt_in(handle: State<'_, PapersHandle>, enabled: bool) -> Result<(), String> {
    handle.store.set_opt_in(enabled).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn papers_toggle_star(
    handle: State<'_, PapersHandle>,
    arxiv_id: String,
    starred: bool,
) -> Result<(), String> {
    handle
        .store
        .toggle_star(&arxiv_id, starred)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn papers_mark_read(
    handle: State<'_, PapersHandle>,
    arxiv_id: String,
) -> Result<(), String> {
    handle
        .store
        .mark_read(&arxiv_id)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn papers_search(
    handle: State<'_, PapersHandle>,
    query: String,
) -> Result<Vec<PaperRecord>, String> {
    // arXiv HTTP runs inside the papers-plugin sidecar process, not
    // here. The host owns timing (none for manual) and the SQLite
    // reads that the UI consumes afterwards. The sidecar opens the
    // same SQLite via APP_DATA_DIR; manual fetches stamp `source =
    // "manual"` and do NOT advance `last_fired_at`.
    let workspace_root = handle.workspace_root.clone();
    let app_data_dir = handle.app_data_dir.clone();
    let q = query.clone();
    tokio::task::spawn_blocking(move || {
        let binary = resolve_binary_path(&workspace_root).map_err(|e| e.to_string())?;
        fetch_via_sidecar(
            &binary,
            &app_data_dir,
            &workspace_root,
            &q,
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
) -> Result<CooldownStateDto, String> {
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
pub async fn papers_refresh_now(handle: State<'_, PapersHandle>) -> Result<usize, String> {
    let sched = handle.scheduler.clone();
    let result = sched.fire().await;
    match result {
        Ok(count) => Ok(count),
        Err(e) => Err(e.to_string()),
    }
}
