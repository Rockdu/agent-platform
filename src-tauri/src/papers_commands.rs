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

use std::sync::Arc;

use papers_plugin::{
    build_arxiv_url, fetch_arxiv_with_retry, parse_arxiv_atom, PaperRecord, PapersStore,
    ARXIV_API_DEFAULT, MAX_RETRY_ATTEMPTS,
};
use serde::Serialize;
use tauri::State;

use crate::papers_scheduler::PapersScheduler;

/// Tauri-managed handle exposed by `lib.rs::run()`.
pub struct PapersHandle {
    pub store: Arc<PapersStore>,
    pub scheduler: PapersScheduler,
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
pub async fn papers_search(
    handle: State<'_, PapersHandle>,
    query: String,
) -> Result<Vec<PaperRecord>, String> {
    let store = handle.store.clone();
    let q = query.clone();
    let result: Result<Vec<PaperRecord>, String> = tokio::task::spawn_blocking(move || {
        if q.trim().is_empty() {
            return Err("empty query".to_string());
        }
        if let Some(wait) = store
            .rate_limit_wait()
            .map_err(|e| e.to_string())?
        {
            return Err(format!("rate limited: wait {wait}s"));
        }
        store.touch_rate_limit().map_err(|e| e.to_string())?;
        let url = build_arxiv_url(ARXIV_API_DEFAULT, &q, 10);
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| e.to_string())?;
        let body = fetch_arxiv_with_retry(&client, &url, MAX_RETRY_ATTEMPTS)
            .map_err(|e| e.to_string())?;
        let mut parsed = parse_arxiv_atom(&body, 10).map_err(|e| e.to_string())?;
        for r in parsed.iter_mut() {
            r.source = "manual".to_string();
        }
        store.insert_dedup(&parsed, &q).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?;
    result
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
